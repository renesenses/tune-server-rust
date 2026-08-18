use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::track_repo::TrackRepo;
use tune_core::license::Feature;
use tune_core::lyrics;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/by-meta", get(lyrics_by_meta))
        .route("/{track_id}", get(get_lyrics_for_track))
        .route("/search", get(search_lyrics))
}

// ---------------------------------------------------------------------------
// Réponses partagées avec GET /library/tracks/{id}/lyrics (contrat identique :
// `{synced, source, lines:[{t_ms,text}]}` / 404 `{"error":"no_lyrics"}`).
// ---------------------------------------------------------------------------

pub(crate) fn no_lyrics_response() -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "no_lyrics"}))).into_response()
}

pub(crate) fn synced_lines_response(
    source: &str,
    lines: &[tune_core::metadata::lyrics::LrcLine],
) -> axum::response::Response {
    let out: Vec<serde_json::Value> = lines
        .iter()
        .map(|l| json!({"t_ms": l.time_ms, "text": l.text}))
        .collect();
    Json(json!({"synced": true, "source": source, "lines": out})).into_response()
}

pub(crate) fn plain_lines_response(source: &str, text: &str) -> Option<axum::response::Response> {
    let out: Vec<serde_json::Value> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| json!({"t_ms": serde_json::Value::Null, "text": l}))
        .collect();
    if out.is_empty() {
        return None;
    }
    Some(Json(json!({"synced": false, "source": source, "lines": out})).into_response())
}

#[derive(Deserialize)]
struct ByMetaParams {
    title: String,
    artist: String,
    /// Album — améliore le matching LRCLIB (pistes streaming Qobuz/Tidal).
    album: Option<String>,
    /// Durée en secondes — LRCLIB s'en sert pour départager les versions ;
    /// précieux pour une piste streaming, absent/0 pour une radio.
    duration: Option<i64>,
}

/// GET /lyrics/by-meta?title=X&artist=Y[&album=Z][&duration=N]
///
/// Paroles par **métadonnées seules**, pour les pistes sans id de
/// bibliothèque : radios (le flux fournit titre + artiste) ET pistes
/// **streaming** Qobuz/Tidal (`current_track.id` est nul — l'endpoint
/// `/library/tracks/{id}/lyrics` ne peut rien pour elles). Pas de fichier,
/// donc ni sidecar .lrc ni tag embarqué : LRCLIB uniquement.
///
/// `album` et `duration` sont optionnels et servent uniquement à affiner le
/// match LRCLIB (une piste streaming les fournit ; une radio non). Même
/// contrat que `GET /library/tracks/{id}/lyrics` :
/// - 200 : `{"synced": bool, "source": "lrclib", "lines": [{"t_ms","text"}]}`
/// - 404 : `{"error": "no_lyrics"}`
///
/// Opt-in par le réglage `lyrics_lrclib_enabled` (même clé que les pistes
/// locales) ; cache `lyrics_cache` sous un id synthétique négatif dérivé de
/// titre+artiste normalisés (`tune_core::lyrics::meta_cache_id`) — les paroles
/// d'un même titre+artiste sont identiques quel que soit l'album, donc l'album
/// n'entre pas dans la clé. Négatifs re-tentés après 14 jours. Jamais de 500 :
/// un échec LRCLIB dégrade en 404 propre (rien mis en cache → retente).
async fn lyrics_by_meta(
    State(state): State<AppState>,
    Query(params): Query<ByMetaParams>,
) -> impl IntoResponse {
    let title = params.title.trim();
    let artist = params.artist.trim();
    if title.is_empty() || artist.is_empty() {
        return no_lyrics_response();
    }
    let album = params
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let duration = params.duration.filter(|d| *d > 0);

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let lrclib_enabled = settings
        .get("lyrics_lrclib_enabled")
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    if !lrclib_enabled {
        return no_lyrics_response();
    }

    let cache_id = tune_core::lyrics::meta_cache_id(title, artist);

    // Cache d'abord (positifs sans expiration ; négatifs re-tentés après 14 j).
    if let Some(entry) = tune_core::lyrics::load_cache_entry(&state.backend, cache_id) {
        if let Some(lrc) = entry
            .synced_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let lines = tune_core::metadata::lyrics::parse_lrc(lrc);
            if !lines.is_empty() {
                return synced_lines_response("lrclib", &lines);
            }
        }
        if let Some(plain) = entry
            .plain_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if let Some(resp) = plain_lines_response("lrclib", plain) {
                return resp;
            }
        }
        if entry.negative_still_fresh() {
            return no_lyrics_response();
        }
    }

    // Radio : ni album ni durée (None) → match titre+artiste. Streaming :
    // album + durée affinent le résultat (versions multiples départagées).
    match tune_core::lyrics::fetch_lrclib_raw(&state.http_client, artist, title, album, duration)
        .await
    {
        Ok(raw) => {
            let raw = raw.unwrap_or_default();
            // Hits ET miss sont mis en cache (miss re-tentés après 14 jours).
            tune_core::lyrics::store_cache_entry(
                &state.backend,
                cache_id,
                title,
                artist,
                raw.synced_lyrics.as_deref(),
                raw.plain_lyrics.as_deref(),
            );
            if let Some(lrc) = raw.synced_lyrics.as_deref() {
                let lines = tune_core::metadata::lyrics::parse_lrc(lrc);
                if !lines.is_empty() {
                    return synced_lines_response("lrclib", &lines);
                }
            }
            if let Some(plain) = raw.plain_lyrics.as_deref() {
                if let Some(resp) = plain_lines_response("lrclib", plain) {
                    return resp;
                }
            }
            no_lyrics_response()
        }
        // Échec réseau/protocole : 404 propre, rien en cache → retentera.
        Err(e) => {
            tracing::debug!(title, artist, error = %e, "lrclib_by_meta_fetch_failed");
            no_lyrics_response()
        }
    }
}

/// GET /lyrics/{track_id}
///
/// Load the track from DB to get title/artist/duration, then fetch
/// lyrics (cache-first, fallback LRCLIB).
///
/// Free tier: plain lyrics only.
/// Premium tier: synced lines + plain text.
async fn get_lyrics_for_track(
    State(state): State<AppState>,
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

    let result = lyrics::get_lyrics(
        &state.backend,
        &state.http_client,
        track_id,
        title,
        artist,
        track.album_title.as_deref(),
        track.duration_ms,
    )
    .await;

    match result {
        Ok(ly) => {
            let is_premium = state.license.check_feature(Feature::SyncedLyrics).await;

            if is_premium {
                Json(json!({
                    "track_id": track_id,
                    "synced": ly.synced,
                    "lines": ly.lines,
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                }))
                .into_response()
            } else {
                // Free tier: plain text only, no synced lines.
                Json(json!({
                    "track_id": track_id,
                    "synced": false,
                    "lines": [],
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                    "premium_required": ly.synced,
                }))
                .into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("lyrics fetch failed: {e}")})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct SearchParams {
    title: String,
    artist: String,
    duration: Option<i64>,
}

/// GET /lyrics/search?title=X&artist=Y&duration=Z
///
/// Search LRCLIB directly (no track_id, no caching).
async fn search_lyrics(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> impl IntoResponse {
    let result = lyrics::fetch_from_lrclib(
        &state.http_client,
        &params.artist,
        &params.title,
        None,
        params.duration,
    )
    .await;

    match result {
        Ok(ly) => {
            let is_premium = state.license.check_feature(Feature::SyncedLyrics).await;

            if is_premium {
                Json(json!({
                    "synced": ly.synced,
                    "lines": ly.lines,
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                }))
                .into_response()
            } else {
                Json(json!({
                    "synced": false,
                    "lines": [],
                    "plain_text": ly.plain_text,
                    "source": ly.source,
                    "premium_required": ly.synced,
                }))
                .into_response()
            }
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": format!("lyrics search failed: {e}")})),
        )
            .into_response(),
    }
}
