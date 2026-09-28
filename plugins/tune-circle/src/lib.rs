//! Greffon natif « circle » (#5018, Tune Circle étape T1) : le cercle.
//!
//! Tune Circle partage avec des proches **invités, et qui ont accepté**. T1 ne
//! partage encore rien : elle construit la relation et sa **révocation**, par
//! laquelle passera tout ce que T2-T5 partageront.
//!
//! ## Architecture
//!
//! ```text
//! client ── /api/v1/ext/circle/… ──▶ routes.rs ──▶ relais.rs ── Bearer ──▶ mozaiklabs /api/v1/circle/…
//!                                                     │
//!                                                     └─ réglages : mozaik_access_token,
//!                                                        mozaik_refresh_token, mozaik_base_url
//! ```
//!
//! * **Le jeton** est celui de la session SSO du serveur
//!   (`tune_core::cloud::sso`, PKCE) : le greffon le relit dans les réglages à
//!   CHAQUE appel, comme `library_sync` et les routes `/cloud/*`, et le
//!   rafraîchit une fois sur un 401 par `MozaikAuth::refresh_token`, comme le
//!   battement de compte. Aucun second jeton n'est créé.
//! * **Aucun stockage local du cercle.** Le cloud porte le droit : chaque
//!   lecture repart vers lui. Une révocation faite par l'autre membre est donc
//!   vraie au prochain `GET /`, sans cache à invalider.
//! * **Journal** : route, statut, durée. Jamais le jeton, jamais un corps
//!   (les invitations envoyées portent l'adresse saisie par l'auteur).

pub mod ecoute;
pub mod relais;
pub mod routes;

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::license::LicenseManager;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::playback::PlaybackManager;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

/// Ce que l'hôte passe au greffon, explicitement, à sa construction.
///
/// La base : le jeton SSO, son jeton de rafraîchissement et l'adresse du
/// cloud y vivent déjà, sous les clés que lisent toutes les fonctions cloud du
/// serveur. La licence (T2, #5325) : `GET /library-sync` dit au propriétaire
/// si son serveur est Premium, avec le même juge que la synchro elle-même.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub license: Arc<LicenseManager>,
    /// T4 (#5327) : l'état des zones, pour savoir si une zone joue encore le
    /// flux d'un contact quand elle tombe en erreur. `None` chez un hôte qui
    /// n'en fournit pas : l'événement `circle.stream_revoked` part alors sur
    /// toute erreur d'une zone suivie.
    pub playback: Option<Arc<PlaybackManager>>,
    /// T4 (#5327) : pour poser la file d'une écoute de contact et inscrire la
    /// source `circle`, dont chaque piste reçoit son billet au moment d'être
    /// jouée (`tune_core::source_url`). `None` : l'écoute est indisponible.
    pub orchestrator: Option<Arc<PlaybackOrchestrator>>,
}

pub struct CirclePlugin {
    services: HostServices,
    /// Construite au `setup` (il faut le bus).
    ecoute: Option<Arc<ecoute::Ecoute>>,
}

impl CirclePlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            services,
            ecoute: None,
        }
    }
}

#[async_trait]
impl TunePlugin for CirclePlugin {
    fn name(&self) -> &str {
        "circle"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Tune Circle — partage entre proches invités. Gratuit ; l'écoute à distance est Premium."
    }
    /// Opt-in, comme `cd` : compilé partout, dormant tant qu'on ne l'installe pas.
    fn default_enabled(&self) -> bool {
        false
    }
    /// Au catalogue (#5018, décision de Bertrand du 25/09), comme `cd`
    /// (#4863) : le gestionnaire propose « Installer », puis
    /// `POST /api/v1/plugins/circle/install` et un redémarrage.
    /// Gratuit : absent de `premium_plugins`, aucun contrôle de droit local.
    /// Seule l'écoute à distance (T4, #5327) est Premium, et c'est le cloud
    /// qui en juge, pour l'auditeur comme pour le propriétaire.
    fn catalogued(&self) -> bool {
        true
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let relais = Arc::new(relais::Relais::new(self.services.backend.clone()));
        // T4 (#5327) : l'écoute chez un contact. Le droit (Premium des deux
        // côtés compris) est jugé par le cloud à la délivrance du billet.
        let hote: Option<Arc<dyn ecoute::HoteLecture>> =
            match (&self.services.orchestrator, &self.services.playback) {
                (Some(orchestrator), Some(playback)) => Some(Arc::new(ecoute::HoteOrchestrateur {
                    backend: self.services.backend.clone(),
                    orchestrator: orchestrator.clone(),
                    playback: playback.clone(),
                })),
                _ => None,
            };
        let ecoute = Arc::new(ecoute::Ecoute::new(
            relais.clone(),
            ctx.event_bus.clone(),
            self.services.playback.clone(),
            hote,
        ));
        if let Some(orchestrator) = &self.services.orchestrator {
            orchestrator
                .sources_url()
                .inscrire(ecoute::SOURCE, ecoute.clone());
        }
        self.ecoute = Some(ecoute.clone());
        ctx.register_router(
            routes::router(relais, self.services.license.clone()).merge(ecoute::router(ecoute)),
        );
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        if let Some(orchestrator) = &self.services.orchestrator {
            orchestrator.sources_url().retirer(ecoute::SOURCE);
        }
        Ok(())
    }

    /// T4 (#5327) : les erreurs de lecture d'une zone qui joue le flux d'un
    /// contact (voir [`ecoute::Ecoute::sur_evenement`]). Rien d'autre.
    async fn on_event(&mut self, event: &TuneEvent) {
        if let Some(ecoute) = &self.ecoute {
            ecoute.sur_evenement(event).await;
        }
    }
}
