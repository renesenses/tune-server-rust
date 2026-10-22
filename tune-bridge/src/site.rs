//! Ce que le relais demande à mozaiklabs.fr pour l'écoute à distance (#6603).
//!
//! Le site décide ; le relais vérifie et rend compte. Quatre échanges :
//!
//! - `POST /api/v1/remote/sessions` : ouverture d'une session, synchrone ;
//! - `POST /api/v1/remote/heartbeat` : battement de toutes les sessions,
//!   chaque minute ; la réponse dit quoi couper et ce qui est révoqué ;
//! - `GET /api/v1/remote/keys` : le jeu de clés signé par la racine ;
//! - `GET /api/v1/remote/servers/{server_id}/key` : la clé publique que le
//!   serveur a inscrite sur le compte qui le possède.
//!
//! Tous passent derrière le trait [`Site`] : les tests le remplacent par un
//! faux site, et aucun test ne fait d'appel réseau.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tune_laissez_passer::Publication;

/// Le site n'a pas répondu, ou pas de façon exploitable (délai, réseau,
/// 5xx, réponse illisible). C'est ce qui déclenche la règle de secours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteInjoignable(pub String);

/// Ouverture d'une session, telle que le relais la demande.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DemandeOuverture {
    pub jti: String,
    pub sub: String,
    pub did: String,
    pub sid: String,
    /// `relais` ici ; `direct` pour le serveur en pair-à-pair.
    pub chemin: &'static str,
    /// Vrai pour une session acceptée pendant une panne du site, rejouée à
    /// son retour.
    pub secours: bool,
}

/// Réponse du site à une ouverture.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReponseOuverture {
    pub ok: bool,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Sessions du même compte à couper : la nouvelle prend la main.
    #[serde(default)]
    pub couper: Vec<String>,
    #[serde(default)]
    pub motif: Option<String>,
    /// Nom de l'appareil choisi par l'utilisateur, pour l'afficher sur
    /// l'appareil coupé et sur le serveur. Jamais dans le laissez-passer.
    #[serde(default)]
    pub appareil: Option<String>,
}

/// Une session dans le battement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BattementSession {
    pub session_id: String,
    pub octets_depuis_le_dernier: u64,
    pub audio_actif: bool,
}

/// Le corps du battement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Battement {
    pub sessions: Vec<BattementSession>,
    /// Requêtes servies par l'ancien lien d'accès depuis le dernier
    /// battement, par `server_id` : le site sait ainsi quels comptes
    /// prévenir avant la fin de ce lien. Aucune IP.
    pub jeton_herite: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Coupure {
    pub session_id: String,
    #[serde(default)]
    pub motif: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Revoques {
    #[serde(default)]
    pub jti: Vec<String>,
    #[serde(default)]
    pub did: Vec<String>,
}

/// Réponse au battement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReponseBattement {
    #[serde(default)]
    pub couper: Vec<Coupure>,
    #[serde(default)]
    pub revoques: Revoques,
    /// Version du jeu de clés en cours sur le site : plus récente que la
    /// nôtre, on le relit tout de suite.
    #[serde(default)]
    pub jeu_de_cles: Option<u64>,
}

#[async_trait]
pub trait Site: Send + Sync {
    async fn ouvrir_session(
        &self,
        demande: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable>;
    async fn battement(&self, corps: &Battement) -> Result<ReponseBattement, SiteInjoignable>;
    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable>;
    /// `Ok(None)` : le site ne connaît aucune clé pour ce serveur.
    async fn cle_du_serveur(&self, server_id: &str) -> Result<Option<String>, SiteInjoignable>;
}

/// Le vrai site, par HTTP, avec le jeton de service du relais
/// (`auth.tune-bridge` côté site).
pub struct SiteHttp {
    client: reqwest::Client,
    base: String,
    jeton: Option<String>,
}

impl SiteHttp {
    pub fn depuis_environnement() -> Self {
        Self {
            client: reqwest::Client::builder()
                // Court : l'ouverture est sur le chemin de l'utilisateur. Au
                // delà, la règle de secours prend le relais.
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            base: crate::trafic::base_depuis_environnement(),
            jeton: crate::trafic::jeton_depuis_environnement(),
        }
    }

    fn jeton(&self) -> Result<&str, SiteInjoignable> {
        self.jeton
            .as_deref()
            .ok_or_else(|| SiteInjoignable("jeton de service absent".into()))
    }

    async fn lire<T: for<'de> Deserialize<'de>>(
        reponse: Result<reqwest::Response, reqwest::Error>,
        refus_lisible: bool,
    ) -> Result<T, SiteInjoignable> {
        let r = reponse.map_err(|e| SiteInjoignable(format!("reseau : {e}")))?;
        let statut = r.status();
        // Un refus explicite (4xx avec un corps `ok: false`) est une réponse,
        // pas une panne : le site a décidé.
        if statut.is_success() || (refus_lisible && statut.is_client_error()) {
            r.json::<T>()
                .await
                .map_err(|e| SiteInjoignable(format!("reponse illisible ({statut}) : {e}")))
        } else {
            Err(SiteInjoignable(format!("statut {statut}")))
        }
    }
}

#[async_trait]
impl Site for SiteHttp {
    async fn ouvrir_session(
        &self,
        demande: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable> {
        let r = self
            .client
            .post(format!("{}/api/v1/remote/sessions", self.base))
            .bearer_auth(self.jeton()?)
            .header("accept", "application/json")
            .json(demande)
            .send()
            .await;
        Self::lire(r, true).await
    }

    async fn battement(&self, corps: &Battement) -> Result<ReponseBattement, SiteInjoignable> {
        let r = self
            .client
            .post(format!("{}/api/v1/remote/heartbeat", self.base))
            .bearer_auth(self.jeton()?)
            .header("accept", "application/json")
            .json(corps)
            .send()
            .await;
        Self::lire(r, false).await
    }

    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable> {
        // Route publique : le jeu de clés porte sa propre preuve (la
        // signature de la racine).
        let r = self
            .client
            .get(format!("{}/api/v1/remote/keys", self.base))
            .header("accept", "application/json")
            .send()
            .await;
        Self::lire(r, false).await
    }

    async fn cle_du_serveur(&self, server_id: &str) -> Result<Option<String>, SiteInjoignable> {
        #[derive(Deserialize)]
        struct Cle {
            #[serde(default)]
            public_key: Option<String>,
        }
        let r = self
            .client
            .get(format!(
                "{}/api/v1/remote/servers/{}/key",
                self.base,
                urlencoding_minimal(server_id)
            ))
            .bearer_auth(self.jeton()?)
            .header("accept", "application/json")
            .send()
            .await
            .map_err(|e| SiteInjoignable(format!("reseau : {e}")))?;
        if r.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !r.status().is_success() {
            return Err(SiteInjoignable(format!("statut {}", r.status())));
        }
        let c: Cle = r
            .json()
            .await
            .map_err(|e| SiteInjoignable(format!("reponse illisible : {e}")))?;
        Ok(c.public_key.filter(|k| !k.is_empty()))
    }
}

/// Un `server_id` est un UUID : seul ce jeu de caractères passe tel quel.
fn urlencoding_minimal(segment: &str) -> String {
    segment
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}
