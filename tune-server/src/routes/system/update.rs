use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tracing::{error, info, warn};

use tune_core::updater::{ReleaseAsset, ReleaseInfo, UpdateChecker};

use crate::state::AppState;

/// Find the extractable archive asset (tar.gz or zip) for the current platform.
/// Excludes .dmg and .exe installers — we want the raw archive containing the binary + web/.
fn find_archive_asset(release: &ReleaseInfo) -> Option<&ReleaseAsset> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    release.assets.iter().find(|a| {
        let name = a.name.to_lowercase();

        // Must be an archive, not an installer
        let is_archive = name.ends_with(".tar.gz") || name.ends_with(".zip");
        if !is_archive {
            return false;
        }

        // Exclude installer-only files
        if name.contains("setup") || name.contains("installer") {
            return false;
        }

        let os_match = match os {
            "macos" => name.contains("macos"),
            "linux" => name.contains("linux"),
            "windows" => name.contains("windows"),
            _ => false,
        };
        let arch_match = match arch {
            "aarch64" => name.contains("aarch64") || name.contains("arm64"),
            "x86_64" => name.contains("x86_64") || name.contains("amd64"),
            _ => true,
        };
        os_match && arch_match
    })
}

/// GET /system/update/check
///
/// Fetches the latest release from GitHub, compares versions, and returns update info.
pub(super) async fn update_check() -> Json<Value> {
    let checker = UpdateChecker::new();
    let current = tune_core::version();

    match checker.check().await {
        Ok(Some(release)) => {
            let asset = find_archive_asset(&release);
            Json(json!({
                "current": current,
                "latest": release.version,
                "update_available": true,
                "download_url": asset.map(|a| &a.browser_download_url),
                "asset_name": asset.map(|a| &a.name),
                "release_notes": release.body,
                "size_bytes": asset.map(|a| a.size).unwrap_or(0),
                "html_url": release.html_url,
                "published_at": release.published_at,
            }))
        }
        Ok(None) => Json(json!({
            "current": current,
            "latest": current,
            "update_available": false,
            "download_url": null,
            "release_notes": null,
            "size_bytes": 0,
        })),
        Err(e) => {
            warn!(error = %e, "update_check_failed");
            Json(json!({
                "current": current,
                "latest": null,
                "update_available": false,
                "error": e,
            }))
        }
    }
}

/// POST /system/update/install
///
/// Downloads the latest release archive, extracts the binary and web/ directory,
/// replaces the current binary, and restarts the server.
pub(super) async fn update_install(State(state): State<AppState>) -> impl IntoResponse {
    // 1. Check for update
    let checker = UpdateChecker::new();
    let release = match checker.check().await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(json!({"status": "up_to_date", "message": "Already running the latest version"})),
            ).into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"status": "error", "message": format!("Failed to check for updates: {e}")})),
            ).into_response();
        }
    };

    let asset = match find_archive_asset(&release) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"status": "error", "message": "No compatible archive found for this platform"})),
            ).into_response();
        }
    };

    info!(
        version = %release.version,
        asset = %asset.name,
        size = asset.size,
        "update_download_starting"
    );

    // 2. Download the archive
    let client = &state.http_client;
    let resp = match client
        .get(&asset.browser_download_url)
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            let status = r.status();
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"status": "error", "message": format!("Download failed: HTTP {status}")})),
            ).into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"status": "error", "message": format!("Download failed: {e}")})),
            )
                .into_response();
        }
    };

    let archive_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({"status": "error", "message": format!("Failed to read download: {e}")}),
                ),
            )
                .into_response();
        }
    };

    info!(size = archive_bytes.len(), "update_downloaded");

    // 3. Extract to a temp directory
    let tmp_dir = std::env::temp_dir().join(format!("tune-update-{}", release.version));
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
    if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": format!("Failed to create temp dir: {e}")})),
        )
            .into_response();
    }

    let is_zip = asset.name.to_lowercase().ends_with(".zip");
    if let Err(e) = extract_archive(&archive_bytes, &tmp_dir, is_zip) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": format!("Extraction failed: {e}")})),
        )
            .into_response();
    }

    info!(dir = %tmp_dir.display(), "update_extracted");

    // 4. Find the extracted binary
    let binary_name = if cfg!(windows) {
        "tune-server.exe"
    } else {
        "tune-server"
    };
    let new_binary = tmp_dir.join(binary_name);
    if !new_binary.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": format!("Binary '{}' not found in archive", binary_name)})),
        ).into_response();
    }

    // 5. Replace binary and web/ directory
    let current_exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"status": "error", "message": format!("Cannot determine current exe: {e}")})),
            ).into_response();
        }
    };

    if cfg!(windows) {
        if let Err(e) = install_windows(&current_exe, &new_binary, &tmp_dir) {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"status": "error", "message": format!("Windows install failed: {e}")})),
            )
                .into_response();
        }
    } else if let Err(e) = install_unix(&current_exe, &new_binary, &tmp_dir) {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": format!("Install failed: {e}")})),
        )
            .into_response();
    }

    info!(
        from = %tune_core::version(),
        to = %release.version,
        "update_installed"
    );

    // 6. Schedule restart (small delay so the HTTP response reaches the client)
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        info!("update_restarting");
        std::process::exit(0);
    });

    Json(json!({
        "status": "installed",
        "previous_version": tune_core::version(),
        "new_version": release.version,
        "message": "Update installed, server restarting...",
    }))
    .into_response()
}

/// Extract a tar.gz or zip archive to the given directory.
fn extract_archive(data: &[u8], dest: &std::path::Path, is_zip: bool) -> Result<(), String> {
    if is_zip {
        extract_zip(data, dest)
    } else {
        extract_tar_gz(data, dest)
    }
}

fn extract_tar_gz(data: &[u8], dest: &std::path::Path) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(data);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|e| format!("tar extraction: {e}"))
}

fn extract_zip(data: &[u8], dest: &std::path::Path) -> Result<(), String> {
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("zip open: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;

        let out_path = match file.enclosed_name() {
            Some(p) => dest.join(p),
            None => continue,
        };

        if file.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("mkdir {}: {e}", out_path.display()))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            let mut out_file = std::fs::File::create(&out_path)
                .map_err(|e| format!("create {}: {e}", out_path.display()))?;
            std::io::copy(&mut file, &mut out_file)
                .map_err(|e| format!("write {}: {e}", out_path.display()))?;
        }
    }

    #[cfg(unix)]
    {
        let binary = dest.join("tune-server");
        if binary.exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).ok();
        }
    }

    Ok(())
}

/// Unix install: rename current binary to .old, put new one in place, update web/.
fn install_unix(
    current_exe: &std::path::Path,
    new_binary: &std::path::Path,
    tmp_dir: &std::path::Path,
) -> Result<(), String> {
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| "Cannot determine binary directory".to_string())?;

    let old_exe = current_exe.with_extension("old");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new_binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod: {e}"))?;
    }

    let staging = current_exe.with_extension("new");
    std::fs::copy(new_binary, &staging).map_err(|e| format!("copy new binary: {e}"))?;

    if old_exe.exists() {
        std::fs::remove_file(&old_exe).ok();
    }
    std::fs::rename(current_exe, &old_exe).map_err(|e| format!("rename current to .old: {e}"))?;

    if let Err(e) = std::fs::rename(&staging, current_exe) {
        error!(error = %e, "rename_new_to_current_failed, rolling back");
        std::fs::rename(&old_exe, current_exe).ok();
        return Err(format!("rename .new to current: {e}"));
    }

    update_web_dir(exe_dir, tmp_dir)?;

    Ok(())
}

/// Windows install: write a bat script that replaces the binary after exit.
fn install_windows(
    current_exe: &std::path::Path,
    new_binary: &std::path::Path,
    tmp_dir: &std::path::Path,
) -> Result<(), String> {
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| "Cannot determine binary directory".to_string())?;

    let new_staging = current_exe.with_extension("new.exe");
    std::fs::copy(new_binary, &new_staging).map_err(|e| format!("copy new binary: {e}"))?;

    update_web_dir(exe_dir, tmp_dir)?;

    let bat_path = exe_dir.join("tune-update.bat");
    let bat_content = format!(
        "@echo off\r\necho Waiting for server to stop...\r\ntimeout /t 2 /nobreak >nul\r\ndel \"{exe}\"\r\nrename \"{new}\" \"{exe_name}\"\r\necho Starting updated server...\r\nstart \"\" \"{exe}\"\r\ndel \"%~f0\"\r\n",
        exe = current_exe.display(),
        new = new_staging.display(),
        exe_name = current_exe
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
    );

    std::fs::write(&bat_path, bat_content).map_err(|e| format!("write update.bat: {e}"))?;

    std::process::Command::new("cmd")
        .args(["/C", "start", "/min", "", &bat_path.to_string_lossy()])
        .spawn()
        .map_err(|e| format!("launch update.bat: {e}"))?;

    Ok(())
}

/// Replace the web/ directory next to the binary with the one from the archive.
fn update_web_dir(exe_dir: &std::path::Path, tmp_dir: &std::path::Path) -> Result<(), String> {
    let new_web = tmp_dir.join("web");
    if !new_web.exists() {
        info!("no web/ directory in archive, skipping web update");
        return Ok(());
    }

    let target_web = exe_dir.join("web");

    if target_web.exists() {
        std::fs::remove_dir_all(&target_web).map_err(|e| format!("remove old web/: {e}"))?;
    }
    copy_dir_all(&new_web, &target_web).map_err(|e| format!("copy new web/: {e}"))?;

    info!(dir = %target_web.display(), "web_directory_updated");
    Ok(())
}

/// Recursively copy a directory.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// GET /system/update/status
pub(super) async fn update_status(State(_state): State<AppState>) -> Json<Value> {
    let update_exists = std::env::temp_dir()
        .read_dir()
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.file_name().to_string_lossy().starts_with("tune-update-"))
        })
        .unwrap_or(false);

    Json(json!({
        "current_version": tune_core::version(),
        "update_pending": update_exists,
    }))
}

/// POST /system/update/apply — kept for backward compatibility.
pub(super) async fn update_apply() -> impl IntoResponse {
    Json(json!({
        "status": "deprecated",
        "message": "Use POST /system/update/install instead",
    }))
}

/// GET /system/changelog
pub(super) async fn changelog() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "entries": [
            {
                "version": "0.8.15",
                "date": "2026-06-01",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Zones = 0 dans le dashboard",
                        "Gapless DLNA triple fix",
                        "WAV Content-Length fix",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Credits Now Playing",
                        "Windows crash log",
                        "MockOutput test infra",
                    ]},
                ]
            },
            {
                "version": "0.8.28",
                "date": "2026-06-03",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Zone creation race condition fix",
                        "PostgreSQL FTS accent search",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Release autonomy pipeline",
                        "PostgreSQL abstraction layer",
                    ]},
                ]
            },
            {
                "version": "0.8.35",
                "date": "2026-06-03",
                "sections": [
                    { "title": "Corrections", "items": [
                        "SSDP non-standard UPnP renderers",
                        "Artwork rescan coalesce bug",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "DLNA cover art profileID in DIDL-Lite",
                        "Cargo audit security check in CI",
                    ]},
                ]
            },
            {
                "version": "0.8.37",
                "date": "2026-06-04",
                "sections": [
                    { "title": "Corrections", "items": [
                        "OAAT streams FLAC directly (native pipeline)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Mood DJ — ambient mix generation",
                    ]},
                ]
            },
            {
                "version": "0.8.39",
                "date": "2026-06-04",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Nested transaction fix in artist_repo",
                        "Signal path shows actual renderer name",
                        "TCP poll before browser open (no sleep)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Output errors surfaced to clients",
                        "Radio favorites: playlist_name + limit params",
                    ]},
                ]
            },
        ]
    }))
}
