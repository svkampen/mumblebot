mod resampling_sink;

use futures::TryStreamExt;
use librespot::{
    core::{Session, SessionConfig, SpotifyId, cache::Cache},
    discovery::Credentials,
    playback::{config::PlayerConfig, mixer::NoOpVolume, player::Player},
};

use log::debug;
use resampling_sink::ResamplingSink;
use tokio::{runtime::Handle, sync::mpsc};
use tokio_util::sync::CancellationToken;

use rspotify::{
    model::{AlbumId, Country, Id, Market, PlayableItem, PlaylistId, SearchResult, TrackId},
    prelude::BaseClient,
};

use std::path::PathBuf;

use crate::types::{Config, Song};

const SPOTIFY_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const SPOTIFY_REDIR_URI: &str = "http://127.0.0.1:8898/login";

use rspotify::model::SearchType;

async fn get_rspotify_session(config: &Config) -> anyhow::Result<rspotify::ClientCredsSpotify> {
    let creds =
        rspotify::Credentials::new(&config.rspotify_client_id, &config.rspotify_client_secret);

    let cfg = rspotify::Config {
        token_cached: true,
        ..Default::default()
    };

    let spot = rspotify::ClientCredsSpotify::with_config(creds, cfg);

    spot.request_token().await?;

    Ok(spot)
}

impl From<rspotify::model::FullTrack> for Song {
    fn from(val: rspotify::model::FullTrack) -> Self {
        Song {
            name: format!("{} - {}", val.artists[0].name, val.name),
            id: val.id.expect("Non-local track should have an ID").uri(),
            song_type: crate::types::SongType::Spotify,
        }
    }
}

impl From<rspotify::model::SimplifiedTrack> for Song {
    fn from(val: rspotify::model::SimplifiedTrack) -> Self {
        Song {
            name: format!("{} - {}", val.artists[0].name, val.name),
            id: val.id.expect("Non-local track should have an ID").uri(),
            song_type: crate::types::SongType::Spotify,
        }
    }
}

pub async fn search_song(config: &Config, query: &str) -> anyhow::Result<Vec<Song>> {
    let spot = get_rspotify_session(config).await?;

    let res = spot
        .search(
            query,
            SearchType::Track,
            Some(Market::Country(Country::Netherlands)),
            None,
            Some(10),
            Some(0),
        )
        .await?;

    match res {
        SearchResult::Tracks(tracks) => {
            let songs = tracks.items.into_iter().map(|ti| ti.into()).collect();
            Ok(songs)
        }
        _ => {
            debug!("No tracks found for search term {:?}", query);
            Ok(Vec::new())
        }
    }
}

pub async fn get_track_by_id(config: &Config, track_uri: &str) -> anyhow::Result<Song> {
    let spot = get_rspotify_session(config).await?;

    let track_info = spot.track(TrackId::from_uri(track_uri)?, None).await?;

    Ok(track_info.into())
}

pub async fn get_playlist_tracks_by_id(
    config: &Config,
    playlist_uri: &str,
) -> anyhow::Result<Vec<Song>> {
    let spot = get_rspotify_session(config).await?;

    let mut playlist_info = spot.playlist_items(PlaylistId::from_uri(playlist_uri)?, None, None);

    let mut tracks = vec![];

    while let Some(item) = playlist_info.try_next().await? {
        if let Some(PlayableItem::Track(track)) = item.track {
            tracks.push(track.into());
        }
    }

    Ok(tracks)
}

pub async fn get_album_tracks_by_id(config: &Config, album_uri: &str) -> anyhow::Result<Vec<Song>> {
    let spot = get_rspotify_session(config).await?;

    let mut album_info = spot.album_track(AlbumId::from_uri(album_uri)?, None);

    let mut tracks = vec![];

    while let Some(track) = album_info.try_next().await? {
        tracks.push(track.into());
    }

    Ok(tracks)
}

async fn get_session() -> Session {
    let session_config = SessionConfig::default();

    let scopes = vec!["streaming"];

    let cache = Cache::new(Some(PathBuf::from(".cache")), None, None, None)
        .expect("Should be able to construct cache");

    let creds = match cache.credentials() {
        Some(creds) => creds,
        None => {
            let client = librespot::oauth::OAuthClientBuilder::new(
                SPOTIFY_CLIENT_ID,
                SPOTIFY_REDIR_URI,
                scopes,
            )
            .open_in_browser()
            .build()
            .expect("got client");

            Credentials::with_access_token(
                client
                    .get_access_token()
                    .expect("got access token")
                    .access_token,
            )
        }
    };

    let session = Session::new(session_config, Some(cache));
    session
        .connect(creds, true)
        .await
        .expect("Connection to Spotify servers");

    session
}

pub async fn play_song(
    song: SpotifyId,
    sink: mpsc::Sender<Vec<i16>>,
    cancel_tok: CancellationToken,
) {
    let session = get_session().await;

    let handle = Handle::current();
    let player_config = PlayerConfig::default();

    let player = Player::new(
        player_config,
        session.clone(),
        Box::new(NoOpVolume),
        move || Box::new(ResamplingSink::new(handle, sink)),
    );

    player.load(song, true, 0);
    tokio::select! {
        _ = player.await_end_of_track() => {}
        _ = cancel_tok.cancelled() => {
            player.stop();
        }
    }
}
