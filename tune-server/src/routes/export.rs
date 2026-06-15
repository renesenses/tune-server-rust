use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tracks.csv", get(export_tracks_csv))
        .route("/albums.csv", get(export_albums_csv))
        .route("/artists.csv", get(export_artists_csv))
}

async fn export_tracks_csv(State(state): State<AppState>) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = repo.list(999999, 0).unwrap_or_default();

    let mut csv = String::from(
        "id,title,artist,album,disc,track,duration_ms,format,sample_rate,bit_depth,file_path\n",
    );
    for t in &tracks {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            t.id.unwrap_or(0),
            csv_escape(&t.title),
            csv_escape(t.artist_name.as_deref().unwrap_or("")),
            csv_escape(t.album_title.as_deref().unwrap_or("")),
            t.disc_number,
            t.track_number,
            t.duration_ms,
            t.format.as_deref().unwrap_or(""),
            t.sample_rate.unwrap_or(0),
            t.bit_depth.unwrap_or(0),
            csv_escape(t.file_path.as_deref().unwrap_or("")),
        ));
    }

    csv_response(csv, "tracks.csv")
}

async fn export_albums_csv(State(state): State<AppState>) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let albums = repo.list(999999, 0).unwrap_or_default();

    let mut csv =
        String::from("id,title,artist,year,genre,format,sample_rate,bit_depth,track_count\n");
    for a in &albums {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{},{},{}\n",
            a.id.unwrap_or(0),
            csv_escape(&a.title),
            csv_escape(a.artist_name.as_deref().unwrap_or("")),
            a.year.unwrap_or(0),
            csv_escape(a.genre.as_deref().unwrap_or("")),
            a.format.as_deref().unwrap_or(""),
            a.sample_rate.unwrap_or(0),
            a.bit_depth.unwrap_or(0),
            a.track_count.unwrap_or(0),
        ));
    }

    csv_response(csv, "albums.csv")
}

async fn export_artists_csv(State(state): State<AppState>) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artists = repo.list(999999, 0).unwrap_or_default();

    let mut csv = String::from("id,name,sort_name,musicbrainz_id\n");
    for a in &artists {
        csv.push_str(&format!(
            "{},{},{},{}\n",
            a.id.unwrap_or(0),
            csv_escape(&a.name),
            csv_escape(a.sort_name.as_deref().unwrap_or("")),
            a.musicbrainz_id.as_deref().unwrap_or(""),
        ));
    }

    csv_response(csv, "artists.csv")
}

fn csv_response(csv: String, filename: &str) -> Result<impl IntoResponse, AppError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    headers.insert(
        "Content-Disposition",
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|e| AppError::internal(e.to_string()))?,
    );
    Ok((StatusCode::OK, headers, csv))
}

fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}
