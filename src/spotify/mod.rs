mod resampling_sink;

use librespot::{
    core::{cache::Cache, Session, SessionConfig, SpotifyId},
    discovery::Credentials,
    playback::{config::PlayerConfig, mixer::NoOpVolume, player::Player},
};

use log::debug;
use resampling_sink::ResamplingSink;
use tokio::{runtime::Handle, sync::mpsc};
use tokio_util::sync::CancellationToken;

use rspotify::{
    model::{Country, Market, SearchResult},
    prelude::BaseClient,
};

use std::path::PathBuf;

use crate::types::Config;

const SPOTIFY_CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";
const SPOTIFY_REDIR_URI: &str = "http://127.0.0.1:8898/login";

use rspotify::model::SearchType;

pub async fn search_song(
    config: &Config,
    query: &str,
) -> anyhow::Result<Vec<rspotify::model::FullTrack>> {
    let creds =
        rspotify::Credentials::new(&config.rspotify_client_id, &config.rspotify_client_secret);

    let cfg = rspotify::Config {
        token_cached: true,
        ..Default::default()
    };

    let spot = rspotify::ClientCredsSpotify::with_config(creds, cfg);

    spot.request_token().await?;

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
        SearchResult::Tracks(tracks) => Ok(tracks.items),
        _ => {
            debug!("No tracks found for search term {:?}", query);
            Ok(Vec::new())
        }
    }
}

async fn get_session() -> Session {
    let session_config = SessionConfig::default();

    let scopes = vec!["streaming"];

    let cache = Cache::new(Some(PathBuf::from(".cache")), None, None, None).unwrap();
    let creds = match cache.credentials() {
        Some(creds) => creds,
        None => {
            let tok =
                librespot::oauth::get_access_token(SPOTIFY_CLIENT_ID, SPOTIFY_REDIR_URI, scopes)
                    .expect("got access token");
            Credentials::with_access_token(tok.access_token)
        }
    };

    let session = Session::new(session_config, Some(cache));
    session
        .connect(creds, true)
        .await
        .expect("Unable to connect to the spotify Servers.");

    return session;
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
