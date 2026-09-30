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
//! * **T3, les rayons (#5326)** : le greffon RÉSOUT les étiquettes et les
//!   collections intelligentes cochées pour un cercle et en pousse les membres
//!   ([`ensembles`], [`rayons`]) ; un pousseur de fond ([`battement`]) les
//!   tient à jour. Il ne garde en mémoire que la liste des ensembles de CE
//!   serveur, jamais servie à personne.
//! * **Journal** : route, statut, durée. Jamais le jeton, jamais un corps
//!   (les invitations envoyées portent l'adresse saisie par l'auteur).

pub mod battement;
pub mod ecoute;
pub mod ensembles;
pub mod lecture;
pub mod playlists;
pub mod rayons;
pub mod references;
pub mod relais;
pub mod resolution;
pub mod routes;

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::license::LicenseManager;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::playback::PlaybackManager;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};
use tune_core::streaming::ServiceRegistry;

/// Ce que l'hôte passe au greffon, explicitement, à sa construction.
///
/// La base : le jeton SSO, son jeton de rafraîchissement et l'adresse du
/// cloud y vivent déjà, sous les clés que lisent toutes les fonctions cloud du
/// serveur. La licence (T2, #5325) : `GET /library-sync` dit au propriétaire
/// si son serveur est Premium, avec le même juge que la synchro elle-même.
/// L'hôte des rayons (T3, #5326) : le profil actif d'une requête et la
/// résolution d'une collection intelligente, par le moteur de la vue.
///
/// T5 (#5328), les playlists collaboratives : le parc des services, pour
/// rejouer une référence chez l'utilisateur (lecture seule : `get_track` et
/// la recherche du moteur de transfert, jamais une écriture chez un service) ;
/// l'orchestrateur et le gestionnaire de lecture, pour jouer la playlist
/// résolue sur une zone.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub license: Arc<LicenseManager>,
    pub hote: Arc<dyn ensembles::Hote>,
    pub services: Arc<tokio::sync::Mutex<ServiceRegistry>>,
    /// T4 (#5327) : pour poser la file d'une écoute de contact et inscrire la
    /// source `circle`, dont chaque piste reçoit son billet au moment d'être
    /// jouée (`tune_core::source_url`).
    pub orchestrator: Arc<PlaybackOrchestrator>,
    /// T4 (#5327) : l'état des zones, pour savoir si une zone joue encore le
    /// flux d'un contact quand elle tombe en erreur.
    pub playback: Arc<PlaybackManager>,
}

pub struct CirclePlugin {
    services: HostServices,
    pousseur: Option<Arc<battement::Pousseur>>,
    tache: Option<tokio::task::JoinHandle<()>>,
    /// Construite au `setup` (il faut le bus).
    ecoute: Option<Arc<ecoute::Ecoute>>,
}

impl CirclePlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            services,
            pousseur: None,
            tache: None,
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
        tune_core::version()
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
        let pousseur = Arc::new(battement::Pousseur::new(
            relais.clone(),
            self.services.hote.clone(),
        ));
        let collaboratif = Arc::new(playlists::Collaboratif {
            relais: relais.clone(),
            resolveur: resolution::Resolveur::new(
                self.services.backend.clone(),
                self.services.services.clone(),
            ),
            lecture: Arc::new(lecture::LectureOrchestrateur {
                backend: self.services.backend.clone(),
                orchestrator: self.services.orchestrator.clone(),
                playback: self.services.playback.clone(),
            }),
        });
        // T4 (#5327) : l'écoute chez un contact. Le droit (Premium des deux
        // côtés compris) est jugé par le cloud à la délivrance du billet.
        let hote: Option<Arc<dyn ecoute::HoteLecture>> =
            Some(Arc::new(ecoute::HoteOrchestrateur {
                backend: self.services.backend.clone(),
                orchestrator: self.services.orchestrator.clone(),
                playback: self.services.playback.clone(),
            }));
        let ecoute = Arc::new(ecoute::Ecoute::new(
            relais.clone(),
            ctx.event_bus.clone(),
            Some(self.services.playback.clone()),
            hote,
        ));
        self.services
            .orchestrator
            .sources_url()
            .inscrire(ecoute::SOURCE, ecoute.clone());
        self.ecoute = Some(ecoute.clone());
        ctx.register_router(
            routes::router(relais, self.services.license.clone())
                .merge(rayons::router(pousseur.clone()))
                .merge(playlists::router(collaboratif))
                .merge(ecoute::router(ecoute)),
        );
        // T3 (#5326) : tenir à jour les rayons cochés.
        self.tache = Some(tokio::spawn(pousseur.clone().tourner()));
        self.pousseur = Some(pousseur);
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        if let Some(t) = self.tache.take() {
            t.abort();
        }
        self.pousseur = None;
        self.services
            .orchestrator
            .sources_url()
            .retirer(ecoute::SOURCE);
        Ok(())
    }

    /// T3 (#5326) : la bibliothèque a changé, une collection partagée peut
    /// avoir gagné ou perdu un album. Réveille le pousseur, sans attendre.
    ///
    /// T4 (#5327) : les erreurs de lecture d'une zone qui joue le flux d'un
    /// contact (voir [`ecoute::Ecoute::sur_evenement`]).
    async fn on_event(&mut self, event: &TuneEvent) {
        if battement::EVENEMENTS_QUI_REVEILLENT.contains(&event.event_type.as_str())
            && let Some(p) = &self.pousseur
        {
            p.reveiller();
        }
        if let Some(ecoute) = &self.ecoute {
            ecoute.sur_evenement(event).await;
        }
    }
}
