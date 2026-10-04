//! #5327 — une file dont l'URL de chaque piste est fournie par un greffon AU
//! MOMENT de la jouer (`crate::source_url`, Tune Circle « Lire l'album »).
//!
//! Par les vraies portes : `play_from_queue` pour le départ, puis la vraie fin
//! de piste (`handle_track_end` → `avancer_avec_reprises`) pour l'avance, sur
//! une zone DLNA simulée. Le fournisseur note chaque demande : c'est la mesure
//! « un billet par piste ».

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::mock::MockOutput;
use crate::playback::PlaybackManager;
use crate::source_url::{FournisseurDUrl, RefusDUrl, UrlFournie};
use crate::streaming::ServiceRegistry;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

const APPAREIL: &str = "dlna:banc-source-url-5327";
const SOURCE: &str = "circle";

/// Ce que le fournisseur répond pour une référence.
#[derive(Clone)]
enum Reponse {
    Url,
    Piste,
    Arret,
}

struct Fournisseur {
    reponses: HashMap<String, Reponse>,
    demandes: std::sync::Mutex<Vec<(i64, String)>>,
}

#[async_trait::async_trait]
impl FournisseurDUrl for Fournisseur {
    async fn url(&self, zone_id: i64, source_id: &str) -> Result<UrlFournie, RefusDUrl> {
        self.demandes
            .lock()
            .unwrap()
            .push((zone_id, source_id.to_string()));
        match self
            .reponses
            .get(source_id)
            .cloned()
            .unwrap_or(Reponse::Url)
        {
            Reponse::Url => Ok(UrlFournie {
                url: format!(
                    "http://127.0.0.1:9/stream/circle/billet-{}",
                    source_id.replace(':', "-")
                ),
                media_format: Some("flac".into()),
            }),
            Reponse::Piste => Err(RefusDUrl::Piste("not_found".into())),
            Reponse::Arret => Err(RefusDUrl::ArretDeLaFile("owner_offline".into())),
        }
    }
}

struct Banc {
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
    fournisseur: Arc<Fournisseur>,
    bus: Arc<EventBus>,
}

fn ligne(reference: &str, titre: &str) -> QueueInput {
    QueueInput::Streaming {
        source: SOURCE.into(),
        source_id: reference.into(),
        title: titre.into(),
        artist: "Miles Davis".into(),
        album: Some("Kind of Blue".into()),
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

impl Banc {
    async fn monter(reponses: &[(&str, Reponse)]) -> Self {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        let zone_id = ZoneRepo::with_backend(db.clone())
            .create("Salon", Some("dlna"), Some(APPAREIL))
            .unwrap();
        PlayQueueRepo::with_backend(db.clone())
            .append(
                zone_id,
                &[
                    ligne("7:1", "So What"),
                    ligne("7:2", "Freddie Freeloader"),
                    ligne("7:3", "Blue in Green"),
                ],
            )
            .unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(
            MockOutput::new(APPAREIL, "Renderer").with_type("dlna"),
        ));
        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let fournisseur = Arc::new(Fournisseur {
            reponses: reponses
                .iter()
                .map(|(r, v)| (r.to_string(), v.clone()))
                .collect(),
            demandes: std::sync::Mutex::new(Vec::new()),
        });
        orchestrator
            .sources_url()
            .inscrire(SOURCE, fournisseur.clone());
        let bus = Arc::new(EventBus::new());
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus.clone());
        Self {
            poller,
            playback,
            db,
            zone_id,
            fournisseur,
            bus,
        }
    }

    async fn depart(&self) {
        self.poller
            .orchestrator
            .play_from_queue(self.zone_id, 0)
            .await
            .expect("la premiere piste devait partir");
        self.playback.update_queue_info(self.zone_id, 0, 3).await;
    }

    async fn fin_de_piste(&self) {
        let etat = self.playback.get_state(self.zone_id).await;
        self.poller.handle_track_end(self.zone_id, &etat).await;
    }

    fn demandes(&self) -> Vec<String> {
        self.fournisseur
            .demandes
            .lock()
            .unwrap()
            .iter()
            .map(|(zone, r)| {
                assert_eq!(*zone, self.zone_id, "la zone est transmise au fournisseur");
                r.clone()
            })
            .collect()
    }

    async fn ecran(&self) -> (i64, Option<String>, Option<String>) {
        let etat = self.playback.get_state(self.zone_id).await;
        let np = etat.now_playing;
        (
            etat.queue_position,
            np.as_ref().map(|n| n.source.clone()),
            np.and_then(|n| n.source_id),
        )
    }

    /// Aucune URL (donc aucun billet) dans la file ni dans la lecture en cours.
    async fn aucune_url_gardee(&self) {
        for e in PlayQueueRepo::with_backend(self.db.clone())
            .get_ordered(self.zone_id)
            .unwrap()
        {
            let sid = e.source_id.unwrap_or_default();
            assert!(
                !sid.contains("http") && !sid.contains("billet"),
                "une URL est entree dans la file : {sid}"
            );
            assert_eq!(e.source.as_deref(), Some(SOURCE));
        }
        if let Some(np) = self.playback.get_state(self.zone_id).await.now_playing {
            let sid = np.source_id.unwrap_or_default();
            assert!(
                !sid.contains("http") && !sid.contains("billet"),
                "une URL est entree dans la lecture en cours : {sid}"
            );
        }
    }
}

/// Trois pistes qui s'enchaînent : UNE demande d'URL par piste, au moment où
/// elle part, et seule la référence reste en file et en lecture.
#[tokio::test]
async fn trois_pistes_s_enchainent_avec_une_url_demandee_par_piste() {
    let banc = Banc::monter(&[]).await;
    banc.depart().await;
    assert_eq!(banc.demandes(), vec!["7:1"]);
    assert_eq!(
        banc.ecran().await,
        (0, Some(SOURCE.into()), Some("7:1".into()))
    );
    banc.aucune_url_gardee().await;

    banc.fin_de_piste().await;
    assert_eq!(banc.demandes(), vec!["7:1", "7:2"]);
    assert_eq!(
        banc.ecran().await,
        (1, Some(SOURCE.into()), Some("7:2".into()))
    );

    banc.fin_de_piste().await;
    assert_eq!(banc.demandes(), vec!["7:1", "7:2", "7:3"]);
    assert_eq!(
        banc.ecran().await,
        (2, Some(SOURCE.into()), Some("7:3".into()))
    );
    banc.aucune_url_gardee().await;
}

/// Une piste refusée (404/409 du cloud) est SAUTÉE, et le dit
/// (`playback.track_skipped`) ; la suivante part.
#[tokio::test]
async fn une_piste_refusee_est_sautee_et_la_suivante_part() {
    let banc = Banc::monter(&[("7:2", Reponse::Piste)]).await;
    let mut rx = banc.bus.subscribe();
    banc.depart().await;
    banc.fin_de_piste().await;

    assert_eq!(banc.demandes(), vec!["7:1", "7:2", "7:3"]);
    assert_eq!(
        banc.ecran().await,
        (2, Some(SOURCE.into()), Some("7:3".into())),
        "7:2 refusee, 7:3 doit jouer"
    );
    let mut sautee = None;
    while let Ok(e) = rx.try_recv() {
        if e.event_type == "playback.track_skipped" {
            sautee = Some(e.data);
        }
    }
    let sautee = sautee.expect("playback.track_skipped n'est pas parti");
    assert_eq!(sautee["position"], 1);
    assert!(
        sautee["reason"]
            .as_str()
            .unwrap()
            .contains(crate::source_url::MOTIF_PISTE_REFUSEE)
    );
    banc.aucune_url_gardee().await;
}

/// Le fournisseur dit « arrêt de la file » (serveur du contact éteint) : la
/// file s'arrête sur-le-champ, et la piste suivante n'est JAMAIS demandée.
#[tokio::test]
async fn un_arret_de_la_file_arrete_la_zone_sans_demander_la_suite() {
    let banc = Banc::monter(&[("7:2", Reponse::Arret)]).await;
    let mut rx = banc.bus.subscribe();
    banc.depart().await;
    banc.fin_de_piste().await;

    assert_eq!(
        banc.demandes(),
        vec!["7:1", "7:2"],
        "apres l'arret, 7:3 ne devait pas etre demandee"
    );
    let etat = banc.playback.get_state(banc.zone_id).await;
    assert_ne!(
        etat.state,
        crate::playback::PlayState::Playing,
        "la zone devait etre arretee"
    );
    while let Ok(e) = rx.try_recv() {
        assert_ne!(
            e.event_type, "playback.track_skipped",
            "un arret n'est pas un saut : {}",
            e.data
        );
    }
}
