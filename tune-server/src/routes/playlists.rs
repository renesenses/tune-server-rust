use std::collections::HashSet;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;

use crate::state::AppState;

#[derive(Deserialize)]
struct Pagination {
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct CreatePlaylist {
    name: String,
    description: Option<String>,
}

#[derive(Deserialize)]
struct UpdatePlaylist {
    name: Option<String>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct AddTracks {
    track_ids: Vec<i64>,
    position: Option<i64>,
    /// Pistes de service (Qobuz, Tidal…) que le client joint à la demande.
    ///
    /// `AddToPlaylistModal.buildAddArgs()` (tune-web-client) le remplit dès que
    /// la piste n'a pas d'`id` local, et laisse alors `track_ids` VIDE. Le
    /// champ n'etait pas declare ici : serde ecarte en silence tout champ
    /// inconnu, `add_tracks` n'ajoutait donc rien et repondait quand meme
    /// `201 Created` (#1848).
    ///
    /// Le contenu n'est pas typé : la route ne peut de toute façon pas le
    /// stocker (voir `add_tracks`), elle n'a besoin que de savoir COMBIEN il y
    /// en a pour pouvoir le dire.
    streaming_tracks: Option<Vec<Value>>,
}

#[derive(Deserialize)]
struct RemoveTrack {
    position: i64,
}

#[derive(Deserialize)]
struct RemoveTracksBody {
    positions: Vec<i64>,
}

#[derive(Deserialize)]
struct ReorderTracksBody {
    track_ids: Vec<i64>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_playlists).post(create_playlist))
        .route("/all", get(list_all_playlists))
        .route("/shared/{token}", get(get_shared_playlist))
        .route("/transfer", post(transfer_playlist))
        .route("/diff", post(diff_playlists))
        .route("/import/m3u", post(import_m3u_file))
        .route("/import/m3u-url", post(import_m3u_url))
        .route("/import/linn", post(import_linn_file))
        .route(
            "/{id}",
            get(get_playlist)
                .put(update_playlist)
                .delete(delete_playlist),
        )
        .route(
            "/{id}/tracks",
            get(get_tracks)
                .post(add_tracks)
                .delete(remove_tracks_batch)
                .put(reorder_tracks),
        )
        .route("/{id}/tracks/remove", post(remove_track))
        .route("/{id}/duplicate", post(duplicate_playlist))
        .route("/{id}/export", get(export_m3u))
        .route("/{id}/share", post(share_playlist))
        .route("/{id}/recover", post(recover_playlist))
        .route("/{id}/recover/apply", post(apply_recovery))
        .route(
            "/collaborative",
            get(list_collaborative).post(create_collaborative),
        )
        .route("/collaborative/{id}", get(get_collaborative))
        .route("/collaborative/{id}/add", post(add_to_collaborative))
        .route("/collaborative/{id}/tracks", get(collaborative_tracks))
        .route("/match", post(match_tracks))
}

async fn list_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let items = repo.list(profile.id(), limit, offset).unwrap_or_default();
    let _total = repo.count(profile.id()).unwrap_or(0);
    Json(json!(items))
}

async fn get_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(pl)) => Json(json!(pl)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn create_playlist(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<CreatePlaylist>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.create(&body.name, body.description.as_deref(), profile.id()) {
        Ok(id) => match repo.get(id) {
            Ok(Some(playlist)) => (StatusCode::CREATED, Json(json!(playlist))).into_response(),
            Ok(None) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "playlist created but not found",
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_playlist(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdatePlaylist>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.update(id, body.name.as_deref(), body.description.as_deref()) {
        Ok(_) => match repo.get(id) {
            Ok(Some(playlist)) => Json(json!(playlist)).into_response(),
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Une erreur de base ne doit PAS se déguiser en playlist vide (#2797) : le
/// client ne peut alors pas distinguer « la playlist est vide » de « la
/// requête a échoué », et l'utilisateur voit une playlist se vider toute
/// seule. On remonte un 500 explicite.
async fn get_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_ids = repo
        .get_track_ids(id)
        .map_err(|e| AppError::internal(e.to_string()))?;
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&track_ids)
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(json!(tracks)))
}

async fn add_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddTracks>,
) -> impl IntoResponse {
    // #1848 — Dominique Comet : « lorsqu'on sélectionne une piste nous n'avons
    // pas les mêmes possibilités sur la bibliothèque et sur Qobuz ».
    //
    // Le client OFFRE « ajouter à une playlist » sur une piste de service
    // (`StreamingView.svelte`, trois listes : album, playlist, recherche) et
    // poste alors `streaming_tracks` avec `track_ids` vide. Ce champ n'étant
    // pas déclaré, serde l'écartait : `add_tracks_deduped(id, &[], …)`
    // n'ajoutait rien, la route répondait `201 Created` avec la playlist, et le
    // modal lisait ce 201 comme un succès — il affichait « ajoutée » devant une
    // playlist restée vide.
    //
    // Ce n'est PAS réparable en stockant la piste : `playlist_tracks.track_id`
    // est `NOT NULL REFERENCES tracks(id)` dans les trois définitions de schéma
    // (sqlite.rs, migrations/postgres, pg_migrate.rs). Une playlist locale ne
    // PEUT pas porter une piste de service. Le refus est donc légitime — c'est
    // de le déguiser en succès qui ne l'était pas. Même doctrine que #1959 sur
    // `save_queue_as_playlist` : 422 avec la raison, et le compte des ignorées
    // sur le cas mixte.
    let distantes = body.streaming_tracks.as_ref().map_or(0, Vec::len);

    if distantes > 0 && body.track_ids.is_empty() {
        tracing::warn!(
            playlist_id = id,
            distantes,
            "playlist_add_tracks_refused_streaming_only"
        );
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "Cette demande ne porte que des pistes de service ({distantes}), \
                 qui ne peuvent pas entrer dans une playlist locale. \
                 Ajoutez-les à une playlist du service, ou ajoutez d'abord ces \
                 titres à votre bibliothèque."
            ),
        )
            .into_response();
    }

    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.add_tracks_deduped(id, &body.track_ids, body.position) {
        Ok(_) => match repo.get(id) {
            Ok(Some(playlist)) => {
                let mut corps = json!(playlist);
                if distantes > 0 {
                    // Une demande mixte perd ses pistes de service en chemin.
                    // Le taire produirait le défaut d'à côté : une playlist
                    // plus courte que la demande, sans que rien ne dise
                    // pourquoi.
                    tracing::info!(
                        playlist_id = id,
                        ajoutees = body.track_ids.len(),
                        ignorees = distantes,
                        "playlist_add_tracks_skipped_streaming"
                    );
                    if let Some(obj) = corps.as_object_mut() {
                        obj.insert("skipped_streaming".into(), json!(distantes));
                    }
                }
                (StatusCode::CREATED, Json(corps)).into_response()
            }
            Ok(None) => StatusCode::NOT_FOUND.into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        },
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_track(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RemoveTrack>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.remove_track(id, body.position) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_tracks_batch(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RemoveTracksBody>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.remove_tracks_at_positions(id, &body.positions) {
        Ok(removed) => Json(json!({"removed": removed})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn reorder_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<ReorderTracksBody>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.reorder_tracks(id, &body.track_ids) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn duplicate_playlist(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let original = match repo.get(id) {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        // A database error is not "no such playlist": answering 404 for a
        // failed read sends the client off looking for a playlist that does
        // exist (#2798).
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    // Reading the source track list used to be `unwrap_or_default()`: a
    // database error produced an EMPTY copy announced as a success — the same
    // shape as #2119. A copy whose source we cannot read is a failure.
    let track_ids = match repo.get_track_ids(id) {
        Ok(ids) => ids,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let new_name = format!("{} (copy)", original.name);
    // Create + fill in ONE transaction. A half-copied playlist has no meaning:
    // either the copy exists complete, or nothing is left behind and the
    // caller is told so (#2798). The old code created the playlist, then threw
    // the track-insert error away with `.ok()` and answered 201 anyway.
    match repo.create_with_tracks(&new_name, None, profile.id(), &track_ids) {
        Ok((new_id, copied)) => (
            StatusCode::CREATED,
            Json(json!({
                "id": new_id,
                "name": new_name,
                "description": Value::Null,
                // Persisted rows, not "tracks we meant to copy".
                "track_count": copied.len(),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(source_playlist = id, error = %e, "playlist_duplicate_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

#[derive(Deserialize)]
struct ExportQuery {
    format: Option<String>,
}

async fn export_m3u(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ExportQuery>,
) -> Result<impl IntoResponse, AppError> {
    let fmt = q.format.as_deref().unwrap_or("m3u");
    if fmt != "m3u" {
        return export_multi_format(State(state.clone()), id, fmt).await;
    }
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = match repo.get(id) {
        Ok(Some(p)) => p,
        _ => return Err(AppError::not_found("playlist not found")),
    };

    // Exporter un M3U vide sur erreur de base produit un fichier qui a l'air
    // valide et détruit la playlist chez qui le réimporte (#2797).
    let track_ids = repo
        .get_track_ids(id)
        .map_err(|e| AppError::internal(e.to_string()))?;
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&track_ids)
        .map_err(|e| AppError::internal(e.to_string()))?;

    let mut m3u = String::from("#EXTM3U\n");
    for t in &tracks {
        let duration_secs = t.duration_ms / 1000;
        let artist = t.artist_name.as_deref().unwrap_or("Unknown");
        m3u.push_str(&format!(
            "#EXTINF:{},{} - {}\n",
            duration_secs, artist, t.title
        ));
        if let Some(ref path) = t.file_path {
            m3u.push_str(path);
            m3u.push('\n');
        }
    }

    let filename = format!("{}.m3u", playlist.name.replace(' ', "_"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "Content-Type",
        axum::http::HeaderValue::from_static("audio/x-mpegurl; charset=utf-8"),
    );
    headers.insert(
        "Content-Disposition",
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|e| AppError::internal(e.to_string()))?,
    );

    Ok((axum::http::StatusCode::OK, headers, m3u))
}

async fn export_multi_format(
    State(state): State<AppState>,
    id: i64,
    format: &str,
) -> Result<(axum::http::StatusCode, axum::http::HeaderMap, String), AppError> {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("playlist not found"))?;
    let track_ids = repo
        .get_track_ids(id)
        .map_err(|e| AppError::internal(e.to_string()))?;
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&track_ids)
        .map_err(|e| AppError::internal(e.to_string()))?;

    let (content, content_type, ext) = match format {
        "json" => {
            let items: Vec<serde_json::Value> = tracks
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "title": t.title, "artist": t.artist_name, "album": t.album_title,
                        "duration_ms": t.duration_ms, "file_path": t.file_path,
                    })
                })
                .collect();
            (
                serde_json::to_string_pretty(
                    &serde_json::json!({"name": playlist.name, "tracks": items}),
                )
                .unwrap_or_default(),
                "application/json",
                "json",
            )
        }
        "csv" => {
            let mut csv = String::from("title,artist,album,duration_ms,file_path\n");
            for t in &tracks {
                csv.push_str(&format!(
                    "\"{}\",\"{}\",\"{}\",{},\"{}\"\n",
                    t.title.replace('"', "\"\""),
                    t.artist_name.as_deref().unwrap_or("").replace('"', "\"\""),
                    t.album_title.as_deref().unwrap_or("").replace('"', "\"\""),
                    t.duration_ms,
                    t.file_path.as_deref().unwrap_or("").replace('"', "\"\""),
                ));
            }
            (csv, "text/csv", "csv")
        }
        "xspf" => {
            let mut xspf = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n",
            );
            xspf.push_str(&format!(
                "  <title>{}</title>\n  <trackList>\n",
                quick_xml::escape::escape(&playlist.name)
            ));
            for t in &tracks {
                xspf.push_str("    <track>\n");
                xspf.push_str(&format!(
                    "      <title>{}</title>\n",
                    quick_xml::escape::escape(&t.title)
                ));
                if let Some(ref a) = t.artist_name {
                    xspf.push_str(&format!(
                        "      <creator>{}</creator>\n",
                        quick_xml::escape::escape(a)
                    ));
                }
                if let Some(ref a) = t.album_title {
                    xspf.push_str(&format!(
                        "      <album>{}</album>\n",
                        quick_xml::escape::escape(a)
                    ));
                }
                xspf.push_str(&format!("      <duration>{}</duration>\n", t.duration_ms));
                if let Some(ref p) = t.file_path {
                    xspf.push_str(&format!(
                        "      <location>{}</location>\n",
                        quick_xml::escape::escape(p)
                    ));
                }
                xspf.push_str("    </track>\n");
            }
            xspf.push_str("  </trackList>\n</playlist>\n");
            (xspf, "application/xspf+xml", "xspf")
        }
        _ => {
            return Err(AppError::bad_request(
                "format must be m3u, json, csv, or xspf",
            ));
        }
    };

    let filename = format!("{}.{ext}", playlist.name.replace(' ', "_"));
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "Content-Type",
        axum::http::HeaderValue::from_str(content_type).unwrap(),
    );
    headers.insert(
        "Content-Disposition",
        axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")).unwrap(),
    );
    Ok((axum::http::StatusCode::OK, headers, content))
}

async fn import_m3u_file(
    State(state): State<AppState>,
    profile: ActiveProfile,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut file_content = String::new();
    let mut playlist_name: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                if playlist_name.is_none() {
                    playlist_name = field.file_name().map(|f| {
                        f.trim_end_matches(".m3u8")
                            .trim_end_matches(".m3u")
                            .to_string()
                    });
                }
                file_content = field.text().await.unwrap_or_default();
            }
            "name" => {
                playlist_name = Some(field.text().await.unwrap_or_default());
            }
            _ => {}
        }
    }

    if file_content.is_empty() {
        // No importable field arrived. Common causes: the multipart body
        // exceeded axum's DefaultBodyLimit (a large .m3u), a field-decode error
        // (the `while let Ok(Some(field))` loop then exits early), or no "file"
        // part was sent. An M3U import that produced nothing otherwise left no
        // server-side trace at all (JP / Dominique, v0.9.28).
        tracing::warn!(
            "m3u_import_file_empty — no importable 'file' field (body too large, or decode error)"
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no file provided"})),
        )
            .into_response();
    }

    let name = playlist_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Imported Playlist".into());

    tracing::info!(name = %name, bytes = file_content.len(), "m3u_import_file_received");

    // Parse M3U and match tracks
    let mut track_ids: Vec<i64> = Vec::new();
    let mut total_entries = 0u32;
    let mut matched = 0u32;
    // Keep only a small SAMPLE of not-found paths for the response; count the
    // rest. A giant playlist (e.g. a 909k-entry radio dump, Dominique) would
    // otherwise return ~80 MB of JSON and freeze the browser.
    let mut not_found_count = 0u32;
    let mut not_found_paths: Vec<String> = Vec::new();
    const MAX_NOT_FOUND_SAMPLE: usize = 100;

    let track_repo = TrackRepo::with_backend(state.backend.clone());

    // Load every local file_path → id ONCE (single query) for O(1) exact-path
    // matching. The old loop ran one get_by_path AND one FTS search() PER line;
    // on a large .m3u whose paths don't match the library layout exactly, every
    // line fell through to the (expensive, unaccent+LIKE+joins) search, so
    // thousands of sequential FTS queries hung the request for minutes and the
    // UI stayed stuck on "loading" until a refresh (Dominique: large M3U
    // freezes). One map lookup replaces the N point queries.
    // NOT `unwrap_or_default()`: an index we failed to read used to become an
    // empty map, so EVERY line fell through to "not found" and the import
    // answered 201 with 0 matched — a success that describes nothing that
    // happened (#2798, same shape as #2119). Refuse before writing anything.
    let path_to_id = match track_repo.get_all_local_file_info() {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(error = %e, "m3u_import_file_index_unavailable");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("library index unavailable: {e}")})),
            )
                .into_response();
        }
    };

    // The FTS fallback (filename→search) stays for paths that don't match
    // exactly, but is BOUNDED: a fully-mismatched huge playlist can't run an
    // unbounded number of costly searches. Beyond the cap, unmatched lines are
    // recorded as not-found without searching (logged once).
    const MAX_SEARCH_FALLBACKS: u32 = 500;
    let mut search_fallbacks = 0u32;
    let mut fallback_capped = false;
    // A search that ERRORS is not a line "absent from the library" — counting
    // it as not-found told the user their file was wrong when the database had
    // failed (#2798). Its own bucket, so total = matched + not_found +
    // lookup_errors always holds.
    let mut lookup_errors = 0u32;

    for line in file_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        total_entries += 1;

        // Exact path match (O(1) map lookup).
        if let Some((id, _, _)) = path_to_id.get(line) {
            track_ids.push(*id);
            matched += 1;
            continue;
        }

        // Filename-stem FTS fallback, bounded.
        if search_fallbacks < MAX_SEARCH_FALLBACKS {
            search_fallbacks += 1;
            let filename = std::path::Path::new(line)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(line);
            match track_repo.search(filename, 1) {
                Ok(results) => {
                    if let Some(track) = results.first()
                        && let Some(id) = track.id
                    {
                        track_ids.push(id);
                        matched += 1;
                        continue;
                    }
                }
                Err(e) => {
                    if lookup_errors == 0 {
                        tracing::warn!(error = %e, "m3u_import_file_lookup_error");
                    }
                    lookup_errors += 1;
                    continue;
                }
            }
        } else {
            fallback_capped = true;
        }

        not_found_count += 1;
        if not_found_paths.len() < MAX_NOT_FOUND_SAMPLE {
            not_found_paths.push(line.to_string());
        }
    }

    if fallback_capped {
        tracing::warn!(
            total_entries,
            matched,
            search_cap = MAX_SEARCH_FALLBACKS,
            "m3u_import_search_fallback_capped — many entries don't match library paths; \
             fuzzy search skipped past the cap to avoid a long-running import"
        );
    }

    // Create + fill atomically. The playlist used to be created first and the
    // track insert dropped with `.ok()`, so a failed insert left an empty
    // playlist behind AND answered 201 (#2798).
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.create_with_tracks(&name, None, profile.id(), &track_ids) {
        Ok((playlist_id, imported)) => {
            let duplicates_skipped = track_ids.len() - imported.len();
            tracing::info!(
                playlist_id,
                name = %name,
                total_entries,
                matched,
                imported = imported.len(),
                duplicates_skipped,
                not_found = not_found_count,
                lookup_errors,
                "m3u_import_file_complete"
            );
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": playlist_id,
                    "name": name,
                    "total_entries": total_entries,
                    // Lines resolved to a library track…
                    "matched": matched,
                    // …and rows actually persisted. They differ when the file
                    // lists the same track twice: the playlist never holds a
                    // duplicate, so `matched` alone would over-report.
                    "imported": imported.len(),
                    "duplicates_skipped": duplicates_skipped,
                    "not_found": not_found_count,
                    // Lines we could not look up at all (database error) —
                    // NOT counted as "not found in your library".
                    "lookup_errors": lookup_errors,
                    // Sample only (capped) — full count is in `not_found`.
                    "not_found_paths": not_found_paths,
                    "not_found_truncated": (not_found_count as usize) > not_found_paths.len(),
                    "track_count": imported.len(),
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::warn!(name = %name, error = %e, "m3u_import_file_create_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// One track parsed out of a Linn `.dpl` playlist (DIDL-Lite per track).
#[derive(Default)]
struct LinnTrack {
    title: String,
    artist: String,
    #[allow(dead_code)]
    album: String,
    res: String,
}

/// Parse a Linn `.dpl` playlist (`<linn:Playlist>` of DIDL-Lite `<item>`s) into
/// a flat list of tracks. We only pull the fields we need to match against the
/// local library: title, artist, album and the resource URL.
fn parse_linn_playlist(xml: &str) -> Vec<LinnTrack> {
    use quick_xml::Reader;
    use quick_xml::events::Event;

    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut tracks: Vec<LinnTrack> = Vec::new();
    let mut cur: Option<LinnTrack> = None;
    let mut tag = String::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                tag = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if tag == "item" {
                    cur = Some(LinnTrack::default());
                }
            }
            Ok(Event::End(ref e)) => {
                if String::from_utf8_lossy(e.local_name().as_ref()) == "item" {
                    if let Some(t) = cur.take() {
                        if !t.title.is_empty() || !t.res.is_empty() {
                            tracks.push(t);
                        }
                    }
                }
                tag.clear();
            }
            Ok(Event::Text(ref e)) => {
                let text = e.decode().map(|s| s.trim().to_string()).unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                if let Some(ref mut t) = cur {
                    match tag.as_str() {
                        "title" if t.title.is_empty() => t.title = text,
                        // dc:creator and the first upnp:artist both carry the
                        // performer — keep whichever we see first.
                        "creator" | "artist" if t.artist.is_empty() => t.artist = text,
                        "album" if t.album.is_empty() => t.album = text,
                        "res" if t.res.is_empty() => t.res = text,
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    tracks
}

/// Decode Linn's `*HH` hex escapes (e.g. `*20` = space) and return the file
/// stem of the last path segment of a `<res>` URL — used as a fallback match key.
fn linn_res_filename_stem(res: &str) -> String {
    let last = res.rsplit('/').next().unwrap_or(res);
    let bytes = last.as_bytes();
    let mut decoded = String::with_capacity(last.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'*' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&last[i + 1..i + 3], 16) {
                decoded.push(b as char);
                i += 3;
                continue;
            }
        }
        decoded.push(bytes[i] as char);
        i += 1;
    }
    std::path::Path::new(&decoded)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&decoded)
        .to_string()
}

/// Import a Linn `.dpl` playlist: parse it, match each track to the local
/// library (by title+artist, falling back to the resource filename), and create
/// a Tune playlist from the matches (Pierre Mack).
async fn import_linn_file(
    State(state): State<AppState>,
    profile: ActiveProfile,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut file_content = String::new();
    let mut playlist_name: Option<String> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name().unwrap_or("") {
            "file" => {
                if playlist_name.is_none() {
                    playlist_name = field
                        .file_name()
                        .map(|f| f.trim_end_matches(".dpl").to_string());
                }
                file_content = field.text().await.unwrap_or_default();
            }
            "name" => playlist_name = Some(field.text().await.unwrap_or_default()),
            _ => {}
        }
    }

    if file_content.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no file provided"})),
        )
            .into_response();
    }

    let name = playlist_name
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "Linn Playlist".into());

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let mut track_ids: Vec<i64> = Vec::new();
    let mut total_entries = 0u32;
    let mut matched = 0u32;
    let mut not_found: Vec<String> = Vec::new();

    for lt in parse_linn_playlist(&file_content) {
        total_entries += 1;

        let mut found: Option<i64> = None;

        // 1) Match by title, preferring a result whose artist matches.
        if !lt.title.is_empty() {
            let results = track_repo.search_by_title(&lt.title, 8).unwrap_or_default();
            let artist_lc = lt.artist.to_lowercase();
            found = results
                .iter()
                .find(|t| {
                    artist_lc.is_empty()
                        || t.artist_name
                            .as_deref()
                            .map(|a| a.to_lowercase().contains(&artist_lc))
                            .unwrap_or(false)
                })
                .or_else(|| results.first())
                .and_then(|t| t.id);
        }

        // 2) Fallback: match by the resource filename stem.
        if found.is_none() && !lt.res.is_empty() {
            let stem = linn_res_filename_stem(&lt.res);
            if !stem.is_empty() {
                if let Ok(results) = track_repo.search(&stem, 1) {
                    found = results.first().and_then(|t| t.id);
                }
            }
        }

        match found {
            Some(id) => {
                track_ids.push(id);
                matched += 1;
            }
            None => not_found.push(if lt.title.is_empty() {
                lt.res.clone()
            } else {
                lt.title.clone()
            }),
        }
    }

    // Same all-or-nothing creation as the M3U import (#2798): the `.ok()` here
    // hid a failed track insert behind a 201 and left an empty playlist.
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.create_with_tracks(&name, None, profile.id(), &track_ids) {
        Ok((playlist_id, imported)) => (
            StatusCode::CREATED,
            Json(json!({
                "id": playlist_id,
                "name": name,
                "total_entries": total_entries,
                "matched": matched,
                "imported": imported.len(),
                "duplicates_skipped": track_ids.len() - imported.len(),
                "not_found": not_found.len(),
                "not_found_titles": not_found,
                "track_count": imported.len(),
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(name = %name, error = %e, "linn_import_file_create_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

#[derive(Deserialize)]
struct TransferPlaylist {
    playlist_id: i64,
    #[allow(dead_code)]
    target_service: Option<String>,
    zone_id: Option<i64>,
}

#[derive(Deserialize)]
struct DiffPlaylists {
    source_service: String,
    source_playlist_id: String,
    target_service: String,
    target_playlist_id: String,
}

#[derive(Deserialize)]
struct ImportM3uUrl {
    url: String,
    name: Option<String>,
}

/// Reject addresses that must never be reachable via a user-supplied URL:
/// loopback, private, link-local (incl. 169.254.169.254 cloud metadata),
/// CGNAT, and their IPv6 equivalents. Blocks the SSRF pivot into internal
/// services. (`Ipv6Addr::is_unique_local`/`is_unicast_link_local` are still
/// unstable, so the v6 ranges are matched by prefix.)
fn is_blocked_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64) // 100.64/10 CGNAT
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// Fetch a user-supplied URL with SSRF guards: http(s) only, resolved host must
/// not be private/reserved, redirects disabled (a 3xx could bounce to an
/// internal address), a request timeout, and a hard body-size cap.
async fn fetch_url_guarded(
    raw_url: &str,
    max_bytes: usize,
) -> Result<String, (StatusCode, String)> {
    let url = reqwest::Url::parse(raw_url)
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid url".to_string()))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err((
            StatusCode::BAD_REQUEST,
            "only http(s) urls are allowed".to_string(),
        ));
    }
    let host = url
        .host_str()
        .ok_or((StatusCode::BAD_REQUEST, "missing host".to_string()))?;
    let port = url.port_or_known_default().unwrap_or(80);
    let mut resolved = false;
    for addr in tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| (StatusCode::BAD_GATEWAY, "dns resolution failed".to_string()))?
    {
        resolved = true;
        if is_blocked_ip(&addr.ip()) {
            return Err((
                StatusCode::FORBIDDEN,
                "url resolves to a private or reserved address".to_string(),
            ));
        }
    }
    if !resolved {
        return Err((StatusCode::BAD_GATEWAY, "host did not resolve".to_string()));
    }

    // Construit depuis `tune_core::http::client::builder()` pour hériter du TLS
    // webpki, en conservant les deux réglages voulus ici : aucune redirection
    // (une redirection contournerait le contrôle d'adresse privée ci-dessus) et
    // un délai d'attente court.
    let client = tune_core::http::client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("client build: {e}"),
            )
        })?;
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("fetch failed: {e}")))?;
    if !resp.status().is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("upstream status {}", resp.status()),
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("read failed: {e}")))?
    {
        if buf.len() + chunk.len() > max_bytes {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "playlist too large".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf).map_err(|_| (StatusCode::BAD_REQUEST, "non-utf8 content".to_string()))
}

async fn import_m3u_url(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<ImportM3uUrl>,
) -> impl IntoResponse {
    // 5 MiB is plenty for an M3U/M3U8 playlist and bounds memory use.
    let m3u_content = match fetch_url_guarded(&body.url, 5 * 1024 * 1024).await {
        Ok(text) => text,
        Err((status, msg)) => return (status, msg).into_response(),
    };

    let name = body.name.unwrap_or_else(|| "Imported Playlist".into());

    // Resolve EVERY line before touching the database. The old order created
    // the playlist first, then ran one `add_tracks_deduped(...).ok()` per line:
    // each dropped error still incremented `matched_tracks`, so the response
    // could claim tracks that were never written, on top of a playlist that
    // could not be rolled back (#2798).
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let mut track_ids: Vec<i64> = Vec::new();
    let mut total_entries = 0u32;
    let mut not_found = 0u32;

    for line in m3u_content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        total_entries += 1;
        match track_repo.get_by_path(line) {
            Ok(Some(track)) => match track.id {
                Some(id) => track_ids.push(id),
                None => not_found += 1,
            },
            Ok(None) => not_found += 1,
            // A lookup that FAILS is not a track "absent from the library".
            // Nothing is written yet, so we can still refuse honestly.
            Err(e) => {
                tracing::warn!(error = %e, "m3u_import_url_lookup_failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("library lookup failed: {e}"),
                )
                    .into_response();
            }
        }
    }

    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.create_with_tracks(&name, None, profile.id(), &track_ids) {
        Ok((playlist_id, imported)) => (
            StatusCode::CREATED,
            Json(json!({
                "id": playlist_id,
                "name": name,
                "total_entries": total_entries,
                // Persisted rows — the counter now describes the database,
                // not the parsing loop's intentions.
                "matched_tracks": imported.len(),
                "duplicates_skipped": track_ids.len() - imported.len(),
                "not_found": not_found,
            })),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(name = %name, error = %e, "m3u_import_url_create_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

// --- Advanced playlist routes ---

async fn list_all_playlists(State(state): State<AppState>, profile: ActiveProfile) -> Json<Value> {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let items = repo.list(profile.id(), 99999, 0).unwrap_or_default();
    Json(json!(items))
}

async fn share_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }

    // Unguessable share token: 128 bits from a CSPRNG. The old token was
    // `clock_nanos XOR (id * constant)` — derived from the share time and the
    // playlist id, so anyone who knew roughly when a playlist was shared could
    // brute-force the token and read it (audit — predictable share tokens).
    let token = uuid::Uuid::new_v4().simple().to_string();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("playlist_share_{id}");
    if let Err(e) = settings.set(&key, &token) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({
        "token": token,
        "url": format!("/api/v1/playlists/shared/{token}"),
    }))
    .into_response()
}

async fn get_shared_playlist(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let all = settings.all().unwrap_or_default();

    let playlist_id = all
        .iter()
        .find(|(k, v)| k.starts_with("playlist_share_") && v == &token)
        .and_then(|(k, _)| {
            k.strip_prefix("playlist_share_")
                .and_then(|s| s.parse::<i64>().ok())
        });

    let playlist_id = match playlist_id {
        Some(id) => id,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let playlist = match repo.get(playlist_id) {
        Ok(Some(p)) => p,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    // Une erreur de base rendait un partage « vide » indistinguable d'une
    // playlist réellement vide (#2797) : 500 explicite.
    let track_ids = match repo.get_track_ids(playlist_id) {
        Ok(ids) => ids,
        Err(e) => return AppError::internal(e.to_string()).into_response(),
    };
    let tracks = match TrackRepo::with_backend(state.backend.clone()).get_multiple(&track_ids) {
        Ok(t) => t,
        Err(e) => return AppError::internal(e.to_string()).into_response(),
    };

    Json(json!({
        "playlist": playlist,
        "tracks": tracks,
    }))
    .into_response()
}

/// Check availability of each track in a playlist ("vérifier la disponibilité").
/// A local track is available when its file still exists on disk; a missing file
/// (deleted/moved/unplugged drive) is reported unavailable. Previously this was
/// a stub that returned the raw playlist, so the UI spun on "vérification…".
async fn recover_playlist(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let pl = match repo.get(id) {
        Ok(Some(p)) => p,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let trepo = TrackRepo::with_backend(state.backend.clone());
    let mut tracks = Vec::with_capacity(track_ids.len());
    let mut available = 0i64;
    let mut unavailable = 0i64;

    for tid in &track_ids {
        let (title, artist, present) = match trepo.get(*tid) {
            Ok(Some(t)) => {
                // Repli de graphie NFC/NFD (#1865) : un `Path::exists()`
                // nu declarait « indisponible » des pistes bien presentes,
                // ecrites en NFD par macOS ou un partage SMB.
                let ok = t
                    .file_path
                    .as_deref()
                    .map(|p| !tune_core::library::local_path::resolve_local_path(p).is_missing())
                    .unwrap_or(false);
                (t.title, t.artist_name.unwrap_or_default(), ok)
            }
            _ => (String::new(), String::new(), false),
        };
        if present {
            available += 1;
        } else {
            unavailable += 1;
        }
        tracks.push(json!({
            "track_id": tid,
            "title": title,
            "artist_name": artist,
            "status": if present { "available" } else { "unavailable" },
            "original_source": "local",
            "alternatives": [],
        }));
    }

    Json(json!({
        "playlist_name": pl.name,
        "total_tracks": track_ids.len(),
        "available": available,
        "unavailable": unavailable,
        "recovered": 0,
        "tracks": tracks,
    }))
    .into_response()
}

async fn transfer_playlist(
    State(state): State<AppState>,
    Json(body): Json<TransferPlaylist>,
) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_ids = match repo.get_track_ids(body.playlist_id) {
        Ok(ids) => ids,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    let zone_id = body.zone_id.unwrap_or(1);
    let queue = PlayQueueRepo::with_backend(state.backend.clone());
    if let Err(e) = queue.add_tracks(zone_id, &track_ids, None) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({ "transferred": track_ids.len() })).into_response()
}

/// Fetch `(title, artist)` pairs for a playlist on a given service. Local reads
/// the DB; streaming services go through the registry. Used by the cross-service
/// diff, which matches on title+artist since the two sides share no track ids.
async fn diff_playlist_tracks(
    state: &AppState,
    service: &str,
    playlist_id: &str,
) -> Vec<(String, String)> {
    // Compare against a manual collection (Elie): a collection is a set of
    // albums (stored in settings), so flatten its albums to their tracks.
    if service == "collection" {
        let cid: i64 = playlist_id.parse().unwrap_or(0);
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let collections: Vec<Value> = settings
            .get("collections")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let album_ids: Vec<i64> = collections
            .iter()
            .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(cid))
            .and_then(|c| c.get("album_ids"))
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        let trepo = TrackRepo::with_backend(state.backend.clone());
        let mut out = Vec::new();
        for aid in album_ids {
            for t in trepo.list_by_album(aid).unwrap_or_default() {
                out.push((t.title, t.artist_name.unwrap_or_default()));
            }
        }
        return out;
    }
    if service == "local" || service.is_empty() {
        let pid: i64 = playlist_id.parse().unwrap_or(0);
        let prepo = PlaylistRepo::with_backend(state.backend.clone());
        let trepo = TrackRepo::with_backend(state.backend.clone());
        prepo
            .get_track_ids(pid)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| trepo.get(id).ok().flatten())
            .map(|t| (t.title, t.artist_name.unwrap_or_default()))
            .collect()
    } else {
        let reg = state.services.lock().await;
        reg.get_playlist_tracks(service, playlist_id)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|v| {
                (
                    v.get("title")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    v.get("artist")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            })
            .collect()
    }
}

async fn diff_playlists(
    State(state): State<AppState>,
    Json(body): Json<DiffPlaylists>,
) -> impl IntoResponse {
    let src = diff_playlist_tracks(&state, &body.source_service, &body.source_playlist_id).await;
    let tgt = diff_playlist_tracks(&state, &body.target_service, &body.target_playlist_id).await;

    let norm =
        |t: &str, a: &str| format!("{}|{}", t.trim().to_lowercase(), a.trim().to_lowercase());
    let src_keys: HashSet<String> = src.iter().map(|(t, a)| norm(t, a)).collect();
    let tgt_keys: HashSet<String> = tgt.iter().map(|(t, a)| norm(t, a)).collect();

    let entry = |t: &str, a: &str, in_s: bool, in_t: bool| {
        json!({
            "title": t, "artist_name": a,
            "in_source": in_s, "in_target": in_t,
            "match_quality": "exact",
        })
    };
    let only_in_source: Vec<Value> = src
        .iter()
        .filter(|(t, a)| !tgt_keys.contains(&norm(t, a)))
        .map(|(t, a)| entry(t, a, true, false))
        .collect();
    let in_both: Vec<Value> = src
        .iter()
        .filter(|(t, a)| tgt_keys.contains(&norm(t, a)))
        .map(|(t, a)| entry(t, a, true, true))
        .collect();
    let only_in_target: Vec<Value> = tgt
        .iter()
        .filter(|(t, a)| !src_keys.contains(&norm(t, a)))
        .map(|(t, a)| entry(t, a, false, true))
        .collect();

    // Best-effort display names: local playlists resolve to their name.
    let name_of = |service: &str, id: &str| -> String {
        if service == "local" || service.is_empty() {
            let prepo = PlaylistRepo::with_backend(state.backend.clone());
            id.parse::<i64>()
                .ok()
                .and_then(|pid| prepo.get(pid).ok().flatten())
                .map(|p| p.name)
                .unwrap_or_else(|| id.to_string())
        } else {
            service.to_string()
        }
    };

    Json(json!({
        "source_name": name_of(&body.source_service, &body.source_playlist_id),
        "target_name": name_of(&body.target_service, &body.target_playlist_id),
        "only_in_source": only_in_source,
        "only_in_target": only_in_target,
        "in_both": in_both,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Recovery apply
// ---------------------------------------------------------------------------

async fn apply_recovery(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let track_ids = repo.get_track_ids(id).unwrap_or_default();
    let mut recovered = 0i64;
    let mut missing = 0i64;

    for tid in &track_ids {
        match track_repo.get(*tid) {
            Ok(Some(t)) if t.file_path.is_some() => {
                let path = t.file_path.as_ref().unwrap();
                // Meme repli qu'au-dessus (#1865) : « encore manquante » ne
                // doit pas vouloir dire « ecrite dans l'autre forme Unicode ».
                if tune_core::library::local_path::resolve_local_path(path).is_missing() {
                    missing += 1;
                } else {
                    recovered += 1;
                }
            }
            _ => missing += 1,
        }
    }

    Json(json!({
        "playlist_id": id,
        "total_tracks": track_ids.len(),
        "recovered": recovered,
        "still_missing": missing,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Collaborative playlists (stored in settings as JSON)
// ---------------------------------------------------------------------------

async fn list_collaborative(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(items))
}

#[derive(Deserialize)]
struct CreateCollaborative {
    name: String,
    description: Option<String>,
}

async fn create_collaborative(
    State(state): State<AppState>,
    Json(body): Json<CreateCollaborative>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let id = format!("collab_{:016x}", nanos);

    let entry = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "tracks": [],
        "created_at": nanos / 1_000_000_000,
    });
    items.push(entry.clone());
    settings
        .set(
            "collaborative_playlists",
            &serde_json::to_string(&items).unwrap_or_default(),
        )
        .ok();
    (StatusCode::CREATED, Json(entry)).into_response()
}

async fn get_collaborative(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    match items.iter().find(|i| i["id"].as_str() == Some(&id)) {
        Some(item) => Json(item.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct AddToCollaborative {
    track_ids: Vec<i64>,
}

async fn add_to_collaborative(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AddToCollaborative>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let Some(entry) = items.iter_mut().find(|i| i["id"].as_str() == Some(&id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let tracks = entry.get_mut("tracks").and_then(|t| t.as_array_mut());
    if let Some(tracks) = tracks {
        for tid in &body.track_ids {
            tracks.push(json!(tid));
        }
    }
    settings
        .set(
            "collaborative_playlists",
            &serde_json::to_string(&items).unwrap_or_default(),
        )
        .ok();
    Json(json!({ "added": body.track_ids.len() })).into_response()
}

async fn collaborative_tracks(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let items: Vec<Value> = settings
        .get("collaborative_playlists")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let Some(entry) = items.iter().find(|i| i["id"].as_str() == Some(&id)) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    let track_ids: Vec<i64> = entry["tracks"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
    Json(json!(tracks)).into_response()
}

// ---------------------------------------------------------------------------
// Match tracks (fuzzy search)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MatchEntry {
    title: String,
    artist: Option<String>,
}

#[derive(Deserialize)]
struct MatchRequest {
    tracks: Vec<MatchEntry>,
}

async fn match_tracks(
    State(state): State<AppState>,
    Json(body): Json<MatchRequest>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let mut results: Vec<Value> = Vec::new();

    for entry in &body.tracks {
        let q = if let Some(ref artist) = entry.artist {
            format!("{} {}", artist, entry.title)
        } else {
            entry.title.clone()
        };
        let matched = track_repo.search(&q, 3).unwrap_or_default();
        results.push(json!({
            "query_title": entry.title,
            "query_artist": entry.artist,
            "matches": matched,
        }));
    }

    Json(json!({ "results": results, "total": body.tracks.len() })).into_response()
}

#[cfg(test)]
mod linn_tests {
    use super::{linn_res_filename_stem, parse_linn_playlist};

    const SAMPLE: &str = r#"<linn:Playlist version="3" xmlns:linn="urn:linn-co-uk/playlist">
  <linn:Track>
    <DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">
      <item id="x" parentID="y" restricted="False">
        <dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">Look Sharp!</dc:title>
        <dc:creator xmlns:dc="http://purl.org/dc/elements/1.1/">Joe Jackson</dc:creator>
        <upnp:album xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">Live 1980/86</upnp:album>
        <upnp:albumArtURI xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">http://x/art.jpg</upnp:albumArtURI>
        <upnp:artist role="AlbumArtist" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">Joe Jackson</upnp:artist>
        <res>http://10.0.0.1:9790/minimserver/*/Musique/CD/Joe*20Jackson/11*20Joe*20Jackson*20-*20Look*20Sharp!.flac</res>
      </item>
    </DIDL-Lite>
  </linn:Track>
  <linn:Track>
    <DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">
      <item id="x2" parentID="y" restricted="False">
        <dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">Sunday Papers</dc:title>
        <upnp:artist xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">Joe Jackson</upnp:artist>
        <res>http://10.0.0.1:9790/minimserver/*/x/02*20Sunday*20Papers.flac</res>
      </item>
    </DIDL-Lite>
  </linn:Track>
</linn:Playlist>"#;

    #[test]
    fn parses_tracks_title_artist_album_res() {
        let tracks = parse_linn_playlist(SAMPLE);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].title, "Look Sharp!");
        assert_eq!(tracks[0].artist, "Joe Jackson"); // dc:creator
        assert_eq!(tracks[0].album, "Live 1980/86"); // NOT the albumArtURI
        assert!(tracks[0].res.ends_with("Look*20Sharp!.flac"));
        assert_eq!(tracks[1].title, "Sunday Papers");
        assert_eq!(tracks[1].artist, "Joe Jackson"); // upnp:artist fallback
    }

    #[test]
    fn decodes_linn_escapes_to_filename_stem() {
        let stem = linn_res_filename_stem(
            "http://10.0.0.1:9790/minimserver/*/x/11*20Joe*20Jackson*20-*20Look*20Sharp!.flac",
        );
        assert_eq!(stem, "11 Joe Jackson - Look Sharp!");
    }
}

#[cfg(test)]
mod ssrf_tests {
    use super::is_blocked_ip;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_internal_and_reserved_addresses() {
        for s in [
            "127.0.0.1",       // loopback
            "10.1.2.3",        // private
            "192.168.0.1",     // private
            "172.16.5.4",      // private
            "169.254.169.254", // link-local / cloud metadata
            "100.64.0.1",      // CGNAT
            "0.0.0.0",         // unspecified
            "::1",             // v6 loopback
            "fe80::1",         // v6 link-local
            "fc00::1",         // v6 unique-local
        ] {
            assert!(is_blocked_ip(&ip(s)), "{s} should be blocked");
        }
    }

    #[test]
    fn allows_public_addresses() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700:4700::1111",
        ] {
            assert!(!is_blocked_ip(&ip(s)), "{s} should be allowed");
        }
    }
}
