//! Greffon natif « entree-audio » (#5051) : capter une entrée audio USB et la
//! diffuser EN DIRECT vers une zone Tune.
//!
//! Platine vinyle par préampli phono USB, transport CD ou lecteur en S/PDIF
//! sur une interface USB, sortie optique d'une TV : Tune le capte au format
//! natif du périphérique et le sert à n'importe quelle zone (locale, DLNA,
//! AirPlay…), par le chemin de lecture EXISTANT.
//!
//! ## Architecture
//!
//! ```text
//! Peripheriques (trait) ── systeme.rs : cpal en entrée (+ CoreAudio : format
//!        │                              physique, fréquence nominale)
//!        │               └─ simule.rs  : signal connu (témoins)
//!        ▼  rappel du pilote → format.rs (f32/i16/i24/i32 → PCM 16/24 bits)
//! anneau.rs   Anneau : débordements, crête, silence numérique, horodatage
//!        ▼
//! lecteur.rs  LecteurCapture : io::Read — amorce, dérive (derive.rs),
//!        │                     tampon avec reprise
//!        ▼
//! fournisseur.rs  FournisseurEntree : FournisseurPcm EN DIRECT « entree-audio »
//!        ▼   (inscrit auprès de l'orchestrateur)
//! hôte : resolve_stream → session de RADIO (sans longueur) → /stream/<id>.wav
//! ```
//!
//! `controleur.rs` tient la capture active, sa zone, et la surveillance de la
//! fréquence (relance propre sur un changement). `autorisation.rs` dit ce que
//! macOS (TCC) permet — toute entrée audio y est un micro.
//!
//! La file de la zone porte UNE ligne, « Entrée audio — <périphérique> »,
//! `source = "entree-audio"`, `source_id = <périphérique>`, sans durée.
//!
//! ## Hors de ce greffon
//!
//! L'enregistrement de l'entrée, la synchronisation avec l'image, Windows
//! (WASAPI compile par cpal, non éprouvé).

pub mod anneau;
pub mod autorisation;
pub mod controleur;
pub mod derive;
pub mod format;
pub mod fournisseur;
pub mod hote;
pub mod lecteur;
pub mod peripheriques;
pub mod routes;
pub mod simule;
#[cfg(feature = "capture")]
pub mod systeme;

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::playback::PlaybackManager;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

use crate::controleur::Controleur;
use crate::fournisseur::{FournisseurEntree, SOURCE};
use crate::hote::{HoteLecture, HoteOrchestrateur};

/// Ce que l'hôte passe au greffon, explicitement, à sa construction.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    /// Pour inscrire la source PCM `entree-audio`, lancer et arrêter une zone.
    pub orchestrator: Arc<PlaybackOrchestrator>,
    /// Pour la file et l'état de lecture des zones.
    pub playback: Arc<PlaybackManager>,
}

pub struct EntreeAudioPlugin {
    services: HostServices,
    controleur: Option<Arc<Controleur>>,
}

impl EntreeAudioPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            services,
            controleur: None,
        }
    }
}

#[async_trait]
impl TunePlugin for EntreeAudioPlugin {
    fn name(&self) -> &str {
        "entree-audio"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Capter une entrée audio (USB, S/PDIF, optique) et la diffuser en direct vers une zone"
    }
    /// Opt-in : compilé partout, dormant tant qu'on ne l'installe pas.
    fn default_enabled(&self) -> bool {
        false
    }
    /// Hors catalogue : aucun écran ne consomme encore ses routes (doctrine
    /// #2090).
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let hote: Arc<dyn HoteLecture> = Arc::new(HoteOrchestrateur {
            backend: self.services.backend.clone(),
            orchestrator: self.services.orchestrator.clone(),
            playback: self.services.playback.clone(),
        });
        let controleur = Controleur::new(peripheriques::du_systeme(), hote);
        tracing::info!(
            pile = controleur.peripheriques().pile(),
            "entree_audio_greffon_pret"
        );
        self.services.orchestrator.sources_pcm().inscrire(
            SOURCE,
            Arc::new(FournisseurEntree {
                controleur: controleur.clone(),
            }),
        );
        ctx.register_router(routes::router(routes::EtatRoutes {
            controleur: controleur.clone(),
            autorisation: autorisation::du_systeme,
        }));
        self.controleur = Some(controleur);
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        self.services.orchestrator.sources_pcm().retirer(SOURCE);
        if let Some(c) = self.controleur.take() {
            c.arreter_capture();
        }
        Ok(())
    }

    async fn on_event(&mut self, _event: &TuneEvent) {}
}

#[cfg(test)]
mod bout_en_bout {
    //! Le chemin de l'HÔTE : une file « entree-audio » posée en base, résolue
    //! par `resolve_queue_item_url`, servie par une vraie session de
    //! l'`AudioStreamer`. Ce sont les octets qu'une zone recevrait.

    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::Mutex;
    use tune_core::db::migrations::run_migrations;
    use tune_core::db::play_queue_repo::PlayQueueRepo;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::db::zone_repo::ZoneRepo;
    use tune_core::http::streamer::AudioStreamer;
    use tune_core::orchestrator::PlaybackOrchestrator;
    use tune_core::outputs::registry::OutputRegistry;
    use tune_core::playback::PlaybackManager;
    use tune_core::streaming::registry::ServiceRegistry;

    use crate::controleur::Controleur;
    use crate::fournisseur::{FournisseurEntree, SOURCE};
    use crate::hote::tests::HoteTemoin;
    use crate::simule::Simulees;

    fn orchestrateur() -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        PlaybackOrchestrator::new(
            Arc::new(db),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            Some("127.0.0.1".into()),
        )
    }

    /// Mode sans fin, de bout en bout : pas de durée ni de longueur, la
    /// session est une radio, et le PCM servi est EXACTEMENT le signal capté,
    /// sans trou ni doublon, bien au-delà du canal de la session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn la_file_de_l_entree_sert_le_signal_capte_sans_fin_ni_longueur() {
        let orch = orchestrateur();
        let c = Controleur::new(
            Simulees::avec("Loopback Audio", 44_100),
            Arc::new(HoteTemoin::default()),
        );
        *c.amorce.lock().unwrap() = Duration::from_millis(200);
        orch.sources_pcm().inscrire(
            SOURCE,
            Arc::new(FournisseurEntree {
                controleur: c.clone(),
            }),
        );
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .create("Zone locale", Some("local"), Some("local:essai-5051"))
            .unwrap();
        PlayQueueRepo::with_backend(orch.db.clone())
            .set_streaming_queue(
                zone,
                &[(
                    "Loopback Audio".into(),
                    "Entrée audio — Loopback Audio".into(),
                    String::new(),
                    None,
                    None,
                    0,
                    Some(SOURCE.into()),
                    None,
                    None,
                )],
            )
            .unwrap();
        let r = orch.resolve_queue_item_url(zone, 0).await.unwrap();
        assert_eq!(r.source.as_deref(), Some(SOURCE));
        assert_eq!(r.duration_ms, None, "un direct n'a pas de durée");
        assert_eq!(r.file_size, None, "un direct n'a pas de longueur");
        assert_eq!(
            (r.sample_rate, r.bit_depth, r.channels),
            (Some(44_100), Some(16), Some(2))
        );
        let sid = r.stream_id.unwrap();
        let session = orch.streamer.sessions_state().lock().await[&sid].clone();
        assert!(session.is_radio);
        // 1,5 s de signal : bien plus que les 8 tronçons du canal.
        let mut pcm = Vec::new();
        while pcm.len() < 44_100 * 4 * 3 / 2 {
            let t = tokio::time::timeout(Duration::from_secs(5), session.recv_chunk())
                .await
                .expect("le direct s'est tu")
                .expect("le direct s'est terminé");
            pcm.extend_from_slice(&t);
        }
        let echantillons: Vec<u16> = pcm
            .chunks_exact(4)
            .map(|t| u16::from_le_bytes([t[0], t[1]]))
            .collect();
        for (i, paire) in echantillons.windows(2).enumerate() {
            assert_eq!(
                paire[1],
                paire[0].wrapping_add(1),
                "trou ou doublon à la trame {i}"
            );
        }
        assert_eq!(
            tune_core::source_pcm::compensation_du_direct(&sid).map(|c| c.bit_perfect()),
            Some(true)
        );
        c.arreter_capture();
    }

    /// Multicanal : un boîtier de capture HDMI peut rendre 6 ou 8 voies en
    /// PCM. La capture garde le nombre de voies NATIF — le repli vers une
    /// zone stéréo est l'affaire du chemin de sortie existant.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn une_entree_six_voies_est_servie_en_six_voies() {
        let orch = orchestrateur();
        let c = Controleur::new(
            Simulees::avec_canaux("HDMI 5.1", 48_000, 6),
            Arc::new(HoteTemoin::default()),
        );
        *c.amorce.lock().unwrap() = Duration::from_millis(100);
        orch.sources_pcm().inscrire(
            SOURCE,
            Arc::new(FournisseurEntree {
                controleur: c.clone(),
            }),
        );
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .create("Zone 5.1", Some("local"), Some("local:essai-5051-6"))
            .unwrap();
        PlayQueueRepo::with_backend(orch.db.clone())
            .set_streaming_queue(
                zone,
                &[(
                    "HDMI 5.1".into(),
                    "Entrée audio — HDMI 5.1".into(),
                    String::new(),
                    None,
                    None,
                    0,
                    Some(SOURCE.into()),
                    None,
                    None,
                )],
            )
            .unwrap();
        let r = orch.resolve_queue_item_url(zone, 0).await.unwrap();
        assert_eq!(r.channels, Some(6));
        let sid = r.stream_id.unwrap();
        let session = orch.streamer.sessions_state().lock().await[&sid].clone();
        assert_eq!(session.info.channels, 6);
        assert_eq!(session.detected_output_format(), Some((48_000, 6)));
        let mut pcm = Vec::new();
        while pcm.len() < 12 * 4_800 {
            let t = tokio::time::timeout(Duration::from_secs(5), session.recv_chunk())
                .await
                .unwrap()
                .unwrap();
            pcm.extend_from_slice(&t);
        }
        // Chaque trame : 6 voies de 16 bits, voie c = n·8 + c.
        let trames: Vec<Vec<u16>> = pcm
            .chunks_exact(12)
            .map(|t| {
                t.chunks(2)
                    .map(|v| u16::from_le_bytes([v[0], v[1]]))
                    .collect()
            })
            .collect();
        for t in &trames {
            for c in 1..6u16 {
                assert_eq!(
                    t[c as usize],
                    t[0].wrapping_add(c),
                    "voies mélangées : {t:?}"
                );
            }
        }
        for p in trames.windows(2) {
            assert_eq!(p[1][0], p[0][0].wrapping_add(8), "trou ou doublon");
        }
        c.arreter_capture();
    }
}
