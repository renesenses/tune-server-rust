//! Greffon natif « cd » (#4863) : lire un CD audio SANS l'extraire.
//!
//! Le disque tourne dans le lecteur de la machine qui fait tourner Tune ; le
//! greffon en lit les secteurs audio en temps réel et les remet au chemin de
//! lecture EXISTANT, vers n'importe quelle zone (locale, OAAT, DLNA…).
//!
//! ## Architecture
//!
//! ```text
//! LecteurDisque (trait)  ── linux.rs  : ioctl /dev/sr*
//!        │                └─ simule.rs : TOC et secteurs en mémoire (témoins)
//!        ▼
//! toc.rs / discid.rs / musicbrainz.rs   (purs : TOC, identifiant, métadonnées)
//!        ▼
//! flux.rs  FluxPiste : io::Read sur [début, fin) — reprise, silence, éjection
//!        ▼
//! fournisseur.rs  FournisseurCd : tune_core::source_pcm::FournisseurPcm « cd »
//!        ▼   (inscrit auprès de l'orchestrateur)
//! hôte : resolve_stream → session WAV finie → /stream/<id>.wav → zone
//! ```
//!
//! La file « le disque » est une file de service ordinaire (`source = "cd"`,
//! `source_id = "<disc_id>/<piste>"`) : l'avance, la piste suivante et
//! précédente, le pré-armement gapless et l'avance dans la piste passent par
//! l'orchestrateur sans rien lui apprendre du CD.
//!
//! ## Hors de ce greffon
//!
//! L'extraction vers la bibliothèque (#2466), macOS (volume AIFF sous
//! `/Volumes`) et Windows (`IOCTL_CDROM_RAW_READ`) : l'abstraction
//! [`lecteur::LecteurDisque`] est prête à les recevoir.

pub mod discid;
pub mod ejection;
pub mod flux;
pub mod fournisseur;
pub mod hote;
pub mod lecteur;
#[cfg(target_os = "linux")]
pub mod linux;
pub mod musicbrainz;
pub mod routes;
pub mod simule;
pub mod toc;

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::playback::PlaybackManager;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

use crate::ejection::{Surveillant, ZonesDuDisque};
use crate::fournisseur::{FournisseurCd, SOURCE};
use crate::hote::{HoteLecture, HoteOrchestrateur};
use crate::lecteur::LecteurDisque;

/// Ce que l'hôte passe au greffon, explicitement, à sa construction.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    /// Pour inscrire la source PCM `cd`, lancer une file et arrêter une zone.
    pub orchestrator: Arc<PlaybackOrchestrator>,
    /// Pour la longueur de file et l'état de lecture des zones.
    pub playback: Arc<PlaybackManager>,
}

pub struct CdPlugin {
    services: HostServices,
    surveillance: Option<tokio::task::JoinHandle<()>>,
}

impl CdPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            services,
            surveillance: None,
        }
    }
}

#[async_trait]
impl TunePlugin for CdPlugin {
    fn name(&self) -> &str {
        "cd"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Lecture directe d'un CD audio vers une zone, sans extraction"
    }
    /// Opt-in : compilé partout, dormant tant qu'on ne l'installe pas.
    fn default_enabled(&self) -> bool {
        false
    }
    /// Hors catalogue tant qu'aucun écran du client ne consomme ses routes
    /// (doctrine #2090, comme `concerts`). Installable nommément :
    /// `POST /api/v1/plugins/cd/install`.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let lecteur: Option<Arc<dyn LecteurDisque>> = lecteur::lecteur_du_systeme();
        let hote: Arc<dyn HoteLecture> = Arc::new(HoteOrchestrateur {
            backend: self.services.backend.clone(),
            orchestrator: self.services.orchestrator.clone(),
            playback: self.services.playback.clone(),
        });
        let zones: ZonesDuDisque = Arc::default();
        match &lecteur {
            Some(l) => {
                tracing::info!(lecteur = %l.chemin(), "cd_lecteur_detecte");
                self.services
                    .orchestrator
                    .sources_pcm()
                    .inscrire(SOURCE, Arc::new(FournisseurCd { lecteur: l.clone() }));
                let s = Surveillant::new(l.clone(), hote.clone(), zones.clone());
                self.surveillance = Some(tokio::spawn(s.tourner()));
            }
            None => tracing::info!(
                plateforme_prise_en_charge = lecteur::plateforme_prise_en_charge(),
                "cd_aucun_lecteur"
            ),
        }
        ctx.register_router(routes::router(routes::EtatRoutes {
            lecteur,
            hote,
            consultation: musicbrainz::MusicBrainz::new(),
            zones,
        }));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        self.services.orchestrator.sources_pcm().retirer(SOURCE);
        if let Some(h) = self.surveillance.take() {
            h.abort();
        }
        Ok(())
    }

    /// Le greffon n'observe pas le bus : l'éjection se voit au lecteur.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

#[cfg(test)]
mod bout_en_bout {
    //! Le chemin de l'HÔTE, pas seulement celui du greffon : une file « cd »
    //! posée en base, résolue par `resolve_queue_item_url` (la porte de
    //! l'avance et du pré-armement gapless), servie par une vraie session
    //! de l'`AudioStreamer`. Ce sont les octets qu'une zone recevrait.

    use std::sync::Arc;

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

    use crate::discid::tests::{ATTENDU, toc_du_vecteur};
    use crate::fournisseur::{FournisseurCd, SOURCE, source_id};
    use crate::simule::{LecteurSimule, contenu_des_secteurs};

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

    /// Tout ce que la session `stream_id` sert, en-tête compris.
    async fn servi(orch: &PlaybackOrchestrator, stream_id: &str) -> Vec<u8> {
        let session = orch.streamer.sessions_state().lock().await[stream_id].clone();
        let mut v = Vec::new();
        while let Some(t) = session.recv_chunk().await {
            v.extend_from_slice(&t);
        }
        v
    }

    /// Une zone en base (la file y est rattachée par clé étrangère), et la
    /// file des 10 pistes du disque.
    fn zone_avec_la_file(orch: &PlaybackOrchestrator) -> i64 {
        let zone = ZoneRepo::with_backend(orch.db.clone())
            .create("Zone CD", Some("dlna"), Some("uuid:essai-cd-4863"))
            .unwrap();
        PlayQueueRepo::with_backend(orch.db.clone())
            .set_streaming_queue(zone, &file(10))
            .unwrap();
        zone
    }

    fn file(n: u8) -> Vec<tune_core::db::play_queue_repo::StreamingQueueItem> {
        (1..=n)
            .map(|p| {
                (
                    source_id(ATTENDU, p),
                    format!("Piste {p}"),
                    String::new(),
                    None,
                    None,
                    0,
                    Some(SOURCE.to_string()),
                    Some(p as i64),
                    Some(1),
                )
            })
            .collect()
    }

    /// Témoins 3 et 4 par le chemin de l'hôte : chaque piste servie en WAV
    /// porte exactement ses secteurs, sa longueur exacte en en-tête, et les
    /// deux pistes mises bout à bout sont la suite continue du disque.
    #[tokio::test]
    async fn la_file_du_disque_sert_des_pistes_exactes_et_jointives() {
        let orch = orchestrateur();
        let lecteur = Arc::new(LecteurSimule::new(toc_du_vecteur()));
        orch.sources_pcm()
            .inscrire(SOURCE, Arc::new(FournisseurCd { lecteur }));
        let zone = zone_avec_la_file(&orch);
        let toc = toc_du_vecteur();

        let mut pcm = Vec::new();
        for (position, numero) in [(1i64, 2u8), (2, 3)] {
            let r = orch.resolve_queue_item_url(zone, position).await.unwrap();
            assert_eq!(r.mime_type, "audio/wav");
            assert!(r.url.ends_with(".wav"), "{}", r.url);
            assert_eq!(
                (r.sample_rate, r.bit_depth, r.channels),
                (Some(44_100), Some(16), Some(2))
            );
            let secteurs = toc.secteurs(numero).unwrap();
            let octets = secteurs as u64 * 2_352;
            assert_eq!(r.file_size, Some(44 + octets));
            assert_eq!(r.duration_ms, toc.duree_ms(numero));
            let v = servi(&orch, r.stream_id.as_deref().unwrap()).await;
            assert_eq!(&v[..4], b"RIFF");
            assert_eq!(
                u32::from_le_bytes(v[40..44].try_into().unwrap()) as u64,
                octets
            );
            let corps = &v[44..];
            let debut = toc.piste(numero).unwrap().debut;
            assert_eq!(
                corps,
                &contenu_des_secteurs(debut, secteurs)[..],
                "piste {numero}"
            );
            pcm.extend_from_slice(corps);
        }
        let debut = toc.piste(2).unwrap().debut;
        let fin = toc.fin_de_piste(3).unwrap();
        assert_eq!(
            pcm,
            contenu_des_secteurs(debut, fin - debut),
            "jonction 2 → 3"
        );
    }

    /// Témoin 7, versant flux : l'éjection en cours de piste termine la
    /// session avant sa longueur annoncée — rien n'est comblé de silence.
    #[tokio::test]
    async fn l_ejection_en_cours_de_piste_coupe_la_session() {
        let orch = orchestrateur();
        let lecteur = Arc::new(LecteurSimule::new(toc_du_vecteur()));
        lecteur.ejecter_apres(3);
        orch.sources_pcm()
            .inscrire(SOURCE, Arc::new(FournisseurCd { lecteur }));
        let zone = zone_avec_la_file(&orch);
        let r = orch.resolve_queue_item_url(zone, 0).await.unwrap();
        let v = servi(&orch, r.stream_id.as_deref().unwrap()).await;
        // En-tête + 3 blocs de 24 secteurs, puis plus rien.
        assert_eq!(v.len(), 44 + 3 * 24 * 2_352);
        assert!((v.len() as u64) < r.file_size.unwrap());
    }
}
