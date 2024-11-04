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
                        let mut output = String::from("<b>Songs in queue:</b><br/>");
                        for song in queue.iter() {
                            output.push_str("<br/>");
                            output.push_str(&song.name);
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
                    if !arg.is_empty() {
                        let songs = spotify::search_song(cfg, arg).await?;
                        if !songs.is_empty() {
                            let track = &songs[0];

                            let song = Song {
                                name: format!("{} - {}", track.artists[0].name, track.name),
                                id: track.id.as_ref().unwrap().uri(),
                                song_type: types::SongType::SPOTIFY,
                            };

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

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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
