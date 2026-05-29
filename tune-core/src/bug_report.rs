use std::process::Stdio;

use regex::Regex;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BugReportData {
    pub version: String,
    pub os: String,
    pub architecture: String,
    pub ffmpeg_version: Option<String>,
    pub tracks: i64,
    pub albums: i64,
    pub artists: i64,
    pub music_dirs: Vec<String>,
    pub zones: Vec<ZoneInfo>,
    pub recent_errors: Vec<String>,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneInfo {
    pub name: String,
    pub output_type: String,
}

pub fn sanitize_path(text: &str) -> String {
    let re = Regex::new(r"(/(?:home|Users|mnt|media)/)([^/\s]+)").unwrap();
    re.replace_all(text, "${1}<user>").to_string()
}

pub fn detect_ffmpeg_version() -> Option<String> {
    let output = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(String::from)
}

pub fn collect_system_info(
    db: &SqliteDb,
    version: &str,
    music_dirs: &[String],
    recent_errors: Vec<String>,
) -> BugReportData {
    let conn = db.connection().lock().unwrap();

    let tracks = conn
        .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0);
    let albums = conn
        .query_row("SELECT COUNT(*) FROM albums", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0);
    let artists = conn
        .query_row("SELECT COUNT(*) FROM artists", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0);

    drop(conn);

    let sanitized_dirs: Vec<String> = music_dirs.iter().map(|d| sanitize_path(d)).collect();

    let recent_errors: Vec<String> = recent_errors
        .iter()
        .map(|e| sanitize_path(e))
        .collect();

    BugReportData {
        version: version.to_string(),
        os: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        architecture: std::env::consts::ARCH.to_string(),
        ffmpeg_version: detect_ffmpeg_version(),
        tracks,
        albums,
        artists,
        music_dirs: sanitized_dirs,
        zones: Vec::new(),
        recent_errors,
        generated_at: chrono::Utc::now().to_rfc3339(),
    }
}

pub fn format_markdown(data: &BugReportData) -> String {
    let mut lines = Vec::new();

    lines.push("## Rapport de bug Tune Server".to_string());
    lines.push(String::new());
    lines.push("### Description du probleme".to_string());
    lines.push(String::new());
    lines.push("_Decrivez le probleme ici..._".to_string());
    lines.push(String::new());
    lines.push("### Etapes pour reproduire".to_string());
    lines.push(String::new());
    lines.push("1. ".to_string());
    lines.push("2. ".to_string());
    lines.push("3. ".to_string());
    lines.push(String::new());

    lines.push("---".to_string());
    lines.push(String::new());
    lines.push("### Informations systeme".to_string());
    lines.push(String::new());
    lines.push(format!("- **Tune Server**: v{}", data.version));
    lines.push(format!("- **OS**: {}", data.os));
    lines.push(format!("- **Architecture**: {}", data.architecture));
    lines.push(format!(
        "- **FFmpeg**: {}",
        data.ffmpeg_version.as_deref().unwrap_or("non disponible")
    ));
    lines.push(String::new());

    lines.push("### Bibliotheque".to_string());
    lines.push(String::new());
    lines.push(format!("- **Pistes**: {}", data.tracks));
    lines.push(format!("- **Albums**: {}", data.albums));
    lines.push(format!("- **Artistes**: {}", data.artists));
    lines.push(String::new());

    if !data.music_dirs.is_empty() {
        lines.push("### Repertoires musicaux".to_string());
        lines.push(String::new());
        for d in &data.music_dirs {
            lines.push(format!("- `{d}`"));
        }
        lines.push(String::new());
    }

    if !data.zones.is_empty() {
        lines.push("### Zones".to_string());
        lines.push(String::new());
        for z in &data.zones {
            lines.push(format!("- {} ({})", z.name, z.output_type));
        }
        lines.push(String::new());
    }

    if !data.recent_errors.is_empty() {
        lines.push("### Erreurs recentes".to_string());
        lines.push(String::new());
        lines.push("```".to_string());
        for err in &data.recent_errors {
            lines.push(err.clone());
        }
        lines.push("```".to_string());
        lines.push(String::new());
    }

    lines.push(format!("_Rapport genere le {}_", data.generated_at));

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_unix_paths() {
        let input = "/home/bertrand/Music/song.flac";
        assert_eq!(sanitize_path(input), "/home/<user>/Music/song.flac");
    }

    #[test]
    fn sanitize_macos_paths() {
        let input = "/Users/john/Music/album/track.mp3";
        assert_eq!(sanitize_path(input), "/Users/<user>/Music/album/track.mp3");
    }

    #[test]
    fn sanitize_no_match() {
        let input = "/var/log/error.log";
        assert_eq!(sanitize_path(input), "/var/log/error.log");
    }

    #[test]
    fn markdown_output_has_sections() {
        let data = BugReportData {
            version: "1.0.0".into(),
            os: "Linux x86_64".into(),
            architecture: "x86_64".into(),
            ffmpeg_version: Some("ffmpeg version 6.0".into()),
            tracks: 1000,
            albums: 100,
            artists: 50,
            music_dirs: vec!["/home/<user>/Music".into()],
            zones: vec![ZoneInfo {
                name: "Living Room".into(),
                output_type: "DLNA".into(),
            }],
            recent_errors: vec!["some error".into()],
            generated_at: "2024-01-01T00:00:00Z".into(),
        };

        let md = format_markdown(&data);
        assert!(md.contains("## Rapport de bug"));
        assert!(md.contains("v1.0.0"));
        assert!(md.contains("1000"));
        assert!(md.contains("Living Room"));
        assert!(md.contains("some error"));
    }

    #[test]
    fn markdown_minimal() {
        let data = BugReportData {
            version: "0.1.0".into(),
            os: "macOS".into(),
            architecture: "aarch64".into(),
            ffmpeg_version: None,
            tracks: 0,
            albums: 0,
            artists: 0,
            music_dirs: Vec::new(),
            zones: Vec::new(),
            recent_errors: Vec::new(),
            generated_at: "now".into(),
        };

        let md = format_markdown(&data);
        assert!(md.contains("non disponible"));
        assert!(!md.contains("Repertoires"));
    }
}
