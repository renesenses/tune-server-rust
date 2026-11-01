//! Greffon natif « calibration » (#7632, C1 — mesure de la pièce d'écoute).
//!
//! ```text
//! tune-calibration (caisse pure)  sweep · deconv · analysis · mic_cal · level
//!        ▲
//! generateur.rs  Deroule / Generateur : le balayage joué, piloté par l'analyseur,
//!        │                             plafond −6 dBFS tenu au dernier étage
//! fournisseur.rs FournisseurBalayage : FournisseurPcm FINI « calibration »,
//!        │                             armé le temps d'une mesure seulement
//! hote.rs        HoteZone : volume lu / forcé / restauré, file + play, stop
//! mesure.rs      Controleur : salves → LevelPolicy → balayage → analyse
//!        │       capture par tune-entree-audio::tampon (cpal, vers tampon)
//! routes.rs      /api/v1/ext/calibration/…
//! ```
//!
//! Pourquoi un greffon et non un module du serveur : comme `entree-audio`,
//! il ouvre un périphérique (le micro) ; opt-in et dormant tant qu'il n'est
//! pas installé, il ne laisse exister ni fournisseur « calibration » ni
//! route tant que l'utilisateur ne l'a pas voulu — aucune lecture possible
//! sans action explicite. Ce que la mesure fait à la zone passe par
//! l'orchestrateur (volume, file, lecture, arrêt), rien de parallèle.
//!
//! Hors de ce greffon : l'étape « Correction de pièce » du chemin du signal
//! (tune-server), l'écran web, le micro du téléphone (C1-bis), l'égaliseur
//! automatique (C2) et le FIR (C3).

pub mod fournisseur;
pub mod generateur;
pub mod hote;
pub mod mesure;
pub mod routes;
pub mod simulation;

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::playback::PlaybackManager;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

use crate::fournisseur::{FournisseurBalayage, SOURCE};
use crate::hote::{HoteOrchestrateur, HoteZone};
use crate::mesure::Controleur;

/// Ce que l'hôte passe au greffon, explicitement, à sa construction.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    /// Pour inscrire la source PCM « calibration », régler le volume, lancer
    /// et arrêter une zone.
    pub orchestrator: Arc<PlaybackOrchestrator>,
    /// Pour la file et l'état de lecture des zones.
    pub playback: Arc<PlaybackManager>,
}

pub struct CalibrationPlugin {
    services: HostServices,
    controleur: Option<Arc<Controleur>>,
}

impl CalibrationPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            services,
            controleur: None,
        }
    }
}

#[async_trait]
impl TunePlugin for CalibrationPlugin {
    fn name(&self) -> &str {
        "calibration"
    }
    fn display_name(&self) -> &str {
        "Calibration de la pièce"
    }
    fn version(&self) -> &str {
        tune_core::version()
    }
    fn description(&self) -> &str {
        "Mesurer la réponse de la pièce d'écoute avec un micro USB étalonné (balayage, réponse impulsionnelle, RT60)"
    }
    /// Opt-in : compilé partout, dormant tant qu'on ne l'installe pas.
    fn default_enabled(&self) -> bool {
        false
    }
    fn catalogued(&self) -> bool {
        true
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let hote: Arc<dyn HoteZone> = Arc::new(HoteOrchestrateur {
            backend: self.services.backend.clone(),
            orchestrator: self.services.orchestrator.clone(),
            playback: self.services.playback.clone(),
            runtime: tokio::runtime::Handle::current(),
        });
        let fournisseur = FournisseurBalayage::new();
        self.services
            .orchestrator
            .sources_pcm()
            .inscrire(SOURCE, fournisseur.clone());
        let controleur = Controleur::new(
            tune_entree_audio::peripheriques::du_systeme(),
            hote,
            fournisseur,
            ctx.data_dir.join("micros"),
        );
        tracing::info!(
            pile = controleur.peripheriques.pile(),
            "calibration_greffon_pret"
        );
        ctx.register_router(routes::router(routes::EtatRoutes {
            controleur: controleur.clone(),
        }));
        self.controleur = Some(controleur);
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        self.services.orchestrator.sources_pcm().retirer(SOURCE);
        if let Some(c) = self.controleur.take() {
            c.annuler_tout();
        }
        Ok(())
    }

    async fn on_event(&mut self, _event: &TuneEvent) {}
}
