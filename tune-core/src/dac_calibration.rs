use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::room_correction::CorrectionFilter;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A community DAC correction profile — parametric EQ corrections measured
/// for a specific DAC model, shared via the mozaiklabs.fr community API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DacProfile {
    /// URL-friendly identifier, e.g. `"topping-d90se"`.
    pub slug: String,
    /// DAC manufacturer, e.g. `"Topping"`.
    pub manufacturer: String,
    /// DAC model name, e.g. `"D90SE"`.
    pub model: String,
    /// Free-text description of the profile / measurement conditions.
    pub description: String,
    /// Parametric EQ correction filters (reuses room_correction types).
    pub corrections: Vec<CorrectionFilter>,
    /// Who contributed this profile (display name).
    pub contributed_by: Option<String>,
    /// Link to the measurement source (e.g. Audio Science Review).
    pub measurement_url: Option<String>,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// Community API base URL
// ---------------------------------------------------------------------------

const COMMUNITY_BASE: &str = "https://mozaiklabs.fr/api/v1/community/dac-profiles";

// ---------------------------------------------------------------------------
// Community API — remote calls
// ---------------------------------------------------------------------------

/// Fetch the full list of community DAC profiles from mozaiklabs.fr.
pub async fn list_community_profiles(client: &reqwest::Client) -> Result<Vec<DacProfile>, String> {
    let resp = client
        .get(COMMUNITY_BASE)
        .send()
        .await
        .map_err(|e| format!("community_fetch_failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("community_api_error: {status} {body}"));
    }

    let profiles: Vec<DacProfile> = resp
        .json()
        .await
        .map_err(|e| format!("community_parse_failed: {e}"))?;

    info!(count = profiles.len(), "dac_community_profiles_fetched");
    Ok(profiles)
}

/// Search community profiles by manufacturer or model name.
pub async fn search_profiles(
    client: &reqwest::Client,
    query: &str,
) -> Result<Vec<DacProfile>, String> {
    let url = format!("{COMMUNITY_BASE}/search");
    let resp = client
        .get(&url)
        .query(&[("q", query)])
        .send()
        .await
        .map_err(|e| format!("community_search_failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("community_search_error: {status} {body}"));
    }

    let profiles: Vec<DacProfile> = resp
        .json()
        .await
        .map_err(|e| format!("community_search_parse_failed: {e}"))?;

    info!(
        query = query,
        count = profiles.len(),
        "dac_community_search"
    );
    Ok(profiles)
}

/// Fetch a single community profile by slug.
pub async fn get_profile(client: &reqwest::Client, slug: &str) -> Result<DacProfile, String> {
    let url = format!("{COMMUNITY_BASE}/{slug}");
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("community_get_failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("community_get_error: {status} {body}"));
    }

    let profile: DacProfile = resp
        .json()
        .await
        .map_err(|e| format!("community_get_parse_failed: {e}"))?;

    info!(slug = slug, "dac_community_profile_fetched");
    Ok(profile)
}

/// Submit a new DAC profile to the community API.
pub async fn submit_profile(
    client: &reqwest::Client,
    profile: &DacProfile,
    instance_id: &str,
) -> Result<(), String> {
    let url = format!("{COMMUNITY_BASE}/submit");
    let body = serde_json::json!({
        "profile": profile,
        "instance_id": instance_id,
    });

    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("community_submit_failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("community_submit_error: {status} {text}"));
    }

    info!(slug = %profile.slug, "dac_profile_submitted_to_community");
    Ok(())
}

// ---------------------------------------------------------------------------
// Local storage — per-zone applied DAC profile
// ---------------------------------------------------------------------------

fn settings_key(zone_id: &str) -> String {
    format!("dac_profile_{zone_id}")
}

/// Save an applied DAC profile for a zone (local settings DB).
pub fn save_local_profile(
    backend: &Arc<dyn DbBackend>,
    zone_id: &str,
    profile: &DacProfile,
) -> Result<(), String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let json = serde_json::to_string(profile).map_err(|e| e.to_string())?;
    settings.set(&settings_key(zone_id), &json)?;
    info!(zone_id = zone_id, slug = %profile.slug, "dac_profile_saved_locally");
    Ok(())
}

/// Load the currently applied DAC profile for a zone, if any.
pub fn load_local_profile(backend: &Arc<dyn DbBackend>, zone_id: &str) -> Option<DacProfile> {
    let settings = SettingsRepo::with_backend(backend.clone());
    match settings.get(&settings_key(zone_id)) {
        Ok(Some(json)) => match serde_json::from_str(&json) {
            Ok(profile) => Some(profile),
            Err(e) => {
                warn!(zone_id = zone_id, error = %e, "dac_profile_parse_error");
                None
            }
        },
        _ => None,
    }
}

/// Remove the applied DAC profile for a zone.
pub fn delete_local_profile(backend: &Arc<dyn DbBackend>, zone_id: &str) -> Result<bool, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let existed = settings
        .get(&settings_key(zone_id))
        .ok()
        .flatten()
        .is_some();
    if existed {
        settings.delete(&settings_key(zone_id)).ok();
        info!(zone_id = zone_id, "dac_profile_removed");
    }
    Ok(existed)
}
