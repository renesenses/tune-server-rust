use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::event_bus::TuneEvent;

pub fn is_enabled() -> bool {
    std::env::var("TUNE_NOTIFICATIONS_ENABLED")
        .map(|v| {
            matches!(
                v.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Le cache d'icônes de notification, sous `temp_dir()`.
///
/// En exploitation, le dossier doit survivre au processus : c'est un cache,
/// et le vider à chaque démarrage retélécharge chaque pochette. Le geste est
/// donc légitime ICI.
fn icon_cache_dir() -> PathBuf {
    icon_cache_dir_in(std::env::temp_dir())
}

/// Le même, sous une racine imposée.
///
/// La racine est un paramètre **pour le test** : `icon_cache_dir_exists`
/// appelait l'autre, donc créait pour de bon `/tmp/tune-notify-icons` et le
/// laissait derrière lui. C'était le dernier résidu d'une passe complète de
/// la suite au 01/09/2026, et le seul que le garde de #3030 ne pouvait pas
/// nommer : le `temp_dir()` fautif n'est pas dans du code de test, il est
/// ici, dans du code de production que le test appelle. Un garde de source
/// lit les tests ; il ne suit pas les appels.
///
/// Le test passe désormais un `ScratchDir`, qui emporte le dossier en
/// sortant de portée.
fn icon_cache_dir_in(racine: impl AsRef<Path>) -> PathBuf {
    let dir = racine.as_ref().join("tune-notify-icons");
    std::fs::create_dir_all(&dir).ok();
    dir
}

async fn download_icon(cover_url: &str, server_base: &str) -> Option<String> {
    if cover_url.is_empty() {
        return None;
    }

    // Local file
    if !cover_url.starts_with("http") && !cover_url.starts_with("/api/") {
        let p = PathBuf::from(cover_url);
        if p.is_file() {
            return Some(cover_url.to_string());
        }
        return None;
    }

    let url = if cover_url.starts_with("/api/") {
        format!("{server_base}{cover_url}")
    } else {
        cover_url.to_string()
    };

    let base_url = url.split('?').next().unwrap_or(&url);
    let hash = {
        use md5::{Digest, Md5};
        let mut hasher = Md5::new();
        hasher.update(base_url.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    let icon_path = icon_cache_dir().join(format!("{hash}.jpg"));

    if icon_path.exists()
        && std::fs::metadata(&icon_path)
            .map(|m| m.len() > 0)
            .unwrap_or(false)
    {
        return Some(icon_path.to_string_lossy().to_string());
    }

    let client = crate::http::client::shared();
    let resp = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() {
        return None;
    }
    std::fs::write(&icon_path, &bytes).ok()?;
    Some(icon_path.to_string_lossy().to_string())
}

async fn show_notification(title: &str, body: &str, _icon_path: Option<&str>) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            escape_applescript(body),
            escape_applescript(title)
        );
        tokio::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
            .await
            .ok();
    }

    #[cfg(target_os = "linux")]
    {
        let mut cmd = tokio::process::Command::new("notify-send");
        cmd.args(["--app-name=Tune", "-t", "5000"]);
        if let Some(icon) = _icon_path {
            cmd.args(["-i", icon]);
        }
        cmd.args([title, body]);
        cmd.output().await.ok();
    }

    #[cfg(target_os = "windows")]
    {
        let ps = format!(
            "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
             [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom, ContentType = WindowsRuntime] | Out-Null; \
             $template = '<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>'; \
             $xml = New-Object Windows.Data.Xml.Dom.XmlDocument; \
             $xml.LoadXml($template); \
             $toast = [Windows.UI.Notifications.ToastNotification]::new($xml); \
             [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Tune Server').Show($toast)",
            escape_xml(title),
            escape_xml(body)
        );
        tokio::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps])
            .output()
            .await
            .ok();
    }
}

#[cfg(target_os = "macos")]
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "windows")]
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn spawn_notification_listener(
    mut rx: broadcast::Receiver<TuneEvent>,
    server_base: Arc<String>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("desktop_notifications_enabled");
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!(skipped = n, "notification_events_lagged");
                    continue;
                }
                Err(_) => break,
            };

            if event.event_type != "playback.track_changed" {
                continue;
            }

            let title = event.data["title"]
                .as_str()
                .or_else(|| event.data["track_title"].as_str())
                .unwrap_or("")
                .to_string();
            if title.is_empty() {
                continue;
            }

            let artist = event.data["artist_name"].as_str().unwrap_or("").to_string();
            let album = event.data["album_title"].as_str().unwrap_or("").to_string();
            let cover = event.data["cover_path"].as_str().unwrap_or("").to_string();

            let body = [&artist, &album]
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(" — ");

            let icon = download_icon(&cover, &server_base).await;
            show_notification(&title, &body, icon.as_deref()).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cache est bien CRÉÉ, et sous la racine qu'on lui donne.
    ///
    /// La racine est un `ScratchDir` : ce test appelait `icon_cache_dir()`,
    /// donc créait `/tmp/tune-notify-icons` pour de vrai et l'y laissait
    /// (#3030). Vérifier `starts_with` n'est pas décoratif — c'est ce qui
    /// interdit qu'on « répare » la fuite en rendant un chemin que la
    /// fonction ne crée plus.
    #[test]
    fn icon_cache_dir_exists() {
        let racine = crate::test_scratch::scratch_dir("tune-notify-cache");
        let dir = icon_cache_dir_in(&racine);
        assert!(dir.is_dir(), "cache d'icônes non créé : {dir:?}");
        assert!(dir.starts_with(racine.path()), "cache hors de sa racine");
        assert_eq!(dir.file_name().unwrap(), "tune-notify-icons");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn applescript_escaping() {
        assert_eq!(escape_applescript(r#"It's a "test""#), r#"It's a \"test\""#);
        assert_eq!(escape_applescript(r"back\slash"), r"back\\slash");
    }
}
