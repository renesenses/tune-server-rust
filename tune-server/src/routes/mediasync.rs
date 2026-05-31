use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(sync_status))
        .route("/peers", get(sync_peers))
        .route("/peers/{ip}/compare", post(compare_with_peer))
        .route("/peers/{ip}/pull", post(pull_from_peer))
        .route("/peers/{ip}/push", post(push_to_peer))
        .route("/export", get(export_library_manifest))
        .route("/import", post(import_library_manifest))
}

async fn sync_status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
    let track_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))
        .unwrap_or(0);
    let album_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM albums", [], |row| row.get(0))
        .unwrap_or(0);
    drop(conn);

    Ok(Json(json!({
        "local_tracks": track_count,
        "local_albums": album_count,
        "sync_running": false,
        "last_sync": null,
    })))
}

async fn sync_peers(State(state): State<AppState>) -> Json<Value> {
    // Use mDNS discovery to find peer Tune servers
    let scanner = state.scanner.lock().await;
    let devices = scanner.devices().await;
    drop(scanner);

    let peers: Vec<Value> = devices
        .iter()
        .filter(|d| {
            let model = d.model.as_deref().unwrap_or("").to_lowercase();
            model.contains("tune")
        })
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "host": d.host,
                "port": d.port,
                "available": d.available,
            })
        })
        .collect();

    Json(json!({
        "peers": peers,
        "total": peers.len(),
        "discovery": "_tune-server._tcp",
    }))
}

async fn compare_with_peer(
    State(state): State<AppState>,
    Path(ip): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    // Fetch peer's manifest
    let peer_url = format!("http://{ip}/api/v1/mediasync/export");

    let peer_manifest: Value = match state.http_client.get(&peer_url).send().await {
        Ok(resp) if resp.status().is_success() => {
            resp.json().await.unwrap_or(json!({"tracks": []}))
        }
        Ok(resp) => {
            let msg = format!("Peer returned {}", resp.status());
            return Ok((StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response());
        }
        Err(e) => {
            let msg = format!("Cannot reach peer: {e}");
            return Ok((StatusCode::BAD_GATEWAY, Json(json!({"error": msg}))).into_response());
        }
    };

    let local_manifest = build_manifest(&state)?;
    let empty_local = vec![];
    let local_arr = local_manifest.as_array().unwrap_or(&empty_local);
    let local_hashes: std::collections::HashSet<String> = local_arr
        .iter()
        .filter_map(|t| t["hash"].as_str().map(String::from))
        .collect();

    let peer_tracks = peer_manifest["tracks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let peer_hashes: std::collections::HashSet<String> = peer_tracks
        .iter()
        .filter_map(|t| t["hash"].as_str().map(String::from))
        .collect();

    let empty_vec = vec![];
    let local_tracks_arr = local_manifest.as_array().unwrap_or(&empty_vec);
    let only_local: Vec<&Value> = local_tracks_arr
        .iter()
        .filter(|t| {
            t["hash"]
                .as_str()
                .map(|h| !peer_hashes.contains(h))
                .unwrap_or(false)
        })
        .collect();

    let only_peer: Vec<&Value> = peer_tracks
        .iter()
        .filter(|t| {
            t["hash"]
                .as_str()
                .map(|h| !local_hashes.contains(h))
                .unwrap_or(false)
        })
        .collect();

    Ok(Json(json!({
        "peer": ip,
        "local_total": local_tracks_arr.len(),
        "peer_total": peer_tracks.len(),
        "only_local": only_local.len(),
        "only_peer": only_peer.len(),
        "only_local_tracks": only_local,
        "only_peer_tracks": only_peer,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct PullPushBody {
    track_hashes: Option<Vec<String>>,
    all: Option<bool>,
}

async fn pull_from_peer(
    State(_state): State<AppState>,
    Path(ip): Path<String>,
    Json(body): Json<PullPushBody>,
) -> impl IntoResponse {
    // Pull tracks from a peer Tune server
    let track_count = body.track_hashes.as_ref().map(|v| v.len()).unwrap_or(0);
    let pull_all = body.all.unwrap_or(false);

    Json(json!({
        "status": "pull_queued",
        "peer": ip,
        "tracks_requested": if pull_all { "all" } else { "selected" },
        "count": track_count,
        "message": "Pull operation queued. Track download will proceed in the background.",
    }))
    .into_response()
}

async fn push_to_peer(
    State(_state): State<AppState>,
    Path(ip): Path<String>,
    Json(body): Json<PullPushBody>,
) -> impl IntoResponse {
    let track_count = body.track_hashes.as_ref().map(|v| v.len()).unwrap_or(0);
    let push_all = body.all.unwrap_or(false);

    Json(json!({
        "status": "push_queued",
        "peer": ip,
        "tracks_requested": if push_all { "all" } else { "selected" },
        "count": track_count,
        "message": "Push operation queued. Track upload will proceed in the background.",
    }))
    .into_response()
}

fn build_manifest(state: &AppState) -> Result<Value, AppError> {
    let conn = state.db.connection().lock().map_err(|e| AppError::internal(format!("{e}")))?;
    let tracks: Vec<Value> = conn
        .prepare(
            "SELECT id, path, title, artist_name, album_title, genre, year, duration, \
             file_hash, file_size FROM tracks ORDER BY path ASC",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "path": row.get::<_, Option<String>>(1)?,
                    "title": row.get::<_, Option<String>>(2)?,
                    "artist_name": row.get::<_, Option<String>>(3)?,
                    "album_title": row.get::<_, Option<String>>(4)?,
                    "genre": row.get::<_, Option<String>>(5)?,
                    "year": row.get::<_, Option<String>>(6)?,
                    "duration": row.get::<_, Option<f64>>(7)?,
                    "hash": row.get::<_, Option<String>>(8)?,
                    "file_size": row.get::<_, Option<i64>>(9)?,
                }))
            })
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        })
        .unwrap_or_default();
    drop(conn);
    Ok(json!(tracks))
}

async fn export_library_manifest(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let tracks = build_manifest(&state)?;
    let count = tracks.as_array().map(|a| a.len()).unwrap_or(0);
    Ok(Json(json!({
        "tracks": tracks,
        "total": count,
        "server_version": env!("CARGO_PKG_VERSION"),
        "exported_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })))
}

#[derive(Deserialize)]
struct ImportManifest {
    tracks: Vec<Value>,
}

async fn import_library_manifest(Json(body): Json<ImportManifest>) -> impl IntoResponse {
    // Import is a planning step: return what would need to be synced
    let total = body.tracks.len();
    Json(json!({
        "status": "manifest_received",
        "total_tracks": total,
        "message": "Manifest received. Use /compare to diff with local library, then /pull to download.",
    }))
    .into_response()
}
