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
    PlaylistsHub,
    Declick,
    /// Analyse acoustique CLAP : la passe d'embeddings qui alimente la radio
    /// acoustique. C'est le traitement le plus lourd du serveur (décodage +
    /// inférence ONNX multi-thread, ~300 Mo résidents).
    AcousticAnalysis,
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
            Feature::PlaylistsHub,
            Feature::Declick,
            Feature::AcousticAnalysis,
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
            Feature::AcousticAnalysis => "Acoustic Analysis",
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
            Feature::PlaylistsHub => "Playlists Hub",
            Feature::Declick => "Dé-ploc",
        }
    }

    /// Le **code stable** du droit, tel qu'il voyage dans un refus 402.
    ///
    /// `display_name` est une étiquette anglaise destinée à l'œil et au
    /// journal ; elle peut être reformulée sans prévenir. Ce code-ci ne le
    /// peut pas : c'est le terme du contrat qu'un client traduit avec ses
    /// propres chaînes (#2419), au même titre que `ModuleRefusal::code` pour
    /// les modules payants (#2392). Le renommer casse la traduction des
    /// clients déjà installés.
    pub fn code(&self) -> &'static str {
        match self {
            Feature::UnlimitedZones => "unlimited_zones",
            Feature::MultiroomSync => "multiroom_sync",
            Feature::DspEq => "dsp_eq",
            Feature::CloudRelay => "cloud_relay",
            Feature::OaatProtocol => "oaat_protocol",
            Feature::CloudBackup => "cloud_backup",
            Feature::SyncedLyrics => "synced_lyrics",
            Feature::ListeningStats => "listening_stats",
            Feature::MultiScrobbling => "multi_scrobbling",
            Feature::AiRecommendations => "ai_recommendations",
            Feature::AcousticAnalysis => "acoustic_analysis",
            Feature::PlaylistTransfer => "playlist_transfer",
            Feature::AdvancedAlarms => "advanced_alarms",
            Feature::MultiProfiles => "multi_profiles",
            Feature::WeeklyDigest => "weekly_digest",
            Feature::AutoEnrichment => "auto_enrichment",
            Feature::RoomCorrection => "room_correction",
            Feature::CloudConfigBackup => "cloud_config_backup",
            Feature::SocialSharing => "social_sharing",
            Feature::DeveloperApi => "developer_api",
            Feature::PluginMarketplace => "plugin_marketplace",
            Feature::MultiServer => "multi_server",
            Feature::DacCalibration => "dac_calibration",
            Feature::BatchConverter => "batch_converter",
            Feature::PlaylistsHub => "playlists_hub",
            Feature::Declick => "declick",
        }
    }

    /// Whether the feature is actually available / functional right now — a
    /// PRODUCT decision, independent of licence entitlement. The Premium
    /// "Fonctionnalités" grid colours each widget from this combined with the
    /// licence `enabled` flag:
    ///   - not licensed          → grey  (locked, upsell)
    ///   - licensed + available   → green (usable, the widget opens its page)
    ///   - licensed + !available  → red   (entitled but not yet available)
    ///
    /// Default is `true` (available). To turn a widget red, add its variant to
    /// the match arm below — this is the single source of truth.
    pub fn available(&self) -> bool {
        match self {
            // Features licensed by Premium but NOT yet available/functional
            // (product decision — Bertrand). These show a red cross in the UI.
            // Note: Dé-ploc IS available, so it is deliberately NOT listed here.
            Feature::SocialSharing | Feature::WeeklyDigest => false,
            _ => true,
        }
    }
}

// ---------------------------------------------------------------------------
// License state
// ---------------------------------------------------------------------------

/// Live signal that the premium license is currently held by ANOTHER server
/// (floating-license single-session model, à la Roon). Set by the heartbeat when
/// the cloud answers `session_conflict:true`; runtime-only (never persisted) —
/// the next heartbeat re-establishes the truth. While present it suppresses
/// premium *here* regardless of the key/account being otherwise valid, and it
/// carries just enough context for the UI to explain why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConflict {
    /// Label (or server_id) of the server currently holding the session.
    pub active_server: Option<String>,
    /// ISO-8601 timestamp of that server's last heartbeat.
    pub active_since: Option<String>,
}

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
    /// Qobuz endpoint order signalled by the cloud license validation:
    /// `true` = route Qobuz API calls through the mozaiklabs proxy first
    /// (founder account), `false` (default) = call the Qobuz API directly
    /// first with the proxy as fallback. Absent on older servers → false.
    #[serde(default)]
    pub qobuz_proxy_first: bool,
    /// Paid MODULE entitlements (stable ids, e.g. "diretta") owned by the
    /// linked account. Separate SKUs — independent of `tier`/premium.
    /// Persisted like the account premium so entitlements survive restarts
    /// and offline starts; refreshed by the cloud validation loop.
    #[serde(default)]
    pub modules: Vec<String>,
    /// Live single-session conflict: `Some` while another server holds the
    /// floating license. Runtime-only — never loaded from settings, so a restart
    /// starts clean and the next heartbeat restores it if still in conflict.
    #[serde(default)]
    pub session_conflict: Option<SessionConflict>,
    /// Le marqueur premium de la CLÉ tel qu'il est persisté (`license_tier`),
    /// AVANT toute dégradation. Purement informatif (#1999) : `tier` est écrasé
    /// deux fois — au chargement quand la grâce est écoulée, et dans
    /// `license_state()` par le tier *effectif* — si bien qu'une fois la grâce
    /// lapsée plus rien en mémoire ne disait qu'il y avait eu une licence. Sans
    /// ce témoin, impossible d'expliquer à l'utilisateur POURQUOI son premium a
    /// disparu. Ne participe à aucune décision d'autorisation.
    #[serde(default)]
    pub key_premium_marker: bool,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Free-tier zone cap when not overridden. Configurable at runtime via
/// `TUNE_FREE_MAX_ZONES` (see `TuneConfig`); premium is always unlimited.
const DEFAULT_FREE_MAX_ZONES: i64 = 3;
// Offline grace once a key HAS been validated online at least once. Shortened
// from 30 to 14 days: enough tolerance for an intermittently-connected server,
// but a revoked or lapsed key falls back to Free sooner. The initial online
// validation is now mandatory (see `set_license_key`), so this only governs
// re-validation, never first activation.
const GRACE_PERIOD_DAYS: i64 = 14;

// ---------------------------------------------------------------------------
// LicenseManager
// ---------------------------------------------------------------------------

pub struct LicenseManager {
    state: Arc<RwLock<LicenseState>>,
    db: Arc<dyn DbBackend>,
    /// Max zones a free-tier instance may create. Set once at construction.
    free_max_zones: i64,
}

impl LicenseManager {
    /// Create a new LicenseManager with the default free-tier zone cap.
    pub fn new(db: Arc<dyn DbBackend>) -> Self {
        Self::new_with_limit(db, DEFAULT_FREE_MAX_ZONES)
    }

    /// Create a new LicenseManager, loading cached state from the settings
    /// table.  If the tier is premium but the last validation is older than
    /// GRACE_PERIOD_DAYS, the tier is degraded to Free.
    pub fn new_with_limit(db: Arc<dyn DbBackend>, free_max_zones: i64) -> Self {
        let settings = SettingsRepo::with_backend(db.clone());

        let license_key = settings.get("license_key").ok().flatten();
        let tier_str = settings.get("license_tier").ok().flatten();
        let expires_at = settings.get("license_expires_at").ok().flatten();
        let last_validated = settings.get("license_last_validated").ok().flatten();

        let hardware_fingerprint = Self::persistent_fingerprint(&settings);

        let mut tier = match tier_str.as_deref() {
            Some("premium") => Tier::Premium,
            _ => Tier::Free,
        };
        // Le marqueur persisté, capturé AVANT la dégradation ci-dessous : c'est
        // lui qui permettra d'expliquer une retombée en Free (#1999).
        let key_premium_marker = tier == Tier::Premium;

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

        // Qobuz endpoint order (founder flag) — persisted like the account
        // premium so the order survives restarts and offline starts.
        let qobuz_proxy_first = settings
            .get("qobuz_proxy_first")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);

        // Module entitlements — persisted like the account premium.
        let modules: Vec<String> = settings
            .get("mozaik_modules")
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default();

        let state = LicenseState {
            tier,
            license_key,
            expires_at,
            last_validated,
            hardware_fingerprint,
            account_premium,
            account_premium_expires,
            account_premium_checked,
            qobuz_proxy_first,
            modules,
            session_conflict: None,
            key_premium_marker,
        };

        Self {
            state: Arc::new(RwLock::new(state)),
            db,
            free_max_zones,
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
    /// Free tier: max `free_max_zones`.  Premium: unlimited.
    pub async fn check_zone_limit(&self, current_count: i64) -> bool {
        match effective_tier(&*self.state.read().await) {
            Tier::Premium => true,
            Tier::Free => current_count < self.free_max_zones,
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

    /// Store a license key **without** granting Premium.
    ///
    /// The tier is only promoted to Premium once the licensing server confirms
    /// the key — via `update_from_server`, called by the heartbeat or an
    /// immediate `/cloud/license/validate` right after this. Previously this
    /// stamped `license_tier=premium` + `last_validated=now`, which let ANY
    /// string unlock Premium locally for the whole 30-day offline grace with no
    /// server round-trip (a free-ride that survived even a `license_valid:false`
    /// verdict). Now a key stays "pending" (Free) until a genuine online
    /// validation succeeds, so a fake key never unlocks anything. Legit users
    /// are promoted within one validation round-trip.
    pub async fn set_license_key(&self, key: &str) -> Result<(), String> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings.set("license_key", key)?;
        // Pending until validated: do NOT set premium or stamp a validation.
        // Clear any stale timestamp so a re-entered key can't ride a previous
        // key's grace window.
        settings.set("license_tier", "free")?;
        settings.delete("license_last_validated").ok();

        let mut state = self.state.write().await;
        state.license_key = Some(key.to_string());
        state.tier = Tier::Free;
        state.last_validated = None;
        state.key_premium_marker = false;

        info!(
            key_prefix = &key[..key.len().min(8)],
            "license_key_stored_pending_validation"
        );
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
        state.key_premium_marker = false;

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
        state.key_premium_marker = tier == Tier::Premium;

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

    /// Set the Qobuz endpoint order flag from the cloud license validation.
    /// `true` = proxy-first (founder account); `false` = direct-first (the
    /// default for every user). Persisted so it survives restarts, mirroring
    /// `set_account_premium`.
    pub async fn set_qobuz_proxy_first(&self, proxy_first: bool) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings
            .set(
                "qobuz_proxy_first",
                if proxy_first { "true" } else { "false" },
            )
            .ok();

        let mut state = self.state.write().await;
        let changed = state.qobuz_proxy_first != proxy_first;
        state.qobuz_proxy_first = proxy_first;

        if changed {
            info!(
                qobuz_proxy_first = proxy_first,
                "license_qobuz_proxy_first_updated"
            );
        }
    }

    /// Current Qobuz endpoint order: `true` = proxy-first (founder account).
    pub async fn qobuz_proxy_first(&self) -> bool {
        self.state.read().await.qobuz_proxy_first
    }

    /// Set the paid-module entitlements from the cloud license validation.
    /// Persisted so entitlements survive restarts and offline starts,
    /// mirroring `set_account_premium`. The cloud is authoritative: an empty
    /// list clears previous entitlements (refund / transfer).
    pub async fn set_modules(&self, modules: Vec<String>) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings
            .set(
                "mozaik_modules",
                &serde_json::to_string(&modules).unwrap_or_else(|_| "[]".into()),
            )
            .ok();

        let mut state = self.state.write().await;
        let changed = state.modules != modules;
        state.modules = modules;

        if changed {
            info!(modules = ?state.modules, "license_modules_updated");
        }
    }

    /// Whether the account owns the paid module `id` (e.g. "diretta").
    /// Module SKUs are independent of the premium tier.
    pub async fn has_module(&self, id: &str) -> bool {
        self.state.read().await.modules.iter().any(|m| m == id)
    }

    /// Snapshot of the owned module ids (for provider contexts / API responses).
    pub async fn modules(&self) -> Vec<String> {
        self.state.read().await.modules.clone()
    }

    /// Record that the floating license is currently held by ANOTHER server
    /// (the cloud answered `session_conflict:true`). This gates the effective
    /// tier down to Free here — an authoritative "not now" — WITHOUT touching the
    /// key or `last_validated`, so premium snaps back the moment the other server
    /// stops pinging and the conflict clears. Runtime-only; not persisted.
    pub async fn set_session_conflict(
        &self,
        active_server: Option<String>,
        active_since: Option<String>,
    ) {
        let mut state = self.state.write().await;
        let was_clear = state.session_conflict.is_none();
        state.session_conflict = Some(SessionConflict {
            active_server,
            active_since,
        });
        if was_clear {
            warn!("license_session_conflict_set (premium suppressed: held by another server)");
        }
    }

    /// Clear a previously recorded session conflict (this server (re)took the
    /// session, or the cloud no longer reports a conflict). No-op if none set.
    pub async fn clear_session_conflict(&self) {
        let mut state = self.state.write().await;
        if state.session_conflict.take().is_some() {
            info!("license_session_conflict_cleared (session reclaimed here)");
        }
    }

    /// Current session conflict, if the license is held elsewhere right now.
    pub async fn session_conflict(&self) -> Option<SessionConflict> {
        self.state.read().await.session_conflict.clone()
    }

    /// Clear the account premium (SSO logout / disconnect). The license-key path
    /// is untouched.
    pub async fn clear_account_premium(&self) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings.delete("mozaik_premium").ok();
        settings.delete("mozaik_premium_expires").ok();
        settings.delete("mozaik_premium_checked").ok();

        settings.delete("mozaik_modules").ok();

        let mut state = self.state.write().await;
        state.account_premium = false;
        state.account_premium_expires = None;
        state.account_premium_checked = None;
        // Module entitlements come from the account — they leave with it.
        state.modules.clear();

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

    /// Stable hardware fingerprint, persisted in `settings` on first use.
    ///
    /// The raw [`hardware_fingerprint`] derives from hostname + machine-id, both
    /// VOLATILE on containerised / NAS installs: a container's `HOSTNAME` is
    /// often its recreatable id and `/etc/machine-id` can regenerate, so the
    /// fingerprint changed on every reinstall/restart. The server then rejected
    /// the (mono-device) license and testers were bounced into the grace/login
    /// loop and had to be reset by hand (Yacine, Synology; recurring). Anchoring
    /// it to the settings table — which lives in the library DB the user keeps —
    /// makes it stable across restarts and reinstalls: computed once, reused
    /// forever after.
    fn persistent_fingerprint(settings: &SettingsRepo) -> String {
        if let Some(fp) = settings.get("hardware_fingerprint").ok().flatten() {
            if fp.len() == 64 {
                return fp;
            }
        }
        let fp = Self::hardware_fingerprint();
        let _ = settings.set("hardware_fingerprint", &fp);
        fp
    }

    /// État de la grâce hors ligne, pour l'affichage (#1999).
    ///
    /// Purement descriptif : ne lit que l'état déjà en mémoire et ne modifie
    /// rien. `None` quand la question ne se pose pas (aucun droit premium en
    /// jeu, ou abonnement réellement échu).
    pub async fn offline_grace(&self) -> Option<OfflineGrace> {
        offline_grace(&*self.state.read().await)
    }

    /// Durée de la fenêtre de grâce hors ligne, en jours — le chiffre à écrire
    /// dans l'interface et la documentation plutôt qu'à recopier à la main.
    pub const fn offline_grace_days() -> i64 {
        GRACE_PERIOD_DAYS
    }

    /// Zone limit for the free tier (exposed for UI display).
    pub fn free_zone_limit(&self) -> i64 {
        self.free_max_zones
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

/// Whether the license *key* currently counts as Premium: tier is Premium, the
/// key's own expiry (if known) has not passed, and it was validated within the
/// offline grace window. Mirrors `account_premium_active` so the key gets the
/// same *live* graceful degradation the SSO account already has: a valid key
/// survives a transient cloud rejection (bad `license_valid:false` verdict,
/// fingerprint re-binding) or an offline period, and is only revoked once grace
/// lapses or on a genuine past-expiry — never on a single bad heartbeat.
fn key_premium_active(state: &LicenseState) -> bool {
    if state.tier != Tier::Premium {
        return false;
    }
    // Key expiry (if known): past expiry → not active.
    if let Some(ref exp) = state.expires_at {
        if is_expired(exp, 0) {
            return false;
        }
    }
    // Offline grace: must have been validated (or set) within the window.
    match state.last_validated {
        Some(ref validated) => !is_expired(validated, GRACE_PERIOD_DAYS),
        None => false,
    }
}

/// Effective tier = Premium if the license key is premium OR the account premium
/// (SSO) is active. Otherwise Free.
///
/// A live single-session conflict overrides everything: while another server
/// holds the floating license, premium is suppressed here even though the key /
/// account are otherwise valid. This is what enforces "one active session at a
/// time" — and, unlike a transient `license_valid:false`, it is authoritative,
/// so it is NOT softened by the offline grace window.
fn effective_tier(state: &LicenseState) -> Tier {
    if state.session_conflict.is_some() {
        return Tier::Free;
    }
    if key_premium_active(state) || account_premium_active(state) {
        Tier::Premium
    } else {
        Tier::Free
    }
}

// ---------------------------------------------------------------------------
// Offline grace — READ-ONLY reporting (#1999)
// ---------------------------------------------------------------------------
//
// Tout ce qui suit ne fait que *décrire* la fenêtre de grâce déjà appliquée par
// `key_premium_active` / `account_premium_active`. Aucune de ces fonctions n'est
// appelée par `effective_tier` : la politique (durée, instant d'expiration,
// fonctions désactivées) est rigoureusement inchangée. Le défaut de #1999 n'est
// pas que la grâce se comporte mal — c'est qu'elle est invisible.

/// À partir de quand on annonce la grâce à l'utilisateur.
///
/// Le battement va vers mozaiklabs.fr toutes les heures
/// (`HEARTBEAT_INTERVAL`) : 48 tentatives ratées d'affilée ne sont plus un
/// hoquet réseau, c'est une coupure. En deçà, se taire — un serveur qui manque
/// deux battements est parfaitement sain et n'a rien à signaler.
///
/// Ce seuil ne gouverne QUE l'affichage. Il ne raccourcit ni n'allonge la
/// grâce : à J+2 comme à J+13, le serveur est premium exactement comme avant.
const GRACE_NOTICE_AFTER_DAYS: i64 = 2;

/// Où en est la revalidation en ligne des droits premium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GracePhase {
    /// Confirmé en ligne récemment — rien à signaler.
    Ok,
    /// Pas de confirmation depuis au moins [`GRACE_NOTICE_AFTER_DAYS`] jours,
    /// mais la fenêtre court toujours : **le premium est intact**.
    Grace,
    /// La fenêtre est écoulée : les droits premium sont retombés en Free en
    /// attendant la prochaine validation réussie.
    Expired,
}

/// Quelle source de premium porte la fenêtre décrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraceSource {
    /// Clé de licence (`license_last_validated`).
    Key,
    /// Compte mozaiklabs.fr lié en SSO (`mozaik_premium_checked`).
    Account,
}

/// État de la grâce hors ligne, tel qu'il remonte à l'interface.
///
/// Ne contient **aucune donnée sensible** : ni clé, ni identifiant d'achat, ni
/// jeton — seulement des dates et un compte de jours.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineGrace {
    pub phase: GracePhase,
    pub source: GraceSource,
    /// Dernière confirmation en ligne réussie (ISO-8601 Zulu). `None` = jamais.
    pub since: Option<String>,
    /// Instant où la fenêtre se referme (`since` + [`GRACE_PERIOD_DAYS`]).
    pub until: Option<String>,
    /// Jours entiers restants, arrondis au supérieur ; 0 une fois écoulée.
    pub days_remaining: i64,
    /// Durée totale de la fenêtre, en jours — le chiffre à afficher.
    pub total_days: i64,
    /// Depuis combien de jours entiers la dernière confirmation date.
    pub days_since_validation: i64,
}

/// Décrit la fenêtre de grâce qui s'applique à cet état — sans rien décider.
///
/// Rend `None` quand la question ne se pose pas : aucun droit premium en jeu
/// (utilisateur Free), ou abonnement dont la date de fin est *réellement*
/// passée — ce dernier cas n'est pas une affaire de réseau et ne doit surtout
/// pas être présenté comme tel.
///
/// Quand les deux sources sont premium, on décrit celle qui tient le plus
/// longtemps : c'est elle qui gouverne, `effective_tier` étant un OU.
pub fn offline_grace(state: &LicenseState) -> Option<OfflineGrace> {
    let mut candidates: Vec<(GraceSource, Option<chrono::DateTime<chrono::Utc>>)> = Vec::new();

    // Clé : marqueur premium PERSISTÉ (pas `tier`, qui est écrasé par la
    // dégradation au chargement et par le tier effectif dans `license_state`)
    // et date de fin propre non dépassée.
    if state.key_premium_marker
        && !state
            .expires_at
            .as_deref()
            .is_some_and(|e| is_expired(e, 0))
    {
        candidates.push((
            GraceSource::Key,
            state.last_validated.as_deref().and_then(parse_timestamp),
        ));
    }

    // Compte SSO : drapeau posé et abonnement non échu.
    if state.account_premium
        && !state
            .account_premium_expires
            .as_deref()
            .is_some_and(|e| is_expired(e, 0))
    {
        candidates.push((
            GraceSource::Account,
            state
                .account_premium_checked
                .as_deref()
                .and_then(parse_timestamp),
        ));
    }

    // `None` (jamais confirmé) trie plus bas que n'importe quel `Some` : la
    // source la mieux revalidée gagne.
    let (source, anchor) = candidates.into_iter().max_by_key(|(_, a)| *a)?;

    let total_days = GRACE_PERIOD_DAYS;

    let Some(anchor) = anchor else {
        // Premium jamais validé en ligne : déjà en Free, et la fenêtre n'a
        // jamais commencé à courir. Le dire, plutôt que de laisser deviner.
        return Some(OfflineGrace {
            phase: GracePhase::Expired,
            source,
            since: None,
            until: None,
            days_remaining: 0,
            total_days,
            days_since_validation: 0,
        });
    };

    let now = chrono::Utc::now();
    let until = anchor + chrono::Duration::days(total_days);
    let elapsed = now - anchor;

    // Aligné au millimètre sur `is_expired(anchor, GRACE_PERIOD_DAYS)`, qui
    // compare `anchor < now - 14j`, soit `until < now`.
    let expired = until <= now;
    let days_remaining = if expired {
        0
    } else {
        // Arrondi au supérieur : tant qu'il reste la moindre heure, on annonce
        // « 1 jour », jamais « 0 ».
        ((until - now).num_seconds() + 86_399).div_euclid(86_400)
    };
    let days_since_validation = elapsed.num_seconds().max(0).div_euclid(86_400);

    let phase = if expired {
        GracePhase::Expired
    } else if days_since_validation >= GRACE_NOTICE_AFTER_DAYS {
        GracePhase::Grace
    } else {
        GracePhase::Ok
    };

    Some(OfflineGrace {
        phase,
        source,
        since: Some(format_utc(anchor)),
        until: Some(format_utc(until)),
        days_remaining,
        total_days,
        days_since_validation,
    })
}

/// Le format Zulu que le reste du fichier persiste et relit.
fn format_utc(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Whether an ISO-8601 (`%Y-%m-%dT%H:%M:%SZ`) timestamp lies in the past.
/// Unlike [`is_expired`] (which fails *closed*: malformed → expired), this fails
/// *open*: unparseable input returns `false` so malformed server data never
/// triggers a license revocation. Used by the heartbeat to tell a genuine past
/// expiry from a transient `license_valid:false` verdict.
pub fn is_timestamp_past(timestamp: &str) -> bool {
    match chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%SZ") {
        Ok(parsed) => parsed.and_utc() < chrono::Utc::now(),
        Err(_) => false,
    }
}

/// Check whether an ISO-8601 timestamp is older than `days` from now.
/// Parse a timestamp coming either from our own settings (`...Z`) or from the
/// licence server, which emits real ISO 8601 **with an offset**
/// (`2026-09-09T11:22:56+02:00`).
///
/// Only the Zulu shape used to be accepted, and an unparseable value was
/// treated as expired — so a licence carrying a genuine expiry was read as
/// already over and the server silently fell back to Free. It never showed
/// because every licence issued so far was `lifetime`, i.e. `expires_at =
/// null`: the branch was never taken. The first real one-month subscription
/// (Bruno Lescarret, 2026-08-09) validated fine against the cloud, then
/// unlocked nothing.
fn parse_timestamp(timestamp: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    // Offset-aware first: that is what the licence server sends.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    // Naive `...Z`, the shape we persist in settings ourselves.
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|n| n.and_utc())
}

fn is_expired(timestamp: &str, days: i64) -> bool {
    let Some(validated) = parse_timestamp(timestamp) else {
        // Still fail closed on a genuinely unreadable value — but the shapes we
        // actually receive are now both understood.
        return true;
    };
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
    validated < cutoff
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Le serveur de licences renvoie une date ISO 8601 **avec décalage**
    /// (`+02:00`), pas du Zulu. Elle n'était pas parsée, et un échec de parsing
    /// vaut « expirée » : un abonnement d'un mois parfaitement valide faisait
    /// retomber le serveur en Free juste après une validation réussie.
    /// Invisible jusqu'ici, toutes les licences émises étant `lifetime` avec
    /// `expires_at = null`.
    #[test]
    fn a_future_expiry_with_offset_is_not_expired() {
        let future = (chrono::Utc::now() + chrono::Duration::days(30))
            .with_timezone(&chrono::FixedOffset::east_opt(2 * 3600).unwrap())
            .to_rfc3339();
        assert!(
            !is_expired(&future, 0),
            "un abonnement qui court encore doit être actif : {future}"
        );
    }

    #[test]
    fn both_timestamp_shapes_are_understood() {
        // Zulu — ce que l'on persiste soi-même dans les settings.
        assert!(parse_timestamp("2020-01-01T00:00:00Z").is_some());
        // Avec décalage — ce que le serveur de licences envoie.
        assert!(parse_timestamp("2026-09-09T11:22:56+02:00").is_some());
        // Illisible — on continue d'échouer côté sûr.
        assert!(parse_timestamp("pas une date").is_none());
    }

    #[test]
    fn a_past_expiry_is_still_expired_whatever_the_shape() {
        assert!(is_expired("2020-01-01T00:00:00Z", 0));
        assert!(is_expired("2020-01-01T00:00:00+02:00", 0));
        assert!(is_expired("illisible", 0), "illisible ⇒ échec côté sûr");
    }

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
    fn all_premium_has_twentyfive_features() {
        // Ce compte est un garde-fou volontaire : ajouter une fonctionnalité
        // premium doit être un acte conscient, pas un effet de bord. Passé de
        // 24 à 25 avec `AcousticAnalysis` (analyse CLAP), qui n'était gardée par
        // rien et tournait sur des installations gratuites.
        assert_eq!(Feature::all_premium().len(), 25);
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
            // A premium key is always stamped when set/validated; these
            // account-focused tests model that reality so the key path (which
            // now requires a recent validation, like the account path) doesn't
            // spuriously read as lapsed.
            last_validated: Some(now_iso()),
            hardware_fingerprint: "test".into(),
            account_premium,
            account_premium_expires,
            account_premium_checked,
            qobuz_proxy_first: false,
            modules: vec![],
            session_conflict: None,
            key_premium_marker: tier == Tier::Premium,
        }
    }

    fn key_state(
        tier: Tier,
        expires_at: Option<String>,
        last_validated: Option<String>,
    ) -> LicenseState {
        LicenseState {
            tier,
            license_key: Some("TUNE-TEST-KEY".into()),
            expires_at,
            last_validated,
            hardware_fingerprint: "test".into(),
            account_premium: false,
            account_premium_expires: None,
            account_premium_checked: None,
            qobuz_proxy_first: false,
            modules: vec![],
            session_conflict: None,
            key_premium_marker: tier == Tier::Premium,
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

    // ---- effective_tier / license key (live grace) ----

    #[test]
    fn effective_premium_key_recently_validated() {
        // Premium key validated now → Premium.
        assert_eq!(
            effective_tier(&key_state(Tier::Premium, None, Some(now_iso()))),
            Tier::Premium
        );
    }

    #[test]
    fn effective_key_survives_within_grace() {
        // Premium key last validated 10 days ago (< 30-day grace) → Premium.
        // This is JP's case: the cloud rejects the key but a valid key must not
        // be revoked on a transient `license_valid:false` verdict.
        assert_eq!(
            effective_tier(&key_state(Tier::Premium, None, Some(past_iso(10)))),
            Tier::Premium
        );
    }

    #[test]
    fn effective_key_free_when_grace_expired() {
        // Not validated for 40 days (past the 30-day grace) → degrade to Free.
        assert_eq!(
            effective_tier(&key_state(Tier::Premium, None, Some(past_iso(40)))),
            Tier::Free
        );
    }

    #[test]
    fn effective_key_free_when_never_validated() {
        assert_eq!(
            effective_tier(&key_state(Tier::Premium, None, None)),
            Tier::Free
        );
    }

    #[test]
    fn effective_key_free_when_expiry_past() {
        // A genuine past expiry revokes even if recently validated.
        assert_eq!(
            effective_tier(&key_state(
                Tier::Premium,
                Some(past_iso(1)),
                Some(now_iso())
            )),
            Tier::Free
        );
    }

    #[test]
    fn effective_key_premium_when_expiry_future() {
        assert_eq!(
            effective_tier(&key_state(
                Tier::Premium,
                Some(future_iso(30)),
                Some(now_iso())
            )),
            Tier::Premium
        );
    }

    // ---- effective_tier / single-session conflict (floating license) ----

    #[test]
    fn effective_free_during_session_conflict_even_with_valid_key() {
        // The key is otherwise premium (validated now, no expiry) but another
        // server holds the session → suppressed to Free here. This is the whole
        // point of the floating-license single-session rule.
        let mut s = key_state(Tier::Premium, None, Some(now_iso()));
        s.session_conflict = Some(SessionConflict {
            active_server: Some("Maison Paris".into()),
            active_since: Some(now_iso()),
        });
        assert_eq!(effective_tier(&s), Tier::Free);
    }

    #[test]
    fn effective_free_during_session_conflict_even_with_account_premium() {
        // Account (SSO) premium is likewise gated off while another server holds
        // the session — the conflict overrides both premium sources.
        let mut s = state(Tier::Free, true, None, Some(now_iso()));
        s.session_conflict = Some(SessionConflict {
            active_server: None,
            active_since: None,
        });
        assert_eq!(effective_tier(&s), Tier::Free);
    }

    #[test]
    fn effective_premium_restored_when_conflict_clears() {
        // Clearing the conflict brings premium straight back — the key/account
        // were never touched, only gated.
        let mut s = key_state(Tier::Premium, None, Some(now_iso()));
        s.session_conflict = Some(SessionConflict {
            active_server: None,
            active_since: None,
        });
        assert_eq!(effective_tier(&s), Tier::Free);
        s.session_conflict = None;
        assert_eq!(effective_tier(&s), Tier::Premium);
    }

    #[tokio::test]
    async fn set_and_clear_session_conflict_gates_the_manager() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend);

        mgr.set_license_key("TUNE-TEST-1234").await.unwrap();
        // A stored key is pending (Free) until the licensing server confirms it;
        // mirror that online validation so the tier is genuinely Premium before
        // we exercise the session-conflict gating.
        mgr.update_from_server(Tier::Premium, None).await;
        assert!(mgr.is_premium().await, "premium after validated activation");

        mgr.set_session_conflict(Some("Maison 2".into()), None)
            .await;
        assert!(
            !mgr.is_premium().await,
            "premium suppressed while held elsewhere"
        );
        assert!(mgr.session_conflict().await.is_some());
        // The key itself is untouched — the tier snapshot is Free but the key
        // survives underneath.
        assert!(mgr.license_state().await.license_key.is_some());

        mgr.clear_session_conflict().await;
        assert!(mgr.is_premium().await, "premium restored once reclaimed");
        assert!(mgr.session_conflict().await.is_none());
    }

    #[test]
    fn license_state_parses_without_session_conflict() {
        // Retro-compat: a cached/legacy state blob without the field → None.
        let json = r#"{
            "tier": "premium",
            "license_key": "TUNE-X",
            "expires_at": null,
            "last_validated": null,
            "hardware_fingerprint": "test"
        }"#;
        let state: LicenseState = serde_json::from_str(json).unwrap();
        assert!(state.session_conflict.is_none());
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
        let mgr = LicenseManager::new_with_limit(backend, 3);
        assert_eq!(mgr.tier().await, Tier::Free);
        assert!(!mgr.is_premium().await);
        assert!(!mgr.check_feature(Feature::DspEq).await);
        // Free tier is capped at the configured limit (3 here).
        assert_eq!(mgr.free_zone_limit(), 3);
        assert!(mgr.check_zone_limit(2).await);
        assert!(!mgr.check_zone_limit(3).await);
    }

    #[tokio::test]
    async fn free_zone_limit_defaults_to_three() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend);
        assert_eq!(mgr.free_zone_limit(), 3);
    }

    #[tokio::test]
    async fn set_key_is_pending_until_validated_then_clear() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend);

        // Storing a key must NOT grant Premium on its own: any string would
        // otherwise unlock Premium locally for the whole grace window with no
        // server round-trip. It stays Free ("pending") until validated online.
        mgr.set_license_key("TUNE-TEST-1234").await.unwrap();
        assert_eq!(mgr.tier().await, Tier::Free, "pending key must stay Free");
        assert!(!mgr.is_premium().await);
        assert!(!mgr.check_feature(Feature::CloudRelay).await);

        // A genuine server confirmation (heartbeat / on-demand validate) promotes it.
        mgr.update_from_server(Tier::Premium, None).await;
        assert_eq!(mgr.tier().await, Tier::Premium);
        assert!(mgr.is_premium().await);
        assert!(mgr.check_feature(Feature::CloudRelay).await);
        assert!(mgr.check_zone_limit(100).await);

        mgr.clear_license().await;
        assert_eq!(mgr.tier().await, Tier::Free);
        assert!(!mgr.is_premium().await);
    }

    #[tokio::test]
    async fn premium_tier_without_validation_is_inactive_across_restart() {
        // Simulate a stale/forged state: tier=premium persisted but no validation
        // timestamp. On (re)load the effective tier must be Free, not Premium.
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let settings = SettingsRepo::with_backend(backend.clone());
        settings.set("license_key", "TUNE-FORGED-0000").unwrap();
        settings.set("license_tier", "premium").unwrap();
        // Note: deliberately NO license_last_validated.

        let mgr = LicenseManager::new(backend);
        assert_eq!(
            mgr.tier().await,
            Tier::Free,
            "premium without a validation timestamp must degrade to Free"
        );
        assert!(!mgr.is_premium().await);
    }

    // ---- qobuz_proxy_first (founder endpoint order) ----

    #[test]
    fn license_state_parses_without_qobuz_proxy_first() {
        // Retro-compat: older servers / cached states without the field → false.
        let json = r#"{
            "tier": "free",
            "license_key": null,
            "expires_at": null,
            "last_validated": null,
            "hardware_fingerprint": "test"
        }"#;
        let state: LicenseState = serde_json::from_str(json).unwrap();
        assert!(!state.qobuz_proxy_first);
    }

    #[test]
    fn license_state_parses_with_qobuz_proxy_first() {
        let json = r#"{
            "tier": "free",
            "license_key": null,
            "expires_at": null,
            "last_validated": null,
            "hardware_fingerprint": "test",
            "qobuz_proxy_first": true
        }"#;
        let state: LicenseState = serde_json::from_str(json).unwrap();
        assert!(state.qobuz_proxy_first);
    }

    #[tokio::test]
    async fn set_qobuz_proxy_first_persists_and_updates_state() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let mgr = LicenseManager::new(backend.clone());

        // Default: direct-first.
        assert!(!mgr.qobuz_proxy_first().await);

        mgr.set_qobuz_proxy_first(true).await;
        assert!(mgr.qobuz_proxy_first().await);
        assert!(mgr.license_state().await.qobuz_proxy_first);

        // Persisted in settings (same pattern as mozaik_premium).
        let settings = SettingsRepo::with_backend(backend.clone());
        assert_eq!(
            settings.get("qobuz_proxy_first").unwrap().as_deref(),
            Some("true")
        );

        // A new manager over the same backend reloads the flag.
        let mgr2 = LicenseManager::new(backend);
        assert!(mgr2.qobuz_proxy_first().await);

        // Re-validation can revoke it.
        mgr2.set_qobuz_proxy_first(false).await;
        assert!(!mgr2.qobuz_proxy_first().await);
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

    // -----------------------------------------------------------------------
    // Grâce hors ligne : la rendre VISIBLE (#1999)
    //
    // Didier (fil forum 1491) ne demandait pas que la grâce change — il
    // demandait à savoir qu'elle existe. Ces tests décrivent ce que
    // l'utilisateur doit pouvoir lire, et surtout ils verrouillent le fait que
    // rien de ce qui est *appliqué* n'a bougé.
    // -----------------------------------------------------------------------

    fn hours_ago_iso(hours: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::hours(hours))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    }

    #[test]
    fn grace_fenetre_de_quatorze_jours_annoncee() {
        // Le chiffre affiché doit venir du code, jamais d'un commentaire.
        let g = offline_grace(&key_state(Tier::Premium, None, Some(now_iso()))).unwrap();
        assert_eq!(g.total_days, 14, "la fenêtre annoncée est celle du code");
        assert_eq!(LicenseManager::offline_grace_days(), 14);
    }

    #[test]
    fn grace_silencieuse_quand_la_validation_est_fraiche() {
        // Un serveur qui a manqué un battement ou deux n'a rien à signaler :
        // afficher une alerte là serait du bruit, pas de l'information.
        let g = offline_grace(&key_state(Tier::Premium, None, Some(hours_ago_iso(6)))).unwrap();
        assert_eq!(g.phase, GracePhase::Ok);
        assert_eq!(g.days_remaining, 14);
        assert_eq!(g.days_since_validation, 0);
    }

    #[test]
    fn grace_entree_apres_deux_jours_sans_revalidation() {
        // 48 battements horaires ratés : ce n'est plus un hoquet. On le dit.
        let g = offline_grace(&key_state(Tier::Premium, None, Some(past_iso(3)))).unwrap();
        assert_eq!(g.phase, GracePhase::Grace, "état visible dès J+2");
        assert_eq!(g.days_since_validation, 3);
    }

    #[test]
    fn grace_jour_restant_correct_a_chaque_etape() {
        // Le compte à rebours que lit l'utilisateur. Un décalage d'un jour ici
        // et la promesse affichée est fausse.
        for (jours_ecoules, restant) in [(0, 14), (1, 13), (7, 7), (13, 1)] {
            let g = offline_grace(&key_state(
                Tier::Premium,
                None,
                Some(past_iso(jours_ecoules)),
            ))
            .unwrap();
            assert_eq!(
                g.days_remaining, restant,
                "à J+{jours_ecoules} il doit rester {restant} jour(s), lu {}",
                g.days_remaining
            );
        }
    }

    #[test]
    fn grace_le_terme_annonce_est_la_derniere_validation_plus_quatorze_jours() {
        let depuis = past_iso(5);
        let g = offline_grace(&key_state(Tier::Premium, None, Some(depuis.clone()))).unwrap();
        assert_eq!(g.since.as_deref(), Some(depuis.as_str()));
        let attendu = (parse_timestamp(&depuis).unwrap() + chrono::Duration::days(14))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        assert_eq!(g.until.as_deref(), Some(attendu.as_str()));
    }

    #[test]
    fn grace_expiree_au_dela_de_quatorze_jours() {
        let g = offline_grace(&key_state(Tier::Premium, None, Some(past_iso(20)))).unwrap();
        assert_eq!(g.phase, GracePhase::Expired);
        assert_eq!(g.days_remaining, 0, "jamais de compte négatif à l'écran");
    }

    #[test]
    fn grace_expiree_quand_le_premium_na_jamais_ete_valide() {
        // Cas « premium sans last_validated » : dégradé d'emblée. L'utilisateur
        // doit lire pourquoi, pas deviner.
        let g = offline_grace(&key_state(Tier::Premium, None, None)).unwrap();
        assert_eq!(g.phase, GracePhase::Expired);
        assert_eq!(g.since, None);
        assert_eq!(g.until, None);
    }

    #[test]
    fn grace_muette_pour_un_utilisateur_gratuit() {
        // Rien à annoncer : pas de droits premium en jeu.
        assert!(offline_grace(&state(Tier::Free, false, None, None)).is_none());
    }

    #[test]
    fn grace_muette_quand_labonnement_est_reellement_echu() {
        // Une date de fin dépassée n'est PAS une coupure réseau. La présenter
        // comme une grâce ferait croire à l'utilisateur qu'il suffit de se
        // reconnecter — ce serait un mensonge.
        assert!(
            offline_grace(&key_state(
                Tier::Premium,
                Some(past_iso(1)),
                Some(now_iso())
            ))
            .is_none()
        );
        assert!(
            offline_grace(&state(Tier::Free, true, Some(past_iso(1)), Some(now_iso()))).is_none()
        );
    }

    #[test]
    fn grace_suit_aussi_le_compte_sso() {
        let g = offline_grace(&state(Tier::Free, true, None, Some(past_iso(4)))).unwrap();
        assert_eq!(g.source, GraceSource::Account);
        assert_eq!(g.phase, GracePhase::Grace);
        assert_eq!(g.days_remaining, 10);
    }

    #[test]
    fn grace_decrit_la_source_qui_tient_le_plus_longtemps() {
        // `effective_tier` est un OU : c'est la source la mieux revalidée qui
        // gouverne, donc c'est elle qu'il faut décrire. Annoncer l'autre
        // afficherait une expiration qui n'aura pas lieu.
        let mut s = key_state(Tier::Premium, None, Some(past_iso(10)));
        s.account_premium = true;
        s.account_premium_checked = Some(now_iso());
        let g = offline_grace(&s).unwrap();
        assert_eq!(g.source, GraceSource::Account);
        assert_eq!(g.phase, GracePhase::Ok);
    }

    #[test]
    fn grace_ne_change_rien_a_ce_qui_est_applique() {
        // LE test de non-durcissement. Pour chaque état décrit, le tier
        // effectif reste exactement celui d'avant #1999 : la visibilité n'est
        // pas une politique.
        let cas: Vec<(&str, LicenseState, Tier)> = vec![
            (
                "frais",
                key_state(Tier::Premium, None, Some(now_iso())),
                Tier::Premium,
            ),
            (
                "J+3 (annoncé)",
                key_state(Tier::Premium, None, Some(past_iso(3))),
                Tier::Premium,
            ),
            (
                "J+13 (dernier jour)",
                key_state(Tier::Premium, None, Some(past_iso(13))),
                Tier::Premium,
            ),
            (
                "J+20 (écoulée)",
                key_state(Tier::Premium, None, Some(past_iso(20))),
                Tier::Free,
            ),
            (
                "jamais validé",
                key_state(Tier::Premium, None, None),
                Tier::Free,
            ),
            (
                "compte J+4",
                state(Tier::Free, true, None, Some(past_iso(4))),
                Tier::Premium,
            ),
        ];
        for (nom, s, attendu) in cas {
            assert_eq!(
                effective_tier(&s),
                attendu,
                "{nom} : la grâce visible ne doit RIEN changer au tier appliqué"
            );
        }
    }

    #[test]
    fn grace_annoncee_tant_que_le_premium_tient_encore() {
        // Corollaire du précédent, dit dans l'autre sens : tant que la phase
        // n'est pas `Expired`, le premium est intact. C'est ce qui permet
        // d'écrire un message rassurant sans mentir.
        for jours in [0, 1, 2, 5, 13] {
            let s = key_state(Tier::Premium, None, Some(past_iso(jours)));
            let g = offline_grace(&s).unwrap();
            assert_ne!(g.phase, GracePhase::Expired, "J+{jours}");
            assert_eq!(effective_tier(&s), Tier::Premium, "J+{jours}");
        }
    }

    #[test]
    fn grace_ne_laisse_fuir_aucune_donnee_sensible() {
        // Le JSON part vers le navigateur : il ne doit porter que des dates et
        // des compteurs, jamais la clé ni un identifiant d'achat.
        let mut s = key_state(Tier::Premium, None, Some(past_iso(3)));
        s.license_key = Some("TUNE-SECRET-0000-1111".into());
        s.account_premium_expires = Some(future_iso(30));
        let v = serde_json::to_value(offline_grace(&s).unwrap()).unwrap();
        let json = v.to_string();
        assert!(
            !json.contains("TUNE-SECRET"),
            "la clé ne doit jamais apparaître : {json}"
        );
        // Liste blanche stricte : un champ ajouté par mégarde tombe ici.
        let mut champs: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        champs.sort_unstable();
        assert_eq!(
            champs,
            [
                "days_remaining",
                "days_since_validation",
                "phase",
                "since",
                "source",
                "total_days",
                "until",
            ],
            "seuls des dates et des compteurs sortent d'ici"
        );
    }

    #[test]
    fn grace_contrat_json_stable_pour_le_client() {
        // Les noms de champs que le client web lit. Les renommer casse l'UI en
        // silence — ce test le rend bruyant.
        let v = serde_json::to_value(offline_grace(&key_state(
            Tier::Premium,
            None,
            Some(past_iso(3)),
        )))
        .unwrap();
        for champ in [
            "phase",
            "source",
            "since",
            "until",
            "days_remaining",
            "total_days",
            "days_since_validation",
        ] {
            assert!(v.get(champ).is_some(), "champ manquant : {champ} dans {v}");
        }
        assert_eq!(v["phase"], "grace");
        assert_eq!(v["source"], "key");
    }

    #[tokio::test]
    async fn grace_expliquee_meme_apres_un_redemarrage_hors_ligne() {
        // Le cas qui compte le plus : la grâce est écoulée, le premium a
        // disparu, le serveur redémarre. `new_with_limit` dégrade `tier` en
        // Free au chargement — plus rien en mémoire ne disait qu'il y avait eu
        // une licence, et l'utilisateur restait sans explication. Le marqueur
        // persisté rend la retombée explicable.
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let settings = SettingsRepo::with_backend(backend.clone());
        settings.set("license_key", "TUNE-TEST-1234").unwrap();
        settings.set("license_tier", "premium").unwrap();
        settings
            .set("license_last_validated", &past_iso(20))
            .unwrap();

        let mgr = LicenseManager::new(backend);
        // Politique inchangée : toujours Free au bout de 14 jours.
        assert!(!mgr.is_premium().await);

        let g = mgr
            .offline_grace()
            .await
            .expect("la retombée doit s'expliquer");
        assert_eq!(g.phase, GracePhase::Expired);
        assert_eq!(g.source, GraceSource::Key);
        assert_eq!(g.days_remaining, 0);
        assert_eq!(g.total_days, 14);
        assert!(g.since.is_some(), "on sait depuis quand : {g:?}");
    }

    #[tokio::test]
    async fn grace_se_rearme_seule_au_retour_du_reseau() {
        // Le point que Didier redoute : faut-il faire quelque chose ? Non. Le
        // battement horaire revalide, `last_validated` est réhorodaté, et la
        // fenêtre repart à 14 jours sans aucun geste de l'utilisateur.
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        // On simule une machine hors ligne depuis 10 jours en écrivant l'état
        // que le serveur aurait relu au démarrage — aucun appel réseau réel.
        let settings = SettingsRepo::with_backend(backend.clone());
        settings.set("license_key", "TUNE-TEST-1234").unwrap();
        settings.set("license_tier", "premium").unwrap();
        settings
            .set("license_last_validated", &past_iso(10))
            .unwrap();

        let mgr = LicenseManager::new(backend);
        let avant = mgr.offline_grace().await.unwrap();
        assert_eq!(avant.phase, GracePhase::Grace);
        assert_eq!(avant.days_remaining, 4);
        assert!(mgr.is_premium().await, "toujours premium pendant la grâce");

        // Le réseau revient : un seul aller-retour du battement suffit.
        mgr.update_from_server(Tier::Premium, None).await;

        let apres = mgr.offline_grace().await.unwrap();
        assert_eq!(apres.phase, GracePhase::Ok, "la grâce se réarme seule");
        assert_eq!(
            apres.days_remaining, 14,
            "fenêtre remise à plein après revalidation"
        );
        assert_eq!(apres.days_since_validation, 0);
        assert!(mgr.is_premium().await);
    }
}
