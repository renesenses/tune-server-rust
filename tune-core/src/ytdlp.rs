//! Managed `yt-dlp` helper binary for YouTube playback.
//!
//! YouTube tightened its anti-bot gating (a `po_token` challenge) so Tune's
//! unauthenticated InnerTube clients now return `LOGIN_REQUIRED` even for public
//! videos. `yt-dlp` tracks that arms race, so Tune uses it as the YouTube stream
//! extraction backend. This module auto-provisions the `yt-dlp` binary (a single
//! self-contained executable — no Python required) into a tools directory and
//! resolves the path used by the extractor.
//!
//! Opt-in: nothing is downloaded until the user clicks "Enable YouTube playback"
//! (which calls [`download`]). Tune works fully without it.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::{info, warn};

/// Cached resolved path to the `yt-dlp` binary (set at startup / after download).
static YTDLP_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<PathBuf>> {
    YTDLP_PATH.get_or_init(|| Mutex::new(None))
}

/// Directory where Tune stores auto-provisioned helper binaries. Mirrors the
/// data-dir resolution used for the DB/artwork (config.rs): `%LOCALAPPDATA%`
/// on Windows, `~/Library/Application Support/Tune` on macOS, `~/.cache/tune`
/// (or `$XDG_CACHE_HOME/tune`) on Linux. Overridable with `TUNE_TOOLS_DIR`.
pub fn tools_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("TUNE_TOOLS_DIR") {
        if !custom.is_empty() {
            let p = PathBuf::from(custom);
            std::fs::create_dir_all(&p).ok();
            return p;
        }
    }
    let base: PathBuf = if cfg!(target_os = "windows") {
        std::env::var("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("TuneServer"))
            .unwrap_or_else(|_| PathBuf::from("TuneServer"))
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/Tune"))
            .unwrap_or_else(|_| std::env::temp_dir().join("tune"))
    } else {
        std::env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("tune")
    };
    let dir = base.join("tools");
    std::fs::create_dir_all(&dir).ok();
    dir
}

/// Local filename we store the binary under (platform-specific).
pub fn binary_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

/// Path the auto-downloaded binary is stored at.
pub fn local_binary_path() -> PathBuf {
    tools_dir().join(binary_filename())
}

/// yt-dlp GitHub release asset name for the current platform, or `None` if
/// unsupported. yt-dlp ships single self-contained binaries (no archive).
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => Some("yt-dlp_macos"),
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        ("windows", "x86") => Some("yt-dlp_x86.exe"),
        _ => None,
    }
}

/// Store the resolved binary path in the process-wide cache so the extractor
/// ([`binary`]) can find it without a DB handle.
pub fn set_binary(path: PathBuf) {
    *cache().lock().unwrap() = Some(path);
}

/// The currently-resolved `yt-dlp` binary path, if any. Read by the YouTube
/// extractor. `None` means YouTube playback is not enabled.
pub fn binary() -> Option<PathBuf> {
    cache().lock().unwrap().clone()
}

/// Resolve the `yt-dlp` binary and populate the cache. Order: an explicit
/// configured path (the `yt_dlp_path` setting), then the auto-download location,
/// then a `yt-dlp` on `PATH`. Returns the resolved path (also cached).
pub async fn resolve(configured_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured_path {
        if !p.is_empty() {
            let pb = PathBuf::from(p);
            if pb.exists() {
                set_binary(pb.clone());
                return Some(pb);
            }
        }
    }
    let local = local_binary_path();
    if local.exists() {
        set_binary(local.clone());
        return Some(local);
    }
    // Fall back to a `yt-dlp` already on PATH.
    let on_path = tokio::process::Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg("yt-dlp")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if on_path {
        let pb = PathBuf::from("yt-dlp");
        set_binary(pb.clone());
        return Some(pb);
    }
    None
}

/// Download the latest `yt-dlp` binary for this platform into [`tools_dir`],
/// make it executable, cache the path, and return `(path, version_tag)`.
pub async fn download() -> Result<(PathBuf, String), String> {
    let asset = asset_name().ok_or_else(|| {
        format!(
            "yt-dlp: unsupported platform {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let client = crate::http::client::builder()
        .user_agent("tune-server")
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("yt-dlp: http client: {e}"))?;

    // Resolve the release + asset URL from the GitHub API.
    let mut req = client
        .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
        .header("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
    }
    let release: serde_json::Value = req
        .send()
        .await
        .map_err(|e| format!("yt-dlp: fetch release: {e}"))?
        .error_for_status()
        .map_err(|e| format!("yt-dlp: release status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("yt-dlp: parse release: {e}"))?;

    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let url = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(asset))
        })
        .and_then(|a| a.get("browser_download_url").and_then(|u| u.as_str()))
        .ok_or_else(|| format!("yt-dlp: asset '{asset}' not found in latest release"))?
        .to_string();

    info!(asset, url = %url, tag = %tag, "ytdlp_download_starting");
    let bytes = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("yt-dlp: download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("yt-dlp: download status: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("yt-dlp: download body: {e}"))?;

    let dest = local_binary_path();
    // Write to a temp file then rename, so a partial download never looks valid.
    let tmp = dest.with_extension("download");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("yt-dlp: write: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("yt-dlp: chmod: {e}"))?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("yt-dlp: install: {e}"))?;

    set_binary(dest.clone());
    info!(path = %dest.display(), tag = %tag, size = bytes.len(), "ytdlp_download_complete");
    Ok((dest, tag))
}

/// Query `yt-dlp --version` for the given binary (best-effort).
pub async fn version_of(path: &Path) -> Option<String> {
    let out = tokio::process::Command::new(path)
        .arg("--version")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        warn!("ytdlp_version_query_failed");
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_matches_current_platform() {
        // On any supported dev/CI platform we must resolve an asset.
        let a = asset_name();
        match std::env::consts::OS {
            "macos" => assert_eq!(a, Some("yt-dlp_macos")),
            "linux" => assert!(a == Some("yt-dlp_linux") || a == Some("yt-dlp_linux_aarch64")),
            "windows" => assert!(a == Some("yt-dlp.exe") || a == Some("yt-dlp_x86.exe")),
            _ => {}
        }
    }

    #[test]
    fn binary_filename_has_exe_on_windows() {
        if cfg!(target_os = "windows") {
            assert_eq!(binary_filename(), "yt-dlp.exe");
        } else {
            assert_eq!(binary_filename(), "yt-dlp");
        }
    }

    #[test]
    fn local_binary_path_under_tools_dir() {
        let p = local_binary_path();
        assert!(p.ends_with(binary_filename()));
        assert!(p.parent().unwrap().ends_with("tools"));
    }
}
