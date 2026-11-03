//! Clé Ed25519 du serveur, pour l'écoute à distance par laissez-passer (#6603).
//!
//! Le serveur se présente au relais avec sa clé publique et signe le défi que
//! le relais lui tend (`relay.rs`) ; le site garde la même clé publique à côté
//! du compte qui possède le serveur, et le relais refuse tout autre
//! enregistrement sous ce `server_id`. Demain, en pair-à-pair, ce sera aussi
//! l'identité d'endpoint du serveur.
//!
//! # Où vit la clé
//!
//! La graine privée est rangée dans le coffre des identifiants
//! (`crate::credentials_vault`, réglage `credentials_vault`). `secrets.rs`
//! classe ce réglage parmi les secrets (fragment `credential`) : il ne sort
//! jamais en clair de `/system/config` et n'entre pas dans les sauvegardes
//! exportées. Elle est créée au premier besoin — au démarrage du client de
//! relais, pont activé — et jamais renouvelée d'elle-même.
//!
//! # Inscription
//!
//! Quand le serveur est relié à un compte mozaiklabs (`library_sync`) et que
//! le relais annonce les sessions par laissez-passer, la clé publique est
//! inscrite sur ce compte :
//! `POST /api/v1/remote/servers/{server_id}/key`, jeton OAuth du compte,
//! corps `{public_key, alg, preuve}`. La preuve est la signature, par la clé
//! elle-même, de `tune-remote/inscription/v1\n<server_id>\n<public_key>` :
//! le site sait que le serveur détient la clé qu'il inscrit. Une fois
//! acceptée, la clé est notée dans [`CLE_INSCRITE`] et n'est plus renvoyée.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use tracing::{info, warn};
use tune_laissez_passer::CleServeur;

use crate::credentials_vault::{CredentialsVault, ServiceCredential};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// Service du coffre qui porte la graine de la clé du serveur.
pub const SERVICE_DU_COFFRE: &str = "ecoute_distante_cle_serveur";
/// Réglage : la clé publique déjà inscrite sur le compte.
pub const CLE_INSCRITE: &str = "ecoute_distante_cle_inscrite";
/// Base du site, surchargeable pour une recette.
pub const BASE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";

fn graine_du_coffre(backend: &Arc<dyn DbBackend>) -> Result<Option<Vec<u8>>, String> {
    let coffre = CredentialsVault::with_backend(backend.clone());
    let Some(c) = coffre.get(SERVICE_DU_COFFRE)? else {
        return Ok(None);
    };
    let graine = c
        .api_secret
        .as_deref()
        .and_then(tune_laissez_passer::b64url)
        .filter(|g| g.len() == 32)
        .ok_or_else(|| "cle du serveur illisible dans le coffre".to_string())?;
    Ok(Some(graine))
}

/// La clé du serveur, si elle existe déjà.
pub fn cle_existante(backend: &Arc<dyn DbBackend>) -> Result<Option<CleServeur>, String> {
    match graine_du_coffre(backend)? {
        Some(g) => CleServeur::depuis_graine(&g)
            .map(Some)
            .map_err(|r| format!("cle du serveur : {r}")),
        None => Ok(None),
    }
}

/// La clé du serveur, créée et rangée dans le coffre au premier appel.
pub fn cle_du_serveur(backend: &Arc<dyn DbBackend>) -> Result<CleServeur, String> {
    let graine = graine_du_serveur(backend)?;
    CleServeur::depuis_graine(&graine).map_err(|r| format!("cle du serveur : {r}"))
}

/// La graine de la clé du serveur, créée et rangée dans le coffre au premier
/// appel. Réservée à la caisse : l'écoute directe (#6785) en tire l'identité
/// d'endpoint pair-à-pair, qui est la MÊME clé (cahier, § 2.2). Elle ne sort
/// jamais du processus.
pub(crate) fn graine_du_serveur(backend: &Arc<dyn DbBackend>) -> Result<[u8; 32], String> {
    if let Some(g) = graine_du_coffre(backend)? {
        let mut graine = [0u8; 32];
        graine.copy_from_slice(&g);
        return Ok(graine);
    }
    let graine = CleServeur::nouvelle_graine().map_err(|r| format!("cle du serveur : {r}"))?;
    CredentialsVault::with_backend(backend.clone()).store(&ServiceCredential {
        service: SERVICE_DU_COFFRE.to_string(),
        username: None,
        token: None,
        api_key: None,
        api_secret: Some(tune_laissez_passer::en_b64url(&graine)),
        extra: Default::default(),
    })?;
    info!("ecoute_distante_cle_serveur_creee");
    Ok(graine)
}

/// Corps de l'inscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CorpsInscription {
    pub public_key: String,
    pub alg: &'static str,
    pub preuve: String,
}

/// Ce que le serveur demande au site pour inscrire sa clé. Les tests le
/// remplacent par un faux site.
#[async_trait]
pub trait InscriptionDeCle: Send + Sync {
    async fn inscrire(
        &self,
        server_id: &str,
        jeton_du_compte: &str,
        jeton_de_liaison: &str,
        corps: &CorpsInscription,
    ) -> Result<(), String>;
}

/// Le vrai site, par HTTP.
pub struct InscriptionHttp {
    pub client: reqwest::Client,
    pub base: String,
}

impl InscriptionHttp {
    pub fn depuis_environnement(client: reqwest::Client) -> Self {
        let base = std::env::var("TUNE_CLOUD_BASE_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| BASE_PAR_DEFAUT.to_string());
        Self { client, base }
    }
}

#[async_trait]
impl InscriptionDeCle for InscriptionHttp {
    async fn inscrire(
        &self,
        server_id: &str,
        jeton_du_compte: &str,
        jeton_de_liaison: &str,
        corps: &CorpsInscription,
    ) -> Result<(), String> {
        let reponse = self
            .client
            .post(format!(
                "{}/api/v1/remote/servers/{server_id}/key",
                self.base
            ))
            .bearer_auth(jeton_du_compte)
            .header(
                crate::cloud::library_sync::EN_TETE_JETON_DE_SERVEUR,
                jeton_de_liaison,
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(15))
            .json(corps)
            .send()
            .await
            .map_err(|e| format!("inscription de la cle : {}", e.without_url()))?;
        if reponse.status().is_success() {
            Ok(())
        } else {
            Err(format!(
                "inscription de la cle : HTTP {}",
                reponse.status().as_u16()
            ))
        }
    }
}

/// Inscrit la clé publique du serveur sur le compte qui le possède, si elle
/// existe, si le serveur est relié et si elle n'y est pas déjà.
///
/// `Ok(true)` : inscrite à l'instant. `Ok(false)` : rien à faire. L'erreur ne
/// porte que la nature de l'échec, jamais un jeton.
pub async fn inscrire_si_besoin(
    backend: &Arc<dyn DbBackend>,
    site: &dyn InscriptionDeCle,
) -> Result<bool, String> {
    let reglages = SettingsRepo::with_backend(backend.clone());
    // Rien tant que le relais n'ouvre pas de sessions par laissez-passer : le
    // site n'a pas de quoi recevoir la clé, et l'appel échouerait à chaque
    // battement.
    if !laissez_passer_actif(&reglages) {
        return Ok(false);
    }
    let Some(cle) = cle_existante(backend)? else {
        return Ok(false);
    };
    let lire = |nom: &str| {
        reglages
            .get(nom)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (Some(server_id), Some(jeton)) = (lire("server_id"), lire("mozaik_access_token")) else {
        return Ok(false);
    };
    let Some(jeton_de_liaison) = lire(crate::cloud::library_sync::CLE_JETON_DE_LIAISON) else {
        return Ok(false);
    };
    let public_key = cle.cle_publique();
    if lire(CLE_INSCRITE).as_deref() == Some(public_key.as_str()) {
        return Ok(false);
    }
    let preuve = cle.signer(&tune_laissez_passer::message_inscription_serveur(
        &server_id,
        &public_key,
    ));
    let corps = CorpsInscription {
        public_key: public_key.clone(),
        alg: "Ed25519",
        preuve,
    };
    if let Err(e) = site
        .inscrire(&server_id, &jeton, &jeton_de_liaison, &corps)
        .await
    {
        warn!(erreur = %e, "ecoute_distante_cle_serveur_non_inscrite");
        return Err(e);
    }
    reglages
        .set(CLE_INSCRITE, &public_key)
        .map_err(|e| format!("reglage non ecrit : {e}"))?;
    info!("ecoute_distante_cle_serveur_inscrite");
    Ok(true)
}

/// [`inscrire_si_besoin`] auprès du vrai site.
pub async fn inscrire_depuis_les_reglages(
    backend: &Arc<dyn DbBackend>,
    client: &reqwest::Client,
) -> Result<bool, String> {
    inscrire_si_besoin(
        backend,
        &InscriptionHttp::depuis_environnement(client.clone()),
    )
    .await
}

/// Réglage tenu par le client de relais : l'ancien lien d'accès est-il encore
/// accepté par le relais ? Écrit à chaque enregistrement, d'après
/// `relay.registered`.
pub const LIEN_HERITE_ACCEPTE: &str = "bridge_lien_herite_accepte";

/// Réglage tenu par le client de relais : le relais ouvre-t-il des sessions
/// par laissez-passer (il tient un jeu de clés signé par la racine) ? Écrit à
/// chaque enregistrement, d'après `relay.registered`.
pub const LAISSEZ_PASSER_ACTIF: &str = "bridge_laissez_passer_actif";

/// L'écoute à distance par laissez-passer est-elle en service sur le relais ?
/// Faux tant qu'il ne l'a pas dit : sans elle, rien ne change pour le
/// serveur — pas d'inscription de clé, pas de lien marqué obsolète.
pub fn laissez_passer_actif(reglages: &SettingsRepo) -> bool {
    reglages
        .get(LAISSEZ_PASSER_ACTIF)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "true")
}

/// Le relais a-t-il fait savoir qu'il n'accepte plus l'ancien lien d'accès ?
/// Faux tant qu'il n'a rien dit : l'absence d'information ne coupe rien.
pub fn lien_herite_coupe(reglages: &SettingsRepo) -> bool {
    reglages
        .get(LIEN_HERITE_ACCEPTE)
        .ok()
        .flatten()
        .is_some_and(|v| v.trim() == "false")
}

#[cfg(test)]
#[path = "cle_serveur_tests.rs"]
mod tests;
