//! Karaoke as a native [`TunePlugin`] (model B, sibling of `tune-dj` #917).
//!
//! Serves karaoke-ready **synced lyrics** — timestamped LRC lines a client can
//! highlight in time with the playhead. Build `tune-server --features karaoke`
//! to get these routes, mounted by the plugin host at `/api/v1/ext/karaoke/…`
//! (the host derives the prefix from `name()` — a plugin never chooses its own).
//!
//! Karaoke is **native**, not WASM: lyrics fetching hits the network (LRCLIB)
//! and caches in the DB, and the WASM runtime has no net/fetch capability.
//!
//! ## Reuse, not reinvention
//!
//! The lyrics pipeline already exists in [`tune_core::lyrics`]. This plugin
//! REUSES [`tune_core::lyrics::get_lyrics`] (cache-first, then LRCLIB) verbatim
//! — it reimplements no LRC parsing and no fetching. It is purely additive: the
//! core `/lyrics/{id}` routes are untouched. What Karaoke adds is a
//! karaoke-shaped surface, including a `/now/{zone_id}` endpoint that ties the
//! lyric lines to a zone's live playback position.
//!
//! Host dependencies are passed explicitly at construction via [`HostServices`]
//! — matching the DJ wiring pattern documented in `tune-server/src/plugins.rs`,
//! so a plugin's real dependencies are visible at the registration site. The
//! router captures those in its own state rather than sharing the host's
//! `AppState`, which keeps `tune-core` free of any `tune-server` type.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::lyrics;
use tune_core::playback::PlaybackManager;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

const PLUGIN_NAME: &str = "karaoke";

/// Host services handed to the Karaoke plugin at construction.
///
/// Passed explicitly (not pulled from [`PluginContext`], which exposes only the
/// DB) so the plugin's real dependencies are visible where it is wired up in
/// `register_builtin_plugins`. Karaoke needs the DB backend (track lookup +
/// lyrics cache), an HTTP client (to fetch+cache from LRCLIB on a cache miss),
/// and the playback manager (to read a zone's now-playing track + position for
/// `/now`). All three are `tune-core` types, so nothing here leaks
/// `tune-server`.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub client: reqwest::Client,
    pub playback: Arc<PlaybackManager>,
}

/// The Karaoke plugin. Owns the services its router needs.
pub struct KaraokePlugin {
    backend: Arc<dyn DbBackend>,
    client: reqwest::Client,
    playback: Arc<PlaybackManager>,
}

impl KaraokePlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            backend: services.backend,
            client: services.client,
            playback: services.playback,
        }
    }
}

#[async_trait]
impl TunePlugin for KaraokePlugin {
    fn name(&self) -> &str {
        PLUGIN_NAME
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Paroles synchronisées façon karaoké (réutilise tune-core::lyrics)"
    }
    // Opt-in: dormant until the user installs it from the plugin manager,
    // rather than running for everyone by default.
    fn default_enabled(&self) -> bool {
        false
    }

    // Hors catalogue (#2090) — pour la raison inverse de DJ. Ce greffon-ci
    // FONCTIONNE : ses trois routes travaillent vraiment. Mais il fait double
    // emploi avec une fonction déjà livrée et déjà atteignable.
    //
    // Le client web sert le karaoké depuis le cœur, sans greffon : le panneau
    // des paroles affiche un bouton « Karaoké » dès que la piste a des paroles
    // synchronisées, et surligne la ligne courante en calculant lui-même son
    // index à partir de la position de lecture — la dernière ligne dont
    // `time <= position`, exactement l'algorithme de `current_line_index` ici.
    // Ces paroles viennent de `/lyrics/{id}`, que ce greffon RÉUTILISE
    // (`tune_core::lyrics::get_lyrics`) au lieu de les produire autrement.
    //
    // Le proposer à l'installation offrirait donc une seconde porte — payée
    // d'une installation et d'un redémarrage — vers ce que l'utilisateur a
    // déjà. Deux karaokés valent moins qu'un.
    //
    // Et cette seconde porte serait la plus étroite : `/now/{zone_id}` abandonne
    // dès que la piste courante n'a pas d'`id` de bibliothèque (« current track
    // is not in the library », plus bas), là où le client sait retomber sur
    // `/lyrics/by-meta` et fait donc marcher le karaoké sur du streaming.
    //
    // Le greffon reste compilé, testé (`tests/karaoke_plugin.rs`) et chargeable
    // en posant `plugin_karaoke_installed=true` : `/now/{zone_id}` garde son
    // intérêt propre pour un client qui n'a pas de boucle de position à lui
    // (une façade embarquée, par exemple). À rebasculer à `true` le jour où un
    // tel client existe.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(
            self.backend.clone(),
            self.client.clone(),
            self.playback.clone(),
        ));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Karaoke is request-driven — the client polls `/lyrics` or `/now`. It
    /// subscribes to no events, so this is a no-op override rather than
    /// receiving every event on the bus for nothing.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

/// Plugin-owned router state. Captures the host services so the router can be a
/// `Router<()>` (as the host requires) without leaking `AppState`.
#[derive(Clone)]
struct KaraokeState {
    backend: Arc<dyn DbBackend>,
    client: reqwest::Client,
    playback: Arc<PlaybackManager>,
}

/// The Karaoke router, `Router<()>` for the plugin host to mount under
/// `/api/v1/ext/karaoke`.
pub fn router(
    backend: Arc<dyn DbBackend>,
    client: reqwest::Client,
    playback: Arc<PlaybackManager>,
) -> Router<()> {
    Router::new()
        .route("/status", get(status))
        .route("/lyrics/{track_id}", get(lyrics_for_track))
        .route("/now/{zone_id}", get(now_for_zone))
        .with_state(KaraokeState {
            backend,
            client,
            playback,
        })
}

/// Compute the index of the currently-active lyric line for a playhead at
/// `position_ms`: the last line whose `time_ms <= position_ms`. Returns `-1`
/// before the first line (or when there are no lines).
fn current_line_index(lines: &[lyrics::LyricLine], position_ms: i64) -> i64 {
    let mut idx: i64 = -1;
    for (i, line) in lines.iter().enumerate() {
        if line.time_ms <= position_ms {
            idx = i as i64;
        } else {
            break;
        }
    }
    idx
}

/// `GET /status` → the plugin's identity, for a client to feature-detect.
async fn status() -> Json<Value> {
    Json(json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "enabled": true,
    }))
}

/// `GET /lyrics/{track_id}` → karaoke-ready structured lyrics for a track.
///
/// Resolves title/artist/duration from the library, then calls
/// [`tune_core::lyrics::get_lyrics`] (cache-first, then LRCLIB). No tier gating
/// — the karaoke surface always returns the synced lines it has.
async fn lyrics_for_track(
    State(state): State<KaraokeState>,
    Path(track_id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(track_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("db error: {e}")})),
            )
                .into_response();
        }
    };

    let title = &track.title;
    let artist = track.artist_name.as_deref().unwrap_or("Unknown");

    match lyrics::get_lyrics(
        &state.backend,
        &state.client,
        track_id,
        title,
        artist,
        track.album_title.as_deref(),
        track.duration_ms,
    )
    .await
    {
        Ok(ly) => Json(json!({
            "track_id": track_id,
            "synced": ly.synced,
            "source": ly.source,
            "lines": ly.lines,
            "plain_text": ly.plain_text,
            "error": Value::Null,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "track_id": track_id,
                "synced": false,
                "lines": [],
                "error": format!("lyrics fetch failed: {e}"),
            })),
        )
            .into_response(),
    }
}

/// `GET /now/{zone_id}` → lyrics for a zone's now-playing track, plus the line
/// index active at the zone's current playback position.
///
/// This is the karaoke payload proper: the client can render the whole lyric
/// scroll and know which line to highlight without recomputing the mapping. It
/// is feasible precisely because the host hands the plugin an
/// `Arc<PlaybackManager>` (a `tune-core` type) at construction, so reading a
/// zone's now-playing track id + position needs nothing from `tune-server`.
async fn now_for_zone(
    State(state): State<KaraokeState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let zone_state = state.playback.get_state(zone_id).await;

    let Some(np) = zone_state.now_playing.as_ref() else {
        return Json(json!({
            "zone_id": zone_id,
            "track_id": Value::Null,
            "position_ms": zone_state.position_ms,
            "current_index": -1,
            "lines": [],
            "error": "nothing playing",
        }))
        .into_response();
    };

    let Some(track_id) = np.track_id else {
        // A streaming source with no library track id: no synced lyrics lookup.
        return Json(json!({
            "zone_id": zone_id,
            "track_id": Value::Null,
            "position_ms": zone_state.position_ms,
            "current_index": -1,
            "lines": [],
            "error": "current track is not in the library",
        }))
        .into_response();
    };

    let title = np.title.clone();
    let artist = np.artist_name.clone().unwrap_or_else(|| "Unknown".into());

    match lyrics::get_lyrics(
        &state.backend,
        &state.client,
        track_id,
        &title,
        &artist,
        np.album_title.as_deref(),
        np.duration_ms,
    )
    .await
    {
        Ok(ly) => {
            let current_index = current_line_index(&ly.lines, zone_state.position_ms);
            Json(json!({
                "zone_id": zone_id,
                "track_id": track_id,
                "position_ms": zone_state.position_ms,
                "current_index": current_index,
                "synced": ly.synced,
                "source": ly.source,
                "lines": ly.lines,
                "error": Value::Null,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "zone_id": zone_id,
                "track_id": track_id,
                "position_ms": zone_state.position_ms,
                "current_index": -1,
                "lines": [],
                "error": format!("lyrics fetch failed: {e}"),
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(time_ms: i64) -> lyrics::LyricLine {
        lyrics::LyricLine {
            time_ms,
            text: String::new(),
        }
    }

    #[test]
    fn current_index_before_first_line_is_minus_one() {
        let lines = vec![line(1000), line(2000)];
        assert_eq!(current_line_index(&lines, 0), -1);
        assert_eq!(current_line_index(&lines, 999), -1);
    }

    #[test]
    fn current_index_tracks_position() {
        let lines = vec![line(1000), line(2000), line(3000)];
        assert_eq!(current_line_index(&lines, 1000), 0);
        assert_eq!(current_line_index(&lines, 1500), 0);
        assert_eq!(current_line_index(&lines, 2000), 1);
        assert_eq!(current_line_index(&lines, 9999), 2);
    }

    #[test]
    fn current_index_empty_is_minus_one() {
        assert_eq!(current_line_index(&[], 5000), -1);
    }
}
