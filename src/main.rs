mod net;
mod sound;
mod spotify;
mod types;

use librespot::core::SpotifyId;
use log::{debug, info};
use rspotify::model::Id;
use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio_rustls::rustls;
use tokio_util::sync::CancellationToken;
use types::{Config, MumbleMsg, PlayerAction, Song};

pub mod mumble_proto {
    include!(concat!(env!("OUT_DIR"), "/mumble_proto.rs"));
}

fn load_config(filename: &str) -> anyhow::Result<Config> {
    let file = std::fs::File::open(filename)?;
    let reader = std::io::BufReader::new(file);

    let cfg = serde_json::from_reader(reader)?;

    Ok(cfg)
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum PlayerState {
    Ready,
    Playing,
    Paused,
    Stopped,
}

async fn player_task(
    mut queue_recv: mpsc::Receiver<PlayerAction>,
    msg_sender: mpsc::Sender<MumbleMsg>,
) -> anyhow::Result<()> {
    let mut queue: VecDeque<types::Song> = VecDeque::new();

    let mut state = PlayerState::Ready;

    let (finish_send, mut finish_recv) = mpsc::channel::<()>(1);

    let mut cancel_tok = CancellationToken::new();

    let streamer = sound::AudioSender::new(msg_sender.clone(), finish_send.clone());

    loop {
        tokio::select! {
            action = queue_recv.recv() => {
                let action = action.unwrap();
                match action {
                    PlayerAction::PlaySong(song) => {
                        if state == PlayerState::Playing
                        {
                            net::send_text_message(
                                &msg_sender,
                                format!("Enqueueing song: {}", song.name)
                            ).await?;
                        }
                        queue.push_back(song);

                        if state == PlayerState::Stopped {
                            state = PlayerState::Ready;
                        }
                    },
                    PlayerAction::Next => {
                        if state == PlayerState::Playing || state == PlayerState::Paused {
                            cancel_tok.cancel();
                            cancel_tok = CancellationToken::new();
                            streamer.stop().await?;
                        }
                        state = PlayerState::Ready;
                    },
                    PlayerAction::Stop => {
                        if state == PlayerState::Playing || state == PlayerState::Paused {
                            cancel_tok.cancel();
                            cancel_tok = CancellationToken::new();
                            streamer.stop().await?;
                        }

                        state = PlayerState::Stopped;
                    },
                    PlayerAction::Pause => {
                        if state == PlayerState::Playing {
                            debug!("Pausing streamer.");
                            streamer.stop().await?;
                            state = PlayerState::Paused;
                        }
                    },
                    PlayerAction::Resume => {
                        if state == PlayerState::Paused {
                            debug!("Resuming paused streamer.");
                            streamer.resume().await;
                            state = PlayerState::Playing;
                        }
                    },
                    PlayerAction::ShowQueue => {
                        let mut output = String::from("Songs in queue: ");
                        for (i, song) in queue.iter().enumerate()
                        {
                            output.push_str(&song.name);
                            if i != queue.len() - 1
                            {
                                output.push_str(", ");
                            }
                        }

                        net::send_text_message(&msg_sender, &output).await?;
                    },
                    PlayerAction::SetVolume(vol) => {
                        streamer.set_volume(vol).await;
                    }
                }
            },
            _ = finish_recv.recv() => {
                state = PlayerState::Ready;
            }
        }

        if state == PlayerState::Ready && !queue.is_empty() {
            debug!("Starting new song playback...");
            let song = queue.pop_front().unwrap();

            net::send_text_message(&msg_sender, format!("Playing song: {}", song.name)).await?;

            let (sink, source) = mpsc::channel(32);

            tokio::spawn(spotify::play_song(
                SpotifyId::from_uri(&song.id).unwrap(),
                sink,
                cancel_tok.clone(),
            ));

            streamer.start(source).await?;

            state = PlayerState::Playing;
        }
    }
}

const SPOTIFY_TRACK_URL_BASE: &str = "https://open.spotify.com/track/";
const SPOTIFY_PLAYLIST_URL_BASE: &str = "https://open.spotify.com/playlist/";

fn tag_stripper(input: &str) -> String {
    let mut output = String::new();

    let mut within_tag = false;

    for c in input.chars() {
        if c == '<' {
            within_tag = true;
        }

        if !within_tag {
            output.push(c);
        }

        if c == '>' {
            within_tag = false;
        }
    }

    output
}

async fn handle_message(
    msg: &MumbleMsg,
    queue_sink: &mpsc::Sender<PlayerAction>,
    cfg: &Config,
) -> anyhow::Result<()> {
    if let MumbleMsg::TextMessage(msg) = msg {
        if msg.message.starts_with(".") {
            let (cmd, arg) = msg.message.split_once(' ').unwrap_or((&msg.message, ""));
            match cmd {
                ".stop" => {
                    queue_sink.send(PlayerAction::Stop).await?;
                }
                ".sp" => {
                    let arg = tag_stripper(arg);
                    if !arg.is_empty() {
                        let song = if arg.starts_with(SPOTIFY_TRACK_URL_BASE) {
                            let rest = &arg[SPOTIFY_TRACK_URL_BASE.len()..];

                            let track_id = if let Some(idx) = rest.find('?') {
                                &rest[..idx]
                            } else {
                                rest
                            };

                            let uri = format!("spotify:track:{}", track_id);
                            debug!("Loading track by URI: {}", uri);

                            let song = spotify::get_track_by_id(cfg, &uri).await?;

                            Some(song)
                        } else {
                            spotify::search_song(cfg, &arg).await?.first().cloned()
                        };

                        if let Some(song) = song {
                            queue_sink.send(PlayerAction::PlaySong(song)).await?;
                        }
                    }
                }
                ".spplaylist" => {
                    let arg = tag_stripper(arg);
                    if arg.starts_with(SPOTIFY_PLAYLIST_URL_BASE) {
                        let playlist_id = if let Some(idx) = arg.find('?') {
                            &arg[SPOTIFY_PLAYLIST_URL_BASE.len()..idx]
                        } else {
                            &arg[SPOTIFY_PLAYLIST_URL_BASE.len()..]
                        };

                        let uri = format!("spotify:playlist:{}", playlist_id);
                        debug!("Loading tracks in playlist: {}", uri);

                        let songs = spotify::get_playlist_tracks_by_id(cfg, &uri).await?;

                        for song in songs {
                            queue_sink.send(PlayerAction::PlaySong(song)).await?;
                        }
                    }
                }
                ".show" => {
                    queue_sink.send(PlayerAction::ShowQueue).await?;
                }
                ".next" => {
                    queue_sink.send(PlayerAction::Next).await?;
                }
                ".pause" => {
                    queue_sink.send(PlayerAction::Pause).await?;
                }
                ".resume" => {
                    queue_sink.send(PlayerAction::Resume).await?;
                }
                ".v" => {
                    if let Ok(v) = arg.parse::<u8>() {
                        queue_sink
                            .send(PlayerAction::SetVolume(v as f64 / 100.0))
                            .await?;
                    }
                }
                _ => {
                    debug!("Unhandled command {:?}", cmd);
                }
            }
        }
    } else {
        match msg {
            MumbleMsg::ServerSync(_) => {
                info!("ServerSync received, connected to server.");
            }
            _ => {}
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let _ = rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .unwrap();

    let cfg = load_config("config.json").expect("config file");

    let (msg_sender, mut msg_receiver) = net::init(&cfg).await?;

    let (queue_sink, queue_source) = mpsc::channel(1);

    let mut player_handle = tokio::spawn(player_task(queue_source, msg_sender.clone()));

    'outer: loop {
        tokio::select! {
            res = &mut player_handle => {
                res.unwrap()?;
                break 'outer;
            }
            msg = msg_receiver.recv() => {
                if let Some(msg) = msg {
                    handle_message(&msg, &queue_sink, &cfg).await?;
                }
            }
        }
    }

    Ok(())
}
