use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Free,
    Premium,
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Free => write!(f, "free"),
            Self::Premium => write!(f, "premium"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    UnlimitedZones,
    MultiroomSync,
    DspEq,
    CloudRelay,
    OaatProtocol,
    CloudBackup,
    SyncedLyrics,
    ListeningStats,
    MultiScrobbling,
    AiRecommendations,
    PlaylistTransfer,
    AdvancedAlarms,
    MultiProfiles,
    WeeklyDigest,
    AutoEnrichment,
    RoomCorrection,
    CloudConfigBackup,
    SocialSharing,
    DeveloperApi,
    PluginMarketplace,
    MultiServer,
    DacCalibration,
    BatchConverter,
}

impl Feature {
    /// All features gated behind Premium.
    pub fn all_premium() -> &'static [Feature] {
        &[
            Feature::UnlimitedZones,
            Feature::MultiroomSync,
            Feature::DspEq,
            Feature::CloudRelay,
            // OAAT is free — open-source protocol, core feature
            // Feature::OaatProtocol,
            Feature::CloudBackup,
            Feature::SyncedLyrics,
            Feature::ListeningStats,
            Feature::MultiScrobbling,
            Feature::AiRecommendations,
            Feature::PlaylistTransfer,
            Feature::AdvancedAlarms,
            Feature::MultiProfiles,
            Feature::WeeklyDigest,
            Feature::AutoEnrichment,
            Feature::RoomCorrection,
            Feature::CloudConfigBackup,
            Feature::SocialSharing,
            Feature::DeveloperApi,
            Feature::PluginMarketplace,
            Feature::MultiServer,
            Feature::DacCalibration,
            Feature::BatchConverter,
        ]
    }

    /// Human-readable display name.
    pub fn display_name(&self) -> &'static str {
        match self {
            Feature::UnlimitedZones => "Unlimited Zones",
            Feature::MultiroomSync => "Multiroom Sync",
            Feature::DspEq => "DSP & EQ",
            Feature::CloudRelay => "Cloud Relay",
            Feature::OaatProtocol => "OAAT Protocol",
            Feature::CloudBackup => "Cloud Backup",
            Feature::SyncedLyrics => "Synced Lyrics",
            Feature::ListeningStats => "Listening Stats",
            Feature::MultiScrobbling => "Multi-Service Scrobbling",
            Feature::AiRecommendations => "AI Recommendations",
            Feature::PlaylistTransfer => "Playlist Transfer",
            Feature::AdvancedAlarms => "Advanced Alarms",
            Feature::MultiProfiles => "Multi-User Profiles",
            Feature::WeeklyDigest => "Weekly Digest",
            Feature::AutoEnrichment => "Auto Metadata Enrichment",
            Feature::RoomCorrection => "Room Correction",
            Feature::CloudConfigBackup => "Cloud Config Backup",
            Feature::SocialSharing => "Social Sharing",
            Feature::DeveloperApi => "Developer API",
            Feature::PluginMarketplace => "Plugin Marketplace",
            Feature::MultiServer => "Multi-Server",
            Feature::DacCalibration => "DAC Calibration",
            Feature::BatchConverter => "Batch Audio Converter",
        }
    }
}

// ---------------------------------------------------------------------------
// License state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LicenseState {
    pub tier: Tier,
    pub license_key: Option<String>,
    pub expires_at: Option<String>,
    pub last_validated: Option<String>,
    pub hardware_fingerprint: String,
    /// Premium granted by the linked mozaiklabs.fr **account** (SSO), independent
    /// of any license key. Second, OR-ed source of premium (see `effective_tier`).
    #[serde(default)]
    pub account_premium: bool,
    /// Subscription end for the account premium (ISO-8601), `None` = no expiry.
    #[serde(default)]
    pub account_premium_expires: Option<String>,
    /// Last time the account premium was confirmed from the server (ISO-8601);
    /// drives the offline grace window.
    #[serde(default)]
    pub account_premium_checked: Option<String>,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const FREE_MAX_ZONES: i64 = 3;
const GRACE_PERIOD_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// LicenseManager
// ---------------------------------------------------------------------------

pub struct LicenseManager {
    state: Arc<RwLock<LicenseState>>,
    db: Arc<dyn DbBackend>,
}

impl LicenseManager {
    /// Create a new LicenseManager, loading cached state from the settings
    /// table.  If the tier is premium but the last validation is older than
    /// GRACE_PERIOD_DAYS, the tier is degraded to Free.
    pub fn new(db: Arc<dyn DbBackend>) -> Self {
        let settings = SettingsRepo::with_backend(db.clone());

        let license_key = settings.get("license_key").ok().flatten();
        let tier_str = settings.get("license_tier").ok().flatten();
        let expires_at = settings.get("license_expires_at").ok().flatten();
        let last_validated = settings.get("license_last_validated").ok().flatten();

        let hardware_fingerprint = Self::hardware_fingerprint();

        let mut tier = match tier_str.as_deref() {
            Some("premium") => Tier::Premium,
            _ => Tier::Free,
        };

        // Grace period check: degrade to Free if last validation is too old.
        if tier == Tier::Premium {
            if let Some(ref validated) = last_validated {
                if is_expired(validated, GRACE_PERIOD_DAYS) {
                    warn!(
                        last_validated = %validated,
                        grace_days = GRACE_PERIOD_DAYS,
                        "license_grace_period_expired, degrading to free"
                    );
                    tier = Tier::Free;
                }
            } else {
                // Premium with no last_validated — degrade.
                warn!("license_premium_without_validation, degrading to free");
                tier = Tier::Free;
            }
        }

        info!(
            tier = %tier,
            has_key = license_key.is_some(),
            fingerprint = %hardware_fingerprint,
            "license_manager_initialized"
        );

        // Account premium (SSO) — loaded as-is; expiry & offline grace are applied
        // live in `effective_tier`, so no load-time degradation is needed here.
        let account_premium = settings
            .get("mozaik_premium")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);
        let account_premium_expires = settings.get("mozaik_premium_expires").ok().flatten();
        let account_premium_checked = settings.get("mozaik_premium_checked").ok().flatten();

        let state = LicenseState {
            tier,
            license_key,
            expires_at,
            last_validated,
            hardware_fingerprint,
            account_premium,
            account_premium_expires,
            account_premium_checked,
        };

        Self {
            state: Arc::new(RwLock::new(state)),
            db,
        }
    }

    /// Effective tier: Premium if EITHER a premium license key OR a valid
    /// account premium (SSO) is active. This is the tier all gating uses.
    pub async fn tier(&self) -> Tier {
        effective_tier(&*self.state.read().await)
    }

    /// Shorthand: is the effective tier Premium?
    pub async fn is_premium(&self) -> bool {
        self.tier().await == Tier::Premium
    }

    /// Check whether a specific feature is enabled. All premium features require
    /// the effective Premium tier (license key or account premium).
    pub async fn check_feature(&self, _feature: Feature) -> bool {
        effective_tier(&*self.state.read().await) == Tier::Premium
    }

    /// Check whether adding a new zone is allowed.
    /// Free tier: max FREE_MAX_ZONES.  Premium: unlimited.
    pub async fn check_zone_limit(&self, current_count: i64) -> bool {
        match effective_tier(&*self.state.read().await) {
            Tier::Premium => true,
            Tier::Free => current_count < FREE_MAX_ZONES,
        }
    }

    /// Clone snapshot of the current license state (for API responses). The
    /// `tier` field reflects the *effective* tier so the UI shows premium even
    /// when it comes from the account rather than a license key.
    pub async fn license_state(&self) -> LicenseState {
        let mut snapshot = self.state.read().await.clone();
        snapshot.tier = effective_tier(&snapshot);
        snapshot
    }

    /// Store a license key and set tier to Premium.
    /// Actual server-side validation happens via heartbeat later.
    pub async fn set_license_key(&self, key: &str) -> Result<(), String> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings.set("license_key", key)?;
        settings.set("license_tier", "premium")?;

        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        settings.set("license_last_validated", &now)?;

        let mut state = self.state.write().await;
        state.license_key = Some(key.to_string());
        state.tier = Tier::Premium;
        state.last_validated = Some(now);

        info!(key_prefix = &key[..key.len().min(8)], "license_key_set");
        Ok(())
    }

    /// Remove the license key and revert to Free.
    pub async fn clear_license(&self) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings.delete("license_key").ok();
        settings.set("license_tier", "free").ok();
        settings.delete("license_expires_at").ok();
        settings.delete("license_last_validated").ok();

        let mut state = self.state.write().await;
        state.license_key = None;
        state.tier = Tier::Free;
        state.expires_at = None;
        state.last_validated = None;

        info!("license_cleared");
    }

    /// Called by heartbeat when the licensing server responds.
    /// Updates tier, expires_at, and last_validated in both memory and DB.
    pub async fn update_from_server(&self, tier: Tier, expires_at: Option<String>) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        settings.set("license_tier", &tier.to_string()).ok();
        settings.set("license_last_validated", &now).ok();

        if let Some(ref exp) = expires_at {
            settings.set("license_expires_at", exp).ok();
        } else {
            settings.delete("license_expires_at").ok();
        }

        let mut state = self.state.write().await;
        state.tier = tier;
        state.expires_at = expires_at;
        state.last_validated = Some(now.clone());

        info!(tier = %tier, validated = %now, "license_updated_from_server");
    }

    /// Set the account premium (SSO) state. Called after an SSO login (and by the
    /// periodic refresh) with the `premium` flag and optional subscription expiry
    /// from `/api/v1/user`. Stamps the check time for the offline grace window.
    pub async fn set_account_premium(&self, premium: bool, expires: Option<String>) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();

        settings
            .set("mozaik_premium", if premium { "true" } else { "false" })
            .ok();
        settings.set("mozaik_premium_checked", &now).ok();
        if let Some(ref exp) = expires {
            settings.set("mozaik_premium_expires", exp).ok();
        } else {
            settings.delete("mozaik_premium_expires").ok();
        }

        let mut state = self.state.write().await;
        state.account_premium = premium;
        state.account_premium_expires = expires;
        state.account_premium_checked = Some(now);

        info!(account_premium = premium, "license_account_premium_updated");
    }

    /// Clear the account premium (SSO logout / disconnect). The license-key path
    /// is untouched.
    pub async fn clear_account_premium(&self) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings.delete("mozaik_premium").ok();
        settings.delete("mozaik_premium_expires").ok();
        settings.delete("mozaik_premium_checked").ok();

        let mut state = self.state.write().await;
        state.account_premium = false;
        state.account_premium_expires = None;
        state.account_premium_checked = None;

        info!("license_account_premium_cleared");
    }

    /// Compute a hardware fingerprint: SHA-256 of (hostname + platform ID).
    /// Returns a 64-char hex string.  Deterministic for a given machine.
    pub fn hardware_fingerprint() -> String {
        let hostname = get_hostname();
        let platform_id = platform_machine_id();

        let input = format!("{hostname}:{platform_id}");
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Zone limit for the free tier (exposed for UI display).
    pub fn free_zone_limit() -> i64 {
        FREE_MAX_ZONES
    }
}

// ---------------------------------------------------------------------------
// Hostname helper
// ---------------------------------------------------------------------------

fn get_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| {
            // Fallback: use the `hostname` command.
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|o| {
                    let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    if s.is_empty() { None } else { Some(s) }
                })
                .unwrap_or_else(|| "unknown-host".to_string())
        })
}

// ---------------------------------------------------------------------------
// Platform-specific machine ID helpers
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn platform_machine_id() -> String {
    // Try /etc/machine-id first (systemd), then /sys/class/dmi/id/product_uuid.
    if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
        let trimmed = id.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    if let Ok(id) = std::fs::read_to_string("/sys/class/dmi/id/product_uuid") {
        let trimmed = id.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    "unknown".to_string()
}

#[cfg(target_os = "macos")]
fn platform_machine_id() -> String {
    // Extract IOPlatformSerialNumber from ioreg.
    if let Ok(output) = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("IOPlatformSerialNumber") {
                // Format: "IOPlatformSerialNumber" = "XXXX"
                if let Some(val) = line.split('=').nth(1) {
                    let serial = val.trim().trim_matches('"').trim().to_string();
                    if !serial.is_empty() {
                        return serial;
                    }
                }
            }
        }
    }
    "unknown".to_string()
}

#[cfg(target_os = "windows")]
fn platform_machine_id() -> String {
    // Use wmic to get the baseboard serial number.
    if let Ok(output) = std::process::Command::new("wmic")
        .args(["baseboard", "get", "serialnumber"])
        .output()
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Skip the header line ("SerialNumber"), take the first data line.
        for line in stdout.lines().skip(1) {
            let trimmed = line.trim().to_string();
            if !trimmed.is_empty() {
                return trimmed;
            }
        }
    }
    "unknown".to_string()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn platform_machine_id() -> String {
    "unknown".to_string()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Whether the account premium (SSO) currently counts as active: flag set, its
/// subscription not past, and last confirmed within the offline grace window.
fn account_premium_active(state: &LicenseState) -> bool {
    if !state.account_premium {
        return false;
    }
    // Subscription end (if known): past expiry → not active.
    if let Some(ref exp) = state.account_premium_expires {
        if is_expired(exp, 0) {
            return false;
        }
    }
    // Offline grace: must have been confirmed from the server recently.
    match state.account_premium_checked {
        Some(ref checked) => !is_expired(checked, GRACE_PERIOD_DAYS),
        None => false,
    }
}

/// Effective tier = Premium if the license key is premium OR the account premium
/// (SSO) is active. Otherwise Free.
fn effective_tier(state: &LicenseState) -> Tier {
    if state.tier == Tier::Premium || account_premium_active(state) {
        Tier::Premium
    } else {
        Tier::Free
    }
}

/// Check whether an ISO-8601 timestamp is older than `days` from now.
fn is_expired(timestamp: &str, days: i64) -> bool {
    let Ok(parsed) = chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%SZ") else {
        // If we can't parse, treat as expired.
        return true;
    };
    let validated = parsed.and_utc();
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
    validated < cutoff
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_serde_roundtrip() {
        let json = serde_json::to_string(&Tier::Premium).unwrap();
        assert_eq!(json, r#""premium""#);
        let back: Tier = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Tier::Premium);
    }

    #[test]
    fn feature_serde_roundtrip() {
        let json = serde_json::to_string(&Feature::DspEq).unwrap();
        assert_eq!(json, r#""dsp_eq""#);
        let back: Feature = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Feature::DspEq);
    }

    #[test]
    fn all_premium_has_twentytwo_features() {
        assert_eq!(Feature::all_premium().len(), 22);
    }

    #[test]
    fn hardware_fingerprint_is_64_hex_chars() {
        let fp = LicenseManager::hardware_fingerprint();
        assert_eq!(fp.len(), 64, "SHA-256 hex should be 64 chars: {fp}");
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {fp}");
    }

    #[test]
    fn is_expired_true_for_old_date() {
        assert!(is_expired("2020-01-01T00:00:00Z", 30));
    }

    #[test]
    fn is_expired_false_for_now() {
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        assert!(!is_expired(&now, 30));
    }

    #[test]
    fn is_expired_true_for_invalid() {
        assert!(is_expired("not-a-date", 30));
    }

    // ---- effective_tier / account premium (SSO) ----

    fn now_iso() -> String {
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    fn future_iso(days: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn past_iso(days: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::days(days))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    fn state(
        tier: Tier,
        account_premium: bool,
        account_premium_expires: Option<String>,
        account_premium_checked: Option<String>,
    ) -> LicenseState {
        LicenseState {
            tier,
            license_key: None,
            expires_at: None,
            last_validated: None,
            hardware_fingerprint: "test".into(),
            account_premium,
            account_premium_expires,
            account_premium_checked,
        }
    }

    #[test]
    fn effective_free_when_nothing() {
        assert_eq!(
            effective_tier(&state(Tier::Free, false, None, None)),
            Tier::Free
        );
    }

    #[test]
    fn effective_premium_via_license_key() {
        // Key premium alone wins, regardless of account fields.
        assert_eq!(
            effective_tier(&state(Tier::Premium, false, None, None)),
            Tier::Premium
        );
    }

    #[test]
    fn effective_premium_via_account_recent_check() {
        // Account premium, confirmed now, no subscription end → Premium.
        assert_eq!(
            effective_tier(&state(Tier::Free, true, None, Some(now_iso()))),
            Tier::Premium
        );
    }

    #[test]
    fn effective_premium_via_account_future_expiry() {
        assert_eq!(
            effective_tier(&state(
                Tier::Free,
                true,
                Some(future_iso(30)),
                Some(now_iso())
            )),
            Tier::Premium
        );
    }

    #[test]
    fn effective_free_when_account_subscription_expired() {
        // Subscription end in the past → not premium even if recently checked.
        assert_eq!(
            effective_tier(&state(Tier::Free, true, Some(past_iso(1)), Some(now_iso()))),
            Tier::Free
        );
    }

    #[test]
    fn effective_free_when_account_grace_expired() {
        // Confirmed 40 days ago, past the 30-day offline grace → degrade.
        assert_eq!(
            effective_tier(&state(Tier::Free, true, None, Some(past_iso(40)))),
            Tier::Free
        );
    }

    #[test]
    fn effective_free_when_account_never_checked() {
        assert_eq!(
            effective_tier(&state(Tier::Free, true, None, None)),
            Tier::Free
        );
    }

    #[test]
    fn effective_premium_key_survives_expired_account() {
        // A premium license key stays premium even if the account premium lapsed.
        assert_eq!(
            effective_tier(&state(
                Tier::Premium,
                true,
                Some(past_iso(1)),
                Some(now_iso())
            )),
            Tier::Premium
        );
    }

    #[test]
    fn display_names_are_non_empty() {
        for f in Feature::all_premium() {
            assert!(!f.display_name().is_empty());
        }
    }

    #[tokio::test]
    async fn license_manager_defaults_to_free() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend);
        assert_eq!(mgr.tier().await, Tier::Free);
        assert!(!mgr.is_premium().await);
        assert!(!mgr.check_feature(Feature::DspEq).await);
        // Free tier allows up to FREE_MAX_ZONES (3) zones.
        assert!(mgr.check_zone_limit(2).await);
        assert!(!mgr.check_zone_limit(3).await);
    }

    #[tokio::test]
    async fn set_and_clear_license() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend);

        mgr.set_license_key("TUNE-TEST-1234").await.unwrap();
        assert_eq!(mgr.tier().await, Tier::Premium);
        assert!(mgr.is_premium().await);
        assert!(mgr.check_feature(Feature::CloudRelay).await);
        assert!(mgr.check_zone_limit(100).await);

        mgr.clear_license().await;
        assert_eq!(mgr.tier().await, Tier::Free);
        assert!(!mgr.is_premium().await);
    }

    #[tokio::test]
    async fn update_from_server() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend);

        mgr.update_from_server(Tier::Premium, Some("2030-12-31T00:00:00Z".to_string()))
            .await;
        assert_eq!(mgr.tier().await, Tier::Premium);

        let state = mgr.license_state().await;
        assert_eq!(state.expires_at.as_deref(), Some("2030-12-31T00:00:00Z"));
        assert!(state.last_validated.is_some());
    }
}
