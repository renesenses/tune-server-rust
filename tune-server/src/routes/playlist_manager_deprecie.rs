//! Liens et sauvegardes de `/playlist-manager` : des ALIAS DÉPRÉCIÉS.
//!
//! Ces routes font double emploi avec le greffon « Playlists converter »
//! (`/api/v1/plugins/playlists-converter/…`, voir
//! `docs/plugins/playlists-converter.md`) :
//!
//! | Route dépréciée | Remplaçante (greffon) |
//! |---|---|
//! | `GET/POST /playlist-manager/links` | `GET/POST /liens` |
//! | `DELETE /playlist-manager/links/{id}` | `POST /lien/supprimer` |
//! | `POST /playlist-manager/links/{id}/sync` | `POST /lien/apercu`, puis `POST /lien/synchroniser` |
//! | `POST /playlist-manager/backup` | `POST /snapshot` (une playlist à la fois) |
//! | `GET /playlist-manager/backups[/{id}]` | `GET /snapshots`, `GET /snapshot?id=` |
//! | `POST /playlist-manager/backups/{id}/restore` | `POST /snapshot/restauration/apercu`, puis `/snapshot/restauration` |
//! | `DELETE /playlist-manager/backups/{id}` | aucune : le greffon garde un anneau de 10 copies par playlist |
//!
//! Les clients livrés (web, appli iPad, appli Flutter) passent désormais par
//! le greffon. Ces alias ne restent que pour les ANCIENS clients : ils
//! répondent comme avant, avec en plus l'en-tête `Deprecation` (RFC 9745),
//! l'en-tête `Sunset` (RFC 8594) et un `Link` vers la route qui les remplace
//! (`rel="successor-version"`).
//!
//! Calendrier (décision de Bertrand, 08/10/2026) : dépréciés dès la 1.0,
//! RETIRÉS à la 1.1. Ce fichier et ses deux `merge` dans
//! `playlist_manager::router` sont alors supprimés. Rien d'autre n'en dépend.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use super::{default_true, load_json_setting, next_id, now_iso, save_json_setting};
use crate::routes::active_profile::ActiveProfile;
use crate::routes::playlists::owned_or_404_response;
use crate::state::AppState;

/// Date de la dépréciation, au format de RFC 9745 (`@` + secondes Unix) :
/// 2026-10-07T00:00:00Z, le jour où le retrait des doublons a été décidé.
pub(crate) const DEPRECATION: &str = "@1791331200";

/// Date de retrait annoncée, au format HTTP-date de RFC 8594 : le retrait
/// est prévu pour la version 1.1 (décision du 08/10/2026).
///
/// ⚠️ DATE PROVISOIRE : la date de publication de la 1.1 n'est pas fixée.
/// C'est la seule valeur à changer quand elle le sera (avec sa copie dans
/// l'essai `playlist_manager_alias_deprecies`).
pub(crate) const SUNSET: &str = "Fri, 01 Jan 2027 00:00:00 GMT";

fn marquer(mut reponse: Response, successeur: &'static str) -> Response {
    let en_tetes = reponse.headers_mut();
    en_tetes.insert("deprecation", HeaderValue::from_static(DEPRECATION));
    en_tetes.insert("sunset", HeaderValue::from_static(SUNSET));
    en_tetes.insert("link", HeaderValue::from_static(successeur));
    reponse
}

async fn marquer_liens(reponse: Response) -> Response {
    marquer(
        reponse,
        "</api/v1/plugins/playlists-converter/liens>; rel=\"successor-version\"",
    )
}

async fn marquer_sauvegardes(reponse: Response) -> Response {
    marquer(
        reponse,
        "</api/v1/plugins/playlists-converter/snapshots>; rel=\"successor-version\"",
    )
}

/// Les alias des liens, marqués dépréciés.
pub(super) fn liens() -> Router<AppState> {
    Router::new()
        .route("/links", get(list_links).post(create_link))
        .route("/links/{id}", axum::routing::delete(delete_link))
        .route("/links/{id}/sync", post(sync_link))
        .layer(axum::middleware::map_response(marquer_liens))
}

/// Les alias des sauvegardes, marqués dépréciés.
pub(super) fn sauvegardes() -> Router<AppState> {
    Router::new()
        .route("/backup", post(backup_playlists))
        .route("/backups", get(list_backups))
        .route("/backups/{id}", get(get_backup).delete(delete_backup))
        .route("/backups/{id}/restore", post(restore_backup))
        .layer(axum::middleware::map_response(marquer_sauvegardes))
}

// ---------------------------------------------------------------------------
// Playlist Links (déprécié)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateLinkRequest {
    local_playlist_id: i64,
    service: String,
    service_playlist_id: String,
    sync_direction: Option<String>,
    sync_interval_minutes: Option<i64>,
}

async fn list_links(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let links = load_json_setting(&settings, "playlist_links");
    Json(json!(links))
}

async fn create_link(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<CreateLinkRequest>,
) -> impl IntoResponse {
    // Le lien désigne une playlist locale par son id, et `sync_link` y écrit.
    // Refuser dès la création évite d'inscrire dans les réglages un lien qui
    // pointe sur la playlist d'un autre profil.
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = owned_or_404_response(&playlist_repo, body.local_playlist_id, profile.id()) {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");
    let id = next_id(&links);
    let link = json!({
        "id": id,
        "local_playlist_id": body.local_playlist_id,
        "service": body.service,
        "service_playlist_id": body.service_playlist_id,
        "sync_direction": body.sync_direction.as_deref().unwrap_or("pull"),
        "sync_interval_minutes": body.sync_interval_minutes.unwrap_or(0),
        "last_synced_at": null,
        "created_at": now_iso(),
    });
    links.push(link.clone());
    save_json_setting(&settings, "playlist_links", &links);
    (StatusCode::CREATED, Json(link)).into_response()
}

async fn delete_link(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");
    let before = links.len();
    links.retain(|l| l.get("id").and_then(|v| v.as_i64()) != Some(id));
    if links.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "playlist_links", &links);
    Json(json!({"ok": true})).into_response()
}

async fn sync_link(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");

    let link = links
        .iter_mut()
        .find(|l| l.get("id").and_then(|v| v.as_i64()) == Some(id));
    let link = match link {
        Some(l) => l,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let service = link
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let service_playlist_id = link
        .get("service_playlist_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let local_playlist_id = link
        .get("local_playlist_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let direction = link
        .get("sync_direction")
        .and_then(|v| v.as_str())
        .unwrap_or("pull")
        .to_string();

    // Les liens vivent dans un réglage commun au foyer et sont donc VISIBLES de
    // tous : sans ce refus, `POST /links/{id}/sync` faisait écrire des pistes
    // dans la playlist locale d'un autre profil, en énumérant simplement les
    // ids de liens. Le refus est posé AVANT l'appel au service distant : rien
    // n'est fetché, rien n'est écrit, et `last_synced_at` reste intact.
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = owned_or_404_response(&playlist_repo, local_playlist_id, profile.id()) {
        return r;
    }

    // Fetch remote playlist tracks
    let registry = state.services.lock().await;
    let svc_arc = match registry.get(&service) {
        Some(arc) => arc,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": format!("Service '{service}' not available")})),
            )
                .into_response();
        }
    };
    drop(registry);

    let svc = svc_arc.read().await;
    let remote_tracks = svc
        .get_playlist_tracks(&service_playlist_id)
        .await
        .unwrap_or_default();
    drop(svc);

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let local_track_ids = playlist_repo
        .get_track_ids(local_playlist_id)
        .unwrap_or_default();

    let mut added_to_local = 0i64;

    if direction == "pull" || direction == "bidirectional" {
        // Match remote tracks against local library and add missing ones
        for rt in &remote_tracks {
            let title = rt.title.as_str();
            let artist = rt.artist.as_str();
            let query = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{title} {artist}")
            };
            if let Ok(results) = track_repo.search(&query, 1)
                && let Some(track) = results.first()
                && let Some(tid) = track.id
                && !local_track_ids.contains(&tid)
            {
                playlist_repo
                    .add_tracks(local_playlist_id, &[tid], None)
                    .ok();
                added_to_local += 1;
            }
        }
    }

    // Update last_synced_at
    link["last_synced_at"] = json!(now_iso());
    save_json_setting(&settings, "playlist_links", &links);

    Json(json!({
        "link_id": id,
        "direction": direction,
        "added_to_local": added_to_local,
        "added_to_remote": 0,
        "removed_from_local": 0,
        "removed_from_remote": 0,
        "conflicts": [],
        "snapshot_saved": false,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Backup / Snapshots
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BackupRequest {
    services: Option<Vec<String>>,
    #[serde(default = "default_true")]
    include_tracks: bool,
}

async fn backup_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<BackupRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let mut snapshots = load_json_setting(&settings, "playlist_snapshots");
    let mut total_playlists = 0usize;
    let mut total_tracks = 0usize;
    let mut service_counts = serde_json::Map::new();

    // Determine which services to back up
    let registry = state.services.lock().await;
    let status_all = registry.status_all().await;
    drop(registry);

    let service_names: Vec<String> = if let Some(ref svcs) = body.services {
        svcs.clone()
    } else {
        let mut names: Vec<String> = status_all
            .iter()
            .filter_map(|s| {
                s.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        names.push("local".to_string());
        names
    };

    for svc_name in &service_names {
        let mut svc_count = 0usize;

        if svc_name == "local" {
            let playlists = playlist_repo
                .list(profile.id(), 99999, 0)
                .unwrap_or_default();
            for pl in &playlists {
                let pl_id = pl.id.unwrap_or(0);
                let mut tracks_data: Vec<Value> = Vec::new();
                if body.include_tracks {
                    let track_ids = playlist_repo.get_track_ids(pl_id).unwrap_or_default();
                    let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
                    tracks_data = tracks
                        .iter()
                        .map(|t| {
                            json!({
                                "title": t.title,
                                "artist_name": t.artist_name,
                                "album_title": t.album_title,
                                "duration_ms": t.duration_ms,
                            })
                        })
                        .collect();
                }
                let snap_id = next_id(&snapshots);
                snapshots.push(json!({
                    "id": snap_id,
                    "source_service": "local",
                    "source_playlist_id": pl_id.to_string(),
                    "playlist_name": pl.name,
                    "track_count": tracks_data.len(),
                    "created_at": now_iso(),
                    "snapshot_data": tracks_data,
                }));
                total_playlists += 1;
                total_tracks += tracks_data.len();
                svc_count += 1;
            }
        } else {
            // Streaming service backup
            let registry = state.services.lock().await;
            let svc_arc = match registry.get(svc_name) {
                Some(arc) => arc,
                None => continue,
            };
            drop(registry);

            let svc = svc_arc.read().await;
            let playlists = svc.get_user_playlists().await.unwrap_or_default();
            for pl in &playlists {
                let pl_name = &pl.name;
                let source_id = &pl.id;

                let mut tracks_data: Vec<Value> = Vec::new();
                if body.include_tracks
                    && let Ok(tracks) = svc.get_playlist_tracks(source_id).await
                {
                    tracks_data = tracks
                        .iter()
                        .map(|t| {
                            json!({
                                "title": t.title,
                                "artist_name": t.artist,
                                "album_title": t.album.as_deref().unwrap_or(""),
                                "duration_ms": t.duration_ms,
                                "source_id": t.id,
                            })
                        })
                        .collect();
                }
                let snap_id = next_id(&snapshots);
                snapshots.push(json!({
                    "id": snap_id,
                    "source_service": svc_name,
                    "source_playlist_id": source_id,
                    "playlist_name": pl_name,
                    "track_count": tracks_data.len(),
                    "created_at": now_iso(),
                    "snapshot_data": tracks_data,
                }));
                total_playlists += 1;
                total_tracks += tracks_data.len();
                svc_count += 1;
            }
        }
        service_counts.insert(svc_name.clone(), json!(svc_count));
    }

    save_json_setting(&settings, "playlist_snapshots", &snapshots);

    Json(json!({
        "backup_id": snapshots.len(),
        "playlists_backed_up": total_playlists,
        "total_tracks_snapshot": total_tracks,
        "services": service_counts,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct BackupsQuery {
    service: Option<String>,
    limit: Option<usize>,
}

async fn list_backups(State(state): State<AppState>, Query(q): Query<BackupsQuery>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let limit = q.limit.unwrap_or(500);

    let filtered: Vec<Value> = snapshots
        .iter()
        .rev()
        .filter(|s| {
            if let Some(ref svc) = q.service {
                s.get("source_service")
                    .and_then(|v| v.as_str())
                    .map(|n| n == svc)
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .take(limit)
        .map(|s| {
            let mut v = s.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("snapshot_data");
            }
            v
        })
        .collect();

    Json(json!(filtered))
}

async fn get_backup(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let snap = snapshots
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_i64()) == Some(id));
    match snap {
        Some(s) => Json(s.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_backup(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut snapshots = load_json_setting(&settings, "playlist_snapshots");
    let before = snapshots.len();
    snapshots.retain(|s| s.get("id").and_then(|v| v.as_i64()) != Some(id));
    if snapshots.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "playlist_snapshots", &snapshots);
    Json(json!({"deleted": true, "id": id})).into_response()
}

#[derive(Deserialize)]
struct RestoreRequest {
    target_name: Option<String>,
    #[serde(default)]
    overwrite_existing: bool,
}

async fn restore_backup(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    body: Option<Json<RestoreRequest>>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let snap = match snapshots
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_i64()) == Some(id))
    {
        Some(s) => s.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let body = body.map(|b| b.0);
    let overwrite = body.as_ref().map(|b| b.overwrite_existing).unwrap_or(false);
    let playlist_name = snap
        .get("playlist_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Restored Playlist");
    let target_name = body
        .as_ref()
        .and_then(|b| b.target_name.as_deref())
        .unwrap_or(playlist_name);

    let snapshot_tracks: Vec<Value> = snap
        .get("snapshot_data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    // Check for existing playlist
    let existing_playlists = playlist_repo
        .list(profile.id(), 99999, 0)
        .unwrap_or_default();
    let existing = existing_playlists.iter().find(|p| p.name == target_name);
    if existing.is_some() && !overwrite {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail": format!("Local playlist '{target_name}' already exists. Use overwrite_existing=true to replace.")})),
        )
            .into_response();
    }

    let playlist_id = if let Some(ex) = existing {
        let pid = ex.id.unwrap_or(0);
        // Clear existing tracks — TOUTES les lignes. L'ancienne boucle
        // retirait les positions 0..n-1, n compté sur les seules pistes
        // LOCALES : depuis #4889 une playlist peut porter des titres de
        // service, qui décalent les positions, et « remplacer » aurait
        // laissé des lignes de l'ancienne version derrière la nouvelle.
        playlist_repo.set_tracks(pid, &[]).ok();
        pid
    } else {
        match playlist_repo.create(
            target_name,
            Some(&format!("Restored from snapshot #{id}")),
            profile.id(),
        ) {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": e})),
                )
                    .into_response();
            }
        }
    };

    // Match snapshot tracks against local library
    let mut matched = 0i64;
    let mut not_found = 0i64;
    let mut matched_ids: Vec<i64> = Vec::new();

    for track in &snapshot_tracks {
        let title = track.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let artist = track
            .get("artist_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if title.is_empty() {
            not_found += 1;
            continue;
        }
        let query = if artist.is_empty() {
            title.to_string()
        } else {
            format!("{title} {artist}")
        };
        if let Ok(results) = track_repo.search(&query, 1)
            && let Some(track) = results.first()
            && let Some(tid) = track.id
        {
            matched_ids.push(tid);
            matched += 1;
            continue;
        }
        not_found += 1;
    }

    if !matched_ids.is_empty() {
        playlist_repo
            .add_tracks(playlist_id, &matched_ids, None)
            .ok();
    }

    Json(json!({
        "local_playlist_id": playlist_id,
        "name": target_name,
        "tracks_restored": matched,
        "tracks_matched": matched,
        "tracks_not_found": not_found,
    }))
    .into_response()
}
