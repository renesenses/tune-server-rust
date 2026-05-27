use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::process::Command;

use crate::state::AppState;

#[derive(Deserialize)]
struct CreateMount {
    mount_type: Option<String>,
    server: String,
    share: String,
    mount_path: String,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct ScanHostQuery {
    host: String,
    protocol: Option<String>,
}

#[derive(Deserialize)]
struct MountRequest {
    host: String,
    share_name: String,
    username: Option<String>,
    password: Option<String>,
    mount_path: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mounts", get(list_mounts).post(create_mount))
        .route("/mounts/{id}", axum::routing::delete(delete_mount))
        .route("/media-servers", get(list_media_servers))
        .route("/shares", get(list_shares))
        .route("/scan-host", get(scan_host))
        .route("/smb/discover", get(list_smb_shares).post(trigger_smb_scan))
        .route("/smb/mounts", get(list_smb_mounts))
        .route("/smb/mount", post(mount_smb_share))
        .route("/media-servers/{id}/browse", get(browse_media_server))
        .route("/media-servers/{id}/item/{item_id}/stream-url", get(media_server_stream_url))
        .route("/media-servers/{id}/item/{item_id}/play/{zone_id}", post(play_media_server_item))
        .route("/mounts/test", post(test_mount))
        .route("/shares/{id}", get(get_share_detail))
}

async fn list_mounts(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare("SELECT id, mount_type, server, share, mount_path, username, active FROM network_mounts ORDER BY id")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "mount_type": row.get::<_, Option<String>>(1).ok().flatten(),
                    "server": row.get::<_, Option<String>>(2).ok().flatten(),
                    "share": row.get::<_, Option<String>>(3).ok().flatten(),
                    "mount_path": row.get::<_, Option<String>>(4).ok().flatten(),
                    "username": row.get::<_, Option<String>>(5).ok().flatten(),
                    "active": row.get::<_, i32>(6).unwrap_or(1) != 0,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}

async fn create_mount(
    State(state): State<AppState>,
    Json(body): Json<CreateMount>,
) -> impl IntoResponse {
    match state.db.execute(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            &body.mount_type.unwrap_or_else(|| "smb".into()) as &dyn rusqlite::types::ToSql,
            &body.server,
            &body.share,
            &body.mount_path,
            &body.username,
            &body.password,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_mount(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    state.db.execute("DELETE FROM network_mounts WHERE id = ?", &[&id]).ok();
    StatusCode::NO_CONTENT
}

async fn list_media_servers() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "UPnP media server discovery pending",
    }))
}

// ---------------------------------------------------------------------------
// SMB discovery and mount management
// ---------------------------------------------------------------------------

/// Return discovered network shares (stub — real mDNS scanning is a future feature).
async fn list_shares() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "Network share discovery pending (mDNS not yet implemented)",
    }))
}

/// Scan a specific host for SMB or NFS shares.
async fn scan_host(Query(q): Query<ScanHostQuery>) -> impl IntoResponse {
    let host = &q.host;
    let protocol = q.protocol.as_deref().unwrap_or("smb");

    let raw_output = if protocol == "smb" {
        // Try smbutil (macOS) first, then smbclient (Linux)
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("smbutil")
                .args(["view", &format!("//guest@{host}")])
                .output(),
        )
        .await;

        match result {
            Ok(Ok(out)) if out.status.success() => {
                String::from_utf8_lossy(&out.stdout).to_string()
            }
            _ => {
                // Fallback to smbclient (Linux)
                let result = tokio::time::timeout(
                    Duration::from_secs(10),
                    Command::new("smbclient")
                        .args(["-N", "-L", &format!("//{host}")])
                        .output(),
                )
                .await;
                match result {
                    Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
                    Ok(Err(e)) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": format!("scan failed: {e}") })),
                        )
                            .into_response();
                    }
                    Err(_) => {
                        return (
                            StatusCode::GATEWAY_TIMEOUT,
                            Json(json!({ "error": "scan timed out" })),
                        )
                            .into_response();
                    }
                }
            }
        }
    } else {
        // NFS: showmount -e host
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("showmount").args(["-e", host]).output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
            Ok(Err(e)) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("scan failed: {e}") })),
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({ "error": "scan timed out" })),
                )
                    .into_response();
            }
        }
    };

    // Parse share names from command output
    let shares: Vec<Value> = raw_output
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !trimmed.starts_with("Sharing")
                && !trimmed.starts_with("---")
                && !trimmed.starts_with("Export")
        })
        .filter_map(|line| {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.is_empty() {
                return None;
            }
            Some(json!({
                "name": parts[0],
                "type": if parts.len() > 1 { parts[1] } else { "Disk" },
                "host": host,
                "protocol": protocol,
                "path": format!("//{host}/{}", parts[0]),
            }))
        })
        .collect();

    Json(json!(shares)).into_response()
}

/// Return cached SMB shares (stub — future mDNS integration).
async fn list_smb_shares() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "SMB share discovery pending",
    }))
}

/// Trigger an SMB network scan using mDNS service discovery.
async fn trigger_smb_scan() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().ok()?;
        let receiver = daemon.browse("_smb._tcp.local.").ok()?;
        let mut shares = Vec::new();

        // Collect discoveries for 3 seconds
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    shares.push(json!({
                        "name": info.get_fullname(),
                        "host": info.get_hostname(),
                        "port": info.get_port(),
                        "addresses": info.get_addresses()
                            .iter()
                            .map(|a| a.to_string())
                            .collect::<Vec<_>>(),
                        "properties": info.get_properties()
                            .iter()
                            .map(|p| (p.key().to_string(), p.val_str().to_string()))
                            .collect::<std::collections::HashMap<_, _>>(),
                    }));
                }
                Ok(_) => {} // other events (SearchStarted, ServiceFound, etc.)
                Err(_) => {} // recv timeout, continue until deadline
            }
        }
        daemon.shutdown().ok();
        Some(shares)
    })
    .await;

    match result {
        Ok(Some(shares)) => {
            let count = shares.len();
            Json(json!({
                "status": "scan_complete",
                "shares": shares,
                "count": count,
            }))
            .into_response()
        }
        _ => Json(json!({
            "status": "scan_failed",
            "shares": [],
        }))
        .into_response(),
    }
}

/// List all stored SMB mounts from the network_mounts table.
async fn list_smb_mounts(State(state): State<AppState>) -> Json<Value> {
    let conn = state.db.connection().lock().unwrap();
    let items: Vec<Value> = conn
        .prepare(
            "SELECT id, server, share, mount_path, username, active \
             FROM network_mounts WHERE mount_type = 'smb' ORDER BY id",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                    "server": row.get::<_, Option<String>>(1).ok().flatten(),
                    "share": row.get::<_, Option<String>>(2).ok().flatten(),
                    "mount_path": row.get::<_, Option<String>>(3).ok().flatten(),
                    "username": row.get::<_, Option<String>>(4).ok().flatten(),
                    "active": row.get::<_, i32>(5).unwrap_or(1) != 0,
                }))
            })
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
        })
        .unwrap_or_default();
    drop(conn);
    Json(json!(items))
}

/// Mount an SMB share: execute the OS mount command, then persist in the database.
async fn mount_smb_share(
    State(state): State<AppState>,
    Json(body): Json<MountRequest>,
) -> impl IntoResponse {
    let share_safe = body.share_name.replace(['/', '\\', ' '], "_");
    let mount_path = body
        .mount_path
        .unwrap_or_else(|| format!("/mnt/{}_{}", body.host, share_safe));

    // Create mount directory
    if let Err(e) = tokio::fs::create_dir_all(&mount_path).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to create mount dir: {e}") })),
        )
            .into_response();
    }

    // Build the mount command depending on the platform
    let mount_result = if cfg!(target_os = "macos") {
        let credentials = match (&body.username, &body.password) {
            (Some(u), Some(p)) => format!("{u}:{p}@"),
            (Some(u), None) => format!("{u}@"),
            _ => "guest@".to_string(),
        };
        let unc = format!("//{credentials}{}/{}", body.host, body.share_name);
        tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("mount_smbfs").args([&unc, &mount_path]).output(),
        )
        .await
    } else {
        // Linux: mount.cifs
        let user = body.username.as_deref().unwrap_or("guest");
        let pass = body.password.as_deref().unwrap_or("");
        let unc = format!("//{}/{}", body.host, body.share_name);
        let opts = format!("username={user},password={pass},vers=3.0");
        tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("mount.cifs")
                .args([&unc, &mount_path, "-o", &opts])
                .output(),
        )
        .await
    };

    let mount_ok = match mount_result {
        Ok(Ok(out)) if out.status.success() => true,
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("mount failed: {stderr}") })),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("mount command failed: {e}") })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({ "error": "mount timed out" })),
            )
                .into_response();
        }
    };

    // Persist to database
    match state.db.execute(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username) VALUES (?, ?, ?, ?, ?)",
        &[
            &"smb" as &dyn rusqlite::types::ToSql,
            &body.host,
            &body.share_name,
            &mount_path,
            &body.username,
        ],
    ) {
        Ok(_) => {
            let id = state.db.last_insert_rowid();
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": id,
                    "mounted": mount_ok,
                    "mount_path": mount_path,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("db error: {e}") })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Media Server browsing / streaming
// ---------------------------------------------------------------------------

async fn browse_media_server(
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Json<Value> {
    let object_id = q.object_id.as_deref().unwrap_or("0");
    Json(json!({
        "server_id": id,
        "object_id": object_id,
        "items": [],
        "total": 0,
        "message": "UPnP ContentDirectory Browse not yet implemented",
    }))
}

#[derive(Deserialize)]
struct BrowseQuery {
    object_id: Option<String>,
}

async fn media_server_stream_url(
    Path((id, item_id)): Path<(String, String)>,
) -> Json<Value> {
    Json(json!({
        "server_id": id,
        "item_id": item_id,
        "stream_url": null,
        "message": "UPnP stream URL resolution not yet implemented",
    }))
}

async fn play_media_server_item(
    Path((id, item_id, zone_id)): Path<(String, String, i64)>,
) -> Json<Value> {
    Json(json!({
        "server_id": id,
        "item_id": item_id,
        "zone_id": zone_id,
        "status": "not_implemented",
        "message": "UPnP media server playback not yet implemented",
    }))
}

#[derive(Deserialize)]
struct TestMountRequest {
    path: String,
}

async fn test_mount(Json(body): Json<TestMountRequest>) -> impl IntoResponse {
    let path = std::path::Path::new(&body.path);
    let exists = path.exists();
    let is_dir = path.is_dir();
    let readable = if exists {
        std::fs::read_dir(path).is_ok()
    } else {
        false
    };
    let file_count = if readable {
        std::fs::read_dir(path)
            .map(|rd| rd.count())
            .unwrap_or(0)
    } else {
        0
    };

    Json(json!({
        "path": body.path,
        "exists": exists,
        "is_directory": is_dir,
        "readable": readable,
        "file_count": file_count,
    }))
}

async fn get_share_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let conn = state.db.connection().lock().unwrap();
    let result = conn.query_row(
        "SELECT id, mount_type, server, share, mount_path, username, active FROM network_mounts WHERE id = ?",
        rusqlite::params![id],
        |row| {
            Ok(json!({
                "id": row.get::<_, Option<i64>>(0).ok().flatten(),
                "mount_type": row.get::<_, Option<String>>(1).ok().flatten(),
                "server": row.get::<_, Option<String>>(2).ok().flatten(),
                "share": row.get::<_, Option<String>>(3).ok().flatten(),
                "mount_path": row.get::<_, Option<String>>(4).ok().flatten(),
                "username": row.get::<_, Option<String>>(5).ok().flatten(),
                "active": row.get::<_, i32>(6).unwrap_or(1) != 0,
            }))
        },
    );
    drop(conn);
    match result {
        Ok(val) => Json(val).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
