//! L'Assistant IA, étapes 1 et 2 — greffon natif (chantier « assistant-ia », GO de
//! Bertrand du 12/10/2026). Conception : `docs/chantier-assistant-ia.md`.
//!
//! Deux usages, avec la clé de l'UTILISATEUR, depuis SON serveur, vers SON
//! fournisseur :
//!
//! 1. une playlist intelligente décrite par une phrase — des RÈGLES du moteur
//!    existant, validées ici, que l'écran montre et que l'utilisateur
//!    enregistre lui-même ([`regles`]) ;
//! 2. des suggestions d'enrichissement sur UN album, acceptées ligne par
//!    ligne, annulables ([`suggestions`]).
//!
//! # Les routes
//!
//! Montées par l'hôte sous `/api/v1/ext/assistant-ia` :
//!
//! | Route | Rôle |
//! |---|---|
//! | `GET /reglages` | réglages, 4 derniers caractères de la clé, consommation du mois |
//! | `PUT /reglages` | change fournisseur, modèle, adresse, plafond, prix, clé |
//! | `POST /tester` | un petit appel réel : `{ok: true}` ou `{ok: false, error, message}` |
//! | `POST /playlist` | `{phrase, cible?}` → règles proposées |
//! | `POST /albums/{id}/suggestions` | suggestions, écart avant/après |
//! | `POST /albums/{id}/appliquer` | `{lignes: [{champ, valeur}]}` |
//! | `POST /albums/{id}/annuler` | `{operation_id?}` |
//! | `GET /albums/{id}/historique` | les opérations de l'album |
//! | `POST /albums/{id}/pistes/suggestions` | œuvre / mouvement par piste (étape 2) |
//! | `POST /albums/{id}/pistes/appliquer` | `{lignes: [{piste_id, champ, valeur}]}` |
//! | `POST /lot/suggestions` | `{album_ids, budget_eur?}` → aperçu d'un lot, arrêt net au plafond |
//! | `POST /lot/appliquer` | `{albums: [{album_id, lignes}]}` |
//! | `POST /lot/annuler` | `{operations: [{album_id, operation_id}]}` |
//! | `POST /diagnostic/apercu` | `{zone_id?, question, langue?, second_avis?}` → le texte EXACT qui partirait, pseudonymisé, et son coût estimé ([`diagnostic`]) |
//! | `POST /diagnostic` | `{apercu_id}` → l'envoi de CET aperçu (une seule fois) ; constat, causes, gestes à essayer — aucune action |
//!
//! La transcription vocale (`POST /api/v1/voice-search` de l'hôte) passe par
//! [`Passerelle::transcrire`] : clé au coffre, coût dans le plafond
//! ([`transcription`]).
//!
//! Gratuit, pour tout le monde : aucune garde Premium. Les RÉGLAGES (`PUT
//! /reglages`, `POST /tester`) sont réservés aux administrateurs dès que
//! l'authentification est active (étape 2).

pub mod appel;
pub mod cache;
pub mod coffre;
pub mod couts;
pub mod diagnostic;
pub mod dialogue;
pub mod fournisseur;
pub mod migration;
pub mod passerelle;
pub mod pistes;
pub mod pseudonymes;
pub mod reglages;
pub mod regles;
mod routes;
pub mod suggestions;
pub mod transcription;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

pub use passerelle::Passerelle;
pub use regles::CritereMoteur;
pub use routes::router_vers;

/// Ce que l'hôte fournit.
#[derive(Clone)]
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    /// Le dossier de la BASE : la clé maîtresse du coffre y vit, hors de la
    /// base elle-même (voir [`coffre`]).
    pub dossier_secret: PathBuf,
    /// Les critères du moteur des règles intelligentes
    /// (`tune_smart_http::criteres::criteres_exposes`).
    pub criteres: Vec<CritereMoteur>,
    /// Le diagnostic en lecture seule (23/10/2026) : ce que l'hôte rassemble
    /// sur une zone. `None` : la route répond « indisponible ».
    pub diagnostic: Option<diagnostic::Collecte>,
}

/// L'état partagé des routes.
#[derive(Clone)]
pub struct Etat {
    pub backend: Arc<dyn DbBackend>,
    pub coffre: Arc<coffre::Coffre>,
    pub cache: Arc<cache::Cache>,
    pub criteres: Arc<Vec<CritereMoteur>>,
    pub fabrique: fournisseur::Fabrique,
    /// Tient la lecture du plafond, l'appel et son comptage ensemble.
    pub verrou_conso: Arc<tokio::sync::Mutex<()>>,
    /// Sérialise les écritures sur les albums (appliquer, annuler).
    pub verrou_albums: Arc<tokio::sync::Mutex<()>>,
    /// La collecte du diagnostic, fournie par l'hôte.
    pub collecte_diagnostic: Option<diagnostic::Collecte>,
    /// Les aperçus de diagnostic montrés, en attente de leur envoi.
    pub apercus: diagnostic::Apercus,
}

impl Etat {
    pub fn new(
        backend: Arc<dyn DbBackend>,
        dossier_secret: &std::path::Path,
        dossier_cache: PathBuf,
        criteres: Vec<CritereMoteur>,
        fabrique: fournisseur::Fabrique,
    ) -> Self {
        Self {
            backend,
            coffre: Arc::new(coffre::du_greffon(dossier_secret)),
            cache: Arc::new(cache::Cache::new(dossier_cache)),
            criteres: Arc::new(criteres),
            fabrique,
            verrou_conso: appel::verrou_de_consommation(),
            verrou_albums: Arc::new(tokio::sync::Mutex::new(())),
            collecte_diagnostic: None,
            apercus: diagnostic::Apercus::default(),
        }
    }

    /// La collecte du diagnostic (l'hôte, ou un faux en essai).
    pub fn avec_diagnostic(mut self, collecte: Option<diagnostic::Collecte>) -> Self {
        self.collecte_diagnostic = collecte;
        self
    }
}

pub struct AssistantIaPlugin {
    services: HostServices,
}

impl AssistantIaPlugin {
    pub fn new(services: HostServices) -> Self {
        Self { services }
    }
}

#[async_trait]
impl TunePlugin for AssistantIaPlugin {
    fn name(&self) -> &str {
        "assistant-ia"
    }

    fn display_name(&self) -> &str {
        "Assistant IA"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &str {
        "Playlists intelligentes décrites en une phrase et suggestions d'enrichissement des albums, avec votre propre clé (Anthropic, compatible OpenAI, Ollama)"
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let services = self.services.clone();
        // Étape 2 : les anciennes clés en clair passent au coffre dès le
        // démarrage. L'hôte la rejoue avant chaque usage hérité (une clé
        // reposée en clair par un ancien client y est rattrapée).
        Passerelle::new(services.backend.clone(), &services.dossier_secret).migrer();
        let etat = Etat::new(
            services.backend,
            &services.dossier_secret,
            ctx.data_dir.join("cache"),
            services.criteres,
            fournisseur::fabrique_reelle(),
        )
        .avec_diagnostic(services.diagnostic);
        ctx.register_router(router_vers(etat));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}
