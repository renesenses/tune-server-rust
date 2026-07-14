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
/// Validates that an update is available, then spawns the download/extract/install
/// cycle in the background and returns immediately.  Progress is exposed via
/// `GET /system/update/status` (`phase` field).
pub(super) async fn update_install(State(state): State<AppState>) -> impl IntoResponse {
    // Prevent concurrent updates
    {
        let phase = state.update_phase.lock().unwrap();
        if let Some(ref p) = *phase {
            if !p.starts_with("failed") {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "status": "already_in_progress",
                        "phase": p,
                    })),
                )
                    .into_response();
            }
        }
    }

    // Guard: refuse update if .no-auto-update flag file exists
    let working_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(ref dir) = working_dir {
        if dir.join(".no-auto-update").exists() {
            warn!("update_blocked_no_auto_update_flag");
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "blocked",
                    "message": "Update blocked: .no-auto-update flag file exists. Remove it to allow updates."
                })),
            )
                .into_response();
        }
    }

    // Guard: refuse update if current binary has postgres but we might lose it
    if cfg!(feature = "postgres") {
        // This is a pre-flight warning; the actual binary check happens after download
    }

    // 1. Check for update (fast — just a GitHub API call)
    let checker = UpdateChecker::new();
    let release = match checker.check().await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(json!({"status": "up_to_date", "message": "Already running the latest version"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"status": "error", "message": format!("Failed to check for updates: {e}")})),
            )
                .into_response();
        }
    };

    let asset = match find_archive_asset(&release) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"status": "error", "message": "No compatible archive found for this platform"})),
            )
                .into_response();
        }
    };

    info!(
        version = %release.version,
        asset = %asset.name,
        size = asset.size,
        "update_download_starting"
    );

    // 2. Mark phase = downloading and spawn the background task
    {
        let mut phase = state.update_phase.lock().unwrap();
        *phase = Some("downloading".into());
    }

    let version = release.version.clone();
    let response_version = version.clone();
    let http_client = state.http_client.clone();
    let update_phase = state.update_phase.clone();

    tokio::spawn(async move {
        let set_phase = |p: &str| {
            // Log every phase, and warn on failures — set_phase was previously
            // silent, so a failed install (e.g. permission denied when Tune is
            // installed under Program Files) left no trace in the logs and the
            // update just "didn't happen" (Dominique, Windows 11).
            if p.starts_with("failed") {
                warn!(phase = %p, "update_phase_failed");
            } else {
                info!(phase = %p, "update_phase");
            }
            *update_phase.lock().unwrap() = Some(p.to_string());
        };

        // --- Download ---
        let archive_bytes = match async {
            let resp = http_client
                .get(&asset.browser_download_url)
                .timeout(std::time::Duration::from_secs(600))
                .send()
                .await
                .map_err(|e| format!("Download failed: {e}"))?;

            if !resp.status().is_success() {
                return Err(format!("Download failed: HTTP {}", resp.status()));
            }

            resp.bytes()
                .await
                .map_err(|e| format!("Failed to read download: {e}"))
        }
        .await
        {
            Ok(b) => {
                info!(size = b.len(), "update_downloaded");
                b
            }
            Err(e) => {
                error!(error = %e, "update_download_failed");
                set_phase(&format!("failed: {e}"));
                return;
            }
        };

        // --- Extract ---
        set_phase("extracting");

        let tmp_dir = std::env::temp_dir().join(format!("tune-update-{}", version));
        // Sweep leftover tune-update-* dirs from earlier updates. The success
        // path used to never remove the extraction dir, so one accumulated per
        // version (Benjithom, Windows: a new folder on every update).
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("tune-update-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        if tmp_dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
        if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
            set_phase(&format!("failed: Failed to create temp dir: {e}"));
            return;
        }

        let is_zip = asset.name.to_lowercase().ends_with(".zip");
        if let Err(e) = extract_archive(&archive_bytes, &tmp_dir, is_zip) {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            set_phase(&format!("failed: Extraction failed: {e}"));
            return;
        }

        info!(dir = %tmp_dir.display(), "update_extracted");

        // --- Install ---
        set_phase("installing");

        let binary_name = if cfg!(windows) {
            "tune-server.exe"
        } else {
            "tune-server"
        };
        let new_binary = tmp_dir.join(binary_name);
        if !new_binary.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            set_phase(&format!(
                "failed: Binary '{}' not found in archive",
                binary_name
            ));
            return;
        }

        // Guard: refuse update if current binary has postgres but new one doesn't
        if cfg!(feature = "postgres") {
            let new_has_pg = std::fs::read(&new_binary)
                .map(|bytes| {
                    let s = String::from_utf8_lossy(&bytes);
                    s.contains("postgresql://") || s.contains("PostgreSQL engine requested")
                })
                .unwrap_or(false);
            if !new_has_pg {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                warn!("update_blocked_missing_postgres_feature");
                set_phase(
                    "failed: Update blocked: current binary has PostgreSQL support but the downloaded release does not.",
                );
                return;
            }
        }

        let current_exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                set_phase(&format!("failed: Cannot determine current exe: {e}"));
                return;
            }
        };

        if cfg!(windows) {
            if let Err(e) = install_windows(&current_exe, &new_binary, &tmp_dir) {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                set_phase(&format!("failed: Windows install failed: {e}"));
                return;
            }
        } else if let Err(e) = install_unix(&current_exe, &new_binary, &tmp_dir) {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            set_phase(&format!("failed: Install failed: {e}"));
            return;
        }

        // Success: install_windows/install_unix have copied the binary + web/
        // into the install dir (the Windows .bat swap works entirely within
        // exe_dir), so the extraction dir is no longer needed. Removing it here
        // stops the per-version accumulation.
        let _ = std::fs::remove_dir_all(&tmp_dir);

        info!(
            from = %tune_core::version(),
            to = %version,
            "update_installed"
        );

        // --- Restart ---
        set_phase("restarting");

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        info!("update_restarting");

        // Restart into the freshly-installed binary.
        //
        // UNIX (macOS/Linux): re-exec in place with execv. This replaces the
        // current process image while keeping the SAME PID, so recovery does
        // NOT depend on an external supervisor. It works identically under the
        // macOS DMG (no launchd/LaunchAgent), inside Docker (PID 1 never dies →
        // the container stays up), when launched from a terminal, and under
        // systemd (no exit → no Restart cycle, no parasite child, no port race).
        //
        // The previous approach — spawn() a child, then exit(0) — only recovered
        // when a supervisor happened to restart on exit (systemd Restart=always):
        // the .18 journal proved the spawned child was itself killed by systemd's
        // KillMode=control-group and did nothing (the process that came back had
        // a different PID). Without a supervisor (Docker, the DMG) nothing
        // restarted, so the server never came back — the reported bug.
        //
        // The listening socket is CLOEXEC (socket2 + std default), so exec()
        // releases port 8888 and the new image rebinds cleanly (main.rs also
        // retries bind). exec() only returns on failure — then we fall back to
        // spawn()+exit(0) so a supervised deployment still recovers.
        //
        // WINDOWS: we must NOT spawn or exec here. The binary is swapped by
        // tune-update.bat, which first waits for THIS process (tune-server.exe)
        // to exit. Starting another process from the still-old binary keeps the
        // image name alive forever, so the .bat's wait_loop never completes
        // (Christophe's log: `update_installed to=0.8.261` then a restart as
        // `version=0.8.260`). Just exit; the .bat swaps the binary and starts it.
        #[cfg(windows)]
        {
            info!(
                "update_windows_exiting_for_bat_swap — tune-update.bat will swap the binary and restart"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::process::exit(0);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let exe = current_exe.clone();
            let args: Vec<String> = std::env::args().skip(1).collect();
            // Let the final status-poll response flush before we swap the image.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            info!(exe = %exe.display(), "update_reexec");
            // exec() replaces this process on success and never returns.
            let err = std::process::Command::new(&exe).args(&args).exec();
            warn!(error = %err, "update_reexec_failed — falling back to spawn+exit");
            match std::process::Command::new(&exe)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
            {
                Ok(child) => {
                    info!(pid = child.id(), exe = %exe.display(), "update_new_process_spawned");
                }
                Err(e) => {
                    warn!(error = %e, "update_restart_spawn_failed — manual restart required");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::process::exit(0);
        }
    });

    // Return immediately — client polls /system/update/status
    Json(json!({
        "status": "downloading",
        "version": response_version,
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
        "@echo off\r\n\
         echo Waiting for Tune server to stop...\r\n\
         :wait_loop\r\n\
         tasklist /FI \"IMAGENAME eq {exe_name}\" 2>nul | find /I \"{exe_name}\" >nul\r\n\
         if not errorlevel 1 (\r\n\
           timeout /t 1 /nobreak >nul\r\n\
           goto wait_loop\r\n\
         )\r\n\
         timeout /t 1 /nobreak >nul\r\n\
         echo Replacing binary...\r\n\
         del \"{exe}\"\r\n\
         if exist \"{exe}\" (\r\n\
           echo File still locked, retrying...\r\n\
           timeout /t 3 /nobreak >nul\r\n\
           del \"{exe}\"\r\n\
         )\r\n\
         rename \"{new}\" \"{exe_name}\"\r\n\
         echo Starting updated server...\r\n\
         start \"\" \"{exe}\"\r\n\
         del \"%~f0\"\r\n",
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

/// Replace the web/ directory with the one from the archive.
/// Writes to both CWD/web (where the server reads) and exe_dir/web (fallback).
fn update_web_dir(exe_dir: &std::path::Path, tmp_dir: &std::path::Path) -> Result<(), String> {
    let new_web = tmp_dir.join("web");
    if !new_web.exists() {
        info!("no web/ directory in archive, skipping web update");
        return Ok(());
    }

    let target_web = if let Ok(custom) = std::env::var("TUNE_WEB_DIR") {
        let p = std::path::PathBuf::from(&custom);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| exe_dir.to_path_buf())
                .join(p)
        }
    } else {
        std::env::current_dir()
            .map(|d| d.join("web"))
            .unwrap_or_else(|_| exe_dir.join("web"))
    };

    if target_web.exists() {
        std::fs::remove_dir_all(&target_web).map_err(|e| format!("remove old web/: {e}"))?;
    }
    copy_dir_all(&new_web, &target_web).map_err(|e| format!("copy new web/: {e}"))?;

    let exe_web = exe_dir.join("web");
    if exe_web != target_web {
        if exe_web.exists() {
            std::fs::remove_dir_all(&exe_web).ok();
        }
        copy_dir_all(&new_web, &exe_web).ok();
    }

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
pub(super) async fn update_status(State(state): State<AppState>) -> Json<Value> {
    let phase = state.update_phase.lock().unwrap().clone();
    let is_failed = phase
        .as_deref()
        .map(|p| p.starts_with("failed"))
        .unwrap_or(false);

    Json(json!({
        "current_version": tune_core::version(),
        "phase": phase,
        "update_in_progress": phase.is_some() && !is_failed,
    }))
}

/// POST /system/update/apply — kept for backward compatibility.
pub(super) async fn update_apply() -> impl IntoResponse {
    Json(json!({
        "status": "deprecated",
        "message": "Use POST /system/update/install instead",
    }))
}

/// GET /system/changelog — fetch from GitHub releases, cache 1 hour.
pub(super) async fn changelog() -> Json<Value> {
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    static CACHE: OnceLock<Mutex<(std::time::Instant, Value)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new((
            std::time::Instant::now() - std::time::Duration::from_secs(7200),
            json!([]),
        ))
    });
    let mut guard = cache.lock().await;

    if guard.0.elapsed() < std::time::Duration::from_secs(3600)
        && guard.1.as_array().is_some_and(|a| !a.is_empty())
    {
        return Json(json!({ "version": tune_core::version(), "entries": guard.1 }));
    }

    let entries = match fetch_github_changelog().await {
        Ok(e) => {
            *guard = (std::time::Instant::now(), e.clone());
            e
        }
        Err(_) => guard.1.clone(),
    };
    drop(guard);
    Json(json!({ "version": tune_core::version(), "entries": entries }))
}

async fn fetch_github_changelog() -> Result<Value, String> {
    let client = tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Tune/2.0")
        .build()
        .map_err(|e| e.to_string())?;

    // Try mozaiklabs.fr proxy first, fallback to GitHub
    let releases: Vec<Value> = match async {
        let resp = client
            .get("https://mozaiklabs.fr/api/tune/releases?per_page=20")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("proxy API {}", resp.status()));
        }
        resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())
    }
    .await
    {
        Ok(r) => r,
        Err(_) => {
            let mut req = client.get(
                "https://api.github.com/repos/renesenses/tune-server-rust/releases?per_page=20",
            );
            if let Ok(token) = std::env::var("GITHUB_TOKEN") {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("GitHub API {}", resp.status()));
            }
            resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())?
        }
    };
    let entries: Vec<Value> = releases
        .iter()
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let version = tag.strip_prefix('v').unwrap_or(tag);
            let date = r["published_at"]
                .as_str()
                .unwrap_or("")
                .split('T')
                .next()
                .unwrap_or("");
            let body = r["body"].as_str().unwrap_or("");
            let mut features = Vec::new();
            let mut fixes = Vec::new();
            let mut improvements = Vec::new();
            for line in body.lines() {
                let trimmed = line
                    .trim()
                    .trim_start_matches("- ")
                    .trim_start_matches("* ");
                if trimmed.is_empty() {
                    continue;
                }
                let lower = line.to_lowercase();
                if lower.contains("fix") || lower.contains("correction") || lower.contains("bug") {
                    fixes.push(trimmed.to_string());
                } else if lower.contains("feat")
                    || lower.contains("nouveaut")
                    || lower.contains("add")
                {
                    features.push(trimmed.to_string());
                } else if lower.contains("improv")
                    || lower.contains("amélio")
                    || lower.contains("perf")
                    || lower.contains("optim")
                {
                    improvements.push(trimmed.to_string());
                } else if trimmed.starts_with("**") || trimmed.starts_with("##") {
                    continue;
                } else {
                    features.push(trimmed.to_string());
                }
            }
            if features.is_empty() && fixes.is_empty() && improvements.is_empty() {
                features.push(format!("Release {version}"));
            }
            Some(json!({
                "version": version,
                "date": date,
                "features": features,
                "fixes": fixes,
                "improvements": improvements,
            }))
        })
        .collect();
    Ok(json!(entries))
}

#[allow(dead_code)]
fn changelog_hardcoded() -> Json<Value> {
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
            {
                "version": "0.8.50",
                "date": "2026-06-05",
                "sections": [
                    { "title": "Nouveautes", "items": [
                        "Auth JWT multi-utilisateurs",
                        "AI Assistant Claude (11 outils)",
                        "Plugin SDK + EventBus",
                        "PostgreSQL abstraction complète",
                        "Tune Bridge (WebSocket cloud-to-home)",
                        "Intégration cloud mozaiklabs.fr (SSO, télémétrie)",
                    ]},
                ]
            },
            {
                "version": "0.8.58",
                "date": "2026-06-06",
                "sections": [
                    { "title": "Corrections", "items": [
                        "ALAC 24-bit décodage (hiss fix)",
                        "WAL checkpoint stale reads",
                        "M4A scan fallback",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Docker officiel multi-arch",
                        "FFmpeg entièrement supprimé — pipeline 100% Rust",
                        "5 décodeurs natifs (ALAC, AAC, MP3, Vorbis, Opus)",
                    ]},
                ]
            },
            {
                "version": "0.8.65",
                "date": "2026-06-08",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Fix DLNA darTZeel coupure 2s",
                        "Volume buttons web client (PUT + int 0-100)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "HQPlayer output (v4/v5/v6)",
                        "OAAT protocol (9 crates, crates.io)",
                        "Community metadata (covers + artist images)",
                        "Forum 7 langues (350 traductions)",
                        "MusicBrainz batch MBID matching",
                    ]},
                ]
            },
            {
                "version": "0.8.70",
                "date": "2026-06-09",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Volume slider (debounce + DLNA normalisation)",
                        "Podcast Affaires Sensibles (feed URL corrigée)",
                        "Zones fantômes filtrées de En cours d'écoute",
                        "Télémétrie report après scan (5 min au lieu de 30s)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Page Ambassadeurs (mozaiklabs.fr/ambassadors)",
                        "Page Fabricants OAAT (mozaiklabs.fr/oaat/manufacturers)",
                        "Admin Tune Cloud (instances, SSO, bridges)",
                        "Threads privés forum",
                        "Images artistes fallback MusicBrainz/Wikimedia",
                    ]},
                ]
            },
            {
                "version": "0.8.83",
                "date": "2026-06-11",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Scrollbar plus large + visible sur Windows",
                        "Thème persisté après sync serveur",
                        "Quoi de neuf : parsing du format changelog API",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Next/Prev instantanés (DLNA async en background)",
                        "Tune Widget macOS (tray app Tauri v2)",
                    ]},
                ]
            },
        ]
    }))
}
