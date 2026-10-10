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

/// Tune Circle T4 (#5327) : l'écoute chez un contact, réglage LOCAL fermé par
/// défaut (décision produit du 10/10). Seule la valeur `"true"` l'ouvre.
///
/// Fermé, ce serveur refuse `relay.circle_stream_request` (il ne sert pas sa
/// bibliothèque à un contact) ET le greffon `circle` refuse d'écouter chez un
/// contact. Indépendant du pont, qui a son propre interrupteur
/// (`TUNE_BRIDGE_CIRCLE_LISTEN`) : le propriétaire peut dire non chez lui.
/// L'écoute à distance du propriétaire (`relay.stream_request`) n'en dépend
/// pas.
///
/// Hors du module `relay` : le greffon le lit dans TOUTES les compositions.
pub const CLE_ECOUTE_DE_CERCLE: &str = "circle_listen_enabled";

/// [`CLE_ECOUTE_DE_CERCLE`] vaut-il `"true"` ? Absent, illisible ou autre :
/// fermé.
pub fn ecoute_de_cercle_ouverte(reglages: &crate::db::settings_repo::SettingsRepo) -> bool {
    reglages
        .get(CLE_ECOUTE_DE_CERCLE)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true")
}
