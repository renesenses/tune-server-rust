//! La session renouvelée tant qu'elle sert (rc14), sur la session longue de
//! #9445.
//!
//! UNE autorité : l'inscription de la session longue (`auth_session_token:{sid}`
//! dans `settings`, `crate::auth`). Le JWT d'accès dure 24 h et porte son
//! `sid` ; le jeton de renouvellement
//! ([`tune_core::db::jetons_de_renouvellement`]) appartient à la famille de
//! même identifiant, et `POST /auth/refresh` reporte l'échéance de
//! l'inscription à 30 jours. Désinscrire une session révoque sa famille ;
//! une famille révoquée (vol détecté) désinscrit sa session.
//!
//! Ce module tient l'horloge, la révocation des familles et le signal qui
//! ferme les WebSocket : chaque WebSocket ouverte avec un JWT de session s'y
//! abonne et se ferme si SA session ne vaut plus. Elle n'est PAS coupée à
//! l'expiration du jeton d'accès : la poignée de main l'a authentifiée, et le
//! client renouvelle son jeton pour ses appels REST et sa prochaine
//! reconnexion.

use std::sync::{Arc, LazyLock};

use tune_core::db::backend::DbBackend;
use tune_core::db::jetons_de_renouvellement::JetonsDeRenouvellementRepo;

static REVOCATIONS: LazyLock<tokio::sync::watch::Sender<u64>> =
    LazyLock::new(|| tokio::sync::watch::channel(0).0);

/// L'heure Unix, en secondes.
pub fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Un récepteur qui change à chaque révocation faite par ce processus.
pub fn abonner_aux_revocations() -> tokio::sync::watch::Receiver<u64> {
    REVOCATIONS.subscribe()
}

/// Prévient les WebSocket ouvertes qu'une session a pu tomber.
pub fn signaler_revocation() {
    REVOCATIONS.send_modify(|n| *n = n.wrapping_add(1));
}

/// Révoque la famille de renouvellement `sid` (sa session vient d'être
/// désinscrite).
pub fn revoquer_famille(db: &Arc<dyn DbBackend>, sid: &str) {
    let n = JetonsDeRenouvellementRepo::with_backend(db.clone()).revoquer_famille(sid, maintenant());
    tracing::info!(session = %sid, jetons = n, "session_revoquee");
    signaler_revocation();
}

/// Révoque toutes les familles (secrets renouvelés).
pub fn revoquer_tout(db: &Arc<dyn DbBackend>) {
    let n = JetonsDeRenouvellementRepo::with_backend(db.clone()).revoquer_tout(maintenant());
    tracing::info!(jetons = n, "toutes_les_sessions_revoquees");
    signaler_revocation();
}

/// La WebSocket ouverte avec ce JWT de session doit-elle se fermer ? Jamais
/// pour une socket sans session (authentification coupée, clé d'API, jeton
/// ordinaire de 24 h) : elle garde le comportement d'avant.
pub fn websocket_a_fermer(
    db: &Arc<dyn DbBackend>,
    session: Option<&crate::auth::JwtClaims>,
) -> bool {
    session.is_some_and(|c| crate::auth::verifier_session(c, db).is_err())
}
