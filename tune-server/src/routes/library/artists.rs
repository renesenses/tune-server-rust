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
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let total = repo.count().unwrap_or(0);
    let items = match repo.list(limit, offset) {
        Ok(artists) => artists,
        Err(e) => {
            tracing::error!(
                error = %e,
                limit,
                offset,
                total,
                "list_artists_query_failed — stats show {total} artists but query returned error"
            );
            Vec::new()
        }
    };
    Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
}

pub(super) async fn get_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(artist)) => Json(json!(artist)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn artist_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
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
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
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
            api_cache_set(&state.backend, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "bio": null})).into_response(),
    }
}

pub(super) async fn artist_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!({"artist": artist.name, "artists": []})).into_response();
    };
    let cache_key = format!("cache:similar:{mbid}");
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
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
            api_cache_set(&state.backend, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

pub(super) async fn artist_metadata(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = repo.get(id).ok().flatten();
    let Some(artist) = artist else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(ref mbid) = artist.musicbrainz_id else {
        return Json(json!(artist)).into_response();
    };
    let cache_key = format!("cache:meta:{mbid}");
    if let Some(cached) = api_cache_get(&state.backend, &cache_key) {
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
            api_cache_set(&state.backend, &cache_key, &data);
            Json(data).into_response()
        }
        _ => Json(json!(artist)).into_response(),
    }
}

pub(super) async fn artist_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_by_artist(id).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

pub(super) async fn artist_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let items = repo.list_by_artist(id).unwrap_or_default();
    Json(json!(items))
}

pub(super) async fn artist_timeline(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match artist_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut albums = repo.list_by_artist(id).unwrap_or_default();
    albums.sort_by(|a, b| a.year.unwrap_or(0).cmp(&b.year.unwrap_or(0)));

    let years: Vec<i32> = albums.iter().filter_map(|a| a.year).collect();
    let mut gaps = Vec::new();
    for w in years.windows(2) {
        if w[1] - w[0] > 1 {
            gaps.push(json!({"from": w[0], "to": w[1], "years": w[1] - w[0]}));
        }
    }

    let items: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!({
        "artist": artist.name,
        "artist_id": id,
        "albums": items,
        "gaps": gaps,
        "career_span": if years.len() >= 2 { Some(json!({"first": years[0], "last": years[years.len()-1], "years": years[years.len()-1] - years[0]})) } else { None },
    }))
    .into_response()
}

pub(super) async fn artist_image(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref image_path) = artist.image_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    if image_path.starts_with("http") {
        return axum::response::Redirect::temporary(image_path).into_response();
    }

    // If it's already a hex hash, use it directly; otherwise hash it
    let hash = if super::artwork_is_hex_hash(image_path) {
        image_path.to_string()
    } else {
        tune_core::library::artwork::artwork_hash(image_path)
    };

    axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}")).into_response()
}

pub(super) async fn artist_image_upload(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
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
    let hash = tune_core::library::artwork::artwork_hash(&format!("artist-{id}"));
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
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let key = format!("reported_artist_image_{id}");
    let val = json!({
        "artist_id": id,
        "reason": body.reason,
        "reported_at": now_iso_utc(),
    });
    settings.set(&key, &val.to_string()).ok();
    Json(json!({"reported": true, "artist_id": id}))
}
