use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_core::http::streamer::AudioStreamer;

use crate::error::AppError;
use crate::routes::active_profile::DEFAULT_PROFILE_ID;
use crate::state::AppState;

fn favicon_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let after_scheme = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))?;
    let host = after_scheme.split('/').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() || !host.contains('.') {
        return None;
    }
    Some(format!(
        "https://www.google.com/s2/favicons?domain={host}&sz=128"
    ))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Deserialize)]
struct CreateRadio {
    name: String,
    #[serde(alias = "stream_url")]
    url: String,
    #[serde(alias = "homepage_url")]
    homepage: Option<String>,
    logo_url: Option<String>,
    country: Option<String>,
    language: Option<String>,
    genre: Option<String>,
    codec: Option<String>,
    bitrate: Option<i32>,
}

/// Ce qu'a donné une passe de rattrapage des logos de station.
///
/// La fonction rendait un `usize`, et ce compteur mentait par omission : `0`
/// disait à la fois « tout le monde avait déjà son logo », « l'annuaire ne
/// connaît pas ces stations » et « je n'ai pas pu joindre l'annuaire ». Trois
/// situations, trois suites à donner — et dans le troisième cas, aucune trace
/// n'était écrite du tout (`if n > 0` côté démarrage). Un serveur sans
/// vignette ne disait donc pas s'il n'avait rien trouvé ou s'il n'avait pas pu
/// chercher. C'est l'information qui manque pour instruire le fil 1508.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RattrapageLogos {
    /// Stations dont le logo vient d'être posé.
    pub updated: usize,
    /// Stations toujours sans logo APRÈS la passe : l'annuaire a répondu et ne
    /// les connaît ni par URL de flux ni par nom.
    pub sans_logo: usize,
    /// L'annuaire n'a pas répondu, ou sa réponse était vide/illisible. Alors
    /// `updated` et `sans_logo` valent tous deux `0` et ne prouvent RIEN.
    pub annuaire_injoignable: bool,
}

impl RattrapageLogos {
    fn injoignable() -> Self {
        Self {
            annuaire_injoignable: true,
            ..Self::default()
        }
    }
}

/// Backfill missing station logos from the mozaiklabs.fr radio directory.
///
/// The seeded default stations (migration `seed_default_radios`) and any station
/// imported without art have no `logo_url`, so the radio list shows the
/// placeholder mic icon (Pascal, v0.9.21). The public directory at
/// `/api/v1/radios` carries a curated logo per station; match our local rows to
/// it by stream URL (then name) and fill in the absolute logo URL. The web
/// client proxies that URL through the LOCAL server (`artworkUrl` →
/// `/library/artwork/proxy`), so it displays even behind a strict CSP.
///
/// Best-effort and cloud-graceful: any network/parse failure is a no-op (Tune
/// works fully without mozaiklabs.fr). Never overwrites a logo already set.
///
/// Rend un [`RattrapageLogos`] et non un simple compteur : voir la note de ce
/// type — `0` seul est indéchiffrable, et c'est précisément l'information qui
/// manquait pour instruire le fil 1508 (#2421).
pub async fn refresh_radio_logos(state: &AppState) -> RattrapageLogos {
    const DIRECTORY_URL: &str = "https://mozaiklabs.fr/api/v1/radios";
    const BASE: &str = "https://mozaiklabs.fr";

    let directory: Vec<Value> = match tune_core::http::client::shared()
        .get(DIRECTORY_URL)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r.json().await.unwrap_or_default(),
        Err(_) => return RattrapageLogos::injoignable(),
    };

    // Normalize a stream URL for matching: scheme-insensitive, no trailing slash.
    let norm = |u: &str| {
        u.trim()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_string()
    };

    let mut by_url: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut by_name: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for item in &directory {
        let Some(logo) = item
            .get("logo_url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let abs = if logo.starts_with("http") {
            logo.to_string()
        } else {
            format!("{BASE}{logo}")
        };
        if let Some(su) = item.get("stream_url").and_then(|v| v.as_str()) {
            by_url.entry(norm(su)).or_insert_with(|| abs.clone());
        }
        if let Some(nm) = item.get("name").and_then(|v| v.as_str()) {
            by_name
                .entry(nm.trim().to_ascii_lowercase())
                .or_insert_with(|| abs.clone());
        }
    }
    if by_url.is_empty() && by_name.is_empty() {
        // 200 avec un corps vide ou illisible (`unwrap_or_default` ci-dessus
        // avale l'erreur de parsage) : on n'a rien appris, donc on ne conclut
        // rien. C'est un « injoignable », pas un « rien à faire ».
        return RattrapageLogos::injoignable();
    }

    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut bilan = RattrapageLogos::default();
    for mut st in repo.list().unwrap_or_default() {
        if st.logo_url.as_deref().is_some_and(|s| !s.trim().is_empty()) {
            continue; // keep an existing / user-set logo
        }
        let logo = by_url
            .get(&norm(&st.url))
            .or_else(|| by_name.get(&st.name.trim().to_ascii_lowercase()))
            .cloned();
        match logo {
            Some(logo) => {
                st.logo_url = Some(logo);
                if repo.update(&st).is_ok() {
                    bilan.updated += 1;
                } else {
                    bilan.sans_logo += 1;
                }
            }
            // L'annuaire a répondu et ne connaît pas cette station, ni par URL
            // de flux ni par nom : son logo restera vide tant que l'entrée n'y
            // sera pas ajoutée. Quatre stations livrées sont dans ce cas —
            // FIP Pop, FIP Monde, FIP Reggae, FIP Nouveautés (#2421).
            None => bilan.sans_logo += 1,
        }
    }
    if bilan.updated > 0 {
        state.event_bus.emit(
            "library.radios_changed",
            json!({"action": "logos_refreshed", "updated": bilan.updated}),
        );
    }
    tracing::info!(
        updated = bilan.updated,
        sans_logo = bilan.sans_logo,
        "radio_logos_refreshed_from_directory"
    );
    bilan
}

async fn refresh_logos_handler(State(state): State<AppState>) -> Json<Value> {
    let bilan = refresh_radio_logos(&state).await;
    // `updated` reste en tête et garde son nom : c'est le champ que la réponse
    // portait déjà.
    Json(json!({
        "updated": bilan.updated,
        "sans_logo": bilan.sans_logo,
        "annuaire_injoignable": bilan.annuaire_injoignable,
    }))
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_radios).post(create_radio))
        .route("/search", get(search_radios))
        .route("/favorites", get(list_favorites))
        .route("/refresh-logos", post(refresh_logos_handler))
        .route("/add", get(add_from_web))
        .route(
            "/{id}",
            get(get_radio).put(update_radio).delete(delete_radio),
        )
        .route(
            "/{id}/audio.wav",
            get(media_server_radio_audio).head(media_server_radio_audio_head),
        )
        .route("/{id}/favorite", post(toggle_favorite))
        .route("/{id}/play/{zone_id}", post(play_radio))
        .route("/{id}/artwork", post(set_radio_artwork))
        .route("/export.m3u", get(export_radios_m3u))
        .route("/import", post(import_radios))
        .route("/import/m3u", post(import_radios_m3u))
}

/// Possède la session radio pendant toute la vie du corps HTTP. Axum détruit
/// le corps aussi bien à EOF que lorsque le renderer coupe sa socket ; le Drop
/// retire alors la session, ferme son canal et fait sortir le décodeur bloquant
/// dès son prochain envoi.
struct MediaServerRadioSessionGuard {
    streamer: Arc<AudioStreamer>,
    stream_id: Option<String>,
}

impl Drop for MediaServerRadioSessionGuard {
    fn drop(&mut self) {
        let Some(stream_id) = self.stream_id.take() else {
            return;
        };
        let streamer = self.streamer.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    streamer.remove_session(&stream_id).await;
                });
            }
            Err(error) => tracing::warn!(
                stream_id,
                error = %error,
                "media_server_radio_cleanup_without_runtime"
            ),
        }
    }
}

fn with_media_server_radio_cleanup(
    response: Response,
    streamer: Arc<AudioStreamer>,
    stream_id: String,
) -> Response {
    let (parts, body) = response.into_parts();
    let mut data = body.into_data_stream();
    let guard = MediaServerRadioSessionGuard {
        streamer,
        stream_id: Some(stream_id),
    };
    let body = Body::from_stream(async_stream::stream! {
        let _guard = guard;
        while let Some(chunk) = data.next().await {
            yield chunk;
        }
    });
    Response::from_parts(parts, body)
}

async fn media_server_radio_audio_head(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    req_headers: HeaderMap,
) -> Response {
    let repo = RadioRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(_)) => tune_stream_http::live_radio_head_response("audio/wav", &req_headers),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn media_server_radio_audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    req_headers: HeaderMap,
) -> Response {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let radio = match repo.get(id) {
        Ok(Some(radio)) => radio,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    };

    // Le Browse et le HEAD sont sans effet. Seul un renderer qui demande
    // réellement le corps compte comme une tentative d'écoute.
    if let Err(error) = repo.record_play(id) {
        tracing::warn!(radio_id = id, error = %error, "media_server_radio_play_not_recorded");
    }
    let stream_id = state
        .orchestrator
        .create_media_server_radio_session(radio.url.clone())
        .await;
    tracing::info!(
        radio_id = id,
        radio = %radio.name,
        stream_id = %stream_id,
        "media_server_radio_stream_started"
    );

    let response = tune_stream_http::handle_stream(
        Path(format!("{stream_id}.wav")),
        State(state.streamer.sessions_state()),
        req_headers,
    )
    .await;
    with_media_server_radio_cleanup(response, state.streamer, stream_id)
}

async fn list_radios(State(state): State<AppState>) -> Json<Value> {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let items = repo.list().unwrap_or_default();
    Json(json!(items))
}

async fn get_radio(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(r)) => Json(json!(r)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_radio(
    State(state): State<AppState>,
    Json(body): Json<CreateRadio>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let auto_logo = if body.logo_url.is_none() {
        favicon_from_url(body.homepage.as_deref().unwrap_or(&body.url))
    } else {
        None
    };
    let station = RadioStation {
        id: None,
        name: body.name,
        url: body.url,
        homepage: body.homepage,
        logo_url: body.logo_url.or(auto_logo),
        country: body.country,
        language: body.language,
        genre: body.genre,
        codec: body.codec,
        bitrate: body.bitrate,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    };
    match repo.create(&station) {
        Ok(id) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "created", "id": id}),
            );
            // Return the full station so the UI can display it immediately
            let created = repo.get(id).ok().flatten().unwrap_or(RadioStation {
                id: Some(id),
                ..station
            });
            (StatusCode::CREATED, Json(json!(created))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateRadioBody {
    name: Option<String>,
    #[serde(alias = "stream_url")]
    url: Option<String>,
    #[serde(alias = "homepage_url")]
    homepage: Option<String>,
    logo_url: Option<String>,
    country: Option<String>,
    language: Option<String>,
    genre: Option<String>,
    codec: Option<String>,
    bitrate: Option<i32>,
    favorite: Option<bool>,
}

async fn update_radio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateRadioBody>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut station) = repo.get(id).ok().flatten() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(name) = body.name {
        station.name = name;
    }
    if let Some(url) = body.url {
        station.url = url;
    }
    if let Some(homepage) = body.homepage {
        station.homepage = Some(homepage);
    }
    if let Some(logo_url) = body.logo_url {
        station.logo_url = Some(logo_url);
    }
    if let Some(country) = body.country {
        station.country = Some(country);
    }
    if let Some(language) = body.language {
        station.language = Some(language);
    }
    if let Some(genre) = body.genre {
        station.genre = Some(genre);
    }
    if let Some(codec) = body.codec {
        station.codec = Some(codec);
    }
    if let Some(bitrate) = body.bitrate {
        station.bitrate = Some(bitrate);
    }
    if let Some(fav) = body.favorite {
        station.is_favorite = fav;
        repo.set_favorite(id, fav).ok();
    }
    match repo.update(&station) {
        Ok(()) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "updated", "id": id}),
            );
            Json(json!(station)).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "deleted", "id": id}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn play_radio(
    State(state): State<AppState>,
    Path((id, zone_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(radio) = repo.get(id).ok().flatten() else {
        return (StatusCode::NOT_FOUND, "radio not found").into_response();
    };

    let play_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: None,
        source: Some("radio".into()),
        source_id: Some(radio.url.clone()),
        title: Some(radio.name.clone()),
        artist_name: Some("Live Radio".into()),
        album_title: Some(radio.name.clone()),
        cover_url: radio.logo_url.clone(),
        duration_ms: None,
        ..Default::default()
    };

    let (output_sent, output_error, stream_url) = match state.orchestrator.play(play_req).await {
        Ok(result) => (result.output_sent, result.error, result.stream_url),
        Err(e) => (false, Some(e), None),
    };

    repo.record_play(id).ok();

    let zone_state = state.playback.get_state(zone_id).await;
    Json(json!({
        "zone_id": zone_id,
        "radio": radio.name,
        "output_sent": output_sent,
        "error": output_error,
        "state": zone_state,
        "stream_url": stream_url,
    }))
    .into_response()
}

async fn search_radios(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Value> {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let items = repo.search(&q.q).unwrap_or_default();
    Json(json!(items))
}

async fn list_favorites(State(state): State<AppState>) -> Json<Value> {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let items = repo.favorites().unwrap_or_default();
    Json(json!(items))
}

#[derive(Deserialize)]
struct FavoriteToggle {
    favorite: Option<bool>,
}

#[derive(Deserialize)]
pub struct AddFromWebQuery {
    pub name: String,
    pub url: String,
    pub genre: Option<String>,
    pub country: Option<String>,
    pub logo_url: Option<String>,
}

async fn add_from_web(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
    Query(q): Query<AddFromWebQuery>,
) -> impl IntoResponse {
    let lang = crate::i18n::lang_from_header(&headers);
    let repo = RadioRepo::with_backend(state.backend.clone());
    let station = RadioStation {
        id: None,
        name: q.name.clone(),
        url: q.url.clone(),
        homepage: None,
        // Fall back to the stream host favicon so a web-added radio shows art.
        logo_url: q.logo_url.clone().or_else(|| favicon_from_url(&q.url)),
        country: q.country,
        language: None,
        genre: q.genre,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    };
    let html = match repo.create(&station) {
        Ok(id) => {
            repo.set_favorite(id, true).ok();
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "created", "id": id}),
            );
            let title = crate::i18n::t(&lang, "radio.addedTitle");
            let body_txt = crate::i18n::t(&lang, "radio.addedBody").replace("{name}", &q.name);
            let close = crate::i18n::t(&lang, "radio.canCloseTab");
            format!(
                r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Tune</title></head>
<body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center"><h1 style="color:#4ade80">{title}</h1><p>{body_txt}</p><p style="color:#888;margin-top:2em">{close}</p></div>
</body></html>"#
            )
        }
        Err(e) => format!(
            r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Tune</title></head>
<body style="font-family:system-ui;background:#1a1a2e;color:#eee;display:flex;justify-content:center;align-items:center;height:100vh;margin:0">
<div style="text-align:center"><h1 style="color:#f87171">{err_title}</h1><p>{e}</p></div>
</body></html>"#,
            err_title = crate::i18n::t(&lang, "radio.errorTitle")
        ),
    };
    axum::response::Html(html)
}

async fn toggle_favorite(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<FavoriteToggle>>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let current = repo.get(id).ok().flatten();
    let Some(current) = current else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let new_state = body
        .and_then(|b| b.favorite)
        .unwrap_or(!current.is_favorite);
    match repo.set_favorite(id, new_state) {
        Ok(_) => {
            state.event_bus.emit(
                "library.radios_changed",
                json!({"action": "favorite_toggled", "id": id, "favorite": new_state}),
            );
            Json(json!({ "id": id, "favorite": new_state })).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Radio artwork / export / import
// ---------------------------------------------------------------------------

async fn set_radio_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: axum::extract::Multipart,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let Some(mut radio) = repo.get(id).ok().flatten() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "radio not found"})),
        )
            .into_response();
    };

    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "image" || name == "artwork" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                } else if ct.contains("webp") {
                    ext = "webp".to_string();
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

    let cache_dir = crate::routes::library::artwork_cache_dir();
    std::fs::create_dir_all(&cache_dir).ok();
    let hash = tune_core::library::artwork::artwork_hash(&format!("radio-upload-{id}"));
    let path = cache_dir.join(format!("{hash}.{ext}"));
    if std::fs::write(&path, &data).is_err() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    radio.logo_url = Some(hash.clone());
    repo.update(&radio).ok();
    Json(json!(radio)).into_response()
}

async fn export_radios_m3u(State(state): State<AppState>) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let stations = repo.list().unwrap_or_default();

    let mut m3u = String::from("#EXTM3U\n");
    for s in &stations {
        m3u.push_str(&format!("#EXTINF:-1,{}\n{}\n", s.name, s.url));
    }

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "Content-Type",
        axum::http::HeaderValue::from_static("audio/x-mpegurl; charset=utf-8"),
    );
    headers.insert(
        "Content-Disposition",
        axum::http::HeaderValue::from_static("attachment; filename=\"radios.m3u\""),
    );
    (axum::http::StatusCode::OK, headers, m3u).into_response()
}

#[derive(Deserialize)]
struct ImportRadiosBody {
    stations: Vec<CreateRadio>,
}

async fn import_radios(
    State(state): State<AppState>,
    Json(body): Json<ImportRadiosBody>,
) -> impl IntoResponse {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut imported = 0i64;
    for s in &body.stations {
        let station = RadioStation {
            id: None,
            name: s.name.clone(),
            url: s.url.clone(),
            homepage: s.homepage.clone(),
            logo_url: s.logo_url.clone().or_else(|| favicon_from_url(&s.url)),
            country: s.country.clone(),
            language: s.language.clone(),
            genre: s.genre.clone(),
            codec: s.codec.clone(),
            bitrate: s.bitrate,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        if repo.create(&station).is_ok() {
            imported += 1;
        }
    }
    (StatusCode::CREATED, Json(json!({ "imported": imported }))).into_response()
}

async fn import_radios_m3u(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let entries = tune_core::library::m3u_parser::parse_m3u_content(&body, true);
    let repo = RadioRepo::with_backend(state.backend.clone());
    let mut imported = 0i64;
    let mut skipped = 0i64;
    for entry in &entries {
        if !entry.is_url {
            skipped += 1;
            continue;
        }
        let name = entry
            .title
            .clone()
            .or_else(|| entry.extra_attrs.get("tvg-name").cloned())
            .unwrap_or_else(|| entry.path.clone());
        // Playlists use several logo attribute spellings (tvg-logo / url-logo /
        // logo); PLS carries none. Fall back to the stream host favicon so every
        // imported radio shows art (Bilou: "pourquoi ne pas les reprendre").
        let logo = entry
            .extra_attrs
            .get("tvg-logo")
            .or_else(|| entry.extra_attrs.get("url-logo"))
            .or_else(|| entry.extra_attrs.get("logo"))
            .cloned()
            .or_else(|| favicon_from_url(&entry.path));
        let group = entry.extra_attrs.get("group-title").cloned();
        let station = RadioStation {
            id: None,
            name,
            url: entry.path.clone(),
            homepage: None,
            logo_url: logo,
            country: None,
            language: None,
            genre: group,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        match repo.create(&station) {
            Ok(_) => imported += 1,
            Err(e) => {
                tracing::debug!(url = %entry.path, error = %e, "radio_import_m3u_entry_failed");
                skipped += 1;
            }
        }
    }
    tracing::info!(
        imported,
        skipped,
        total = entries.len(),
        "radio_import_m3u_complete"
    );
    (
        StatusCode::CREATED,
        Json(json!({ "imported": imported, "skipped": skipped, "total": entries.len() })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Radio Favorites
// ---------------------------------------------------------------------------

pub fn radio_favorites_router() -> Router<AppState> {
    Router::new()
        .route(
            "/",
            get(list_radio_favorites)
                .post(save_radio_favorite)
                .delete(delete_all_radio_favorites),
        )
        .route("/count", get(radio_favorites_count))
        .route("/is-favorite", get(is_radio_favorite))
        .route("/save-current", post(save_current_as_favorite))
        .route("/create-playlist", post(create_playlist_from_favorites))
        .route("/{fav_id}", axum::routing::delete(delete_radio_favorite))
}

#[derive(Deserialize)]
struct RadioFavPagination {
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn list_radio_favorites(
    State(state): State<AppState>,
    Query(q): Query<RadioFavPagination>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let limit = q.limit.unwrap_or(100);
    let offset = q.offset.unwrap_or(0);
    let rows = state
        .backend
        .query_many(
            "SELECT id, title, artist, station_name, cover_url, stream_url, saved_at FROM radio_favorites ORDER BY saved_at DESC LIMIT ? OFFSET ?",
            &[&limit as &dyn ToSqlValue, &offset as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "title": r.get(1).and_then(|v| v.as_string()),
                "artist": r.get(2).and_then(|v| v.as_string()),
                "station_name": r.get(3).and_then(|v| v.as_string()),
                "cover_url": r.get(4).and_then(|v| v.as_string()),
                "stream_url": r.get(5).and_then(|v| v.as_string()),
                "saved_at": r.get(6).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

async fn radio_favorites_count(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let count: i64 = state
        .backend
        .query_one("SELECT COUNT(*) FROM radio_favorites", &[])
        .map_err(|e| AppError::internal(e))?
        .and_then(|r| r.get(0).and_then(|v| v.as_i64()))
        .unwrap_or(0);
    Ok(Json(json!({ "count": count })))
}

#[derive(Deserialize)]
struct IsFavoriteQuery {
    title: String,
    artist: Option<String>,
}

async fn is_radio_favorite(
    State(state): State<AppState>,
    Query(q): Query<IsFavoriteQuery>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let artist = q.artist.unwrap_or_default();
    let exists: bool = state
        .backend
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM radio_favorites WHERE title = ? AND artist = ?)",
            &[&q.title as &dyn ToSqlValue, &artist as &dyn ToSqlValue],
        )
        .map_err(|e| AppError::internal(e))?
        .and_then(|r| r.get(0).and_then(|v| v.as_i64()))
        .map(|v| v != 0)
        .unwrap_or(false);
    Ok(Json(json!({ "is_favorite": exists })))
}

#[derive(Deserialize)]
struct SaveRadioFavorite {
    title: String,
    artist: Option<String>,
    station_name: Option<String>,
    cover_url: Option<String>,
    stream_url: Option<String>,
}

async fn save_radio_favorite(
    State(state): State<AppState>,
    Json(body): Json<SaveRadioFavorite>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let artist = body.artist.unwrap_or_default();
    let station = body.station_name.unwrap_or_default();
    match state.backend.execute_returning_id(
        // `INSERT OR IGNORE` is SQLite-only; the Postgres backend forwards it
        // verbatim → `syntax error at or near "OR"` (500), so saving a radio
        // favorite failed on PG (.15) and the heart never lit up. `ON CONFLICT
        // DO NOTHING` (no target) is valid on both SQLite (3.24+) and Postgres.
        // `saved_at` est ecrit EXPLICITEMENT, et non laisse au defaut
        // CURRENT_TIMESTAMP : celui-ci rend « 2026-08-22 13:45:00 », de l'UTC
        // sans marqueur de fuseau. `new Date()` traite alors la chaine comme
        // deja locale et l'ecran affichait deux heures d'avance en ete
        // (Reivax66, fil forum #1515). Meme forme que `RadioRepo::record_play`,
        // qui faisait deja juste a cote.
        "INSERT INTO radio_favorites (title, artist, station_name, cover_url, stream_url, saved_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        &[&body.title as &dyn ToSqlValue, &artist as &dyn ToSqlValue, &station as &dyn ToSqlValue, &body.cover_url as &dyn ToSqlValue, &body.stream_url as &dyn ToSqlValue, &tune_core::db::radio_repo::maintenant_iso8601() as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

// Clear the entire radio favorites list (DELETE /radio-favorites).
// `DELETE FROM radio_favorites` (no WHERE) is portable across SQLite and
// Postgres. Returns a JSON body (not 204) because the web client does
// `JSON.parse` on the response and chokes on an empty body.
async fn delete_all_radio_favorites(State(state): State<AppState>) -> impl IntoResponse {
    match state.backend.execute("DELETE FROM radio_favorites", &[]) {
        Ok(_) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_radio_favorite(
    State(state): State<AppState>,
    Path(fav_id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .execute(
            "DELETE FROM radio_favorites WHERE id = ?",
            &[&fav_id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct SaveCurrentBody {
    zone_id: i64,
}

async fn save_current_as_favorite(
    State(state): State<AppState>,
    Json(body): Json<SaveCurrentBody>,
) -> impl IntoResponse {
    let zone_state = state.playback.get_state(body.zone_id).await;
    let Some(np) = zone_state.now_playing else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "nothing playing" })),
        )
            .into_response();
    };

    let title = np.title.clone();
    let artist = np.artist_name.clone().unwrap_or_default();
    let station_name = if np.source == "radio" {
        np.album_title.clone().unwrap_or_default()
    } else {
        String::new()
    };
    let cover_url = np.cover_path.clone();

    use tune_core::db::backend::ToSqlValue;
    match state.backend.execute_returning_id(
        // Portable upsert-ignore (SQLite `INSERT OR IGNORE` 500s on the PG
        // backend — see save_radio_favorite above). This is the /save-current
        // path the heart button hits.
        "INSERT INTO radio_favorites (title, artist, station_name, cover_url) VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
        &[&title as &dyn ToSqlValue, &artist as &dyn ToSqlValue, &station_name as &dyn ToSqlValue, &cover_url as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id, "title": title, "artist": artist }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Create playlist from radio favorites
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreatePlaylistFromFavBody {
    name: Option<String>,
    playlist_name: Option<String>,
    /// Target: "local" (default) or a connected streaming service name
    /// (e.g. "qobuz", "tidal", "deezer").
    service: Option<String>,
    limit: Option<usize>,
}

async fn create_playlist_from_favorites(
    State(state): State<AppState>,
    body: Option<Json<CreatePlaylistFromFavBody>>,
) -> Result<axum::response::Response, AppError> {
    let favorites: Vec<(String, String)> = state
        .backend
        .query_many(
            "SELECT title, artist FROM radio_favorites ORDER BY saved_at DESC",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            (
                r.get(0).and_then(|v| v.as_string()).unwrap_or_default(),
                r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
            )
        })
        .collect();

    if favorites.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no favorites to create playlist from"})),
        )
            .into_response());
    }

    let (name, limit, service) = match body {
        Some(Json(ref b)) => {
            let n = b
                .playlist_name
                .clone()
                .or(b.name.clone())
                .unwrap_or_else(|| "Radio Favorites".into());
            let l = b.limit.unwrap_or(200);
            (n, l, b.service.clone())
        }
        None => ("Radio Favorites".into(), 200, None),
    };

    let favorites: Vec<(String, String)> = if limit < favorites.len() {
        favorites.into_iter().take(limit).collect()
    } else {
        favorites
    };

    // Beaucoup de radios livrent tout dans le StreamTitle ICY : le favori
    // arrive alors avec artist vide et title « Artiste - Titre ». Découper
    // avant matching, sinon le titre composite ne ressemble à aucun vrai
    // titre et rien ne matche (forum #1234, Xavier).
    let favorites: Vec<(String, String)> = favorites
        .into_iter()
        .map(|(title, artist)| {
            if artist.trim().is_empty() {
                if let Some((a, t)) = title.split_once(" - ") {
                    let (a, t) = (a.trim(), t.trim());
                    if !a.is_empty() && !t.is_empty() {
                        return (t.to_string(), a.to_string());
                    }
                }
            }
            (title, artist)
        })
        .collect();

    // Streaming target: resolve each favorite onto the service (smart-matched,
    // ISRC-aware) and build the playlist there — Hi-Res where the service offers it.
    let target = service.unwrap_or_else(|| "local".into());
    if target != "local" {
        return create_streaming_playlist_from_favorites(&state, &target, &name, &favorites).await;
    }

    // Local target: match each favorite against the local library.
    let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let playlist_id = match repo.create(&name, None, DEFAULT_PROFILE_ID) {
        Ok(id) => id,
        Err(e) => {
            return Ok(
                (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
            );
        }
    };

    // Même matcher que la cible streaming : l'ancien chemin prenait LE PREMIER
    // résultat de la recherche plein-texte sans aucun scoring — « Nightswimming »
    // capté sur FIP pouvait atterrir sur n'importe quelle piste dont le titre
    // contient le mot. On score les 10 premiers candidats et on refuse sous le
    // plancher approximatif (0.6), comme côté Qobuz.
    use tune_core::library::track_matcher::{MatchCandidate, find_best_match};
    let mut matched = 0i64;
    // Rapport par favori, comme la cible streaming. Sans lui, un « 3 sur 12 »
    // ne dit pas LESQUELS ont échoué ni pourquoi, et l'utilisateur ne peut ni
    // corriger un tag ni signaler utilement.
    let mut report: Vec<serde_json::Value> = Vec::with_capacity(favorites.len());
    // Deux favoris capturés sur des stations différentes peuvent désigner la
    // même piste locale : sans ce garde, elle entrait deux fois dans la
    // playlist.
    let mut already: std::collections::HashSet<i64> = std::collections::HashSet::new();

    for (title, artist) in &favorites {
        let q = if artist.is_empty() {
            title.clone()
        } else {
            format!("{artist} {title}")
        };
        // ⚠ Le `if let Ok(...)` sans branche d'erreur a rendu #1235
        // indiagnosticable pendant des semaines DU CÔTÉ STREAMING : trois modes
        // d'échec — recherche en erreur, zéro résultat, score sous le seuil —
        // produisaient tous le même silence. #1079 l'a instrumenté là-bas ; ce
        // chemin-ci était resté aveugle.
        let results = match track_repo.search(&q, 10) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(query = %q, error = %e, "radio_fav_local_search_failed");
                report.push(json!({"title": title, "artist": artist, "status": "search_failed"}));
                continue;
            }
        };
        let n = results.len();
        let candidates: Vec<MatchCandidate> = results
            .iter()
            .filter(|t| t.id.is_some())
            .map(|t| MatchCandidate {
                title: t.title.clone(),
                artist_name: t.artist_name.clone().unwrap_or_default(),
                album_title: t.album_title.clone().unwrap_or_default(),
                source_id: t.id.unwrap_or(0).to_string(),
                duration_ms: t.duration_ms,
                isrc: String::new(),
                score: 0.0,
                match_method: String::new(),
                confidence: String::new(),
            })
            .collect();
        let outcome = find_best_match(title, artist, "", 0, &candidates).best_match;
        let best = outcome
            .as_ref()
            .filter(|m| m.score >= tune_core::streaming::matching::MATCH_APPROX_SCORE);

        let Some(m) = best else {
            // Distinguer « rien trouvé » de « trouvé mais refusé au seuil » :
            // le second désigne un tag à corriger, le premier une absence.
            match outcome.as_ref() {
                Some(top) => {
                    tracing::warn!(query = %q, results = n, top = %top.title, score = top.score, "radio_fav_local_match_rejected");
                    report.push(json!({
                        "title": title, "artist": artist, "status": "rejected",
                        "best_candidate": top.title, "score": top.score,
                    }));
                }
                None => {
                    tracing::info!(query = %q, results = n, "radio_fav_local_no_candidate");
                    report.push(json!({"title": title, "artist": artist, "status": "not_found"}));
                }
            }
            continue;
        };

        let Ok(id) = m.source_id.parse::<i64>() else {
            report.push(json!({"title": title, "artist": artist, "status": "not_found"}));
            continue;
        };
        if !already.insert(id) {
            report.push(
                json!({"title": title, "artist": artist, "status": "duplicate", "track_id": id}),
            );
            continue;
        }
        // `matched` ne s'incrémente QUE si l'ajout a réussi. Auparavant le
        // compteur montait avant l'écriture : une playlist pouvait annoncer
        // douze pistes et n'en contenir aucune.
        match repo.add_tracks(playlist_id, &[id], None) {
            Ok(_) => {
                matched += 1;
                tracing::info!(query = %q, results = n, track_id = id, score = m.score, "radio_fav_local_match_ok");
                report.push(json!({
                    "title": title, "artist": artist, "status": "matched",
                    "track_id": id, "score": m.score,
                }));
            }
            Err(e) => {
                tracing::warn!(track_id = id, error = %e, "radio_fav_local_add_failed");
                already.remove(&id);
                report.push(json!({"title": title, "artist": artist, "status": "add_failed"}));
            }
        }
    }

    tracing::info!(
        playlist_id,
        favorites = favorites.len(),
        matched,
        "radio_fav_local_playlist_done"
    );

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": playlist_id,
            "name": name,
            "favorites_count": favorites.len(),
            "matched_tracks": matched,
            "results": report,
        })),
    )
        .into_response())
}

/// Build the playlist on a streaming service: smart-match (ISRC-aware) each radio
/// favorite onto the service catalogue via `best_stream_match`, then create the
/// playlist on that service and add the matched tracks. Returns a per-favorite
/// report so the client can show what matched and what didn't.
async fn create_streaming_playlist_from_favorites(
    state: &AppState,
    service: &str,
    name: &str,
    favorites: &[(String, String)],
) -> Result<axum::response::Response, AppError> {
    let svc_arc = {
        let reg = state.services.lock().await;
        match reg.get(service) {
            Some(a) => a,
            None => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("service '{service}' not found or not connected")
                    })),
                )
                    .into_response());
            }
        }
    };
    let svc = svc_arc.read().await;

    let mut matched_ids: Vec<String> = Vec::new();
    let mut details: Vec<Value> = Vec::new();
    for (title, artist) in favorites {
        // radio favorites carry no ISRC/duration, so match on normalized title+artist.
        //
        // Requêtes essayées dans l'ordre : « artiste titre », puis titre seul,
        // puis titre tronqué avant « ( » ou « - ». Le matcher normalise déjà les
        // suffixes « (feat. X) / (Live) » pour SCORER, mais la REQUÊTE envoyée
        // au service les contenait encore, et Qobuz ne renvoyait alors aucun
        // candidat (#1235 : une recherche manuelle sans le suffixe trouvait la
        // piste). On s'arrête à la première requête qui produit un match sûr ;
        // à défaut, on garde le meilleur match approximatif (0.6–0.7) rencontré,
        // exposé `status: "approximate"` — avant, cette bande était jetée et
        // devenait « not_found ».
        let mut queries: Vec<String> = Vec::new();
        if artist.is_empty() {
            queries.push(title.clone());
        } else {
            queries.push(format!("{artist} {title}"));
            queries.push(title.clone());
        }
        let stripped = title
            .split_once(" (")
            .map(|(a, _)| a)
            .unwrap_or(title)
            .split_once(" - ")
            .map(|(a, _)| a)
            .unwrap_or_else(|| title.split_once(" (").map(|(a, _)| a).unwrap_or(title))
            .trim()
            .to_string();
        if !stripped.is_empty() && stripped != *title {
            if artist.is_empty() {
                queries.push(stripped.clone());
            } else {
                queries.push(format!("{artist} {stripped}"));
            }
        }

        // Instrumentation for #1235 (Reivax66: "aucun résultat trouvé" on Qobuz
        // even though a manual Qobuz search finds the track). Without logging we
        // cannot tell three very different failure modes apart: the service
        // search erroring (previously swallowed by `Err(_) => None`), the search
        // returning zero candidates, or the matcher rejecting every candidate.
        // Log the query, result count, the top candidate the service returned,
        // and the accept/reject decision so the next tester log pinpoints it.
        let mut best: Option<(tune_core::streaming::traits::StreamTrack, f64)> = None;
        for q in &queries {
            match svc.search(q, 10).await {
                Ok(results) => {
                    let n = results.tracks.len();
                    let top = results
                        .tracks
                        .first()
                        .map(|t| format!("{} — {}", t.artist, t.title))
                        .unwrap_or_else(|| "<none>".into());
                    match tune_core::streaming::matching::best_stream_match_scored(
                        title,
                        artist,
                        "",
                        0,
                        &results.tracks,
                    ) {
                        Some((t, score))
                            if score >= tune_core::streaming::matching::MATCH_ACCEPT_SCORE =>
                        {
                            tracing::info!(service = %service, query = %q, results = n, top = %top, score, "radio_fav_match_ok");
                            best = Some((t.clone(), score));
                            break;
                        }
                        Some((t, score)) => {
                            tracing::info!(service = %service, query = %q, results = n, top = %top, score, "radio_fav_match_approx");
                            if best.as_ref().map(|(_, s)| score > *s).unwrap_or(true) {
                                best = Some((t.clone(), score));
                            }
                        }
                        None => {
                            tracing::warn!(service = %service, query = %q, results = n, top = %top, "radio_fav_match_rejected");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(service = %service, query = %q, error = %e, "radio_fav_search_failed");
                }
            }
        }
        match best {
            Some((t, score)) => {
                let sure = score >= tune_core::streaming::matching::MATCH_ACCEPT_SCORE;
                details.push(json!({
                    "title": title,
                    "artist": artist,
                    "matched_title": t.title,
                    "matched_artist": t.artist,
                    "matched_id": t.id,
                    "status": if sure { "matched" } else { "approximate" },
                }));
                matched_ids.push(t.id);
            }
            None => details.push(json!({
                "title": title,
                "artist": artist,
                "status": "not_found",
            })),
        }
    }

    tracing::info!(
        service = %service,
        favorites = favorites.len(),
        matched = matched_ids.len(),
        "radio_fav_playlist_matching_done"
    );

    let mut remote_playlist_id: Option<String> = None;
    if !matched_ids.is_empty() {
        match svc
            .create_playlist(name, Some("Created by Tune from radio favorites"))
            .await
        {
            Ok(pid) => {
                if let Err(e) = svc.add_tracks_to_playlist(&pid, &matched_ids).await {
                    tracing::warn!(error = %e, "radio_fav_add_tracks_failed");
                }
                remote_playlist_id = Some(pid);
            }
            Err(e) => {
                tracing::warn!(
                    service = %service,
                    error = %e,
                    "radio_fav_create_playlist_failed (service may not support write)"
                );
                return Ok((
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": format!("could not create playlist on '{service}': {e}"),
                        "matched_tracks": matched_ids.len(),
                        "details": details,
                    })),
                )
                    .into_response());
            }
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "service": service,
            "name": name,
            "favorites_count": favorites.len(),
            "matched_tracks": matched_ids.len(),
            "remote_playlist_id": remote_playlist_id,
            "details": details,
        })),
    )
        .into_response())
}

// ---------------------------------------------------------------------------
// Global Alarms CRUD
// ---------------------------------------------------------------------------

pub fn alarms_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_alarms).post(create_alarm_global))
        .route(
            "/{id}",
            axum::routing::put(update_alarm).delete(delete_alarm_global),
        )
        .route("/{id}/snooze", post(snooze_alarm))
        .route("/{id}/test", post(test_alarm))
}

async fn list_alarms(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state.backend.query_many(
        "SELECT id, name, time, days, one_shot, skip_holidays, zone_id, source_type, source_id, source_name, volume, fade_duration_s, enabled, last_fired_at, created_at, fade_in_seconds, days_of_week, multi_zone_ids FROM alarms ORDER BY time",
        &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "name": r.get(1).and_then(|v| v.as_string()).unwrap_or_else(|| "Alarm".into()),
                "time": r.get(2).and_then(|v| v.as_string()),
                "days": r.get(3).and_then(|v| v.as_string()),
                "one_shot": r.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                "skip_holidays": r.get(5).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
                "zone_id": r.get(6).and_then(|v| v.as_i64()),
                "source_type": r.get(7).and_then(|v| v.as_string()),
                "source_id": r.get(8).and_then(|v| v.as_string()),
                "source_name": r.get(9).and_then(|v| v.as_string()),
                "volume": r.get(10).and_then(|v| v.as_i64()),
                "fade_duration_s": r.get(11).and_then(|v| v.as_i64()),
                "enabled": r.get(12).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
                "last_fired_at": r.get(13).and_then(|v| v.as_string()),
                "created_at": r.get(14).and_then(|v| v.as_string()),
                "fade_in_seconds": r.get(15).and_then(|v| v.as_i64()),
                "days_of_week": r.get(16).and_then(|v| v.as_string()),
                "multi_zone_ids": r.get(17).and_then(|v| v.as_string()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

#[derive(Deserialize)]
struct CreateAlarmGlobal {
    name: Option<String>,
    time: String,
    days: Option<String>,
    one_shot: Option<bool>,
    skip_holidays: Option<bool>,
    zone_id: Option<i64>,
    source_type: Option<String>,
    source_id: Option<String>,
    source_name: Option<String>,
    volume: Option<f64>,
    fade_duration_s: Option<i32>,
    fade_in_seconds: Option<i32>,
    enabled: Option<bool>,
    /// 7-char bitmask "1010100" (Mon..Sun). Premium only for non-"1111111".
    days_of_week: Option<String>,
    /// JSON array of zone IDs, e.g. "[1,3,5]". Premium only.
    multi_zone_ids: Option<String>,
}

async fn create_alarm_global(
    State(state): State<AppState>,
    profile: crate::routes::active_profile::ActiveProfile,
    Json(body): Json<CreateAlarmGlobal>,
) -> impl IntoResponse {
    let is_premium = state.license.is_premium().await;

    // Free tier: max 1 alarm
    if !is_premium {
        let count: i64 = state
            .backend
            .query_one("SELECT COUNT(*) FROM alarms", &[])
            .ok()
            .flatten()
            .and_then(|r| r.get(0).and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if count >= 1 {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "premium_required",
                    "feature": "Advanced Alarms",
                    "message": "Free tier allows 1 alarm. Upgrade to Tune Premium for unlimited alarms.",
                    "upgrade_url": "https://mozaiklabs.fr/pricing"
                })),
            )
                .into_response();
        }
    }

    // Free tier: no advanced fields
    let fade_in_seconds = body.fade_in_seconds.unwrap_or(0);
    let days_of_week = body
        .days_of_week
        .clone()
        .unwrap_or_else(|| "1111111".into());
    let multi_zone_ids = body.multi_zone_ids.clone().unwrap_or_default();

    if !is_premium {
        let has_fade = fade_in_seconds > 0;
        let has_multi_zone = !multi_zone_ids.is_empty() && multi_zone_ids != "[]";
        let has_day_selection = days_of_week != "1111111";

        if has_fade || has_multi_zone || has_day_selection {
            return (
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "premium_required",
                    "feature": "Advanced Alarms",
                    "message": "Fade-in, multi-zone, and day scheduling require Tune Premium.",
                    "upgrade_url": "https://mozaiklabs.fr/pricing"
                })),
            )
                .into_response();
        }
    }

    let enabled_int: i32 = if body.enabled.unwrap_or(true) { 1 } else { 0 };
    let one_shot_int: i32 = if body.one_shot.unwrap_or(false) { 1 } else { 0 };
    let skip_holidays_int: i32 = if body.skip_holidays.unwrap_or(false) {
        1
    } else {
        0
    };

    use tune_core::db::backend::ToSqlValue;
    let name = body.name.unwrap_or_else(|| "Alarm".into());
    let days = body.days.unwrap_or_else(|| "0,1,2,3,4".into());
    let volume = body.volume.unwrap_or(0.3);
    let fade_duration_s = body.fade_duration_s.unwrap_or(60);
    let multi_zone_ids_opt: Option<String> = if multi_zone_ids.is_empty() {
        None
    } else {
        Some(multi_zone_ids)
    };
    let profile_id = profile.id();
    match state.backend.execute_returning_id(
        "INSERT INTO alarms (name, time, days, one_shot, skip_holidays, zone_id, source_type, source_id, source_name, volume, fade_duration_s, fade_in_seconds, enabled, days_of_week, multi_zone_ids, profile_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[&name as &dyn ToSqlValue, &body.time as &dyn ToSqlValue, &days as &dyn ToSqlValue, &one_shot_int as &dyn ToSqlValue, &skip_holidays_int as &dyn ToSqlValue, &body.zone_id as &dyn ToSqlValue, &body.source_type as &dyn ToSqlValue, &body.source_id as &dyn ToSqlValue, &body.source_name as &dyn ToSqlValue, &volume as &dyn ToSqlValue, &fade_duration_s as &dyn ToSqlValue, &fade_in_seconds as &dyn ToSqlValue, &enabled_int as &dyn ToSqlValue, &days_of_week as &dyn ToSqlValue, &multi_zone_ids_opt as &dyn ToSqlValue, &profile_id as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateAlarm {
    name: Option<String>,
    time: Option<String>,
    days: Option<String>,
    one_shot: Option<bool>,
    skip_holidays: Option<bool>,
    zone_id: Option<i64>,
    source_type: Option<String>,
    source_id: Option<String>,
    source_name: Option<String>,
    volume: Option<f64>,
    fade_duration_s: Option<i32>,
    fade_in_seconds: Option<i32>,
    enabled: Option<bool>,
    days_of_week: Option<String>,
    multi_zone_ids: Option<String>,
}

async fn update_alarm(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateAlarm>,
) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::{SqlValue, ToSqlValue};

    // Gate advanced fields for Free tier
    let is_premium = state.license.is_premium().await;
    if !is_premium {
        let has_fade = body.fade_in_seconds.map(|v| v > 0).unwrap_or(false);
        let has_multi_zone = body
            .multi_zone_ids
            .as_ref()
            .map(|s| !s.is_empty() && s != "[]")
            .unwrap_or(false);
        let has_day_selection = body
            .days_of_week
            .as_ref()
            .map(|s| s != "1111111")
            .unwrap_or(false);

        if has_fade || has_multi_zone || has_day_selection {
            return Ok((
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "premium_required",
                    "feature": "Advanced Alarms",
                    "message": "Fade-in, multi-zone, and day scheduling require Tune Premium.",
                    "upgrade_url": "https://mozaiklabs.fr/pricing"
                })),
            )
                .into_response());
        }
    }

    // Build SET clause dynamically from provided fields
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<SqlValue> = Vec::new();

    if let Some(ref name) = body.name {
        sets.push("name = ?".into());
        values.push(name.to_sql_value());
    }
    if let Some(ref time) = body.time {
        sets.push("time = ?".into());
        values.push(time.to_sql_value());
    }
    if let Some(ref days) = body.days {
        sets.push("days = ?".into());
        values.push(days.to_sql_value());
    }
    if let Some(one_shot) = body.one_shot {
        sets.push("one_shot = ?".into());
        values.push((one_shot as i32).to_sql_value());
    }
    if let Some(skip_holidays) = body.skip_holidays {
        sets.push("skip_holidays = ?".into());
        values.push((skip_holidays as i32).to_sql_value());
    }
    if let Some(zone_id) = body.zone_id {
        sets.push("zone_id = ?".into());
        values.push(zone_id.to_sql_value());
    }
    if let Some(ref source_type) = body.source_type {
        sets.push("source_type = ?".into());
        values.push(source_type.to_sql_value());
    }
    if let Some(ref source_id) = body.source_id {
        sets.push("source_id = ?".into());
        values.push(source_id.to_sql_value());
    }
    if let Some(ref source_name) = body.source_name {
        sets.push("source_name = ?".into());
        values.push(source_name.to_sql_value());
    }
    if let Some(volume) = body.volume {
        sets.push("volume = ?".into());
        values.push(volume.to_sql_value());
    }
    if let Some(fade_duration_s) = body.fade_duration_s {
        sets.push("fade_duration_s = ?".into());
        values.push(fade_duration_s.to_sql_value());
    }
    if let Some(fade_in_seconds) = body.fade_in_seconds {
        sets.push("fade_in_seconds = ?".into());
        values.push(fade_in_seconds.to_sql_value());
    }
    if let Some(enabled) = body.enabled {
        sets.push("enabled = ?".into());
        values.push((enabled as i32).to_sql_value());
    }
    if let Some(ref days_of_week) = body.days_of_week {
        sets.push("days_of_week = ?".into());
        values.push(days_of_week.to_sql_value());
    }
    if let Some(ref multi_zone_ids) = body.multi_zone_ids {
        sets.push("multi_zone_ids = ?".into());
        values.push(multi_zone_ids.to_sql_value());
    }

    if sets.is_empty() {
        return Ok((StatusCode::BAD_REQUEST, "no fields to update").into_response());
    }

    let sql = format!("UPDATE alarms SET {} WHERE id = ?", sets.join(", "));
    values.push(id.to_sql_value());

    let params_ref: Vec<&dyn ToSqlValue> = values.iter().map(|v| v as &dyn ToSqlValue).collect();
    match state.backend.execute(&sql, &params_ref) {
        Ok(0) => Ok(StatusCode::NOT_FOUND.into_response()),
        Ok(_) => Ok(Json(json!({ "id": id, "updated": true })).into_response()),
        Err(e) => Ok((StatusCode::INTERNAL_SERVER_ERROR, e).into_response()),
    }
}

async fn delete_alarm_global(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    match state
        .backend
        .execute("DELETE FROM alarms WHERE id = ?", &[&id as &dyn ToSqlValue])
    {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn snooze_alarm(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    match state.backend.execute(
        "UPDATE alarms SET last_fired_at = NULL WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    ) {
        Ok(0) => StatusCode::NOT_FOUND.into_response(),
        Ok(_) => Json(json!({ "id": id, "snoozed": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// `POST /alarms/{id}/test` — trigger an alarm immediately for testing.
/// Premium only.
async fn test_alarm(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    // Premium gate
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::AdvancedAlarms,
    )
    .await
    {
        return resp;
    }

    let scheduler = std::sync::Arc::new(tune_core::alarms::AlarmScheduler::with_backend(
        state.backend.clone(),
        state.orchestrator.clone(),
    ));

    match scheduler.get_alarm(id) {
        Ok(Some(alarm)) => {
            scheduler.fire_alarm(&alarm).await;
            Json(json!({ "id": id, "tested": true })).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::http::streamer::StreamInfo;

    #[tokio::test]
    async fn abandonner_le_corps_retire_la_session_radio_ephemere() {
        let streamer = Arc::new(AudioStreamer::new(0));
        let (stream_id, _tx, _data_ready, _session) = streamer
            .create_radio_session(
                StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    ..Default::default()
                },
                4,
            )
            .await;
        assert!(
            streamer
                .sessions_state()
                .lock()
                .await
                .contains_key(&stream_id)
        );

        let response = with_media_server_radio_cleanup(
            Response::new(Body::empty()),
            streamer.clone(),
            stream_id.clone(),
        );
        drop(response);

        for _ in 0..10 {
            if !streamer
                .sessions_state()
                .lock()
                .await
                .contains_key(&stream_id)
            {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("la session {stream_id} survit au corps HTTP abandonné");
    }
}
