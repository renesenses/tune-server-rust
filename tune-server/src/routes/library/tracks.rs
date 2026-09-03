use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use lofty::file::TaggedFileExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::TrackRepo;

use super::query_multi::track_filter_from_raw;
use crate::error::AppError;
use crate::routes::filtre_sources::FiltreSources;

/// Identifiant de la passe de relecture des métadonnées au registre
/// `background_tasks` (#2129).
const TACHE_RESCAN_METADATA: &str = "rescan_metadata";

/// Cadence de publication de l'avancement au registre.
const JALON_AVANCEMENT_RESCAN: usize = 50;

/// Build a JSON array string for the `genres` column from parsed metadata.
fn build_genres_json(genres: &[String], genre: Option<&str>) -> Option<String> {
    if !genres.is_empty() {
        Some(serde_json::to_string(genres).unwrap_or_default())
    } else if let Some(g) = genre {
        if g.is_empty() {
            None
        } else {
            let split = tune_core::metadata::split_genre_tag(g);
            if split.is_empty() {
                None
            } else {
                Some(serde_json::to_string(&split).unwrap_or_default())
            }
        }
    } else {
        None
    }
}

/// Apply freshly-read metadata from disk onto an existing Track struct.
fn apply_metadata_to_track(
    track: &mut tune_core::db::models::Track,
    m: &tune_core::metadata::TrackMetadata,
) {
    if let Some(ref v) = m.title {
        track.title = v.clone();
    }
    if let Some(ref v) = m.artist {
        track.artist_name = Some(v.clone());
    }
    track.album_artist = m.album_artist.clone();
    track.genre = m.genre.clone();
    track.genres = build_genres_json(&m.genres, m.genre.as_deref());
    track.composer = m
        .credits
        .iter()
        .find(|c| c.role == "composer")
        .map(|c| c.name.clone());
    track.year = m.year.map(|y| y as i32);
    track.bpm = m.bpm;
    track.label = m.label.clone();
    track.isrc = m.isrc.clone();
    track.musicbrainz_recording_id = m.musicbrainz_recording_id.clone();
    track.sample_rate = m.sample_rate.map(|s| s as i32);
    track.bit_depth = m.bit_depth.map(|b| b as i32);
    track.channels = m.channels.unwrap_or(2) as i32;
    track.duration_ms = m.duration_ms.unwrap_or(0) as i64;
    track.format = m.format.clone();
    track.track_number = m.track_number.unwrap_or(0) as i32;
    track.disc_number = m.disc_number.unwrap_or(1) as i32;
    track.disc_subtitle = m.disc_subtitle.clone();
}

/// Recopie le Dynamic Range PAR PISTE sur des pistes sérialisées (#1388).
///
/// Le tag `DYNAMIC RANGE` est lu au scan et rangé dans
/// `track_metadata['dr_track']` (#1806). Depuis #2809 il ressortait sur les
/// pistes d'un album — et sur elles SEULES : la table des titres et la fiche
/// d'une piste, qui affichent pourtant la même ligne de qualité, sortaient
/// nues. C'est le trou que ce chemin ferme.
///
/// Même clé, même contrat que sur les pistes d'un album : `dynamic_range` est
/// ABSENTE quand la piste n'a pas le tag — jamais `null`, jamais `0`, DR0
/// étant la mesure d'un master saturé et non une absence.
///
/// Une seule requête indexée par page, et aucune du tout sur une page vide
/// (`get_key_for_tracks` court-circuite sur une liste d'identifiants vide).
fn joindre_dr_par_piste(state: &AppState, items: Vec<tune_core::db::models::Track>) -> Vec<Value> {
    let track_ids: Vec<i64> = items.iter().filter_map(|t| t.id).collect();
    let dr = TrackMetadataRepo::with_backend(state.backend.clone())
        .get_key_for_tracks("dr_track", &track_ids)
        .unwrap_or_default();
    super::albums::attach_track_tags(items, &[("dynamic_range", &dr)])
}

#[derive(Deserialize)]
pub(super) struct QuickFavQuery {
    profile_id: Option<i64>,
}

/// Query parameters for GET /library/tracks.
///
/// ⚠️ **Les facettes ne sont PAS des champs de cette structure.** La
/// `Deserialize` dérivée refuse une clé en double (`duplicate field`), donc
/// `?format=aiff&format=flac` rendait 400 tant qu'un champ `format` existait
/// ici (#2168). Elles se lisent toutes dans `query_multi::track_filter_from_raw`,
/// à partir de la chaîne de requête BRUTE — qui reprend au passage la
/// validation de type que `serde` assurait (`?year=abc` → 400).
///
/// Ne restent ici que la pagination et ce qui ne peut pas se répéter.
#[derive(Deserialize, Default)]
pub(super) struct TrackFilterQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    /// Facette Collections : nom d'une collection manuelle ou intelligente.
    /// MONOVALUÉE — voir `TrackFilter::collection_ids`.
    pub collection: Option<String>,
}

pub(super) async fn list_tracks(
    State(state): State<AppState>,
    Query(p): Query<TrackFilterQuery>,
    RawQuery(raw): RawQuery,
) -> Result<Json<Value>, AppError> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);

    // Facettes à plusieurs valeurs : la clé répétée (`?format=aiff&format=flac`)
    // se lit dans la chaîne BRUTE, que `serde_urlencoded` ne sait pas agréger —
    // et qu'il refuse même en double.
    let mut filter = track_filter_from_raw(raw.as_deref())?;

    // Resolve the collection name so /library/tracks?collection=<name> filters
    // to its members. A MANUAL collection resolves to album ids (JSON settings);
    // a SMART collection resolves to concrete track ids (its compiled rule query).
    // Manual wins on a name clash. An unknown name → empty album set → matches
    // nothing (the requested collection is simply empty).
    //
    // ⚠️ Résolution PARTAGÉE avec le compteur de facettes (#1864) : les deux
    // routes doivent désigner le même ensemble, sinon le rail annonce des
    // effectifs que cette liste ne rend pas.
    let scope = p
        .collection
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|name| super::facets::resolve_collection(&state, name))
        .unwrap_or_default();

    filter.collection_ids = scope.albums;
    filter.collection_track_ids = scope.tracks;

    // ⚠️ `is_active()` doit rester le MIROIR EXACT des prédicats que
    // `list_filtered` va produire. S'il rend `true` sans qu'aucun prédicat ne
    // suive, la route emprunte le chemin filtré, n'y filtre rien, et rend la
    // bibliothèque ENTIÈRE en annonçant un filtre actif — c'est exactement ce
    // que faisait `?favorite=1` avant #2168.
    if filter.is_active() {
        match repo.list_filtered(&filter, limit, offset) {
            Ok((items, total)) => {
                let items = joindre_dr_par_piste(&state, items);
                Ok(Json(
                    json!({"items": items, "total": total, "limit": limit, "offset": offset}),
                ))
            }
            Err(e) => {
                tracing::error!(error = %e, "list_tracks_filtered_query_failed");
                Ok(Json(
                    json!({"items": [], "total": 0, "limit": limit, "offset": offset}),
                ))
            }
        }
    } else {
        // Même exclusion des albums masqués que le chemin facetté (#1391) :
        // sans elle, la vue par défaut fuirait ce que la vue filtrée cache.
        let total = repo.count_visible().unwrap_or(0);
        let items = match repo.list_visible(limit, offset) {
            Ok(tracks) => tracks,
            Err(e) => {
                tracing::error!(
                    error = %e,
                    limit,
                    offset,
                    total,
                    "list_tracks_query_failed — stats show {total} tracks but query returned error"
                );
                Vec::new()
            }
        };
        let items = joindre_dr_par_piste(&state, items);
        Ok(Json(
            json!({"items": items, "total": total, "limit": limit, "offset": offset}),
        ))
    }
}

pub(super) async fn track_count(State(state): State<AppState>) -> Json<Value> {
    let count = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    Json(json!({ "count": count }))
}

#[derive(Deserialize)]
pub(super) struct SimilarParams {
    limit: Option<i64>,
}

/// GET /library/tracks/{id}/similar — acoustically similar tracks ("Plus comme
/// ça", Phase 2). Ranks the library by cosine distance to the seed's CLAP
/// embedding via `acoustic_neighbors`, hydrates the tracks and re-emits them in
/// similarity order with a `similarity` score. Empty (not an error) when the
/// seed has no embedding yet — the audio-embedding pass hasn't covered it, or
/// this build never computed vectors — so the client can fall back gracefully.
pub(super) async fn track_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(p): Query<SimilarParams>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200) as usize;
    let neighbors =
        tune_core::audio::embedding_store::acoustic_neighbors(&state.backend, id, limit);
    if neighbors.is_empty() {
        return Json(json!({ "seed_track_id": id, "count": 0, "items": [] }));
    }
    let ids: Vec<i64> = neighbors.iter().map(|(t, _)| *t).collect();
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .list_by_ids(&ids)
        .unwrap_or_default();
    let by_id: std::collections::HashMap<i64, &tune_core::db::models::Track> =
        tracks.iter().filter_map(|t| t.id.map(|i| (i, t))).collect();
    // Re-emit in acoustic-rank order (list_by_ids is unordered) with the score.
    let items: Vec<Value> = neighbors
        .iter()
        .filter_map(|(tid, score)| {
            let t = by_id.get(tid)?;
            let mut v = serde_json::to_value(t).ok()?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "similarity".into(),
                    json!((score * 1000.0).round() / 1000.0),
                );
            }
            Some(v)
        })
        .collect();
    Json(json!({ "seed_track_id": id, "count": items.len(), "items": items }))
}

pub(super) async fn get_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(track)) => {
            // Dynamic Range par piste (#1388) : la fiche d'une piste rend le
            // même champ que les pistes d'un album. Sans tag, la clé reste
            // absente et la charge utile est celle d'avant, au bit près.
            let v = joindre_dr_par_piste(&state, vec![track])
                .into_iter()
                .next()
                .unwrap_or_default();
            Json(v).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn stream_track_audio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    _req_headers: HeaderMap,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref file_path) = track.file_path else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // La graphie du disque, pas celle de la base : sur un nom décomposé
    // (macOS, SMB/CIFS), `metadata()` échouait et la route rendait 404 pour un
    // fichier présent — la même piste partant pourtant sans broncher par le
    // chemin de lecture de l'orchestrateur, qui, lui, replie déjà (#1865).
    let file_path = tune_core::library::local_path::resolve_existing_local_path(file_path)
        .unwrap_or_else(|| file_path.clone());
    let path = std::path::Path::new(&file_path);
    let file_size = match tokio::fs::metadata(path).await {
        Ok(m) => m.len(),
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Seconde frontière de lecture, et la plus trompeuse (#3234). Le chemin
    // de l'orchestrateur n'est pas le seul : c'est PAR ICI que le lecteur du
    // navigateur et le serveur média UPnP demandent les octets. Sur un ISO
    // SACD, `from_extension` rend `None`, le `Content-Type` retombe sur
    // `application/octet-stream`, et la route rend un 200 parfait suivi de
    // 4 Go d'image disque : le lecteur reste muet sans qu'aucune erreur ne
    // soit jamais rendue.
    //
    // Le fichier existe et il est lisible — un 404 mentirait. Ce qui manque
    // est un outil que Tune ne fournit pas, et le motif rendu ici est mot pour
    // mot celui du rapport de parcours (#2992) et celui de la route de
    // lecture, pour que l'utilisateur ne lise pas trois phrases différentes
    // pour un seul empêchement.
    if let Some(motif) = tune_core::audio::iso_sacd::refus_de_lecture(path) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "format_not_playable",
                "message": motif,
            })),
        )
            .into_response();
    }
    let mime = track
        .format
        .as_deref()
        .and_then(tune_core::audio::formats::AudioFormat::from_extension)
        .map(|f| f.mime_type().to_string())
        .unwrap_or_else(|| "application/octet-stream".into());

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_str(&mime)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert("Content-Length", HeaderValue::from(file_size));
    headers.insert("Accept-Ranges", HeaderValue::from_static("bytes"));

    let path_owned = file_path.clone();
    let body = Body::from_stream(async_stream::stream! {
        if let Ok(mut file) = tokio::fs::File::open(&path_owned).await {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 65536];
            loop {
                match file.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(&buf[..n])),
                    Err(_e) => { break; }
                }
            }
        }
    });

    (StatusCode::OK, headers, body).into_response()
}

pub(super) async fn rescan_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let mut track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    let Some(ref file_path) = track.file_path else {
        return (StatusCode::BAD_REQUEST, "no file path").into_response();
    };

    // #1865 : le chemin stocké est en NFC, le disque peut porter le NFD.
    let file_path = tune_core::library::local_path::resolve_existing_local_path(file_path)
        .unwrap_or_else(|| file_path.clone());
    let meta = tune_core::metadata::read_metadata(std::path::Path::new(&file_path));
    match meta {
        Some(m) => {
            apply_metadata_to_track(&mut track, &m);

            if let Err(e) = repo.update(&track) {
                tracing::warn!(track_id = id, error = %e, "rescan_track_update_failed");
            }

            Json(json!({
                "status": "ok",
                "track_id": id,
                "title": m.title,
                "artist": m.artist,
                "album": m.album,
                "genre": m.genre,
                "genres": m.genres,
                "sample_rate": m.sample_rate,
                "bit_depth": m.bit_depth,
                "duration_ms": m.duration_ms,
                "year": m.year,
            }))
            .into_response()
        }
        None => (StatusCode::INTERNAL_SERVER_ERROR, "failed to read metadata").into_response(),
    }
}

pub(super) async fn quick_fav_track(
    State(state): State<AppState>,
    profile: crate::routes::active_profile::ActiveProfile,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or_else(|| profile.id());
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let is_fav = repo.is_favorite(profile_id, "track", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "track", id).ok();
    } else {
        repo.add_favorite(profile_id, "track", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "track_id": id}))
}

pub(super) async fn track_all_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let mut result = serde_json::to_value(&track).unwrap_or_default();

    // Try reading raw file tags with lofty — sur la graphie du disque (#1865).
    if let Some(path) = track
        .file_path
        .as_deref()
        .and_then(tune_core::library::local_path::resolve_existing_local_path)
    {
        if let Ok(tagged) = lofty::read_from_path(&path) {
            let tags: Vec<Value> = tagged
                .tags()
                .iter()
                .map(|tag| {
                    json!({
                        "tag_type": format!("{:?}", tag.tag_type()),
                        "items": tag.items().map(|item| format!("{:?}", item)).collect::<Vec<_>>(),
                    })
                })
                .collect();
            result["file_tags"] = json!(tags);
        }
    }

    Json(result).into_response()
}

/// GET /api/v1/library/tracks/{id}/lyrics — mode « Grand écran + paroles ».
///
/// Contract (the web client is built against this — do not change):
/// - 200: `{"synced": bool, "source": "lrc"|"tag"|"lrclib",
///          "lines": [{"t_ms": <u64|null>, "text": "..."}]}`
///   (`t_ms` is null when the source is unsynchronized)
/// - 404: `{"error": "no_lyrics"}` when no source has lyrics.
///
/// Resolution cascade:
/// 1. Sidecar `.lrc` / `.LRC` next to the audio file → synced, source "lrc".
/// 2. Embedded tag (USLT/LYRICS via lofty; the scanner also persists it in
///    `track_metadata` under the `lyrics` key). LRC timestamps inside the
///    tag → synced; otherwise raw lines with `t_ms: null` → unsynced.
/// 3. LRCLIB, only when the `lyrics_lrclib_enabled` setting is "true"
///    (cache-first, negatives retried after 14 days).
///
/// Never returns 500 for a track without lyrics; LRCLIB failures degrade
/// to a clean 404. No premium gate: this is a display feature.
pub(super) async fn track_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    // Réponses partagées avec GET /lyrics/by-meta (même contrat JSON).
    use crate::routes::lyrics::{
        no_lyrics_response as no_lyrics, plain_lines_response as plain_response,
        synced_lines_response as synced_response,
    };

    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return no_lyrics(),
    };

    // 1. Sidecar .lrc / .LRC next to the audio file.
    if let Some(ref path) = track.file_path {
        if let Some(content) = tune_core::metadata::lyrics::find_sidecar_lrc(path) {
            let lines = tune_core::metadata::lyrics::parse_lrc(&content);
            if !lines.is_empty() {
                return synced_response("lrc", &lines);
            }
        }
    }

    // 2. Embedded tag: scanner-persisted `track_metadata['lyrics']` first
    // (no file I/O), then a direct lofty read of the file.
    let meta_repo =
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(state.backend.clone());
    let tag_lyrics = meta_repo
        .get_all(id)
        .ok()
        .and_then(|m| m.get("lyrics").cloned())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            track
                .file_path
                .as_deref()
                .and_then(tune_core::metadata::lyrics::read_embedded_lyrics)
        });
    if let Some(text) = tag_lyrics {
        let lines = tune_core::metadata::lyrics::parse_lrc(&text);
        if !lines.is_empty() {
            return synced_response("tag", &lines);
        }
        if let Some(resp) = plain_response("tag", &text) {
            return resp;
        }
    }

    // 3. LRCLIB — opt-in via the generic settings key `lyrics_lrclib_enabled`.
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let lrclib_enabled = settings
        .get("lyrics_lrclib_enabled")
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    if !lrclib_enabled {
        return no_lyrics();
    }

    let artist = track.artist_name.clone().unwrap_or_default();
    if artist.is_empty() || track.title.is_empty() {
        return no_lyrics();
    }

    // Cache first (positive entries never expire; negatives retry after 14 d).
    if let Some(entry) = tune_core::lyrics::load_cache_entry(&state.backend, id) {
        if let Some(lrc) = entry
            .synced_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let lines = tune_core::metadata::lyrics::parse_lrc(lrc);
            if !lines.is_empty() {
                return synced_response("lrclib", &lines);
            }
        }
        if let Some(plain) = entry
            .plain_lyrics
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if let Some(resp) = plain_response("lrclib", plain) {
                return resp;
            }
        }
        if entry.negative_still_fresh() {
            return no_lyrics();
        }
    }

    let duration_secs = (track.duration_ms > 0).then_some(track.duration_ms / 1000);
    match tune_core::lyrics::fetch_lrclib_raw(
        &state.http_client,
        &artist,
        &track.title,
        track.album_title.as_deref(),
        duration_secs,
    )
    .await
    {
        Ok(raw) => {
            let raw = raw.unwrap_or_default();
            // Cache both hits and misses (misses are retried after 14 days).
            tune_core::lyrics::store_cache_entry(
                &state.backend,
                id,
                &track.title,
                &artist,
                raw.synced_lyrics.as_deref(),
                raw.plain_lyrics.as_deref(),
            );
            if let Some(lrc) = raw.synced_lyrics.as_deref() {
                let lines = tune_core::metadata::lyrics::parse_lrc(lrc);
                if !lines.is_empty() {
                    return synced_response("lrclib", &lines);
                }
            }
            if let Some(plain) = raw.plain_lyrics.as_deref() {
                if let Some(resp) = plain_response("lrclib", plain) {
                    return resp;
                }
            }
            no_lyrics()
        }
        // Network/protocol failure: clean 404, nothing cached so the next
        // request retries.
        Err(e) => {
            tracing::debug!(track_id = id, error = %e, "lrclib_fetch_failed");
            no_lyrics()
        }
    }
}

pub(super) async fn track_synced_lyrics(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());

    // Check DB cache
    if let Ok(Some(cached)) = repo.get_synced_lyrics(id) {
        let lines: Value = serde_json::from_str(&cached).unwrap_or(Value::Null);
        return Json(json!({ "track_id": id, "synced": true, "lines": lines })).into_response();
    }

    // Try sidecar .lrc file
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        _ => return (StatusCode::NOT_FOUND, "track not found").into_response(),
    };

    if let Some(ref path) = track.file_path {
        if let Some(lrc_content) = tune_core::metadata::lyrics::find_sidecar_lrc(path) {
            let lines = tune_core::metadata::lyrics::parse_lrc(&lrc_content);
            if !lines.is_empty() {
                let json_str = serde_json::to_string(&lines).unwrap_or_default();
                repo.set_synced_lyrics(id, &json_str).ok();
                return Json(
                    json!({ "track_id": id, "synced": true, "lines": lines, "source": "lrc_file" }),
                )
                .into_response();
            }
        }
    }

    Json(json!({ "track_id": id, "synced": false, "lines": null })).into_response()
}

pub(super) async fn track_source_links(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let repo = tune_core::db::source_link_repo::SourceLinkRepo::with_backend(state.backend.clone());
    let links = repo.get_by_track(id).unwrap_or_default();
    Json(json!({ "track_id": id, "links": links }))
}

pub(super) async fn identify_track(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<Value>,
) -> impl IntoResponse {
    let api_key = match state.config.acoustid_api_key.as_deref() {
        Some(k) if !k.is_empty() => k.to_string(),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "TUNE_ACOUSTID_API_KEY not configured"})),
            )
                .into_response();
        }
    };
    if !tune_core::metadata::fingerprint::fpcalc_available() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "fpcalc not installed"})),
        )
            .into_response();
    }

    let track_id = match body["track_id"].as_i64() {
        Some(id) => id,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "track_id required"})),
            )
                .into_response();
        }
    };

    let repo = TrackRepo::with_backend(state.backend.clone());
    let track = match repo.get(track_id) {
        Ok(Some(t)) => t,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "track not found"})),
            )
                .into_response();
        }
    };

    let file_path = match track.file_path.as_deref() {
        Some(p) => p.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "track has no file"})),
            )
                .into_response();
        }
    };

    let fp = match tune_core::metadata::fingerprint::generate_fingerprint(&file_path).await {
        Ok(fp) => fp,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };

    let matches =
        tune_core::metadata::fingerprint::lookup_acoustid(&api_key, &fp.fingerprint, fp.duration)
            .await
            .unwrap_or_default();

    let best = matches.first();
    let confidence = best.map(|m| m.score).unwrap_or(0.0);

    repo.set_acoustid(track_id, &fp.fingerprint, confidence)
        .ok();

    if let Some(m) = best {
        enregistrer_identification_acoustique(&state.backend, track_id, m);
    }

    Json(json!({
        "track_id": track_id,
        "matched": best.is_some(),
        "confidence": confidence,
        "result": best,
    }))
    .into_response()
}

pub(super) async fn track_waveform(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = TrackRepo::with_backend(state.backend.clone());

    // Return cached waveform if available
    if let Ok(Some(cached)) = repo.get_waveform(id) {
        return Json(json!({ "track_id": id, "waveform": serde_json::from_str::<Value>(&cached).unwrap_or(Value::Null) })).into_response();
    }

    // Generate on demand
    let track = match repo.get(id) {
        Ok(Some(t)) => t,
        Ok(None) => return (StatusCode::NOT_FOUND, "track not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let file_path = match track.file_path.as_deref() {
        Some(p) => p.to_string(),
        None => {
            return Json(json!({ "track_id": id, "waveform": null, "error": "no file path" }))
                .into_response();
        }
    };

    // Le décodeur reçoit la graphie du disque, pas celle de la base : sur les
    // pistes dont le nom est décomposé, `generate_waveform` rendait un vecteur
    // vide et la route répondait « file unreadable » pour un fichier qui se
    // joue très bien (#1865).
    let file_path = tune_core::library::local_path::resolve_existing_local_path(&file_path)
        .unwrap_or(file_path);
    let points = tune_core::audio::analyzer::generate_waveform(&file_path, 200).await;
    if points.is_empty() {
        return Json(json!({ "track_id": id, "waveform": null, "error": "file unreadable or unsupported format" })).into_response();
    }

    let json_str = serde_json::to_string(&points).unwrap_or_default();
    repo.set_waveform(id, &json_str).ok();

    Json(json!({ "track_id": id, "waveform": points })).into_response()
}

/// POST /api/v1/library/rescan-metadata
///
/// Re-reads tags from audio files for all local tracks and updates the DB.
/// Unlike a full scan, this does NOT discover new files or remove missing ones --
/// it only refreshes metadata (genre, year, artist, etc.) for tracks already in
/// the library. This is what users need after editing tags externally.
pub(super) async fn rescan_metadata(State(state): State<AppState>) -> impl IntoResponse {
    let backend = state.backend.clone();
    let event_bus = state.event_bus.clone();

    // Inscription au registre des tâches de fond (#2129). `rescan_metadata_status`
    // vaut « running » pendant toute la passe, mais ce réglage ne se lit que
    // depuis l'écran qui l'a lancée : relire les étiquettes de chaque fichier
    // d'une bibliothèque dure longtemps, et rien ne le disait ailleurs.
    let garde_tache = state.background_tasks.begin(
        TACHE_RESCAN_METADATA,
        "Relecture des métadonnées des fichiers…",
        "maintenance",
    );
    let taches = state.background_tasks.clone();

    tokio::spawn(async move {
        let _garde_tache = garde_tache; // libère la tâche à la fin de ce futur
        let backend_inner = backend.clone();
        let result = tokio::task::spawn_blocking(move || {
            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend_inner.clone());
            if let Err(e) = settings.set("rescan_metadata_status", "running") {
                tracing::warn!(error = %e, "rescan_metadata_status_set_failed");
            }

            let track_repo = TrackRepo::with_backend(backend_inner.clone());
            let tracks = match track_repo.list_all_local() {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "rescan_metadata_list_failed");
                    settings.set("rescan_metadata_status", "idle").ok();
                    return;
                }
            };

            let total = tracks.len();
            let mut updated = 0usize;
            let mut skipped = 0usize;
            let mut errors = 0usize;
            taches.update_progress(TACHE_RESCAN_METADATA, 0, total as u64, "Métadonnées");

            for track in tracks {
                // Jalon en TÊTE de boucle, pas en queue : trois branches de ce
                // corps sortent par `continue` (pas de chemin, fichier absent,
                // étiquettes illisibles). Placé en queue, l'avancement resterait
                // à 0 sur une bibliothèque dont le disque est démonté — le cas
                // où l'on a justement besoin de voir que la passe tourne.
                // Un jalon, pas une publication par piste : le registre émet un
                // événement WebSocket à chaque changement.
                let traitees = updated + skipped + errors;
                if traitees % JALON_AVANCEMENT_RESCAN == 0 {
                    taches.update_progress(
                        TACHE_RESCAN_METADATA,
                        traitees as u64,
                        total as u64,
                        "Métadonnées",
                    );
                }

                let Some(ref file_path) = track.file_path else {
                    skipped += 1;
                    continue;
                };

                // Passe de fond : sans le repli, toute une bibliothèque venue
                // d'un Mac est comptée « sautée » et ne voit jamais ses
                // étiquettes relues — 147 pistes sur 46 877 pour `.18` (#1865).
                let Some(reel) =
                    tune_core::library::local_path::resolve_existing_local_path(file_path)
                else {
                    skipped += 1;
                    continue;
                };
                let path = std::path::Path::new(&reel);

                let Some(meta) = tune_core::metadata::read_metadata(path) else {
                    errors += 1;
                    continue;
                };

                let mut t = track.clone();
                apply_metadata_to_track(&mut t, &meta);

                match track_repo.update(&t) {
                    Ok(_) => updated += 1,
                    Err(e) => {
                        tracing::warn!(track_id = ?t.id, error = %e, "rescan_metadata_update_failed");
                        errors += 1;
                    }
                }
            }

            // Refresh album genre/quality from their tracks
            backend_inner.execute_batch(
                "UPDATE albums SET \
                 genre = (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1), \
                 genres = (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL AND t.genres != '' LIMIT 1), \
                 format = (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1), \
                 sample_rate = (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id), \
                 bit_depth = (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id) \
                 WHERE source = 'local' OR source IS NULL",
            )
            .ok();

            settings.set("rescan_metadata_status", "idle").ok();
            settings
                .set(
                    "rescan_metadata_result",
                    &serde_json::json!({
                        "total": total,
                        "updated": updated,
                        "skipped": skipped,
                        "errors": errors,
                    })
                    .to_string(),
                )
                .ok();

            tracing::info!(total, updated, skipped, errors, "rescan_metadata_complete");

            event_bus.emit(
                "library.rescan_metadata.completed",
                serde_json::json!({
                    "total": total,
                    "updated": updated,
                    "skipped": skipped,
                    "errors": errors,
                }),
            );
        })
        .await;

        if let Err(e) = result {
            tracing::error!("rescan_metadata_task_panicked: {:?}", e);
            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend);
            settings.set("rescan_metadata_status", "idle").ok();
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": "rescan_metadata_started" })),
    )
}

/// GET /api/v1/library/rescan-metadata/status
pub(super) async fn rescan_metadata_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let status = settings
        .get("rescan_metadata_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let result = settings
        .get("rescan_metadata_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    Json(json!({
        "status": status,
        "result": result,
    }))
}

// --- Track extended metadata endpoints ---

/// GET /api/v1/library/tracks/{id}/metadata
/// Returns all extended metadata key-value pairs for a track.
pub(super) async fn track_metadata_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::track_metadata_repo::TrackMetadataRepo;

    let repo = TrackMetadataRepo::with_backend(state.backend.clone());
    match repo.get_all(id) {
        Ok(meta) => Json(json!(meta)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// PUT /api/v1/library/tracks/{id}/metadata
/// Batch-sets extended metadata fields from a JSON object body.
/// After saving to DB, also writes tags to the audio file (best-effort).
pub(super) async fn track_metadata_put(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use tune_core::db::track_metadata_repo::TrackMetadataRepo;

    // Verify the track exists and get its file_path
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let file_path = match track_repo.get(id) {
        Ok(Some(track)) => track.file_path,
        Ok(None) => return (StatusCode::NOT_FOUND, "track not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    // Save to DB (source of truth)
    let repo = TrackMetadataRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.set_batch(id, &body) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    // Write tags to file (best-effort, don't fail the request)
    let mut file_write_error: Option<String> = None;
    if let Some(ref path) = file_path {
        if let Err(e) = tune_core::metadata::tag_writer::write_metadata_to_file(path, &body).await {
            tracing::warn!(
                track_id = id,
                path = path.as_str(),
                error = e.as_str(),
                "tag_write_to_file_failed"
            );
            file_write_error = Some(e);
        }
    }

    let mut resp = json!({"status": "ok", "fields": body.len()});
    if let Some(err) = file_write_error {
        resp["file_write_warning"] = json!(err);
    }
    Json(resp).into_response()
}

/// Ce qu'une reconnaissance acoustique a le droit d'ecrire.
///
/// Deux ecritures INDEPENDANTES, et c'est tout l'objet de cette fonction :
///
/// - le **titre** ne se remplace que s'il n'en est pas un (`Track 03`,
///   `Unknown…`). AcoustID rend le titre canonique de l'enregistrement, qui
///   n'est pas forcement celui que l'utilisateur a choisi ;
/// - l'**identifiant d'enregistrement** n'a rien a voir avec le titre
///   affiche. Il etait pourtant ecrit par la MEME requete, sous la meme garde :
///   une piste correctement titree — l'immense majorite d'une bibliotheque —
///   voyait donc son identifiant, obtenu a 0,8 de confiance, purement jete.
///
/// L'identifiant se REMPLIT, il ne s'ecrase pas : celui qui vient des tags du
/// fichier (Picard) fait autorite sur une reconnaissance acoustique.
///
/// C'est la cle dont depend tout rapprochement par oeuvre (#2374), et sa
/// couverture est le verrou du chantier d'identification.
fn enregistrer_identification_acoustique(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    track_id: i64,
    m: &tune_core::metadata::fingerprint::AcoustIdMatch,
) {
    use tune_core::db::backend::ToSqlValue;

    /// En dessous, la reconnaissance n'engage rien : on ne touche a rien.
    const CONFIANCE_MINIMALE: f64 = 0.8;
    if m.score < CONFIANCE_MINIMALE {
        return;
    }

    if !m.title.is_empty() {
        backend
            .execute(
                "UPDATE tracks SET title = ? \
                 WHERE id = ? AND (title LIKE 'Track %' OR title LIKE 'Unknown%')",
                &[&m.title as &dyn ToSqlValue, &track_id as &dyn ToSqlValue],
            )
            .ok();
    }

    if !m.recording_id.is_empty() {
        backend
            .execute(
                "UPDATE tracks SET musicbrainz_recording_id = ? \
                 WHERE id = ? AND (musicbrainz_recording_id IS NULL OR musicbrainz_recording_id = '')",
                &[&m.recording_id as &dyn ToSqlValue, &track_id as &dyn ToSqlValue],
            )
            .ok();
    }
}

#[cfg(test)]
mod identification_acoustique_tests {
    use super::enregistrer_identification_acoustique;
    use std::sync::Arc;
    use tune_core::db::backend::{DbBackend, ToSqlValue};
    use tune_core::db::models::Track;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::db::track_repo::TrackRepo;
    use tune_core::metadata::fingerprint::AcoustIdMatch;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        Arc::new(db)
    }

    fn piste(backend: &Arc<dyn DbBackend>, titre: &str, mbid: Option<&str>) -> i64 {
        let repo = TrackRepo::with_backend(backend.clone());
        let mut t = Track::new(titre.into());
        t.file_path = Some(format!("/music/{titre}.flac"));
        t.musicbrainz_recording_id = mbid.map(|s| s.to_string());
        repo.create(&t).unwrap()
    }

    fn lire(backend: &Arc<dyn DbBackend>, id: i64) -> (String, Option<String>) {
        let rows = backend
            .query_many(
                "SELECT title, musicbrainz_recording_id FROM tracks WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap();
        let r = rows.first().expect("la piste doit exister");
        (
            r.first().and_then(|v| v.as_string()).unwrap_or_default(),
            r.get(1)
                .and_then(|v| v.as_string())
                .filter(|s| !s.is_empty()),
        )
    }

    fn reconnaissance(score: f64) -> AcoustIdMatch {
        AcoustIdMatch {
            recording_id: "6f2f9b9e-1111-2222-3333-444455556666".into(),
            title: "So What".into(),
            artist: "Miles Davis".into(),
            score,
        }
    }

    /// LA contre-epreuve de #2374.
    ///
    /// Une piste correctement titree — l'immense majorite d'une bibliotheque —
    /// doit garder son titre ET recevoir son identifiant. L'ancien code
    /// ecrivait les deux dans la meme requete, sous la garde du titre : cet
    /// identifiant etait donc jete.
    #[test]
    fn une_piste_bien_titree_recoit_quand_meme_son_identifiant() {
        let backend = base();
        let id = piste(&backend, "So What (Take 2)", None);

        enregistrer_identification_acoustique(&backend, id, &reconnaissance(0.93));

        let (titre, mbid) = lire(&backend, id);
        assert_eq!(
            titre, "So What (Take 2)",
            "le titre choisi par l'utilisateur ne doit pas etre remplace"
        );
        assert_eq!(
            mbid.as_deref(),
            Some("6f2f9b9e-1111-2222-3333-444455556666"),
            "l'identifiant d'enregistrement etait jete des que le titre etait correct (#2374)"
        );
    }

    /// Un identifiant venu des tags du fichier fait autorite : on remplit, on
    /// n'ecrase pas.
    #[test]
    fn un_identifiant_deja_present_n_est_pas_ecrase() {
        let backend = base();
        let id = piste(&backend, "So What", Some("celui-des-tags"));

        enregistrer_identification_acoustique(&backend, id, &reconnaissance(0.99));

        assert_eq!(lire(&backend, id).1.as_deref(), Some("celui-des-tags"));
    }

    /// Le titre sans titre, lui, est bien corrige — l'acquis d'avant.
    #[test]
    fn un_titre_qui_n_en_est_pas_un_est_corrige() {
        let backend = base();
        let id = piste(&backend, "Track 03", None);

        enregistrer_identification_acoustique(&backend, id, &reconnaissance(0.91));

        let (titre, mbid) = lire(&backend, id);
        assert_eq!(titre, "So What");
        assert!(mbid.is_some());
    }

    /// En dessous du seuil, rien ne bouge : ni titre, ni identifiant.
    #[test]
    fn une_reconnaissance_douteuse_n_ecrit_rien() {
        let backend = base();
        let id = piste(&backend, "Track 03", None);

        enregistrer_identification_acoustique(&backend, id, &reconnaissance(0.42));

        let (titre, mbid) = lire(&backend, id);
        assert_eq!(titre, "Track 03");
        assert_eq!(mbid, None);
    }
}

#[cfg(test)]
mod filtre_actif_tests {
    use super::super::query_multi::track_filter_from_raw;
    use tune_core::db::facet_filter::TrackFilter;

    /// La requête telle qu'elle arrive sur le fil — le chemin de `list_tracks`.
    fn depuis(raw: &str) -> TrackFilter {
        match track_filter_from_raw(Some(raw)) {
            Ok(f) => f,
            Err(_) => panic!("requête acceptable : {raw}"),
        }
    }

    /// Le garde-fou de la régression : `original_year` seul DOIT compter comme
    /// un filtre. Il ne comptait pas — il avait atterri après un `;`, dans une
    /// fermeture `|| …` que le compilateur signalait (« unused closure ») sans
    /// faire échouer la compilation. Neuf checks de CI verts ne l'ont pas vu.
    #[test]
    fn annee_denregistrement_seule_est_un_filtre() {
        assert!(
            depuis("original_year=1969").is_active(),
            "filtrer sur l'année d'enregistrement partait sur le chemin NON filtré"
        );
    }

    /// Une requête nue ne filtre rien — sinon le chemin rapide (liste complète
    /// paginée) ne serait jamais emprunté.
    #[test]
    fn une_requete_nue_ne_filtre_rien() {
        assert!(!depuis("limit=50&offset=0").is_active());
        assert!(!TrackFilter::default().is_active());
    }

    /// Une chaîne vide n'est pas un filtre : `?favorite=` arrive ainsi depuis
    /// le client quand la facette est désélectionnée.
    #[test]
    fn une_chaine_vide_nest_pas_un_filtre() {
        let q = depuis("favorite=&playlist=&untagged=&collection=&folder=&format=&genre=");
        assert!(!q.is_active(), "une facette vide ne doit pas filtrer");
    }

    /// ⚠️ Défaut RÉEL corrigé au passage. Avant #2168, une valeur hors du
    /// vocabulaire fermé comptait comme un filtre (`Option::is_some`) mais ne
    /// produisait AUCUNE condition SQL (`_ => {}`) : la route empruntait le
    /// chemin filtré, n'y filtrait rien, et rendait la bibliothèque ENTIÈRE
    /// avec un total qui la confirmait. `is_active` teste désormais le
    /// vocabulaire, pas la présence.
    #[test]
    fn une_valeur_hors_vocabulaire_ne_rend_plus_toute_la_bibliotheque() {
        assert!(!depuis("favorite=1").is_active());
        assert!(!depuis("untagged=mbid").is_active());
        assert!(depuis("favorite=album").is_active());
        assert!(depuis("untagged=cover").is_active());
    }

    /// Chaque facette, prise SEULE, doit compter. Ce test est la raison d'être
    /// de l'extraction : il échouera si une facette est ajoutée et oubliée dans
    /// `TrackFilter::is_active` — exactement le défaut corrigé ici.
    #[test]
    fn chaque_facette_compte_comme_un_filtre() {
        let cas = [
            ("genre", "genre=Rock"),
            ("year", "year=1994"),
            ("format", "format=flac"),
            ("sample_rate", "sample_rate=96000"),
            ("bit_depth", "bit_depth=24"),
            ("source", "source=local"),
            ("label", "label=ECM"),
            ("composer", "composer=Bach"),
            ("artist", "artist=Miles+Davis"),
            ("country", "country=FR"),
            ("mood", "mood=calme"),
            ("source_media", "source_media=CD"),
            ("original_year", "original_year=1969"),
            ("dr", "dr=14"),
            ("rating", "rating=4"),
            ("favorite", "favorite=track"),
            ("playlist", "playlist=Ma+liste"),
            ("untagged", "untagged=genre"),
            ("folder", "folder=%2Fmnt%2Fmusic"),
            ("q", "q=so+what"),
        ];
        for (nom, raw) in cas {
            assert!(
                depuis(raw).is_active(),
                "la facette « {nom} » ne compte pas comme un filtre"
            );
        }
        // `collection` ne passe pas par la chaîne brute : la route la résout en
        // identifiants avant d'appeler `list_filtered`.
        let sel = TrackFilter {
            collection_ids: Some(vec![12]),
            ..Default::default()
        };
        assert!(sel.is_active(), "la facette « collection » ne compte pas");
    }

    /// Le cas de Cyrille (fil 1513) : deux formats et deux fréquences cochés.
    #[test]
    fn plusieurs_valeurs_dans_une_meme_facette() {
        let q = depuis("format=aiff&format=flac&sample_rate=44100&sample_rate=352800");
        assert_eq!(q.formats, vec!["aiff".to_string(), "flac".to_string()]);
        assert_eq!(q.sample_rates, vec![44100, 352800]);
        assert!(q.is_active());
    }

    /// Rétrocompatibilité : une URL enregistrée avant #2168 (une valeur par
    /// facette) donne exactement le même filtre.
    #[test]
    fn une_url_ancienne_reste_lue_a_lidentique() {
        let q = depuis("genre=Jazz&format=flac&year=1971&limit=3000");
        assert_eq!(q.genres, vec!["Jazz".to_string()]);
        assert_eq!(q.formats, vec!["flac".to_string()]);
        assert_eq!(q.years, vec![1971]);
    }
}

// ── « Autres versions de ce titre » — #2372 ───────────────────────────────

#[derive(Deserialize)]
pub(super) struct VersionsParams {
    /// Plafond des versions LOCALES rendues. Le streaming a son propre
    /// budget, borne par service.
    limit: Option<i64>,
    /// Interroger aussi les services de streaming. Vrai par defaut : c'est le
    /// coeur de la demande de FabienM (« pour les curieux, proposer les
    /// versions trouvees dans les services streaming »). Le client peut le
    /// couper pour un premier rendu immediat.
    ///
    /// ⚠️ CONSERVE, et deliberement. `sources` ci-dessous le recouvre presque
    /// entierement — `streaming=false` dit la meme chose que `sources=local` —
    /// mais presque n'est pas assez : ce parametre est publie et
    /// `tune-web-client` l'envoie a CHAQUE appel (`getTrackVersions`,
    /// `src/lib/api.ts`, toujours `?streaming=true`). Un parametre qu'on
    /// casse est une regression silencieuse pour qui l'appelle deja.
    streaming: Option<bool>,
    /// D'ou viennent les versions rendues — le meme `sources` que
    /// `GET /search` (#3226), `GET /home/other-versions` et
    /// `GET /home/artist-releases`. Absent = « Tous », soit exactement le
    /// comportement d'avant. Contrat complet dans
    /// [`crate::routes::filtre_sources`].
    ///
    /// ## Comment il cohabite avec `streaming`
    ///
    /// `streaming=false` est un VETO sur la moitie streaming, jamais une
    /// autorisation. Il ne peut que RETIRER des services, et il ne ressuscite
    /// jamais le local. Les deux parametres commutent donc, et aucun des
    /// appels d'aujourd'hui ne change de reponse :
    ///
    /// | appel                           | `versions` (local) | `streaming` |
    /// |---------------------------------|--------------------|-------------|
    /// | (rien)                          | rendues            | rendues     |
    /// | `streaming=false`               | rendues            | **vide**    |
    /// | `streaming=true`                | rendues            | rendues     |
    /// | `sources=local`                 | rendues            | **vide**    |
    /// | `sources=qobuz`                 | **vide**           | Qobuz seul  |
    /// | `sources=qobuz&streaming=false` | **vide**           | **vide**    |
    ///
    /// Les trois premieres lignes sont mot pour mot ce que la route rendait
    /// avant : `streaming` seul ne consulte jamais `sources`, et `sources`
    /// absent vaut [`FiltreSources::tout`].
    sources: Option<String>,
}

/// Rassemble les autres versions d'une piste. Rend `None` si la piste
/// n'existe pas — le handler en fait un 404.
///
/// Sorti du handler pour etre testable sans monter un routeur : les tests
/// posent une bibliotheque en memoire et appellent directement.
pub(super) async fn rassembler_versions(
    state: &AppState,
    id: i64,
    limite: i64,
    avec_streaming: bool,
    filtre: &FiltreSources,
) -> Option<Value> {
    // Le morceau de reference : son titre, son artiste de piste (celui affiche
    // a l'auditeur) et l'album lui-meme. L'artiste d'album n'est qu'un repli :
    // sur une compilation « Artistes divers », le premier ferait precisement
    // perdre les versions de l'interprete reel (#2638).
    //
    // L'ISRC, la duree et l'annee viennent avec : ce sont les signaux qui
    // CLASSENT les candidats (`routes::versions::score_version`). La section
    // d'accueil ne les a pas — son vivier est `listen_history` —, cette
    // route-ci les a, et c'est tout l'interet de partir d'une vraie ligne de
    // `tracks`.
    let e = state.backend.engine();
    let sql = format!(
        "SELECT t.title, COALESCE(ar2.name, ar.name, ''), COALESCE(al.title, ''), \
                t.isrc, t.duration_ms, al.year \
         FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON al.artist_id = ar.id \
         LEFT JOIN artists ar2 ON t.artist_id = ar2.id \
         WHERE t.id = {}",
        crate::routes::versions::marqueur(e, 1)
    );
    let cols = state
        .backend
        .query_one(&sql, &[&id as &dyn ToSqlValue])
        .ok()
        .flatten()?;
    let reference = crate::routes::versions::Reference {
        titre: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
        artiste: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        album: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        isrc: cols.get(3).and_then(|v| v.as_string()),
        duree_ms: cols.get(4).and_then(|v| v.as_i64()),
        annee: cols.get(5).and_then(|v| v.as_i64()),
    };

    // ⚠️ La ligne `tracks` ci-dessus est lue QUOI QU'IL ARRIVE, meme quand le
    // local n'est pas demande : c'est l'ENTREE de la question — le titre,
    // l'artiste, l'ISRC, la duree —, pas une reponse. Sous `sources=qobuz`,
    // cette route doit chercher les versions Qobuz DE CETTE PISTE, pas
    // repondre 404 parce qu'on n'a pas demande le local. Vivier n'est pas
    // contenu.
    let locales = if filtre.local_demande() {
        crate::routes::versions::versions_locales(state, &reference, Some(id), limite)
    } else {
        // Pas « calculer puis jeter » : la requete ne part pas.
        Vec::new()
    };
    let streaming = if avec_streaming {
        crate::routes::versions::versions_streaming(state, &reference, filtre).await
    } else {
        Vec::new()
    };

    // La MEME forme qu'un groupe de `GET /home/other-versions` : l'ecran qui
    // dessine deja la section d'accueil rend celui-ci sans une ligne de plus.
    Some(json!({
        "track_id": id,
        "title": reference.titre,
        "artist_name": reference.artiste,
        "played_album": reference.album,
        "versions": locales,
        "streaming": streaming,
    }))
}

/// `GET /library/tracks/{id}/versions` — les autres versions de CE titre,
/// bibliotheque ET services de streaming.
///
/// La section d'accueil `GET /home/other-versions` sait deja rapprocher les
/// versions, mais son vivier est l'historique d'ecoute : un morceau jamais
/// ecoute recemment n'y apparait jamais. FabienM l'a dit mot pour mot (fil
/// 1538, 24/08) : « elles se resument aux simples dernieres ecoutes ». Cette
/// route prend UNE piste en entree — celle designee dans le menu « … » —, et
/// reutilise le meme rapprochement (`routes::versions`).
///
/// 404 quand la piste n'existe pas ; un groupe aux deux listes vides quand
/// elle existe sans autre version : « aucune autre version connue » est une
/// reponse, pas une erreur.
pub(super) async fn track_versions(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(p): Query<VersionsParams>,
) -> impl IntoResponse {
    let limite = p.limit.unwrap_or(50).clamp(1, 200);
    let avec_streaming = p.streaming.unwrap_or(true);
    let filtre = FiltreSources::depuis(p.sources.as_deref());
    match rassembler_versions(&state, id, limite, avec_streaming, &filtre).await {
        Some(v) => Json(v).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests_versions_piste {
    use super::rassembler_versions;
    // `FiltreSources::tout()` = `sources` absent = « Tous ». Ces essais
    // portaient sur le rapprochement, pas sur la provenance : ils disent
    // desormais explicitement qu'ils ne filtrent rien, et gardent donc le
    // meme sens qu'avant l'ajout du parametre.
    use crate::routes::filtre_sources::FiltreSources;
    use crate::state::AppState;
    use tune_core::db::backend::ToSqlValue;

    /// Pose « Billie Jean » sur Thriller ET sur Number Ones, plus un morceau
    /// sans rapport. Rend l'id de la piste de Thriller.
    fn bibliotheque_de_test(state: &AppState) -> i64 {
        let b = &state.backend;
        b.execute("INSERT INTO artists (name) VALUES ('Michael Jackson')", &[])
            .unwrap();
        let mj = b.last_insert_rowid();
        b.execute("INSERT INTO artists (name) VALUES ('Chris Cornell')", &[])
            .unwrap();
        let cc = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Thriller', ?1)",
            &[&mj as &dyn ToSqlValue],
        )
        .unwrap();
        let thriller = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Number Ones', ?1)",
            &[&mj as &dyn ToSqlValue],
        )
        .unwrap();
        let number_ones = b.last_insert_rowid();
        b.execute(
            "INSERT INTO albums (title, artist_id) VALUES ('Euphoria Morning', ?1)",
            &[&cc as &dyn ToSqlValue],
        )
        .unwrap();
        let euphoria = b.last_insert_rowid();

        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('Billie Jean', ?1, ?2, 294000, '/a.flac')",
            &[&thriller as &dyn ToSqlValue, &mj as &dyn ToSqlValue],
        )
        .unwrap();
        let seed = b.last_insert_rowid();
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('billie jean', ?1, ?2, 289000, '/b.flac')",
            &[&number_ones as &dyn ToSqlValue, &mj as &dyn ToSqlValue],
        )
        .unwrap();
        // Une REPRISE : même titre, autre artiste. Le rapprochement LOCAL est
        // volontairement strict sur l'artiste — elle ne doit pas sortir.
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('Billie Jean', ?1, ?2, 301000, '/c.flac')",
            &[&euphoria as &dyn ToSqlValue, &cc as &dyn ToSqlValue],
        )
        .unwrap();
        // Un morceau sans rapport, sur le MÊME album que la graine.
        b.execute(
            "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
             VALUES ('Beat It', ?1, ?2, 258000, '/d.flac')",
            &[&thriller as &dyn ToSqlValue, &mj as &dyn ToSqlValue],
        )
        .unwrap();
        seed
    }

    /// Le cœur de #2372 : depuis UNE piste, l'autre version portée par un
    /// autre album ressort. Sans historique d'écoute — c'est tout l'objet :
    /// `GET /home/other-versions` n'aurait rien rendu ici.
    #[tokio::test]
    async fn une_piste_donne_ses_autres_versions_sans_historique() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let seed = bibliotheque_de_test(&state);

        let v = rassembler_versions(&state, seed, 50, false, &FiltreSources::tout())
            .await
            .expect("la piste existe");

        assert_eq!(v["title"].as_str(), Some("Billie Jean"));
        assert_eq!(v["artist_name"].as_str(), Some("Michael Jackson"));
        assert_eq!(v["played_album"].as_str(), Some("Thriller"));
        let versions = v["versions"].as_array().expect("un tableau de versions");
        assert_eq!(
            versions.len(),
            1,
            "une seule autre version attendue, obtenu {versions:?}"
        );
        assert_eq!(versions[0]["album_title"].as_str(), Some("Number Ones"));
        assert_eq!(versions[0]["duration_ms"].as_i64(), Some(289_000));
    }

    /// La piste de départ ne se propose pas elle-même, et son propre album
    /// n'entre pas dans la liste.
    #[tokio::test]
    async fn la_piste_de_depart_et_son_album_sont_ecartes() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let seed = bibliotheque_de_test(&state);

        let v = rassembler_versions(&state, seed, 50, false, &FiltreSources::tout())
            .await
            .unwrap();
        let versions = v["versions"].as_array().unwrap();
        for ver in versions {
            assert_ne!(
                ver["track_id"].as_i64(),
                Some(seed),
                "la graine se propose elle-même"
            );
            assert_ne!(
                ver["album_title"].as_str(),
                Some("Thriller"),
                "l'album de départ ressort : {ver:?}"
            );
        }
    }

    /// Contre-epreuve de #2638, avec les libelles vus chez FabienM. La graine
    /// vit sur une compilation attribuee a « Artistes divers », mais porte
    /// bien Kate Bush comme artiste de piste. Les suffixes d'edition ne
    /// doivent plus vider la liste locale, et la reprise d'un autre artiste
    /// reste exclue du rapprochement local.
    #[tokio::test]
    async fn running_up_that_hill_retrouve_ses_trois_versions_locales() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;

        b.execute("INSERT INTO artists (name) VALUES ('Kate Bush')", &[])
            .unwrap();
        let kate = b.last_insert_rowid();
        b.execute("INSERT INTO artists (name) VALUES ('Artistes divers')", &[])
            .unwrap();
        let divers = b.last_insert_rowid();
        b.execute(
            "INSERT INTO artists (name) VALUES ('Thomas Mery & The desert fox')",
            &[],
        )
        .unwrap();
        let thomas = b.last_insert_rowid();

        let album = |titre: &str, artiste: i64| {
            b.execute(
                "INSERT INTO albums (title, artist_id) VALUES (?1, ?2)",
                &[&titre as &dyn ToSqlValue, &artiste as &dyn ToSqlValue],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let hit = album("Hit Collection", divers);
        let before = album("Before The Dawn", kate);
        let hounds = album("Hounds Of Love", kate);
        let reprise = album("Label Effervescence Pain Perdu", thomas);

        let piste = |titre: &str, album_id: i64, artiste: i64, chemin: &str| {
            b.execute(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, file_path) \
                 VALUES (?1, ?2, ?3, 296000, ?4)",
                &[
                    &titre as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                    &artiste as &dyn ToSqlValue,
                    &chemin as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let seed = piste("Running Up that Hill", hit, kate, "/hit.flac");
        piste(
            "Running Up That Hill (A Deal With God)",
            before,
            kate,
            "/before.flac",
        );
        piste(
            "Running Up That Hill (A Deal With God)",
            hounds,
            kate,
            "/hounds.flac",
        );
        piste(
            "Running Up That Hill (12' Mix) [Bonus Track]",
            hounds,
            kate,
            "/mix.flac",
        );
        piste("Running up that hill", reprise, thomas, "/reprise.flac");

        let v = rassembler_versions(&state, seed, 50, false, &FiltreSources::tout())
            .await
            .expect("la piste existe");
        assert_eq!(v["artist_name"].as_str(), Some("Kate Bush"));
        let versions = v["versions"].as_array().expect("versions locales");
        assert_eq!(versions.len(), 3, "versions rendues : {versions:?}");
        assert!(versions.iter().all(|x| {
            x["album_title"].as_str() == Some("Before The Dawn")
                || x["album_title"].as_str() == Some("Hounds Of Love")
        }));
        assert!(
            versions
                .iter()
                .all(|x| { x["album_title"].as_str() != Some("Label Effervescence Pain Perdu") })
        );
    }

    /// « Beat It » n'a aucune autre version : un groupe VIDE, pas une erreur.
    /// Le client en tire « aucune autre version connue ».
    #[tokio::test]
    async fn un_morceau_sans_autre_version_rend_un_groupe_vide() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        bibliotheque_de_test(&state);
        let id: i64 = state
            .backend
            .query_one("SELECT id FROM tracks WHERE title = 'Beat It'", &[])
            .unwrap()
            .and_then(|c| c.first().and_then(|v| v.as_i64()))
            .unwrap();

        let v = rassembler_versions(&state, id, 50, false, &FiltreSources::tout())
            .await
            .unwrap();
        assert_eq!(v["versions"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["streaming"].as_array().map(Vec::len), Some(0));
    }

    /// Une piste inconnue rend `None` — le handler en fait un 404, pas un
    /// groupe vide qui ferait croire à un morceau sans version.
    #[tokio::test]
    async fn une_piste_inconnue_n_est_pas_un_groupe_vide() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        assert!(
            rassembler_versions(&state, 999_999, 50, false, &FiltreSources::tout())
                .await
                .is_none()
        );
    }

    /// La bibliothèque de Gros Bidon (#2372, fil 1627) : le titre de base, son
    /// remaster nommé AVEC UN TIRET comme le nomment Qobuz, Tidal et Deezer,
    /// et — le piège — un homonyme d'un autre artiste, remasterisé lui aussi.
    ///
    /// Rend l'id de la piste de base.
    fn bibliotheque_remasters(state: &AppState) -> i64 {
        let b = &state.backend;
        let artiste = |nom: &str| {
            b.execute(
                "INSERT INTO artists (name) VALUES (?1)",
                &[&nom as &dyn ToSqlValue],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let album = |titre: &str, artiste_id: i64, annee: i64| {
            b.execute(
                "INSERT INTO albums (title, artist_id, year) VALUES (?1, ?2, ?3)",
                &[
                    &titre as &dyn ToSqlValue,
                    &artiste_id as &dyn ToSqlValue,
                    &annee as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let piste = |titre: &str, album_id: i64, artiste_id: i64, duree: i64, isrc: &str| {
            b.execute(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, isrc, file_path) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                &[
                    &titre as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                    &artiste_id as &dyn ToSqlValue,
                    &duree as &dyn ToSqlValue,
                    &isrc as &dyn ToSqlValue,
                    &format!("/{titre}-{album_id}.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.last_insert_rowid()
        };

        let sade = artiste("Sade");
        let autre = artiste("The Smooth Operators");
        let diamond = album("Diamond Life", sade, 1984);
        let ultimate = album("The Ultimate Collection", sade, 2011);
        let ailleurs = album("Ailleurs", autre, 2011);

        let base = piste("Smooth Operator", diamond, sade, 291_000, "GBAAA8400001");
        piste(
            "Smooth Operator - 2011 Remastered",
            ultimate,
            sade,
            291_000,
            "GBAAA8400001",
        );
        // L'HOMONYME : même titre, même suffixe, autre artiste. Le
        // rapprochement local nomme l'artiste depuis #2497 — il ne doit pas
        // sortir.
        piste(
            "Smooth Operator - 2011 Remastered",
            ailleurs,
            autre,
            180_000,
            "FRXXX1100001",
        );
        base
    }

    /// ⭐ ÉPREUVE 1 — le cas de Gros Bidon : un titre remasterisé, avec tiret,
    /// est reconnu comme une version du titre de base.
    ///
    /// Avant ce lot, `titre_est_base_de` n'acceptait que ` (` et ` [` : cette
    /// liste était VIDE, et « Autres versions » ratait la convention de
    /// nommage la plus répandue des remasters.
    #[tokio::test]
    async fn un_remaster_avec_tiret_est_une_autre_version() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let base = bibliotheque_remasters(&state);

        let v = rassembler_versions(&state, base, 50, false, &FiltreSources::tout())
            .await
            .expect("la piste existe");
        let versions = v["versions"].as_array().expect("versions locales");
        assert_eq!(
            versions.len(),
            1,
            "une seule autre version attendue, obtenu {versions:?}"
        );
        assert_eq!(
            versions[0]["title"].as_str(),
            Some("Smooth Operator - 2011 Remastered"),
            "le titre RETROUVÉ doit être rendu tel quel : {versions:?}"
        );
        assert_eq!(
            versions[0]["album_title"].as_str(),
            Some("The Ultimate Collection")
        );
    }

    /// ⭐ ÉPREUVE 2 — l'artiste compte : deux morceaux de même titre par des
    /// artistes différents ne sont pas mélangés.
    ///
    /// C'est la contre-épreuve du SABOTAGE : retirer la condition d'artiste de
    /// `predicat_rapprochement` fait tomber CE test, et lui seul dit pourquoi.
    ///
    /// La définition retenue est écrite en toutes lettres : dans la
    /// BIBLIOTHÈQUE, « autre version » veut dire **même titre, MÊME artiste,
    /// autre album**. La reprise par un autre interprète n'entre pas ici — elle
    /// n'entre que par le STREAMING, où elle est étiquetée `reprise` et où le
    /// libellé permet à l'auditeur de la distinguer.
    #[tokio::test]
    async fn l_homonyme_d_un_autre_artiste_n_entre_pas_dans_les_versions_locales() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let base = bibliotheque_remasters(&state);

        let v = rassembler_versions(&state, base, 50, false, &FiltreSources::tout())
            .await
            .expect("la piste existe");
        let versions = v["versions"].as_array().expect("versions locales");
        for version in versions {
            assert_ne!(
                version["album_title"].as_str(),
                Some("Ailleurs"),
                "un homonyme d'un AUTRE artiste est sorti : {versions:?}"
            );
        }
        assert_eq!(
            versions.len(),
            1,
            "l'artiste n'est plus dans le rapprochement : {versions:?}"
        );
    }

    /// Le score classe ce que l'arbre binaire laissait dans l'ordre du moteur.
    ///
    /// La version qui partage l'ISRC de la référence passe devant celle qui ne
    /// le partage pas, même quand les deux sont également « même artiste,
    /// autre album ». Avant ce lot les deux étaient indiscernables.
    #[tokio::test]
    async fn la_version_de_meme_isrc_passe_devant() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let b = &state.backend;
        b.execute("INSERT INTO artists (name) VALUES ('Sade')", &[])
            .unwrap();
        let sade = b.last_insert_rowid();
        let album = |titre: &str| {
            b.execute(
                "INSERT INTO albums (title, artist_id) VALUES (?1, ?2)",
                &[&titre as &dyn ToSqlValue, &sade as &dyn ToSqlValue],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        // « Abbey » AVANT « Zenith » dans tous les ordres alphabétiques : si
        // le score ne comptait pas, c'est « Abbey » qui sortirait en tête.
        let abbey = album("Abbey");
        let zenith = album("Zenith");
        let diamond = album("Diamond Life");
        let piste = |titre: &str, album_id: i64, isrc: &str, chemin: &str| {
            b.execute(
                "INSERT INTO tracks (title, album_id, artist_id, duration_ms, isrc, file_path) \
                 VALUES (?1, ?2, ?3, 291000, ?4, ?5)",
                &[
                    &titre as &dyn ToSqlValue,
                    &album_id as &dyn ToSqlValue,
                    &sade as &dyn ToSqlValue,
                    &isrc as &dyn ToSqlValue,
                    &chemin as &dyn ToSqlValue,
                ],
            )
            .unwrap();
            b.last_insert_rowid()
        };
        let base = piste("Smooth Operator", diamond, "GBAAA8400001", "/base.flac");
        piste("Smooth Operator", abbey, "USXXX9900001", "/abbey.flac");
        piste("Smooth Operator", zenith, "GBAAA8400001", "/zenith.flac");

        let v = rassembler_versions(&state, base, 50, false, &FiltreSources::tout())
            .await
            .expect("la piste existe");
        let versions = v["versions"].as_array().expect("versions locales");
        assert_eq!(versions.len(), 2, "versions rendues : {versions:?}");
        assert_eq!(
            versions[0]["album_title"].as_str(),
            Some("Zenith"),
            "l'ISRC identique doit passer devant l'ordre alphabétique : {versions:?}"
        );
        assert!(
            versions[0]["score"].as_i64() > versions[1]["score"].as_i64(),
            "le score doit être décroissant : {versions:?}"
        );
    }
}

/// Inscription de la relecture des métadonnées au registre des tâches de fond
/// (#2129).
///
/// **Hermétique : aucun accès disque, aucun appel réseau.** La base en mémoire
/// ne contient aucune piste locale, donc la passe ne lit aucun fichier.
#[cfg(test)]
mod tests_tache_de_fond_rescan_metadata {
    use super::*;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// `rescan_metadata_status` vaut « running » pendant toute la passe, mais
    /// ce réglage n'est lu que par l'écran qui l'a lancée. Relire les
    /// étiquettes de chaque fichier d'une bibliothèque de dizaines de milliers
    /// de pistes prend un temps long, et rien ne le signalait ailleurs.
    #[tokio::test]
    async fn la_relecture_des_metadonnees_s_inscrit_au_registre() {
        let state = etat();
        let _ = rescan_metadata(State(state.clone())).await;

        let ids: Vec<String> = state
            .background_tasks
            .snapshot()
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert!(
            ids.contains(&TACHE_RESCAN_METADATA.to_string()),
            "la relecture des métadonnées doit figurer au registre des tâches \
             de fond, sinon le bandeau global ne peut pas l'afficher (#2129) — \
             registre observé : {ids:?}"
        );
    }

    /// Témoin anti-régression : la route garde son 202 et son message.
    #[tokio::test]
    async fn le_contrat_de_la_route_est_inchange() {
        let state = etat();
        let reponse = rescan_metadata(State(state.clone())).await.into_response();
        assert_eq!(reponse.status(), StatusCode::ACCEPTED);

        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .unwrap();
        let corps: Value = serde_json::from_slice(&octets).unwrap();
        assert_eq!(corps["status"], "rescan_metadata_started");
    }
}
