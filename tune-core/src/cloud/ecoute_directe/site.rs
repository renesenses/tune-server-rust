//! Ce que le serveur demande au site pour ses sessions DIRECTES (#6785,
//! étape 11 du cahier du contrôle central).
//!
//! Le relais s'authentifie auprès du site par son jeton de service ; le
//! serveur, lui, par sa clé Ed25519 inscrite sur le compte
//! (`cle_serveur.rs`). Chaque requête porte :
//!
//! | En-tête | Contenu |
//! |---|---|
//! | `X-Tune-Server-Id` | le `server_id` |
//! | `X-Tune-Horodatage` | secondes Unix |
//! | `X-Tune-Nonce` | 256 bits aléatoires, base64url |
//! | `X-Tune-Signature` | Ed25519 de [`tune_laissez_passer::message_requete_serveur`], base64url |
//!
//! Routes attendues (le site les fera ensuite ; contrat dans
//! `docs/ecoute-a-distance-contrat.md`) :
//!
//! - `POST /api/v1/remote/servers/{server_id}/sessions` : ouverture, même
//!   corps et même réponse que l'ouverture du relais, `chemin: "direct"` ;
//! - `POST /api/v1/remote/servers/{server_id}/heartbeat` : battement, même
//!   réponse que celui du relais ;
//! - `GET /api/v1/remote/keys` : le jeu de clés, publique.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tune_laissez_passer::{CleServeur, Publication};

/// Le site n'a pas répondu, ou pas de façon exploitable (délai, réseau, 5xx,
/// réponse illisible). C'est ce qui déclenche la règle de secours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteInjoignable(pub String);

/// Ouverture d'une session directe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DemandeOuverture {
    pub jti: String,
    pub sub: String,
    pub did: String,
    pub sid: String,
    /// Toujours `direct` ici.
    pub chemin: &'static str,
    /// Session acceptée pendant une panne du site, rejouée à son retour.
    pub secours: bool,
}

/// Réponse du site à une ouverture : la même que pour le relais.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReponseOuverture {
    pub ok: bool,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Sessions du même compte à couper, relais ou direct : la nouvelle
    /// prend la main.
    #[serde(default)]
    pub couper: Vec<String>,
    #[serde(default)]
    pub motif: Option<String>,
    /// Nom de l'appareil choisi par l'utilisateur.
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

/// Le corps du battement du serveur.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Battement {
    pub sessions: Vec<BattementSession>,
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

/// Réponse au battement : la même que pour le relais.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ReponseBattement {
    #[serde(default)]
    pub couper: Vec<Coupure>,
    #[serde(default)]
    pub revoques: Revoques,
    #[serde(default)]
    pub jeu_de_cles: Option<u64>,
}

/// Le site, vu de l'écoute directe. Les tests le remplacent par un faux.
#[async_trait]
pub trait SiteDirect: Send + Sync {
    async fn ouvrir_session(
        &self,
        demande: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable>;
    async fn battement(&self, corps: &Battement) -> Result<ReponseBattement, SiteInjoignable>;
    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable>;
}

/// Les en-têtes signés d'une requête du serveur au site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntetesSignes {
    pub horodatage: i64,
    pub nonce: String,
    pub signature: String,
}

/// Signe une requête : voir l'en-tête du module.
pub fn signer_requete(
    cle: &CleServeur,
    methode: &str,
    chemin: &str,
    server_id: &str,
    horodatage: i64,
    corps: &[u8],
) -> Option<EntetesSignes> {
    let nonce = tune_laissez_passer::jeton_aleatoire()?;
    let signature = cle.signer(&tune_laissez_passer::message_requete_serveur(
        methode, chemin, server_id, horodatage, &nonce, corps,
    ));
    Some(EntetesSignes {
        horodatage,
        nonce,
        signature,
    })
}

/// Le vrai site, par HTTP, requêtes signées par la clé du serveur.
pub struct SiteHttp {
    client: reqwest::Client,
    base: String,
    server_id: String,
    cle: Arc<CleServeur>,
}

impl SiteHttp {
    /// Base : `TUNE_CLOUD_BASE_URL`, à défaut `https://mozaiklabs.fr` (la
    /// même règle que l'inscription de la clé). Changer la base ne change
    /// pas la racine : un laissez-passer n'est jamais accepté que signé par
    /// une clé que la racine EMBARQUÉE a signée.
    pub fn depuis_environnement(server_id: String, cle: Arc<CleServeur>) -> Self {
        let base = std::env::var("TUNE_CLOUD_BASE_URL")
            .ok()
            .map(|v| v.trim().trim_end_matches('/').to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| crate::cloud::cle_serveur::BASE_PAR_DEFAUT.to_string());
        Self {
            client: crate::http::client::builder()
                // Court : l'ouverture est sur le chemin de l'utilisateur.
                // Au-delà, la règle de secours joue.
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
            base,
            server_id,
            cle,
        }
    }

    async fn poster<T: for<'de> Deserialize<'de>>(
        &self,
        route: &str,
        corps: &impl Serialize,
    ) -> Result<T, SiteInjoignable> {
        let chemin = format!("/api/v1/remote/servers/{}/{route}", self.server_id);
        let octets =
            serde_json::to_vec(corps).map_err(|e| SiteInjoignable(format!("corps : {e}")))?;
        let signes = signer_requete(
            &self.cle,
            "POST",
            &chemin,
            &self.server_id,
            super::maintenant(),
            &octets,
        )
        .ok_or_else(|| SiteInjoignable("generateur aleatoire indisponible".into()))?;
        let reponse = self
            .client
            .post(format!("{}{chemin}", self.base))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .header("x-tune-server-id", &self.server_id)
            .header("x-tune-horodatage", signes.horodatage.to_string())
            .header("x-tune-nonce", &signes.nonce)
            .header("x-tune-signature", &signes.signature)
            .body(octets)
            .send()
            .await
            .map_err(|e| SiteInjoignable(format!("reseau : {}", e.without_url())))?;
        let statut = reponse.status();
        // Un refus explicite (4xx avec un corps lisible) est une réponse, pas
        // une panne : le site a décidé.
        if statut.is_success() || statut.is_client_error() {
            reponse
                .json::<T>()
                .await
                .map_err(|_| SiteInjoignable(format!("reponse illisible ({statut})")))
        } else {
            Err(SiteInjoignable(format!("statut {statut}")))
        }
    }
}

#[async_trait]
impl SiteDirect for SiteHttp {
    async fn ouvrir_session(
        &self,
        demande: &DemandeOuverture,
    ) -> Result<ReponseOuverture, SiteInjoignable> {
        self.poster("sessions", demande).await
    }

    async fn battement(&self, corps: &Battement) -> Result<ReponseBattement, SiteInjoignable> {
        self.poster("heartbeat", corps).await
    }

    async fn jeu_de_cles(&self) -> Result<Publication, SiteInjoignable> {
        let reponse = self
            .client
            .get(format!("{}/api/v1/remote/keys", self.base))
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| SiteInjoignable(format!("reseau : {}", e.without_url())))?;
        if !reponse.status().is_success() {
            return Err(SiteInjoignable(format!("statut {}", reponse.status())));
        }
        reponse
            .json::<Publication>()
            .await
            .map_err(|_| SiteInjoignable("jeu de cles illisible".into()))
    }
}
