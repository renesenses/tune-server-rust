use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::track_repo::TrackRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::metadata::{MetadataUpdate, write_metadata};

use crate::state::AppState;

#[derive(Deserialize)]
struct TrackEdit {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    year: Option<u32>,
    composer: Option<String>,
    label: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct AlbumEdit {
    title: Option<String>,
    artist_name: Option<String>,
    genre: Option<String>,
    year: Option<i32>,
    label: Option<String>,
}

#[derive(Deserialize)]
struct ArtistEdit {
    name: Option<String>,
    sort_name: Option<String>,
    bio: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tracks/{id}/edit", post(edit_track))
        .route("/albums/{id}/edit", post(edit_album))
        .route("/artists/{id}/edit", post(edit_artist))
        .route("/doubtful", axum::routing::get(list_doubtful_metadata))
}

async fn edit_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<TrackEdit>,
) -> impl IntoResponse {
    let repo = TrackRepo::new(state.db.clone());
    let mut track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref file_path) = track.file_path {
        let update = MetadataUpdate {
            title: body.title.clone(),
            artist: body.artist.clone(),
            album: body.album.clone(),
            album_artist: body.album_artist.clone(),
            genre: body.genre.clone(),
            track_number: body.track_number,
            disc_number: body.disc_number,
            year: body.year,
            composer: body.composer.clone(),
            label: body.label.clone(),
        };

        if let Err(e) = write_metadata(std::path::Path::new(file_path), &update) {
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("tag write failed: {e}")).into_response();
        }
    }

    if let Some(ref v) = body.title { track.title = v.clone(); }
    if let Some(ref v) = body.artist { track.artist_name = Some(v.clone()); }
    if let Some(ref v) = body.album { track.album_title = Some(v.clone()); }
    if let Some(ref v) = body.genre { track.genre = Some(v.clone()); }
    if let Some(v) = body.track_number { track.track_number = v as i32; }
    if let Some(v) = body.disc_number { track.disc_number = v as i32; }
    if let Some(v) = body.year { track.year = Some(v as i32); }
    if let Some(ref v) = body.composer { track.composer = Some(v.clone()); }
    if let Some(ref v) = body.label { track.label = Some(v.clone()); }

    repo.update(&track).ok();

    Json(json!({ "status": "ok", "track_id": id })).into_response()
}

async fn edit_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AlbumEdit>,
) -> impl IntoResponse {
    let repo = AlbumRepo::new(state.db);
    let mut album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.title { album.title = v.clone(); }
    if let Some(ref v) = body.genre { album.genre = Some(v.clone()); }
    if let Some(v) = body.year { album.year = Some(v); }
    if let Some(ref v) = body.label { album.label = Some(v.clone()); }

    repo.update(&album).ok();

    Json(json!({ "status": "ok", "album_id": id })).into_response()
}

async fn edit_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ArtistEdit>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db);
    let mut artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.name { artist.name = v.clone(); }
    if let Some(ref v) = body.sort_name { artist.sort_name = Some(v.clone()); }
    if let Some(ref v) = body.bio { artist.bio = Some(v.clone()); }

    repo.update(&artist).ok();

    Json(json!({ "status": "ok", "artist_id": id })).into_response()
}

async fn list_doubtful_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let track_repo = TrackRepo::new(state.db);
    // Find tracks with suspicious metadata: missing artist, very short duration, etc.
    let all_tracks = track_repo.search("", 99999).unwrap_or_default();
    let doubtful: Vec<serde_json::Value> = all_tracks
        .iter()
        .filter(|t| {
            let no_artist = t.artist_name.as_ref().map(|a| a.is_empty() || a == "Unknown Artist").unwrap_or(true);
            let very_short = t.duration_ms > 0 && t.duration_ms < 5000;
            let no_album = t.album_title.as_ref().map(|a| a.is_empty()).unwrap_or(true);
            no_artist || very_short || no_album
        })
        .map(|t| {
            let mut reasons = Vec::new();
            if t.artist_name.as_ref().map(|a| a.is_empty() || a == "Unknown Artist").unwrap_or(true) {
                reasons.push("missing_artist");
            }
            if t.duration_ms > 0 && t.duration_ms < 5000 {
                reasons.push("very_short");
            }
            if t.album_title.as_ref().map(|a| a.is_empty()).unwrap_or(true) {
                reasons.push("missing_album");
            }
            json!({
                "id": t.id,
                "title": t.title,
                "artist_name": t.artist_name,
                "album_title": t.album_title,
                "duration_ms": t.duration_ms,
                "reasons": reasons,
            })
        })
        .collect();
    Json(json!({
        "items": doubtful,
        "total": doubtful.len(),
    }))
    .into_response()
}
