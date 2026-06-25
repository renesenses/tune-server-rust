use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::playback::NowPlaying;

// ---------------------------------------------------------------------------
// Sharing profile — user preferences for what to share publicly
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharingProfile {
    pub display_name: String,
    pub public_url: Option<String>,
    pub enabled: bool,
    pub share_now_playing: bool,
    pub share_history: bool,
    pub share_top_artists: bool,
}

impl Default for SharingProfile {
    fn default() -> Self {
        Self {
            display_name: String::new(),
            public_url: None,
            enabled: false,
            share_now_playing: false,
            share_history: false,
            share_top_artists: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Now Listening card — what gets shared externally
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowListeningCard {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub source: String,
    pub shared_at: String,
}

impl NowListeningCard {
    /// Build a card from the current NowPlaying data.
    pub fn from_now_playing(np: &NowPlaying) -> Self {
        Self {
            title: np.title.clone(),
            artist: np.artist_name.clone(),
            album: np.album_title.clone(),
            cover_url: np.cover_path.clone(),
            format: np.format.clone(),
            sample_rate: np.sample_rate,
            bit_depth: np.bit_depth,
            source: np.source.clone(),
            shared_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Top artist entry (for public profile)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopArtist {
    pub name: String,
    pub plays: i64,
}

// ---------------------------------------------------------------------------
// Public profile — what external consumers (mozaiklabs.fr) see
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicProfileData {
    pub display_name: String,
    pub now_listening: Option<NowListeningCard>,
    pub top_artists: Vec<TopArtist>,
    pub total_plays: i64,
    pub member_since: Option<String>,
}

// ---------------------------------------------------------------------------
// Persistence helpers — save/load SharingProfile from settings
// ---------------------------------------------------------------------------

const SETTINGS_KEY: &str = "social_sharing_profile";

/// Load the sharing profile from the settings table.
/// Returns `Default` if none saved yet.
pub fn load_profile(db: &Arc<dyn DbBackend>) -> SharingProfile {
    let settings = SettingsRepo::with_backend(db.clone());
    settings
        .get(SETTINGS_KEY)
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// Save the sharing profile to the settings table.
pub fn save_profile(db: &Arc<dyn DbBackend>, profile: &SharingProfile) -> Result<(), String> {
    let settings = SettingsRepo::with_backend(db.clone());
    let json =
        serde_json::to_string(profile).map_err(|e| format!("serialize sharing profile: {e}"))?;
    settings.set(SETTINGS_KEY, &json)
}

// ---------------------------------------------------------------------------
// SVG badge generation
// ---------------------------------------------------------------------------

/// Generate an SVG badge showing what is currently playing.
/// Returns a placeholder badge if nothing is playing or sharing is disabled.
pub fn render_now_listening_svg(card: Option<&NowListeningCard>) -> String {
    match card {
        Some(c) => {
            let title = xml_escape(&c.title);
            let artist = c
                .artist
                .as_deref()
                .map(xml_escape)
                .unwrap_or_else(|| "Unknown Artist".into());
            let format_badge = c
                .format
                .as_deref()
                .map(|f| {
                    let sr = c
                        .sample_rate
                        .map(|s| format!(" {}kHz", s / 1000))
                        .unwrap_or_default();
                    let bd = c
                        .bit_depth
                        .map(|b| format!("/{}bit", b))
                        .unwrap_or_default();
                    format!("{f}{sr}{bd}")
                })
                .unwrap_or_default();
            format!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="56" viewBox="0 0 400 56">
  <rect width="400" height="56" rx="8" fill="#1a1a2e"/>
  <text x="36" y="22" font-family="system-ui,sans-serif" font-size="13" fill="#4ade80">&#9835; Now listening</text>
  <text x="36" y="38" font-family="system-ui,sans-serif" font-size="12" fill="#eee">{title} — {artist}</text>
  <text x="364" y="22" font-family="system-ui,sans-serif" font-size="10" fill="#888" text-anchor="end">{format_badge}</text>
</svg>"##
            )
        }
        None => {
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="56" viewBox="0 0 400 56">
  <rect width="400" height="56" rx="8" fill="#1a1a2e"/>
  <text x="200" y="32" font-family="system-ui,sans-serif" font-size="12" fill="#888" text-anchor="middle">Nothing playing right now</text>
</svg>"##
                .to_string()
        }
    }
}

/// Minimal XML/SVG escaping for text content.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
