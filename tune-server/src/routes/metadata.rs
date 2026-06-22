use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
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
struct AlbumEdit {
    title: Option<String>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    genre: Option<String>,
    year: Option<i32>,
    label: Option<String>,
    release_date: Option<String>,
    original_date: Option<String>,
}

#[derive(Deserialize)]
struct ArtistEdit {
    name: Option<String>,
    sort_name: Option<String>,
    bio: Option<String>,
}

#[derive(Deserialize)]
struct PaginationParams {
    limit: Option<i64>,
    offset: Option<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/tracks/{id}/edit", post(edit_track))
        .route("/albums/{id}/edit", post(edit_album))
        .route("/artists/{id}/edit", post(edit_artist))
        .route("/doubtful", get(list_doubtful_metadata))
        // Lookup (MusicBrainz)
        .route("/lookup/track", get(lookup_track))
        .route("/lookup/album", get(lookup_album))
        // Suggestions
        .route("/suggestions", get(list_suggestions))
        .route("/suggestions/{id}/accept", post(accept_suggestion))
        .route("/suggestions/{id}/reject", post(reject_suggestion))
        .route("/suggestions/auto-apply", post(auto_apply_suggestions))
        .route("/suggestions/tracks/{track_id}", get(suggestions_for_track))
        .route("/suggestions/albums/{album_id}", get(suggestions_for_album))
        // Artist enrichment
        .route("/artists/{id}/enrich", get(enrich_artist))
        .route("/artists/{id}/similar", get(similar_artists))
        // Cover art enrichment (web client compatibility)
        .route("/covers/album/{id}", post(fetch_album_cover))
        .route("/covers/search", get(search_covers))
        // Genre fix tools
        .route("/fix-genres", post(fix_genres))
        .route("/fix-genres-by-artist", post(fix_genres_by_artist))
        .route(
            "/fix-genres-by-artist-fuzzy",
            post(fix_genres_by_artist_fuzzy),
        )
        .route("/fix-genres-by-family", post(fix_genres_by_family))
        // Album merge (targeted, by IDs)
        .route("/albums/merge", post(merge_albums))
}

async fn edit_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<TrackEdit>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
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
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("tag write failed: {e}"),
            )
                .into_response();
        }
    }

    if let Some(ref v) = body.title {
        track.title = v.clone();
    }
    if let Some(ref v) = body.artist {
        track.artist_name = Some(v.clone());
    }
    if let Some(ref v) = body.album {
        track.album_title = Some(v.clone());
    }
    if let Some(ref v) = body.genre {
        track.genre = Some(v.clone());
    }
    if let Some(v) = body.track_number {
        track.track_number = v as i32;
    }
    if let Some(v) = body.disc_number {
        track.disc_number = v as i32;
    }
    if let Some(v) = body.year {
        track.year = Some(v as i32);
    }
    if let Some(ref v) = body.composer {
        track.composer = Some(v.clone());
    }
    if let Some(ref v) = body.label {
        track.label = Some(v.clone());
    }

    repo.update(&track).ok();

    Json(json!({ "status": "ok", "track_id": id })).into_response()
}

async fn edit_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AlbumEdit>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let mut album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.title {
        album.title = v.clone();
    }
    if let Some(ref v) = body.genre {
        album.genre = Some(v.clone());
    }
    if let Some(v) = body.year {
        album.year = Some(v);
    }
    if let Some(ref v) = body.label {
        album.label = Some(v.clone());
    }
    if let Some(ref v) = body.release_date {
        album.release_date = Some(v.clone());
    }
    if let Some(ref v) = body.original_date {
        album.original_date = Some(v.clone());
    }
    // artist_id takes priority; fall back to artist_name resolution
    if let Some(aid) = body.artist_id {
        album.artist_id = Some(aid);
    } else if let Some(ref name) = body.artist_name {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        if let Ok(Some(artist)) = artist_repo.get_by_name(name) {
            album.artist_id = artist.id;
        }
    }

    repo.update(&album).ok();

    Json(json!({ "status": "ok", "album_id": id })).into_response()
}

async fn edit_artist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ArtistEdit>,
) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let mut artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.name {
        artist.name = v.clone();
    }
    if let Some(ref v) = body.sort_name {
        artist.sort_name = Some(v.clone());
    }
    if let Some(ref v) = body.bio {
        artist.bio = Some(v.clone());
    }

    repo.update(&artist).ok();

    Json(json!({ "status": "ok", "artist_id": id })).into_response()
}

async fn list_doubtful_metadata(
    State(state): State<AppState>,
    Query(p): Query<PaginationParams>,
) -> impl IntoResponse {
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let total = track_repo.count_doubtful().unwrap_or(0);
    let tracks = track_repo.list_doubtful(limit, offset).unwrap_or_default();
    let items: Vec<serde_json::Value> = tracks
        .iter()
        .map(|t| {
            let mut reasons = Vec::new();
            if t.artist_name
                .as_ref()
                .map(|a| a.is_empty() || a == "Unknown Artist")
                .unwrap_or(true)
            {
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
        "items": items,
        "total": total,
        "limit": limit,
        "offset": offset,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// MusicBrainz Lookup
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct LookupTrackQuery {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

async fn lookup_track(Query(q): Query<LookupTrackQuery>) -> Json<serde_json::Value> {
    let results = tune_core::metadata::matcher::lookup_track(
        q.title.as_deref().unwrap_or(""),
        q.artist.as_deref().unwrap_or(""),
        q.album.as_deref().unwrap_or(""),
    )
    .await;
    Json(json!(results))
}

#[derive(Deserialize)]
struct LookupAlbumQuery {
    title: Option<String>,
    artist: Option<String>,
}

async fn lookup_album(Query(q): Query<LookupAlbumQuery>) -> Json<serde_json::Value> {
    let results = tune_core::metadata::matcher::lookup_album(
        q.title.as_deref().unwrap_or(""),
        q.artist.as_deref().unwrap_or(""),
    )
    .await;
    Json(json!(results))
}

// ---------------------------------------------------------------------------
// Metadata Suggestions
// ---------------------------------------------------------------------------

async fn list_suggestions(State(state): State<AppState>) -> Json<serde_json::Value> {
    let count = state.suggestion_store.count_pending().unwrap_or(0);
    Json(json!({ "pending": count }))
}

async fn suggestions_for_track(
    State(state): State<AppState>,
    Path(track_id): Path<i64>,
) -> Json<serde_json::Value> {
    let items = state
        .suggestion_store
        .pending_for_track(track_id)
        .unwrap_or_default();
    Json(json!(items))
}

async fn suggestions_for_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> Json<serde_json::Value> {
    let items = state
        .suggestion_store
        .pending_for_album(album_id)
        .unwrap_or_default();
    Json(json!(items))
}

async fn accept_suggestion(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match state.suggestion_store.accept(id) {
        Ok(Some(s)) => Json(json!(s)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn reject_suggestion(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    match state.suggestion_store.reject(id) {
        Ok(()) => Json(json!({"status": "rejected"})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

#[derive(Deserialize)]
struct AutoApplyBody {
    threshold: Option<f64>,
}

async fn auto_apply_suggestions(
    State(state): State<AppState>,
    Json(body): Json<AutoApplyBody>,
) -> Json<serde_json::Value> {
    let threshold = body.threshold.unwrap_or(0.9);
    let applied = state
        .suggestion_store
        .auto_apply_above(threshold)
        .unwrap_or_default();
    Json(json!({ "applied": applied.len(), "items": applied }))
}

// ---------------------------------------------------------------------------
// Artist Enrichment
// ---------------------------------------------------------------------------

async fn enrich_artist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let lastfm_key = settings
        .get("lastfm_api_key")
        .ok()
        .flatten()
        .or_else(|| std::env::var("LASTFM_API_KEY").ok())
        .unwrap_or_default();

    if lastfm_key.is_empty() {
        return Json(json!({"error": "no lastfm api key configured"})).into_response();
    }

    let client = reqwest::Client::new();
    let resp = client
        .get("http://ws.audioscrobbler.com/2.0/")
        .query(&[
            ("method", "artist.getinfo"),
            ("artist", &artist.name),
            ("api_key", &lastfm_key),
            ("format", "json"),
            ("lang", "fr"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            if let Ok(data) = r.json::<serde_json::Value>().await {
                let bio_obj = &data["artist"]["bio"];
                let summary = bio_obj["summary"].as_str().unwrap_or("");
                let content = bio_obj["content"].as_str().unwrap_or("");
                let tags: Vec<String> = data["artist"]["tags"]["tag"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| t["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let similar: Vec<serde_json::Value> = data["artist"]["similar"]["artist"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let listeners = data["artist"]["stats"]["listeners"].as_str().unwrap_or("0");
                let playcount = data["artist"]["stats"]["playcount"].as_str().unwrap_or("0");

                // Strip all HTML tags from Last.fm bio
                let clean = |s: &str| -> String {
                    let mut result = String::with_capacity(s.len());
                    let mut in_tag = false;
                    for c in s.chars() {
                        if c == '<' {
                            in_tag = true;
                        } else if c == '>' {
                            in_tag = false;
                        } else if !in_tag {
                            result.push(c);
                        }
                    }
                    result
                        .split("Read more on Last.fm")
                        .next()
                        .unwrap_or(&result)
                        .trim()
                        .to_string()
                };

                let mut bio = clean(content);
                let bio_summary = clean(summary);

                // Fallback to Wikipedia if Last.fm bio is empty
                if bio.is_empty() || bio.len() < 20 {
                    if let Ok(wiki) = client
                        .get(&format!(
                            "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
                            urlencoding::encode(&artist.name)
                        ))
                        .timeout(std::time::Duration::from_secs(10))
                        .send()
                        .await
                    {
                        if let Ok(wd) = wiki.json::<serde_json::Value>().await {
                            if let Some(extract) = wd["extract"].as_str() {
                                if extract.len() > bio.len() {
                                    bio = extract.to_string();
                                }
                            }
                        }
                    }
                    // Try French Wikipedia too
                    if bio.is_empty() || bio.len() < 50 {
                        if let Ok(wiki_fr) = client
                            .get(&format!(
                                "https://fr.wikipedia.org/api/rest_v1/page/summary/{}",
                                urlencoding::encode(&artist.name)
                            ))
                            .timeout(std::time::Duration::from_secs(10))
                            .send()
                            .await
                        {
                            if let Ok(wd) = wiki_fr.json::<serde_json::Value>().await {
                                if let Some(extract) = wd["extract"].as_str() {
                                    if extract.len() > bio.len() {
                                        bio = extract.to_string();
                                    }
                                }
                            }
                        }
                    }
                }

                // Update artist bio in DB if content is richer
                if bio.len() > artist.bio.as_deref().unwrap_or("").len() {
                    let _ = repo.update_bio(id, &bio);
                }

                return Json(json!({
                    "data": {
                        "bio": bio,
                        "bio_summary": bio_summary,
                        "tags": tags,
                        "similar_artists": similar,
                        "listeners": listeners,
                        "playcount": playcount,
                        "enrichment_status": "complete"
                    }
                }))
                .into_response();
            }
            Json(json!(null)).into_response()
        }
        _ => Json(json!(null)).into_response(),
    }
}

async fn similar_artists(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let artist = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let api_base = settings
        .get("artist_enrichment_api")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://api.mozaiklabs.fr".into());

    let mut client =
        tune_core::metadata::artist_enrichment::ArtistEnrichmentClient::new(Some(&api_base), 10);
    let data = client.get_similar(&artist.name).await;
    Json(json!(data)).into_response()
}

// ---------------------------------------------------------------------------
// Genre Fix Tools
// ---------------------------------------------------------------------------

/// Names to skip for genre propagation (compilations / unknown).
const SKIP_ARTIST_NAMES: &[&str] = &[
    "various artists",
    "various",
    "va",
    "v.a.",
    "unknown artist",
    "unknown",
    "?",
    "compilation",
    "compilations",
];

fn is_skip_artist(name: &str) -> bool {
    let lower = name.to_lowercase();
    SKIP_ARTIST_NAMES.iter().any(|&s| s == lower)
}

/// Genre synonym map — maps Last.fm/Discogs tags to canonical genre names.
fn genre_map() -> HashMap<&'static str, &'static str> {
    let entries: &[(&str, &str)] = &[
        ("rock", "Rock"),
        ("alternative rock", "Rock"),
        ("indie rock", "Rock"),
        ("classic rock", "Rock"),
        ("hard rock", "Rock"),
        ("progressive rock", "Progressive Rock"),
        ("post-rock", "Rock"),
        ("psychedelic rock", "Rock"),
        ("punk rock", "Punk"),
        ("pop", "Pop"),
        ("indie pop", "Pop"),
        ("synthpop", "Pop"),
        ("electropop", "Pop"),
        ("dream pop", "Pop"),
        ("chamber pop", "Pop"),
        ("art pop", "Pop"),
        ("jazz", "Jazz"),
        ("smooth jazz", "Jazz"),
        ("free jazz", "Jazz"),
        ("vocal jazz", "Jazz"),
        ("cool jazz", "Jazz"),
        ("bebop", "Jazz"),
        ("hard bop", "Jazz"),
        ("post-bop", "Jazz"),
        ("jazz fusion", "Jazz"),
        ("avant-garde jazz", "Jazz"),
        ("contemporary jazz", "Jazz"),
        ("electronic", "Electronic"),
        ("ambient", "Electronic"),
        ("downtempo", "Electronic"),
        ("idm", "Electronic"),
        ("trip-hop", "Electronic"),
        ("house", "Electronic"),
        ("techno", "Electronic"),
        ("electronica", "Electronic"),
        ("chillout", "Electronic"),
        ("classical", "Classical"),
        ("contemporary classical", "Classical"),
        ("modern classical", "Classical"),
        ("baroque", "Classical"),
        ("romantic", "Classical"),
        ("orchestral", "Classical"),
        ("chamber music", "Classical"),
        ("opera", "Classical"),
        ("blues", "Blues"),
        ("electric blues", "Blues"),
        ("delta blues", "Blues"),
        ("soul", "Soul"),
        ("neo-soul", "Soul"),
        ("r&b", "R&B"),
        ("rnb", "R&B"),
        ("funk", "Funk"),
        ("hip-hop", "Hip-Hop"),
        ("hip hop", "Hip-Hop"),
        ("rap", "Hip-Hop"),
        ("metal", "Metal"),
        ("heavy metal", "Metal"),
        ("progressive metal", "Metal"),
        ("folk", "Folk"),
        ("indie folk", "Folk"),
        ("folk rock", "Folk"),
        ("country", "Country"),
        ("alt-country", "Country"),
        ("reggae", "Reggae"),
        ("dub", "Reggae"),
        ("world", "World"),
        ("afrobeat", "World"),
        ("latin", "World"),
        ("bossa nova", "World"),
        ("chanson", "Chanson"),
        ("chanson francaise", "Chanson"),
        ("french", "Chanson"),
        ("singer-songwriter", "Singer-Songwriter"),
        ("soundtrack", "Soundtrack"),
        ("film score", "Soundtrack"),
        ("new wave", "New Wave"),
        ("post-punk", "New Wave"),
        ("experimental", "Experimental"),
        ("avant-garde", "Experimental"),
    ];
    entries.iter().copied().collect()
}

/// Pick a genre from external service tags. When `allowed` is provided,
/// only return a value already in the user's library vocabulary.
fn normalize_genre(
    tags: &[String],
    allowed: Option<&HashMap<String, String>>,
    gmap: &HashMap<&str, &str>,
) -> Option<String> {
    if let Some(allowed) = allowed {
        // Direct hit in user's existing genres (case-insensitive).
        for tag in tags {
            let t = tag.trim();
            if t.is_empty() {
                continue;
            }
            if let Some(hit) = allowed.get(&t.to_lowercase()) {
                return Some(hit.clone());
            }
        }
        // Synonym -> canonical bucket, but only if that bucket exists.
        for tag in tags {
            if let Some(&bucket) = gmap.get(tag.to_lowercase().trim()) {
                if let Some(hit) = allowed.get(&bucket.to_lowercase()) {
                    return Some(hit.clone());
                }
            }
        }
        return None;
    }

    // Unconstrained path.
    for tag in tags {
        if let Some(&normalized) = gmap.get(tag.to_lowercase().trim()) {
            return Some(normalized.to_string());
        }
    }
    for tag in tags {
        let t = tag.trim();
        if t.len() > 2 && t.len() < 30 && t.parse::<f64>().is_err() {
            // Title-case the tag.
            let mut chars = t.chars();
            let first = chars
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            return Some(format!("{first}{}", chars.as_str()));
        }
    }
    None
}

/// Strip hi-res suffixes like "(96kHz/24bit)" from album titles for API lookups.
fn clean_album_title(title: &str) -> String {
    // Remove patterns like (44.1kHz), (96kHz/24bit), (192kHz 24bit) etc.
    let mut result = String::with_capacity(title.len());
    let mut depth = 0i32;
    let mut paren_start = 0;
    for (i, c) in title.char_indices() {
        if c == '(' {
            if depth == 0 {
                paren_start = i;
            }
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth <= 0 {
                depth = 0;
                // Check if the parenthesized content looks like a hi-res suffix.
                let inner = &title[paren_start + 1..i];
                let lower = inner.to_lowercase();
                if lower.contains("khz") || lower.contains("hz") {
                    // Skip this parenthesized part (and leading whitespace).
                    while result.ends_with(' ') {
                        result.pop();
                    }
                } else {
                    // Keep it.
                    result.push_str(&title[paren_start..=i]);
                }
            }
        } else if depth == 0 {
            result.push(c);
        }
    }
    result.trim().to_string()
}

/// Regex-free artist normalization for fuzzy grouping.
/// Strips leading "The ", trailing ensemble suffixes (Quartet, Trio, Orchestra, etc.),
/// and featuring clauses.
fn normalize_artist_for_grouping(name: &str) -> String {
    let mut n = name.trim().to_string();
    if n.is_empty() {
        return String::new();
    }

    // Strip leading "The "
    if n.to_lowercase().starts_with("the ") {
        n = n[4..].to_string();
    }

    // Iteratively strip trailing ensemble suffixes.
    for _ in 0..3 {
        let trimmed = strip_ensemble_suffix(&n);
        if trimmed == n {
            break;
        }
        n = trimmed;
    }

    n.to_lowercase().trim().to_string()
}

/// Strip trailing ensemble/featuring suffixes from artist name.
fn strip_ensemble_suffix(name: &str) -> String {
    let lower = name.to_lowercase();
    let lower = lower.trim();

    // Ensemble words that can appear at the end.
    let ensemble_words = &[
        "quartet",
        "quintet",
        "trio",
        "sextet",
        "septet",
        "octet",
        "nonet",
        "orchestra",
        "big band",
        "bigband",
        "band",
        "ensemble",
        "group",
        "project",
        "combo",
        "collective",
        "players",
    ];

    // Check for "all stars" / "all-stars" variants at end.
    for pat in &["all stars", "all-stars", "allstars", "all star"] {
        if lower.ends_with(pat) {
            let cut = name.len() - pat.len();
            return name[..cut].trim().to_string();
        }
        // Also match "all star <word>" or "all-star <word>".
        if let Some(pos) = lower.rfind(pat) {
            if pos > 0 {
                return name[..pos].trim().to_string();
            }
        }
    }

    // Check for ensemble words at the end.
    for &word in ensemble_words {
        if lower.ends_with(word) {
            let cut = name.len() - word.len();
            let before = name[..cut].trim();
            if !before.is_empty() {
                return before.to_string();
            }
        }
    }

    // Check for "feat.", "featuring", "and his/her", "with his/her", "& the/his/her".
    for pat in &[
        "feat.",
        "feat ",
        "featuring ",
        "and his ",
        "and her ",
        "with his ",
        "with her ",
        "& the ",
        "& his ",
        "& her ",
    ] {
        if let Some(pos) = lower.rfind(pat) {
            if pos > 0 {
                return name[..pos].trim().to_string();
            }
        }
    }

    name.trim().to_string()
}

/// Genre family classification rules. First keyword match wins.
const GENRE_FAMILY_RULES: &[(&str, &str)] = &[
    ("soul", "soul-funk"),
    ("funk", "soul-funk"),
    ("r&b", "soul-funk"),
    ("rnb", "soul-funk"),
    ("jazz", "jazz"),
    ("classical", "classical"),
    ("baroque", "classical"),
    ("opera", "classical"),
    ("orchestral", "classical"),
    ("blues", "blues"),
    ("chanson", "chanson"),
    ("variét", "chanson"),
    ("bossa", "world"),
    ("afro", "world"),
    ("latin", "world"),
    ("reggae", "world"),
    ("tango", "world"),
    ("world", "world"),
    ("electro", "electro"),
    ("electronic", "electro"),
    ("techno", "electro"),
    ("ambient", "electro"),
    ("idm", "electro"),
    ("house", "electro"),
    ("folk", "folk"),
    ("country", "country"),
    ("soundtrack", "soundtrack"),
    ("film", "soundtrack"),
    ("rap", "hip-hop"),
    ("hip-hop", "hip-hop"),
    ("hip hop", "hip-hop"),
    ("metal", "metal"),
    ("punk", "punk"),
    ("rock", "rock"),
    ("pop", "pop"),
];

fn genre_family(genre: &str) -> &'static str {
    let g = genre.to_lowercase();
    for &(kw, fam) in GENRE_FAMILY_RULES {
        if g.contains(kw) {
            return fam;
        }
    }
    "other"
}

#[derive(Deserialize)]
struct CoherenceParams {
    min_coherence: Option<f64>,
}

/// Helper: fetch all albums with artist info for genre propagation.
fn fetch_albums_with_artists(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Result<Vec<(i64, String, Option<String>, Option<i64>, Option<String>)>, String> {
    let rows = backend.query_many(
        "SELECT al.id, al.title, al.genre, al.artist_id, ar.name as artist_name \
         FROM albums al \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         WHERE al.artist_id IS NOT NULL",
        &[],
    )?;

    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                r.get(2).and_then(|v| v.as_string()),
                r.get(3).and_then(|v| v.as_i64()),
                r.get(4).and_then(|v| v.as_string()),
            )
        })
        .collect())
}

// ---------------------------------------------------------------------------
// POST /fix-genres-by-artist
// ---------------------------------------------------------------------------

async fn fix_genres_by_artist(
    State(state): State<AppState>,
    Query(params): Query<CoherenceParams>,
) -> impl IntoResponse {
    let min_coherence = params.min_coherence.unwrap_or(0.7);

    let rows = match fetch_albums_with_artists(&state.backend) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": e})),
            )
                .into_response();
        }
    };

    // Group by artist_id: collect known genres + ids missing genre.
    let mut by_artist_genres: HashMap<i64, HashMap<String, usize>> = HashMap::new();
    let mut by_artist_missing: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    let mut artist_names: HashMap<i64, String> = HashMap::new();

    for (album_id, title, genre, artist_id, artist_name) in &rows {
        let aid = match artist_id {
            Some(id) => *id,
            None => continue,
        };
        let name = artist_name.as_deref().unwrap_or("").trim().to_string();
        if name.is_empty() || is_skip_artist(&name) {
            continue;
        }
        artist_names.entry(aid).or_insert_with(|| name.clone());
        let g = genre.as_deref().unwrap_or("").trim().to_string();
        if !g.is_empty() {
            *by_artist_genres
                .entry(aid)
                .or_default()
                .entry(g)
                .or_insert(0) += 1;
        } else {
            by_artist_missing
                .entry(aid)
                .or_default()
                .push((*album_id, title.clone()));
        }
    }

    let mut fixed = 0usize;
    let mut skipped_low_coherence = 0usize;
    let mut skipped_no_known_genre = 0usize;
    let mut details: Vec<serde_json::Value> = Vec::new();

    for (aid, missing) in &by_artist_missing {
        let counter = match by_artist_genres.get(aid) {
            Some(c) => c,
            None => {
                skipped_no_known_genre += missing.len();
                continue;
            }
        };
        let total_known: usize = counter.values().sum();
        let (top_genre, top_count) = counter
            .iter()
            .max_by_key(|&(_, c)| *c)
            .map(|(g, c)| (g.clone(), *c))
            .unwrap();
        let coherence = if total_known > 0 {
            top_count as f64 / total_known as f64
        } else {
            0.0
        };
        if coherence < min_coherence {
            skipped_low_coherence += missing.len();
            continue;
        }
        for (album_id, title) in missing {
            state
                .db
                .execute(
                    "UPDATE albums SET genre = ? WHERE id = ?",
                    &[
                        &top_genre as &dyn rusqlite::types::ToSql,
                        album_id as &dyn rusqlite::types::ToSql,
                    ],
                )
                .ok();
            fixed += 1;
            if details.len() < 200 {
                details.push(json!({
                    "album": title,
                    "artist": artist_names.get(aid).unwrap_or(&"?".to_string()),
                    "genre": top_genre,
                    "coherence": (coherence * 100.0).round() / 100.0,
                    "based_on": total_known,
                }));
            }
        }
    }

    let total_candidates: usize = by_artist_missing.values().map(|m| m.len()).sum();

    Json(json!({
        "ok": true,
        "total": total_candidates,
        "fixed": fixed,
        "skipped_low_coherence": skipped_low_coherence,
        "skipped_no_known_genre": skipped_no_known_genre,
        "min_coherence": min_coherence,
        "details": details,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /fix-genres-by-artist-fuzzy
// ---------------------------------------------------------------------------

async fn fix_genres_by_artist_fuzzy(
    State(state): State<AppState>,
    Query(params): Query<CoherenceParams>,
) -> impl IntoResponse {
    let min_coherence = params.min_coherence.unwrap_or(0.7);

    let rows = match fetch_albums_with_artists(&state.backend) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": e})),
            )
                .into_response();
        }
    };

    // Group by normalized artist name (fuzzy).
    let mut by_group_genres: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let mut by_group_missing: HashMap<String, Vec<(i64, String, String)>> = HashMap::new();
    let mut group_display: HashMap<String, String> = HashMap::new();

    for (album_id, title, genre, _artist_id, artist_name) in &rows {
        let name = artist_name.as_deref().unwrap_or("").trim().to_string();
        if name.is_empty() || is_skip_artist(&name) {
            continue;
        }
        let key = normalize_artist_for_grouping(&name);
        if key.is_empty() {
            continue;
        }
        // Display name: shortest variant as canonical label.
        let current = group_display
            .get(&key)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if name.len() < current {
            group_display.insert(key.clone(), name.clone());
        }
        let g = genre.as_deref().unwrap_or("").trim().to_string();
        if !g.is_empty() {
            *by_group_genres
                .entry(key.clone())
                .or_default()
                .entry(g)
                .or_insert(0) += 1;
        } else {
            by_group_missing
                .entry(key)
                .or_default()
                .push((*album_id, title.clone(), name.clone()));
        }
    }

    let mut fixed = 0usize;
    let mut skipped_low_coherence = 0usize;
    let mut skipped_no_known_genre = 0usize;
    let mut details: Vec<serde_json::Value> = Vec::new();

    for (key, missing) in &by_group_missing {
        let counter = match by_group_genres.get(key) {
            Some(c) => c,
            None => {
                skipped_no_known_genre += missing.len();
                continue;
            }
        };
        let total_known: usize = counter.values().sum();
        let (top_genre, top_count) = counter
            .iter()
            .max_by_key(|&(_, c)| *c)
            .map(|(g, c)| (g.clone(), *c))
            .unwrap();
        let coherence = if total_known > 0 {
            top_count as f64 / total_known as f64
        } else {
            0.0
        };
        if coherence < min_coherence {
            skipped_low_coherence += missing.len();
            continue;
        }
        for (album_id, title, original_artist) in missing {
            state
                .db
                .execute(
                    "UPDATE albums SET genre = ? WHERE id = ?",
                    &[
                        &top_genre as &dyn rusqlite::types::ToSql,
                        album_id as &dyn rusqlite::types::ToSql,
                    ],
                )
                .ok();
            fixed += 1;
            if details.len() < 200 {
                details.push(json!({
                    "album": title,
                    "artist": original_artist,
                    "group": group_display.get(key).unwrap_or(key),
                    "genre": top_genre,
                    "coherence": (coherence * 100.0).round() / 100.0,
                    "based_on": total_known,
                }));
            }
        }
    }

    let total_candidates: usize = by_group_missing.values().map(|m| m.len()).sum();

    Json(json!({
        "ok": true,
        "total": total_candidates,
        "fixed": fixed,
        "skipped_low_coherence": skipped_low_coherence,
        "skipped_no_known_genre": skipped_no_known_genre,
        "min_coherence": min_coherence,
        "details": details,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /fix-genres-by-family
// ---------------------------------------------------------------------------

async fn fix_genres_by_family(
    State(state): State<AppState>,
    Query(params): Query<CoherenceParams>,
) -> impl IntoResponse {
    let min_coherence = params.min_coherence.unwrap_or(0.7);

    let rows = match fetch_albums_with_artists(&state.backend) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": e})),
            )
                .into_response();
        }
    };

    // group_key -> family_name -> count
    let mut family_counts: HashMap<String, HashMap<&str, usize>> = HashMap::new();
    // group_key -> family_name -> specific_genre -> count
    let mut family_specific: HashMap<String, HashMap<&str, HashMap<String, usize>>> =
        HashMap::new();
    let mut by_group_missing: HashMap<String, Vec<(i64, String, String)>> = HashMap::new();
    let mut group_display: HashMap<String, String> = HashMap::new();

    for (album_id, title, genre, _artist_id, artist_name) in &rows {
        let name = artist_name.as_deref().unwrap_or("").trim().to_string();
        if name.is_empty() || is_skip_artist(&name) {
            continue;
        }
        let key = normalize_artist_for_grouping(&name);
        if key.is_empty() {
            continue;
        }
        let current = group_display
            .get(&key)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if name.len() < current {
            group_display.insert(key.clone(), name.clone());
        }
        let g = genre.as_deref().unwrap_or("").trim().to_string();
        if !g.is_empty() {
            let fam = genre_family(&g);
            *family_counts
                .entry(key.clone())
                .or_default()
                .entry(fam)
                .or_insert(0) += 1;
            *family_specific
                .entry(key.clone())
                .or_default()
                .entry(fam)
                .or_default()
                .entry(g)
                .or_insert(0) += 1;
        } else {
            by_group_missing
                .entry(key)
                .or_default()
                .push((*album_id, title.clone(), name.clone()));
        }
    }

    let mut fixed = 0usize;
    let mut skipped_low_coherence = 0usize;
    let mut skipped_no_known_genre = 0usize;
    let mut skipped_only_other_family = 0usize;
    let mut details: Vec<serde_json::Value> = Vec::new();

    for (key, missing) in &by_group_missing {
        let counter = match family_counts.get(key) {
            Some(c) => c,
            None => {
                skipped_no_known_genre += missing.len();
                continue;
            }
        };
        let total_known: usize = counter.values().sum();

        // Pick top family (excluding "other").
        let top_family = counter
            .iter()
            .filter(|&(f, _)| *f != "other")
            .max_by_key(|&(_, c)| *c);
        let (top_family, top_count) = match top_family {
            Some((f, c)) => (*f, *c),
            None => {
                skipped_only_other_family += missing.len();
                continue;
            }
        };
        let coherence = if total_known > 0 {
            top_count as f64 / total_known as f64
        } else {
            0.0
        };
        if coherence < min_coherence {
            skipped_low_coherence += missing.len();
            continue;
        }

        // Most common specific genre within that family.
        let target_genre = family_specific
            .get(key)
            .and_then(|fam_map| fam_map.get(top_family))
            .and_then(|specific| {
                specific
                    .iter()
                    .max_by_key(|&(_, c)| *c)
                    .map(|(g, _)| g.clone())
            })
            .unwrap_or_default();

        if target_genre.is_empty() {
            continue;
        }

        for (album_id, title, original_artist) in missing {
            state
                .db
                .execute(
                    "UPDATE albums SET genre = ? WHERE id = ?",
                    &[
                        &target_genre as &dyn rusqlite::types::ToSql,
                        album_id as &dyn rusqlite::types::ToSql,
                    ],
                )
                .ok();
            fixed += 1;
            if details.len() < 200 {
                details.push(json!({
                    "album": title,
                    "artist": original_artist,
                    "group": group_display.get(key).unwrap_or(key),
                    "family": top_family,
                    "genre": target_genre,
                    "family_coherence": (coherence * 100.0).round() / 100.0,
                    "based_on": total_known,
                }));
            }
        }
    }

    let total_candidates: usize = by_group_missing.values().map(|m| m.len()).sum();

    Json(json!({
        "ok": true,
        "total": total_candidates,
        "fixed": fixed,
        "skipped_low_coherence": skipped_low_coherence,
        "skipped_no_known_genre": skipped_no_known_genre,
        "skipped_only_other_family": skipped_only_other_family,
        "min_coherence": min_coherence,
        "details": details,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /fix-genres (Last.fm + Discogs)
// ---------------------------------------------------------------------------

async fn fix_genres(State(state): State<AppState>) -> impl IntoResponse {
    let svc_mgr = tune_core::services_manager::ServicesManager::with_backend(state.backend.clone());

    // Prefer DB-stored credentials, fall back to config / settings repo.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let lastfm_key = svc_mgr
        .get_credential("lastfm", "api_key")
        .or_else(|| settings.get("lastfm_api_key").ok().flatten())
        .filter(|s| !s.is_empty());
    let discogs_token = svc_mgr
        .get_credential("discogs", "token")
        .or_else(|| state.config.discogs_token.clone())
        .filter(|s| !s.is_empty());

    if lastfm_key.is_none() && discogs_token.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "No Last.fm or Discogs credentials configured"})),
        )
            .into_response();
    }

    // Fetch albums with no genre.
    let rows = {
        let result: Result<Vec<(i64, String, Option<String>)>, String> = state
            .backend
            .query_many(
                "SELECT al.id, al.title, ar.name as artist_name \
             FROM albums al \
             LEFT JOIN artists ar ON al.artist_id = ar.id \
             WHERE al.genre IS NULL OR al.genre = '' \
             ORDER BY al.title",
                &[],
            )
            .map(|rows| {
                rows.into_iter()
                    .map(|r| {
                        (
                            r.get(0).and_then(|v| v.as_i64()).unwrap_or(0),
                            r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                            r.get(2).and_then(|v| v.as_string()),
                        )
                    })
                    .collect()
            });
        // result is already Result<Vec<...>, String> from the backend query above.
        match result {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error": e})),
                )
                    .into_response();
            }
        }
    };

    if rows.is_empty() {
        return Json(json!({"ok": true, "total": 0, "fixed": 0})).into_response();
    }

    // When respect_vocabulary is on, only assign genres already in library.
    let respect_vocab = settings
        .get("metadata_fix_genres_respect_vocabulary")
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    let allowed_genres: Option<HashMap<String, String>> = if respect_vocab {
        state
            .backend
            .query_many(
                "SELECT DISTINCT genre FROM albums WHERE genre IS NOT NULL AND genre <> ''",
                &[],
            )
            .ok()
            .map(|rows| {
                rows.into_iter()
                    .filter_map(|r| r.first().and_then(|v| v.as_string()))
                    .map(|g| {
                        let lower = g.to_lowercase();
                        (lower, g)
                    })
                    .collect::<HashMap<String, String>>()
            })
    } else {
        None
    };

    let gmap = genre_map();
    let total = rows.len();
    let mut fixed = 0usize;
    let mut details: Vec<serde_json::Value> = Vec::new();

    let client = &state.http_client;

    for (album_id, album_title, artist_name) in &rows {
        let artist_name = artist_name.as_deref().unwrap_or("");
        if album_title.is_empty() || album_title == "Unknown Album" {
            continue;
        }

        let clean_title = clean_album_title(album_title);
        let mut genre: Option<String> = None;

        // 1) Last.fm
        if let Some(ref api_key) = lastfm_key {
            let resp = client
                .get("https://ws.audioscrobbler.com/2.0/")
                .query(&[
                    ("method", "album.getinfo"),
                    ("api_key", api_key.as_str()),
                    ("artist", artist_name),
                    ("album", &clean_title),
                    ("format", "json"),
                ])
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;

            if let Ok(resp) = resp {
                if resp.status().is_success() {
                    if let Ok(data) = resp.json::<serde_json::Value>().await {
                        let tags: Vec<String> = data
                            .get("album")
                            .and_then(|a| a.get("tags"))
                            .and_then(|t| t.get("tag"))
                            .and_then(|t| t.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
                                    .map(|s| s.to_string())
                                    .collect()
                            })
                            .unwrap_or_default();
                        genre = normalize_genre(&tags, allowed_genres.as_ref(), &gmap);
                    }
                }
            }
        }

        // 2) Discogs fallback
        if genre.is_none() {
            if let Some(ref token) = discogs_token {
                let mut query_params = vec![
                    ("release_title".to_string(), clean_title.clone()),
                    ("type".to_string(), "release".to_string()),
                    ("per_page".to_string(), "3".to_string()),
                ];
                if !artist_name.is_empty() && artist_name != "Unknown Artist" && artist_name != "?"
                {
                    query_params.push(("artist".to_string(), artist_name.to_string()));
                }

                let resp = client
                    .get("https://api.discogs.com/database/search")
                    .query(&query_params)
                    .header("User-Agent", "TuneServer/2.0 +https://mozaiklabs.fr")
                    .header("Authorization", format!("Discogs token={token}"))
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await;

                if let Ok(resp) = resp {
                    if resp.status().as_u16() == 429 {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    } else if resp.status().is_success() {
                        if let Ok(data) = resp.json::<serde_json::Value>().await {
                            if let Some(results) = data.get("results").and_then(|r| r.as_array()) {
                                for hit in results {
                                    let mut styles: Vec<String> = Vec::new();
                                    if let Some(arr) = hit.get("style").and_then(|v| v.as_array()) {
                                        for v in arr {
                                            if let Some(s) = v.as_str() {
                                                styles.push(s.to_string());
                                            }
                                        }
                                    }
                                    if let Some(arr) = hit.get("genre").and_then(|v| v.as_array()) {
                                        for v in arr {
                                            if let Some(s) = v.as_str() {
                                                styles.push(s.to_string());
                                            }
                                        }
                                    }
                                    genre =
                                        normalize_genre(&styles, allowed_genres.as_ref(), &gmap);
                                    if genre.is_some() {
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if let Some(ref g) = genre {
            state
                .db
                .execute(
                    "UPDATE albums SET genre = ? WHERE id = ?",
                    &[
                        g as &dyn rusqlite::types::ToSql,
                        album_id as &dyn rusqlite::types::ToSql,
                    ],
                )
                .ok();
            fixed += 1;
            if details.len() < 100 {
                details.push(json!({
                    "album": album_title,
                    "artist": artist_name,
                    "genre": g,
                }));
            }
        }

        // Rate-limit to avoid hammering APIs.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    Json(json!({
        "ok": true,
        "total": total,
        "fixed": fixed,
        "not_found": total - fixed,
        "details": details,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /covers/album/{id} — web client compatibility endpoint
// ---------------------------------------------------------------------------

/// Fetch and assign a cover to an album. Tries Cover Art Archive (via MBID
/// or MusicBrainz search), then Discogs. Compatible with the Python server's
/// `POST /metadata/covers/album/{id}` endpoint that the web client calls.
async fn fetch_album_cover(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"ok": false, "error": "album not found"})),
            )
                .into_response();
        }
    };

    let artist = album.artist_name.as_deref().unwrap_or("");

    // Step 1: Determine MBID — use existing or search MusicBrainz
    let mbid = match album
        .musicbrainz_release_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => {
            if !artist.is_empty() && !album.title.is_empty() {
                let found =
                    tune_core::library::artwork::search_musicbrainz_release(artist, &album.title)
                        .await;
                if let Some(ref discovered_mbid) = found {
                    state
                        .db
                        .execute(
                            "UPDATE albums SET musicbrainz_release_id = ? WHERE id = ? AND (musicbrainz_release_id IS NULL OR musicbrainz_release_id = '')",
                            &[discovered_mbid as &dyn rusqlite::types::ToSql, &id],
                        )
                        .ok();
                    tracing::info!(
                        album_id = id,
                        mbid = %discovered_mbid,
                        album = %album.title,
                        "fetch_album_cover_mbid_discovered"
                    );
                }
                found
            } else {
                None
            }
        }
    };

    // Step 2: Try Cover Art Archive
    if let Some(ref mbid_val) = mbid {
        if let Some(data) = tune_core::library::artwork::fetch_cover_art(mbid_val).await {
            let cache_dir = super::library::artwork_cache_dir();
            let hash = tune_core::library::artwork::artwork_hash(mbid_val);
            if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, "jpg").is_some()
            {
                repo.force_update_cover_path(id, &hash).ok();
                return Json(json!({
                    "ok": true,
                    "cover_path": hash,
                    "source": "coverartarchive",
                    "size": data.len(),
                }))
                .into_response();
            }
        }
    }

    // Step 3: Try Discogs fallback
    let discogs_token = state
        .config
        .discogs_token
        .clone()
        .or_else(|| std::env::var("TUNE_DISCOGS_TOKEN").ok())
        .or_else(|| std::env::var("DISCOGS_TOKEN").ok())
        .unwrap_or_default();

    if !discogs_token.is_empty() && !album.title.is_empty() {
        let cache_dir = super::library::artwork_cache_dir();
        let cache_str = cache_dir.to_string_lossy().to_string();
        if let Some(path) = tune_core::library::cover_fetcher::fetch_cover_from_discogs(
            &album.title,
            artist,
            &discogs_token,
            &cache_str,
        )
        .await
        {
            // Extract filename as hash or use the path
            let hash = std::path::Path::new(&path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(&path)
                .to_string();
            repo.force_update_cover_path(id, &hash).ok();
            return Json(json!({
                "ok": true,
                "cover_path": hash,
                "source": "discogs",
            }))
            .into_response();
        }
    }

    Json(json!({"ok": false, "error": "No cover found"})).into_response()
}

// ---------------------------------------------------------------------------
// GET /covers/search — search for covers from multiple sources
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CoverSearchParams {
    album: String,
    artist: Option<String>,
    release_id: Option<String>,
}

async fn search_covers(
    State(state): State<AppState>,
    Query(params): Query<CoverSearchParams>,
) -> impl IntoResponse {
    let discogs_token = state
        .config
        .discogs_token
        .clone()
        .or_else(|| std::env::var("TUNE_DISCOGS_TOKEN").ok())
        .or_else(|| std::env::var("DISCOGS_TOKEN").ok())
        .unwrap_or_default();

    let cache_dir = super::library::artwork_cache_dir();
    let cache_str = cache_dir.to_string_lossy().to_string();

    let results = tune_core::library::cover_fetcher::search_covers(
        &params.album,
        params.artist.as_deref().unwrap_or(""),
        params.release_id.as_deref().unwrap_or(""),
        &discogs_token,
        &cache_str,
    )
    .await;

    let results_json: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            json!({
                "source": r.source,
                "local_path": r.local_path,
            })
        })
        .collect();

    Json(json!({"results": results_json}))
}

// ---------------------------------------------------------------------------
// POST /albums/merge — merge specific albums by IDs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MergeAlbumsRequest {
    album_ids: Vec<i64>,
}

async fn merge_albums(
    State(state): State<AppState>,
    Json(body): Json<MergeAlbumsRequest>,
) -> impl IntoResponse {
    if body.album_ids.len() < 2 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "need at least 2 album_ids"})),
        )
            .into_response();
    }

    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let mut best_id = body.album_ids[0];
    let mut best_count = 0i64;
    for &aid in &body.album_ids {
        let cnt = track_repo
            .list_by_album(aid)
            .map(|t| t.len() as i64)
            .unwrap_or(0);
        if cnt > best_count {
            best_count = cnt;
            best_id = aid;
        }
    }

    let mut tracks_moved = 0i64;
    let mut merged_ids = Vec::new();
    for &aid in &body.album_ids {
        if aid == best_id {
            continue;
        }
        let moved = state
            .db
            .execute(
                "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                &[
                    &best_id as &dyn rusqlite::types::ToSql,
                    &aid as &dyn rusqlite::types::ToSql,
                ],
            )
            .unwrap_or(0) as i64;
        tracks_moved += moved;
        state
            .db
            .execute(
                "DELETE FROM albums WHERE id = ?",
                &[&aid as &dyn rusqlite::types::ToSql],
            )
            .ok();
        merged_ids.push(aid);
    }

    state
        .db
        .execute_batch(
            "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)",
        )
        .ok();

    let total_tracks = track_repo
        .list_by_album(best_id)
        .map(|t| t.len() as i64)
        .unwrap_or(0);

    Json(json!({
        "master_id": best_id,
        "tracks_moved": tracks_moved,
        "total_tracks": total_tracks,
        "merged_ids": merged_ids,
    }))
    .into_response()
}
