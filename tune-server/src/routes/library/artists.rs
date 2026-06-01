use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::track_repo::TrackRepo;

use super::{Pagination, api_cache_get, api_cache_set, artwork_cache_dir, now_iso_utc};

#[derive(Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct ImageReportBody {
    reason: Option<String>,
}

pub(super) async fn list_artists(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = ArtistRepo::new(state.db);
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let total = repo.count().unwrap_or(0);
    let items = repo.list(limit, offset).unwrap_or_default();
    Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
}

pub(super) async fn get_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db);
    match repo.get(id) {
        Ok(Some(artist)) => Json(json!(artist)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(super) async fn artist_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!({"artist": artist.name, "bio": null, "error": "no MusicBrainz ID"}))
            .into_response();
    };
    let lang = q.lang.as_deref().unwrap_or("fr");
    let cache_key = format!("cache:bio:{mbid}:{lang}");
    if let Some(cached) = api_cache_get(&state.db, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/bio?lang={lang}"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            api_cache_set(&state.db, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "bio": null})).into_response(),
    }
}

pub(super) async fn artist_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!({"artist": artist.name, "artists": []})).into_response();
    };
    let cache_key = format!("cache:similar:{mbid}");
    if let Some(cached) = api_cache_get(&state.db, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/similar"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            api_cache_set(&state.db, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

pub(super) async fn artist_metadata(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::new(state.db.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!(artist)).into_response();
    };
    let cache_key = format!("cache:meta:{mbid}");
    if let Some(cached) = api_cache_get(&state.db, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            api_cache_set(&state.db, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!(artist)).into_response(),
    }
}

pub(super) async fn artist_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = AlbumRepo::new(state.db);
    let items = repo.list_by_artist(id).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

pub(super) async fn artist_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::new(state.db);
    let items = repo.list_by_artist(id).unwrap_or_default();
    Json(json!(items))
}

pub(super) async fn artist_timeline(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::new(state.db.clone());
    let artist_repo = ArtistRepo::new(state.db);
    let artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut albums = repo.list_by_artist(id).unwrap_or_default();
    albums.sort_by(|a, b| a.year.unwrap_or(0).cmp(&b.year.unwrap_or(0)));
    let items: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!({
        "artist": artist.name,
        "artist_id": id,
        "albums": items,
    }))
    .into_response()
}

pub(super) async fn artist_image_upload(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let artist_repo = ArtistRepo::new(state.db);
    let mut artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "artist not found"})),
            )
                .into_response();
        }
    };
    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }
    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };
    let cache_dir = artwork_cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();
    let hash = tune_core::artwork::artwork_hash(&format!("artist-{id}"));
    let path = cache_dir.join(format!("{hash}.{ext}"));
    if std::fs::write(&path, &data).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }
    artist.image_path = Some(hash.clone());
    artist.image_source = Some("upload".into());
    artist_repo.update(&artist).ok();
    Json(json!({
        "artist_id": id,
        "hash": hash,
        "size": data.len(),
    }))
    .into_response()
}

pub(super) async fn artist_image_report(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ImageReportBody>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::new(state.db);
    let key = format!("reported_artist_image_{id}");
    let val = json!({
        "artist_id": id,
        "reason": body.reason,
        "reported_at": now_iso_utc(),
    });
    settings.set(&key, &val.to_string()).ok();
    Json(json!({"reported": true, "artist_id": id}))
}
