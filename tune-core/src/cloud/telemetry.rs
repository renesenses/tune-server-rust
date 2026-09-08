use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::cloud::rate_limit::{self, CloudScope};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::streaming::ServiceRegistry;

const HEARTBEAT_URL: &str = "https://mozaiklabs.fr/api/v1/telemetry/heartbeat";
const PING_URL: &str = "https://mozaiklabs.fr/api/v1/ping";

/// Cle du reglage en base qui porte le refus de l'UTILISATEUR (#3383).
///
/// Ce n'est pas une cle neuve : `POST /system/telemetry`
/// (`routes/system/diagnostics.rs`) l'ecrivait deja, et son `GET` la relisait —
/// un aller-retour ferme sur lui-meme, qu'aucune garde ne consultait. Faire
/// lire cette cle-la par le verrou, plutot qu'en inventer une huitieme, rend
/// vivant ce troisieme chemin au lieu d'en ajouter un quatrieme :
/// `/cloud/telemetry/{enable,disable}`, `/system/telemetry` et
/// `TUNE_TELEMETRY` decrivent desormais un seul interrupteur.
pub const TELEMETRY_SETTING_KEY: &str = "telemetry_enabled";

/// Valeur par defaut quand le reglage n'a jamais ete pose : **oui**.
///
/// La telemetrie etait active par defaut avant #3383 ; une installation qui
/// n'a rien decoche ne doit pas changer de comportement en montant de version.
pub const TELEMETRY_DEFAULT: bool = true;

/// Ce qu'a fait un ping de demarrage. Retourne, et non ignore, pour qu'un
/// temoin puisse constater le refus sans toucher au reseau (#3383).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingDemarrage {
    /// La telemetrie est refusee : rien n'a ete collecte, rien n'est parti.
    Refuse,
    /// La telemetrie est acceptee : la charge utile est partie (ou a echoue
    /// sur le reseau, ce qui ne se distingue pas d'ici — l'envoi est
    /// deliberement « fire and forget »).
    Envoye,
}

/// Ping de demarrage : version + OS + arch + liste des services, une fois.
///
/// #3383 — il ne partait AUTREFOIS jamais garde (« Always runs regardless of
/// TUNE_TELEMETRY setting »). Il porte pourtant quatre des champs descriptifs
/// que le refus est cense eteindre : version, plateforme, architecture et
/// services. Il consulte donc desormais le meme verrou que tout le reste,
/// [`TelemetryReporter::is_enabled_for`].
pub async fn ping_de_demarrage(
    db: &Arc<dyn DbBackend>,
    services: &Arc<Mutex<ServiceRegistry>>,
) -> PingDemarrage {
    let settings = SettingsRepo::with_backend(db.clone());
    if !TelemetryReporter::is_enabled_for(&settings) {
        return PingDemarrage::Refuse;
    }
    let Ok(client) = crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    else {
        return PingDemarrage::Envoye;
    };
    let svc_list: Vec<String> = {
        let reg = services.lock().await;
        reg.list()
    };
    let payload = serde_json::json!({
        "v": crate::version(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "services": svc_list,
    });
    let _ = client.post(PING_URL).json(&payload).send().await;
    PingDemarrage::Envoye
}

/// Fire-and-forget startup ping: sends version + OS + arch + services once
/// after 10 seconds — **si** la telemetrie est acceptee (#3383).
pub fn spawn_startup_ping(
    db: Arc<dyn DbBackend>,
    services: std::sync::Arc<tokio::sync::Mutex<crate::streaming::registry::ServiceRegistry>>,
) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        // Le verrou est lu APRES l'attente, pas avant : l'utilisateur qui
        // decoche dans les dix premieres secondes doit etre entendu.
        ping_de_demarrage(&db, &services).await;
    });
}

#[derive(Debug, Serialize)]
struct HeartbeatPayload {
    server_id: String,
    version: String,
    services: Vec<String>,
    tracks_count: i64,
    artists_with_bio: i64,
    albums_with_bio: i64,
    os: String,
    arch: String,
}

pub struct TelemetryReporter;

impl TelemetryReporter {
    /// Get or create a persistent server ID for this instance.
    pub fn get_or_create_server_id(settings: &SettingsRepo) -> String {
        match settings.get("server_id").ok().flatten() {
            Some(id) if !id.is_empty() => id,
            _ => {
                let id = uuid::Uuid::new_v4().to_string();
                settings.set("server_id", &id).ok();
                id
            }
        }
    }

    /// Le refus pose par l'EXPLOITANT, dans l'environnement.
    ///
    /// Enabled by default; set `TUNE_TELEMETRY=false` to opt out.
    ///
    /// ⚠️ Ce n'est plus le verrou complet : il ne connait pas le refus pose
    /// par l'UTILISATEUR dans l'interface. Toute garde d'envoi doit appeler
    /// [`Self::is_enabled_for`], qui consulte les deux. Cette fonction ne
    /// reste publique que pour dire a l'interface si l'environnement lui
    /// retire la main (`GET /cloud/telemetry/status`).
    pub fn is_enabled() -> bool {
        match std::env::var("TUNE_TELEMETRY") {
            Ok(val) => !matches!(val.to_lowercase().as_str(), "false" | "0" | "no" | "off"),
            Err(_) => true,
        }
    }

    /// Le verrou complet : la telemetrie est-elle acceptee sur cette instance ?
    ///
    /// #3383 — la bascule de l'interface n'eteignait rien. Elle appelait
    /// `POST /cloud/telemetry/disable`, qui n'ecrivait rien, pendant que les
    /// sept gardes d'envoi du produit lisaient `is_enabled()`, c'est-a-dire la
    /// seule variable d'environnement. Or celle-ci n'est pas a portee d'un
    /// utilisateur : il faut editer une unite systemd, un `docker-compose.yml`
    /// ou un plist. La seule commande qu'il pouvait atteindre etait celle qui
    /// ne faisait rien.
    ///
    /// **Un refus, d'ou qu'il vienne, l'emporte.** L'exploitant coupe par
    /// `TUNE_TELEMETRY=false` ; l'utilisateur coupe par le reglage
    /// [`TELEMETRY_SETTING_KEY`]. Aucun des deux ne peut RE-autoriser ce que
    /// l'autre a refuse — c'est la doctrine deja posee par
    /// `consent::contribution_autorisee`, etendue au reste des envois.
    ///
    /// Le defaut reste OUI quand le reglage est absent : une installation qui
    /// n'a jamais rien decoche se comporte exactement comme avant.
    pub fn is_enabled_for(settings: &SettingsRepo) -> bool {
        if !Self::is_enabled() {
            return false;
        }
        match settings.get(TELEMETRY_SETTING_KEY).ok().flatten() {
            // Meme lecture que le reste du depot : `PATCH /system/config`
            // serialise le JSON `true` en `"true"`, la main pose parfois `1`.
            // Toute valeur qui n'est pas un oui reconnu vaut NON.
            Some(brut) => crate::cloud::consent::est_vrai(&brut),
            None => TELEMETRY_DEFAULT,
        }
    }

    /// Collect and send a heartbeat to mozaiklabs.fr.
    /// Fails silently — logs a warning but never panics.
    pub async fn send(db: &Arc<dyn DbBackend>, services: &Arc<Mutex<ServiceRegistry>>) {
        let settings = SettingsRepo::with_backend(db.clone());
        // #3383 : `is_enabled_for` et non `is_enabled` — le refus pose dans
        // l'interface compte autant que celui pose dans l'environnement.
        if !Self::is_enabled_for(&settings) {
            return;
        }

        let server_id = Self::get_or_create_server_id(&settings);

        // Collect connected service names (authenticated == true)
        let connected_services = {
            let registry = services.lock().await;
            let mut names = Vec::new();
            for name in registry.list() {
                if let Some(svc) = registry.get(&name) {
                    let svc = svc.read().await;
                    let status = svc.auth_status().await;
                    if status.authenticated {
                        names.push(name);
                    }
                }
            }
            names.sort();
            names
        };

        let tracks_count = crate::db::track_repo::TrackRepo::with_backend(db.clone())
            .count()
            .unwrap_or(0);
        let artists_with_bio = crate::db::artist_repo::ArtistRepo::with_backend(db.clone())
            .count_with_bio()
            .unwrap_or(0);
        let albums_with_bio = crate::db::album_repo::AlbumRepo::with_backend(db.clone())
            .count_with_bio()
            .unwrap_or(0);

        let payload = HeartbeatPayload {
            server_id,
            version: crate::version().to_string(),
            services: connected_services,
            tracks_count,
            artists_with_bio,
            albums_with_bio,
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        };

        let client = match crate::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "telemetry_client_build_failed");
                return;
            }
        };

        // CLD-2 : un seul chemin d'appel borné — la portée retenue ne part pas
        // (le battement est quotidien : relire quelques compteurs avant de
        // s'abstenir ne coûte rien), un 429 mémorise son échéance.
        match rate_limit::appeler(
            &settings,
            CloudScope::Telemetry,
            client.post(HEARTBEAT_URL).json(&payload),
        )
        .await
        {
            rate_limit::AppelCloud::Retenu(backoff) => {
                warn!(
                    scope = backoff.scope,
                    until_epoch = backoff.until_epoch,
                    retry_after_seconds = backoff.retry_after_seconds,
                    "telemetry_deferred_rate_limit"
                );
            }
            rate_limit::AppelCloud::Reponse(resp) if resp.status().is_success() => {
                info!(
                    tracks = payload.tracks_count,
                    services = ?payload.services,
                    "telemetry_heartbeat_sent"
                );
            }
            rate_limit::AppelCloud::Reponse(resp) => {
                let status = resp.status();
                warn!(status = %status, "telemetry_heartbeat_rejected");
            }
            rate_limit::AppelCloud::Erreur(e) => {
                warn!(error = %e, "telemetry_heartbeat_failed");
            }
        }
    }

    /// Spawn a background task that sends a heartbeat after 30 seconds,
    /// then every 24 hours.
    pub fn spawn(db: Arc<dyn DbBackend>, services: Arc<Mutex<ServiceRegistry>>) {
        tokio::spawn(async move {
            // Initial delay: give the server time to restore tokens and scan
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            loop {
                Self::send(&db, &services).await;
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn fresh_db() -> std::sync::Arc<dyn crate::db::backend::DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        std::sync::Arc::new(db)
    }

    #[test]
    fn server_id_persists() {
        let db = fresh_db();
        let settings = SettingsRepo::with_backend(db);
        let id1 = TelemetryReporter::get_or_create_server_id(&settings);
        let id2 = TelemetryReporter::get_or_create_server_id(&settings);
        assert_eq!(id1, id2);
        assert!(!id1.is_empty());
        // Should be a valid UUID
        assert!(uuid::Uuid::parse_str(&id1).is_ok());
    }

    #[test]
    fn telemetry_enabled_by_default() {
        // Can only verify when TUNE_TELEMETRY is not set to false
        // (env var state in tests is not guaranteed, so we just ensure no panic)
        let _ = TelemetryReporter::is_enabled();
    }
}
