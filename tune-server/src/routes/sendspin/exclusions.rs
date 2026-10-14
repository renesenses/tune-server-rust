//! La LISTE D'EXCLUSION des enceintes Sendspin (#3326, décision de Bertrand
//! du 10/10/2026).
//!
//! Une enceinte qui appartient à un autre serveur (Music Assistant, par
//! exemple) ne doit pas être disputée par Tune. Chaque entrée est un
//! identifiant d'enceinte :
//! - son `client_id` (clé publique, seule identité durable du protocole,
//!   connue dès le `client/init`) ;
//! - ou l'identifiant de l'ANNONCE mDNS (`DiscoveredDevice::id`), pour une
//!   enceinte jamais contactée : l'annonce ne porte aucune identité.
//!
//! Effets : la boucle de composition ne compose jamais vers une enceinte
//! exclue ; une connexion entrante d'une enceinte exclue est fermée dès son
//! `client/init`, sans `server/init` (échec silencieux, comme tout refus de
//! la phase de poignée de main dans la spécification) ; une session en cours
//! est close au moment de l'exclusion.
//!
//! Stockée dans les réglages (`sendspin_exclusions`, tableau JSON trié).
use std::collections::BTreeSet;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

pub const CLE_REGLAGE: &str = "sendspin_exclusions";
/// Un identifiant d'enceinte est court ; borne contre un corps abusif.
pub const LONGUEUR_MAX: usize = 200;
/// Et la liste aussi.
pub const NOMBRE_MAX: usize = 500;

/// La liste en vigueur. Un réglage illisible vaut une liste vide, et le dit.
pub fn lire(db: &Arc<dyn DbBackend>) -> BTreeSet<String> {
    match SettingsRepo::with_backend(db.clone()).get(CLE_REGLAGE) {
        Ok(Some(v)) => serde_json::from_str::<Vec<String>>(&v)
            .map(|l| l.into_iter().collect())
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "sendspin_exclusions_illisibles");
                BTreeSet::new()
            }),
        Ok(None) => BTreeSet::new(),
        Err(e) => {
            tracing::warn!(error = %e, "sendspin_exclusions_indisponibles");
            BTreeSet::new()
        }
    }
}

pub fn ecrire(db: &Arc<dyn DbBackend>, liste: &BTreeSet<String>) -> Result<(), String> {
    let texte =
        serde_json::to_string(&liste.iter().collect::<Vec<_>>()).map_err(|e| e.to_string())?;
    SettingsRepo::with_backend(db.clone()).set(CLE_REGLAGE, &texte)
}

/// Un identifiant recevable : non vide, borné, sans caractère de contrôle.
#[must_use]
pub fn identifiant_valide(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= LONGUEUR_MAX && !id.chars().any(char::is_control)
}
