use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;

use crate::error::AppError;
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
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct MountRequest {
    host: String,
    share_name: String,
    username: Option<String>,
    password: Option<String>,
    mount_path: Option<String>,
    #[serde(default)]
    dry_run: bool,
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
        .route(
            "/media-servers/{id}/item/{item_id}/stream-url",
            get(media_server_stream_url),
        )
        .route(
            "/media-servers/{id}/item/{item_id}/play/{zone_id}",
            post(play_media_server_item),
        )
        .route("/mounts/test", post(test_mount))
        .route("/shares/{id}", get(get_share_detail))
}

async fn list_mounts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state.backend.query_many(
        "SELECT id, mount_type, server, share, mount_path, username, active FROM network_mounts ORDER BY id", &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "mount_type": r.get(1).and_then(|v| v.as_string()),
                "server": r.get(2).and_then(|v| v.as_string()),
                "share": r.get(3).and_then(|v| v.as_string()),
                "mount_path": r.get(4).and_then(|v| v.as_string()),
                "username": r.get(5).and_then(|v| v.as_string()),
                "active": r.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

async fn create_mount(
    State(state): State<AppState>,
    Json(body): Json<CreateMount>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let mount_type = body.mount_type.unwrap_or_else(|| "smb".into());
    match state.backend.execute_returning_id(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password) VALUES (?, ?, ?, ?, ?, ?)",
        &[&mount_type as &dyn ToSqlValue, &body.server as &dyn ToSqlValue, &body.share as &dyn ToSqlValue, &body.mount_path as &dyn ToSqlValue, &body.username as &dyn ToSqlValue, &body.password as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_mount(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    state
        .backend
        .execute(
            &format!("DELETE FROM network_mounts WHERE id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}

async fn list_media_servers(State(state): State<AppState>) -> Json<Value> {
    let servers = state.media_servers.lock().await;
    let items: Vec<Value> = servers
        .values()
        .map(|ms| {
            json!({
                "id": ms.id,
                "name": ms.name,
                "manufacturer": ms.manufacturer,
                "model": ms.model,
                "host": ms.host,
                "port": ms.port,
                "location": ms.location,
            })
        })
        .collect();
    let total = items.len();
    Json(json!({
        "items": items,
        "total": total,
    }))
}

// ---------------------------------------------------------------------------
// SMB discovery and mount management
// ---------------------------------------------------------------------------

/// Discover network shares via mDNS service browsing (_smb._tcp).
async fn list_shares() -> Json<Value> {
    let result = tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().ok()?;
        let receiver = daemon.browse("_smb._tcp.local.").ok()?;
        let mut shares = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    let host = info.get_hostname().trim_end_matches('.').to_string();
                    let addrs: Vec<String> = info
                        .get_addresses()
                        .iter()
                        .map(|a| a.to_ip_addr().to_string())
                        .collect();
                    let ip = addrs.first().cloned().unwrap_or_default();
                    let name = info
                        .get_fullname()
                        .split("._smb._tcp")
                        .next()
                        .unwrap_or(&host)
                        .to_string();
                    let key = format!("{}:{}", ip, info.get_port());
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key);
                    shares.push(json!({
                        "id": format!("smb://{}", ip),
                        "name": name,
                        "host": if ip.is_empty() { host.clone() } else { ip },
                        "hostname": host,
                        "port": info.get_port(),
                        "protocol": "smb",
                        "available": true,
                    }));
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        daemon.shutdown().ok();
        Some(shares)
    })
    .await;

    match result {
        Ok(Some(shares)) => Json(json!(shares)),
        _ => Json(json!([])),
    }
}

/// Scan a specific host for SMB or NFS shares.
async fn scan_host(
    headers: axum::http::HeaderMap,
    Query(q): Query<ScanHostQuery>,
) -> impl IntoResponse {
    let lang = crate::i18n::lang_from_header(&headers);
    let host = &q.host;
    let protocol = q.protocol.as_deref().unwrap_or("smb");

    let raw_output = if protocol == "smb" {
        // Platform-specific SMB share enumeration
        let mut output = String::new();
        let mut success = false;
        let mut last_error = String::new();

        // Windows: net view \\host
        if !success {
            if let Ok(Ok(out)) = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("net")
                    .args(["view", &format!("\\\\{host}")])
                    .output(),
            )
            .await
            {
                if out.status.success() {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                    success = true;
                } else {
                    last_error = String::from_utf8_lossy(&out.stderr).to_string();
                }
            }
        }

        // macOS: smbutil view
        if !success {
            let smb_user = q.username.as_deref().unwrap_or("guest");
            let smb_url = if let Some(ref pw) = q.password {
                if !pw.is_empty() {
                    format!("//{}:{}@{}", smb_user, pw, host)
                } else {
                    format!("//{}@{}", smb_user, host)
                }
            } else {
                format!("//{}@{}", smb_user, host)
            };
            if let Ok(Ok(out)) = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("smbutil").args(["view", &smb_url]).output(),
            )
            .await
            {
                if out.status.success() {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                    success = true;
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    if !stdout.trim().is_empty() {
                        output = stdout;
                        success = true;
                    } else {
                        last_error = stderr;
                    }
                }
            }
        }

        // Linux: smbclient -N -L
        if !success {
            let mut smb_args = vec!["-L".to_string(), format!("//{host}")];
            if let Some(ref user) = q.username {
                smb_args.push("-U".to_string());
                if let Some(ref pw) = q.password {
                    if !pw.is_empty() {
                        smb_args.push(format!("{}%{}", user, pw));
                    } else {
                        smb_args.push(user.clone());
                        smb_args.push("-N".to_string());
                    }
                } else {
                    smb_args.push(user.clone());
                    smb_args.push("-N".to_string());
                }
            } else {
                smb_args.push("-N".to_string());
            }
            match tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("smbclient").args(&smb_args).output(),
            )
            .await
            {
                Ok(Ok(out)) => {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                }
                Ok(Err(e)) => {
                    // smbclient not available — use last_error from previous tools
                    tracing::warn!(host = %host, error = %e, "network_smb_smbclient_spawn_failed (smbclient not installed?)");
                }
                Err(_) => {
                    tracing::warn!(host = %host, "network_smb_scan_timed_out (smbclient -L)");
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        Json(json!({ "error": "scan timed out" })),
                    )
                        .into_response();
                }
            }
        }

        if output.trim().is_empty() && !last_error.is_empty() {
            tracing::warn!(host = %host, error = %last_error.trim(), "network_smb_scan_failed");
            let msg = if last_error.contains("Authentication")
                || last_error.contains("auth")
                || last_error.contains("STATUS_ACCESS_DENIED")
            {
                crate::i18n::t(&lang, "net.smbAccessDenied").replace("{error}", &last_error)
            } else {
                crate::i18n::t(&lang, "net.smbScanFailed")
                    .replace("{host}", host)
                    .replace("{error}", &last_error)
            };
            return (StatusCode::OK, Json(json!({ "shares": [], "error": msg }))).into_response();
        }

        output
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
                tracing::warn!(host = %host, error = %e, "network_nfs_showmount_spawn_failed (showmount not installed?)");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("scan failed: {e}") })),
                )
                    .into_response();
            }
            Err(_) => {
                tracing::warn!(host = %host, "network_nfs_scan_timed_out (showmount -e)");
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({ "error": "scan timed out" })),
                )
                    .into_response();
            }
        }
    };

    // Parse share names from command output.
    let shares: Vec<Value> = if protocol == "smb" {
        // smbclient -L / smbutil view / net view all print a "Sharename Type
        // Comment" table where column 2 is the share TYPE (Disk / Printer /
        // IPC). Keying on that type is far more robust than a prefix filter:
        // the previous filter tested "Sharing" (typo — the header word is
        // "Sharename") so the header was never dropped, and every non-empty
        // line — the header, the `----` rule, client-side Kerberos warnings,
        // `mkdir failed on /var/lib/samba/lock`, `SMB1 disabled…`, the second
        // Server/Workgroup table — was emitted as a bogus "share" (Dominique,
        // Fedora). Only rows whose 2nd column is a real file/printer share type
        // survive; IPC$ (admin share, never a music source) is dropped too.
        raw_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 2 {
                    return None;
                }
                let name = parts[0];
                let stype = parts[1];
                let is_share_type = stype.eq_ignore_ascii_case("Disk")
                    || stype.eq_ignore_ascii_case("Printer")
                    || stype.eq_ignore_ascii_case("Print");
                // Skip admin/hidden shares ($-suffixed: IPC$, ADMIN$, C$, …).
                if !is_share_type || name.ends_with('$') {
                    return None;
                }
                Some(json!({
                    "name": name,
                    "type": stype,
                    "host": host,
                    "protocol": protocol,
                    "path": format!("//{host}/{name}"),
                }))
            })
            .collect()
    } else {
        // NFS `showmount -e host`: "Export list for host:" header then
        // "/export/path  clients" rows — the export path is column 1.
        raw_output
            .lines()
            .filter(|line| {
                let t = line.trim();
                !t.is_empty() && !t.starts_with("Export") && !t.starts_with("---")
            })
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let name = *parts.first()?;
                Some(json!({
                    "name": name,
                    "type": "NFS",
                    "host": host,
                    "protocol": protocol,
                    "path": format!("{host}:{name}"),
                }))
            })
            .collect()
    };

    tracing::info!(
        host = %host,
        protocol,
        shares = shares.len(),
        "network_scan_host_complete"
    );
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
                            .map(|a| a.to_ip_addr().to_string())
                            .collect::<Vec<_>>(),
                        "properties": info.get_properties()
                            .iter()
                            .map(|p| (p.key().to_string(), p.val_str().to_string()))
                            .collect::<std::collections::HashMap<_, _>>(),
                    }));
                }
                Ok(_) => {}  // other events (SearchStarted, ServiceFound, etc.)
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
async fn list_smb_mounts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, server, share, mount_path, username, active \
             FROM network_mounts WHERE mount_type = 'smb' ORDER BY id",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "server": r.get(1).and_then(|v| v.as_string()),
                "share": r.get(2).and_then(|v| v.as_string()),
                "mount_path": r.get(3).and_then(|v| v.as_string()),
                "username": r.get(4).and_then(|v| v.as_string()),
                "active": r.get(5).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
            })
        })
        .collect();
    Ok(Json(json!(items)))
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

    // Dry run: just test reachability without mounting
    if body.dry_run {
        let reachable = tokio::net::TcpStream::connect(format!("{}:445", body.host))
            .await
            .is_ok();
        return Json(json!({
            "ok": reachable,
            "host": body.host,
            "share_name": body.share_name,
            "message": if reachable { "Host reachable on SMB port 445" } else { "Cannot reach host on port 445" },
        }))
        .into_response();
    }

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
            Command::new("mount_smbfs")
                .args([&unc, &mount_path])
                .output(),
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
    use tune_core::db::backend::ToSqlValue;
    match state.backend.execute_returning_id(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username) VALUES (?, ?, ?, ?, ?)",
        &[&"smb" as &dyn ToSqlValue, &body.host as &dyn ToSqlValue, &body.share_name as &dyn ToSqlValue, &mount_path as &dyn ToSqlValue, &body.username as &dyn ToSqlValue],
    ) {
        Ok(id) => {
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
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Json<Value> {
    let object_id = q.object_id.as_deref().unwrap_or("0");
    let servers = state.media_servers.lock().await;
    let ms = match servers.get(&id) {
        Some(ms) => ms.clone(),
        None => {
            return Json(json!({
                "object_id": object_id,
                "containers": [],
                "items": [],
                "total_matches": 0,
                "number_returned": 0,
            }));
        }
    };
    drop(servers);

    // UPnP Browse returns results in PAGES. The old code issued a single
    // Browse with RequestedCount=200 and returned only that page, so a server
    // with thousands of albums showed just its first page (~100 on MinimServer /
    // Twonky / Asset, which cap a single response) — "le résumé est juste mais la
    // liste est très incomplète (~100 sur x xxx)" (Pierre M). Loop over
    // StartingIndex, accumulating children until NumberReturned==0 or
    // StartingIndex>=TotalMatches, with a safety bound.
    const PAGE_SIZE: u32 = 200;
    const MAX_PAGES: u32 = 500; // up to 100k children
    let client = reqwest::Client::new();
    let mut containers: Vec<Value> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    let mut starting_index: u32 = 0;
    let mut total_matches: u32 = 0;

    for _page in 0..MAX_PAGES {
        let soap_body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ObjectID>{object_id}</ObjectID>
<BrowseFlag>BrowseDirectChildren</BrowseFlag>
<Filter>*</Filter>
<StartingIndex>{starting_index}</StartingIndex>
<RequestedCount>{PAGE_SIZE}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Browse>
</s:Body>
</s:Envelope>"#
        );

        let resp = match client
            .post(&ms.content_directory_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .body(soap_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "browse_media_server soap_error server={} start={starting_index} err={e}",
                    ms.name
                );
                break;
            }
        };

        let body = resp.text().await.unwrap_or_default();
        let (mut page_containers, mut page_items) = parse_didl_browse_response(&body);
        let parsed = (page_containers.len() + page_items.len()) as u32;

        // NumberReturned / TotalMatches are un-escaped siblings of the escaped
        // DIDL <Result> in the SOAP body — no collision with the payload.
        let number_returned: u32 = extract_xml_tag(&body, "NumberReturned")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(parsed);
        if let Some(tm) = extract_xml_tag(&body, "TotalMatches").and_then(|s| s.trim().parse().ok())
        {
            total_matches = tm;
        }

        containers.append(&mut page_containers);
        items.append(&mut page_items);

        if number_returned == 0 || parsed == 0 {
            break;
        }
        // Advance by what the server actually returned (robust against servers
        // that page smaller than RequestedCount).
        starting_index += number_returned.max(parsed);
        if total_matches != 0 && starting_index >= total_matches {
            break;
        }
    }

    let fetched = containers.len() + items.len();
    let total = (total_matches as usize).max(fetched);

    Json(json!({
        "object_id": object_id,
        "containers": containers,
        "items": items,
        "total_matches": total,
        "number_returned": fetched,
    }))
}

fn parse_didl_browse_response(xml: &str) -> (Vec<Value>, Vec<Value>) {
    let result_start = xml.find("<Result>").or_else(|| xml.find("<Result "));
    let result_end = xml.find("</Result>");
    let didl = match (result_start, result_end) {
        (Some(s), Some(e)) => {
            let after = &xml[s..];
            let content_start = after.find('>').map(|i| s + i + 1).unwrap_or(s);
            &xml[content_start..e]
        }
        _ => return (vec![], vec![]),
    };
    let decoded = didl
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");

    let mut containers = Vec::new();
    let mut items = Vec::new();

    for tag in ["container", "item"] {
        let open = format!("<{tag} ");
        let close = format!("</{tag}>");
        let mut pos = 0;
        while let Some(start) = decoded[pos..].find(&open) {
            let abs_start = pos + start;
            if let Some(end) = decoded[abs_start..].find(&close) {
                let element = &decoded[abs_start..abs_start + end + close.len()];
                let id = extract_attr(element, "id").unwrap_or_default();
                let parent_id = extract_attr(element, "parentID").unwrap_or_default();
                let title = extract_xml_tag(element, "dc:title").unwrap_or_default();
                let album_art_uri = extract_xml_tag(element, "upnp:albumArtURI");
                let artist = extract_xml_tag(element, "upnp:artist")
                    .or_else(|| extract_xml_tag(element, "dc:creator"));

                if tag == "container" {
                    let child_count: u32 = extract_attr(element, "childCount")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    containers.push(json!({
                        "id": id,
                        "parent_id": parent_id,
                        "title": title,
                        "child_count": child_count,
                        "album_art_uri": album_art_uri,
                    }));
                } else {
                    let album = extract_xml_tag(element, "upnp:album");
                    // A server may announce SEVERAL <res> per item — Lyrion/LMS
                    // lists the original file (download.flc, with duration) plus
                    // on-the-fly transcodes (download.pcm headerless raw PCM with
                    // duration 0:00, download.mp3, …). The old code took the FIRST
                    // <res> blindly, so whenever the raw-PCM transcode came first
                    // the DLNA renderer was handed an unplayable headerless stream
                    // (Yacine: immediate failure, 0:00, replay loop). Pick the best
                    // resource instead; single-res items are untouched.
                    let resources = parse_res_elements(element);
                    let best = select_best_res(&resources);
                    let res_url = best.map(|r| r.url.clone());
                    // Real resolution + codec from the CHOSEN res@ attributes.
                    // Without these the signal path defaulted to "AAC 44kHz/16bit —
                    // Avec perte", mislabelling a hi-res ALAC (audio/mp4) as lossy AAC
                    // (Yves: NAS ALAC shown as AAC while the DartZeel read 24-bit).
                    let duration_ms = best.and_then(|r| r.duration_ms);
                    let sample_rate = best.and_then(|r| r.sample_rate);
                    let bit_depth = best.and_then(|r| r.bit_depth);
                    let channels = best.and_then(|r| r.channels);
                    let protocol_info = best.and_then(|r| r.protocol_info.clone());
                    items.push(json!({
                        "id": id,
                        "title": title,
                        "artist": artist,
                        "album": album,
                        "res_url": res_url,
                        "album_art_uri": album_art_uri,
                        "duration_ms": duration_ms,
                        "sample_rate": sample_rate,
                        "bit_depth": bit_depth,
                        "channels": channels,
                        "protocol_info": protocol_info,
                    }));
                }

                pos = abs_start + end + close.len();
            } else {
                break;
            }
        }
    }
    (containers, items)
}

/// One `<res>` element of a DIDL-Lite item.
#[derive(Debug, Clone)]
struct DidlRes {
    url: String,
    protocol_info: Option<String>,
    duration_ms: Option<u64>,
    sample_rate: Option<u32>,
    bit_depth: Option<u16>,
    channels: Option<u16>,
}

/// Parse every `<res …>url</res>` of a DIDL item, in document order.
fn parse_res_elements(element: &str) -> Vec<DidlRes> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(start) = element[pos..].find("<res") {
        let abs = pos + start;
        // Only match the actual <res> tag ("<res " / "<res>"), not e.g. <resType>.
        let after = &element[abs + 4..];
        if !(after.starts_with(' ') || after.starts_with('>')) {
            pos = abs + 4;
            continue;
        }
        let Some(tag_end_rel) = element[abs..].find('>') else {
            break;
        };
        let tag_end = abs + tag_end_rel;
        let res_tag = &element[abs..tag_end];
        let Some(close_rel) = element[tag_end..].find("</res>") else {
            break;
        };
        let url = element[tag_end + 1..tag_end + close_rel].trim().to_string();
        if !url.is_empty() {
            out.push(DidlRes {
                url,
                protocol_info: extract_attr(res_tag, "protocolInfo"),
                duration_ms: extract_attr(res_tag, "duration")
                    .and_then(|d| parse_upnp_duration(&d)),
                sample_rate: extract_attr(res_tag, "sampleFrequency")
                    .and_then(|s| s.parse::<u32>().ok()),
                bit_depth: extract_attr(res_tag, "bitsPerSample")
                    .and_then(|s| s.parse::<u16>().ok()),
                channels: extract_attr(res_tag, "nrAudioChannels")
                    .and_then(|s| s.parse::<u16>().ok()),
            });
        }
        pos = tag_end + close_rel + "</res>".len();
    }
    out
}

/// Format-preference rank for a `<res>` — lower is better.
///
/// 0 = original/lossless WITH headers (flac/flc, alac/m4a, wav, aiff)
/// 1 = encapsulated lossy (mp3, aac, ogg/opus, wma)
/// 2 = unknown audio format
/// 3 = raw headerless PCM (audio/L16, audio/L24, LPCM, .pcm) — LMS announces
///     these transcodes with duration 0:00 and DLNA renderers choke on them
/// 4 = non-audio res (cover images some servers attach as extra <res>)
fn res_format_rank(res: &DidlRes) -> u8 {
    let path = res
        .url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    // protocolInfo = "http-get:*:<mime>:<extra>"
    let mime = res
        .protocol_info
        .as_deref()
        .and_then(|p| p.split(':').nth(2))
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if mime.starts_with("image/") || mime.starts_with("video/") {
        return 4;
    }
    if mime.starts_with("audio/l16")
        || mime.starts_with("audio/l24")
        || mime.contains("lpcm")
        || path.ends_with(".pcm")
    {
        return 3;
    }
    const LOSSLESS_EXT: [&str; 8] = [
        ".flac", ".flc", ".m4a", ".mp4", ".alac", ".wav", ".aif", ".aiff",
    ];
    const LOSSLESS_MIME: [&str; 11] = [
        "audio/flac",
        "audio/x-flac",
        "audio/mp4",
        "audio/m4a",
        "audio/x-m4a",
        "audio/wav",
        "audio/x-wav",
        "audio/wave",
        "audio/aiff",
        "audio/x-aiff",
        "audio/x-aif",
    ];
    if LOSSLESS_EXT.iter().any(|e| path.ends_with(e)) || LOSSLESS_MIME.contains(&mime.as_str()) {
        return 0;
    }
    const LOSSY_EXT: [&str; 6] = [".mp3", ".aac", ".ogg", ".oga", ".opus", ".wma"];
    const LOSSY_MIME: [&str; 9] = [
        "audio/mpeg",
        "audio/mp3",
        "audio/aac",
        "audio/x-aac",
        "audio/ogg",
        "audio/x-ogg",
        "application/ogg",
        "audio/opus",
        "audio/x-ms-wma",
    ];
    if LOSSY_EXT.iter().any(|e| path.ends_with(e)) || LOSSY_MIME.contains(&mime.as_str()) {
        return 1;
    }
    2
}

/// Pick the best `<res>` of an item: by format rank, then prefer a resource
/// with a non-zero `duration` attribute, then keep document order. Items that
/// announce a single res keep it unconditionally (behaviour unchanged for
/// servers that only expose one resource, even raw PCM).
fn select_best_res(resources: &[DidlRes]) -> Option<&DidlRes> {
    if resources.len() <= 1 {
        return resources.first();
    }
    resources
        .iter()
        .enumerate()
        .min_by_key(|(i, r)| {
            (
                res_format_rank(r),
                u8::from(r.duration_ms.unwrap_or(0) == 0),
                *i,
            )
        })
        .map(|(_, r)| r)
}

fn parse_upnp_duration(d: &str) -> Option<u64> {
    let parts: Vec<&str> = d.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().ok()?;
        let m: f64 = parts[1].parse().ok()?;
        let s: f64 = parts[2].parse().ok()?;
        Some((h * 3_600_000.0 + m * 60_000.0 + s * 1_000.0) as u64)
    } else if parts.len() == 2 {
        let m: f64 = parts[0].parse().ok()?;
        let s: f64 = parts[1].parse().ok()?;
        Some((m * 60_000.0 + s * 1_000.0) as u64)
    } else {
        None
    }
}

fn extract_attr(element: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=\"");
    let start = element.find(&pattern)? + pattern.len();
    let end = element[start..].find('"')? + start;
    Some(element[start..end].to_string())
}

fn extract_xml_tag(element: &str, tag: &str) -> Option<String> {
    let open_full = format!("<{tag}>");
    let open_attr = format!("<{tag} ");
    let close = format!("</{tag}>");
    let content_start = if let Some(s) = element.find(&open_full) {
        s + open_full.len()
    } else if let Some(s) = element.find(&open_attr) {
        let after = &element[s..];
        after.find('>')? + s + 1
    } else {
        return None;
    };
    let content_end = element[content_start..].find(&close)? + content_start;
    Some(element[content_start..content_end].to_string())
}

#[derive(Deserialize)]
struct BrowseQuery {
    object_id: Option<String>,
}

async fn media_server_stream_url(Path((id, item_id)): Path<(String, String)>) -> Json<Value> {
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
        std::fs::read_dir(path).map(|rd| rd.count()).unwrap_or(0)
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
) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    let result = state.backend.query_one(
        &format!(
            "SELECT id, mount_type, server, share, mount_path, username, active \
             FROM network_mounts WHERE id = {p1}"
        ),
        &[&id as &dyn ToSqlValue],
    );
    match result {
        Ok(Some(r)) => Ok(Json(json!({
            "id": r.get(0).and_then(|v| v.as_i64()),
            "mount_type": r.get(1).and_then(|v| v.as_string()),
            "server": r.get(2).and_then(|v| v.as_string()),
            "share": r.get(3).and_then(|v| v.as_string()),
            "mount_path": r.get(4).and_then(|v| v.as_string()),
            "username": r.get(5).and_then(|v| v.as_string()),
            "active": r.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        }))
        .into_response()),
        Ok(None) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(_) => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_didl_browse_response, parse_res_elements, select_best_res};

    /// Build a SOAP Browse response whose escaped DIDL contains one item with
    /// the given raw `<res>` elements (LMS-style).
    fn soap_with_res(res_elements: &str) -> String {
        let didl = format!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="t1" parentID="a1" restricted="1"><dc:title>Track</dc:title><upnp:artist>Artist</upnp:artist><upnp:album>Album</upnp:album>{res_elements}</item></DIDL-Lite>"#
        );
        let escaped = didl
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        format!(
            "<s:Envelope><s:Body><u:BrowseResponse><Result>{escaped}</Result><NumberReturned>1</NumberReturned><TotalMatches>1</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>"
        )
    }

    // Yacine's case: LMS announces the headerless raw-PCM transcode FIRST
    // (duration 0:00), then the original FLAC (with duration), then an MP3
    // transcode. The FLAC must win.
    const LMS_MULTI_RES: &str = concat!(
        r#"<res protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM" duration="0:00:00">http://192.168.1.7:9000/music/123/download.pcm</res>"#,
        r#"<res protocolInfo="http-get:*:audio/x-flac:*" duration="0:04:33.000" sampleFrequency="44100" bitsPerSample="16" nrAudioChannels="2">http://192.168.1.7:9000/music/123/download.flc</res>"#,
        r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:04:33.000">http://192.168.1.7:9000/music/123/download.mp3</res>"#,
    );

    #[test]
    fn multi_res_lms_prefers_flac_over_raw_pcm() {
        let resources = parse_res_elements(LMS_MULTI_RES);
        assert_eq!(resources.len(), 3);
        let best = select_best_res(&resources).expect("a res must be selected");
        assert!(
            best.url.ends_with("download.flc"),
            "expected the FLAC original, got {}",
            best.url
        );
        assert_eq!(best.duration_ms, Some(273_000));
        assert_eq!(best.sample_rate, Some(44100));
        assert_eq!(best.bit_depth, Some(16));
        assert_eq!(
            best.protocol_info.as_deref(),
            Some("http-get:*:audio/x-flac:*")
        );
    }

    #[test]
    fn multi_res_end_to_end_didl_parse_picks_flac() {
        let soap = soap_with_res(LMS_MULTI_RES);
        let (containers, items) = parse_didl_browse_response(&soap);
        assert!(containers.is_empty());
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item["res_url"].as_str().unwrap(),
            "http://192.168.1.7:9000/music/123/download.flc"
        );
        // duration/resolution must come from the CHOSEN res, not the first one
        assert_eq!(item["duration_ms"].as_u64(), Some(273_000));
        assert_eq!(item["sample_rate"].as_u64(), Some(44100));
        assert_eq!(
            item["protocol_info"].as_str(),
            Some("http-get:*:audio/x-flac:*")
        );
    }

    #[test]
    fn single_res_pcm_is_kept() {
        // A server that only announces raw PCM must keep working as before.
        let element = r#"<res protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:*">http://10.0.0.2:9000/music/9/download.pcm</res>"#;
        let resources = parse_res_elements(element);
        assert_eq!(resources.len(), 1);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("download.pcm"));
    }

    #[test]
    fn only_lossy_res_picks_mp3() {
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/L16:*" duration="0:00:00">http://h:9000/music/5/download.pcm</res>"#,
            r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:03:10.000">http://h:9000/music/5/download.mp3</res>"#,
        );
        let resources = parse_res_elements(element);
        assert_eq!(resources.len(), 2);
        let best = select_best_res(&resources).unwrap();
        assert!(
            best.url.ends_with("download.mp3"),
            "mp3 must beat raw pcm, got {}",
            best.url
        );
        assert_eq!(best.duration_ms, Some(190_000));
    }

    #[test]
    fn equal_format_prefers_res_with_duration() {
        // Same rank (both FLAC): the one with a real duration wins even if
        // listed second.
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/flac:*">http://h/1/nodur.flac</res>"#,
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/dur.flac</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("dur.flac"));
    }

    #[test]
    fn equal_format_and_duration_keeps_document_order() {
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/first.flac</res>"#,
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/second.flac</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("first.flac"));
    }

    #[test]
    fn image_res_never_beats_audio() {
        // Some servers attach the cover as an extra <res>.
        let element = concat!(
            r#"<res protocolInfo="http-get:*:image/jpeg:*">http://h/cover.jpg</res>"#,
            r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:03:00.000">http://h/track.mp3</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("track.mp3"));
    }
}
