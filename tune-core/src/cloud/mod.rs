pub mod bio_sync;
pub mod community;
pub mod community_sync;
pub mod consent;
pub mod digest;
pub mod library_reconcile;
pub mod library_sync;
pub mod metadata_proposals;
pub mod playlist_hub;
pub mod plugins;
pub mod proprietaire;
pub mod rate_limit;
pub mod recommendations;
pub mod refusal;
#[cfg(feature = "cloud-relay")]
pub mod relay;
pub mod sauvegarde_config;
pub mod sso;
pub mod support;
pub mod telemetry;
pub mod tune_tested;

/// Posé par le relais (`relay`) sur CHAQUE requête qu'il rejoue en local,
/// après les en-têtes venus du distant : un appelant distant peut ajouter des
/// en-têtes, jamais retirer celui-ci. Une route qui ne doit servir que le
/// navigateur de CETTE machine (ouvrir un dossier dans le gestionnaire de
/// fichiers, tune-web-client#1875) le lit pour refuser : vue du serveur, une
/// requête relayée arrive de 127.0.0.1 comme le navigateur local.
///
/// Hors du module `relay`, qui n'existe qu'avec la fonction `cloud-relay` : la
/// route qui refuse doit le connaître dans TOUTES les compositions.
pub const ENTETE_RELAIS: &str = "x-tune-relais";
