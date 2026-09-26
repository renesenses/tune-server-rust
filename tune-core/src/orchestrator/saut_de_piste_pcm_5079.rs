//! #5079 (GgB, fil 1945) — choisir une autre piste d'un CD pendant la
//! lecture coupait le son 3 à 4 s au début de la nouvelle piste.
//!
//! ## La cause, mesurée sur un vrai lecteur (Apple SuperDrive, 26/09/2026)
//!
//! Saut « piste 2 → piste 5 », disque en rotation, cinq essais : la piste
//! visée arrive à ~5× le temps réel quand l'ancienne pompe est arrêtée
//! (premier octet ~0,3 s, aucun manque), mais à 0,55×–1,8× quand elle lit
//! encore — jusqu'à 4 s de manque sur les 5 premières secondes. La pompe
//! d'une piste lit à pleine vitesse jusqu'à remplir son canal (~41 s de
//! CD) ; rien ne l'arrêtait quand la zone passait à une autre piste.
//!
//! ## Les témoins
//!
//! Un faux disque à UN bras : chaque lecture prend 10 ms et compte les
//! lectures simultanées.
//!
//! 1. `une_lecture_explicite_arrete_la_pompe_precedente_de_la_zone` : après
//!    la résolution de la piste B, la pompe de A ne lit plus rien, et le
//!    disque n'a jamais servi deux lectures à la fois. Sur la base, A
//!    continue de lire (rouge).
//! 2. `un_pre_armement_gapless_laisse_finir_la_piste_qui_joue` : la
//!    contre-épreuve de périmètre. Le pré-armement de la piste suivante ne
//!    doit PAS couper la piste en cours, qui n'a pas fini d'être lue.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};
use crate::streaming::registry::ServiceRegistry;

use super::{PlayRequest, PlaybackOrchestrator};

/// Le bras : combien de lectures sont en cours, et le pire vu.
#[derive(Default)]
struct Bras {
    en_cours: AtomicUsize,
    pire: AtomicUsize,
}

/// Une piste : le nombre de lectures qu'elle a demandées au disque.
struct Piste {
    bras: Arc<Bras>,
    lectures: Arc<AtomicUsize>,
}

impl std::io::Read for Piste {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.bras.en_cours.fetch_add(1, Ordering::SeqCst) + 1;
        self.bras.pire.fetch_max(n, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(10));
        self.bras.en_cours.fetch_sub(1, Ordering::SeqCst);
        self.lectures.fetch_add(1, Ordering::SeqCst);
        buf.fill(1);
        Ok(buf.len())
    }
}

struct FauxDisque {
    bras: Arc<Bras>,
    /// Compteur de lectures par piste ouverte, dans l'ordre d'ouverture.
    pistes: std::sync::Mutex<Vec<Arc<AtomicUsize>>>,
}

impl FournisseurPcm for FauxDisque {
    fn ouvrir(&self, _source_id: &str, _depuis_ms: u64) -> Result<FluxPcm, String> {
        let lectures = Arc::new(AtomicUsize::new(0));
        self.pistes.lock().unwrap().push(lectures.clone());
        Ok(FluxPcm {
            format: FormatPcm::CD,
            // Une minute : la pompe ne remplit pas son canal pendant l'épreuve.
            octets: 60 * FormatPcm::CD.octets_par_seconde(),
            duree_ms: 60_000,
            lecteur: Box::new(Piste {
                bras: self.bras.clone(),
                lectures,
            }),
        })
    }
}

fn orchestrateur() -> (PlaybackOrchestrator, Arc<FauxDisque>) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let orch = PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let disque = Arc::new(FauxDisque {
        bras: Arc::default(),
        pistes: Default::default(),
    });
    orch.sources_pcm().inscrire("faux-cd-5079", disque.clone());
    (orch, disque)
}

fn piste(zone_id: i64, n: u8) -> PlayRequest {
    PlayRequest {
        zone_id,
        source: Some("faux-cd-5079".into()),
        source_id: Some(format!("disque/{n}")),
        ..Default::default()
    }
}

fn lectures(disque: &FauxDisque, i: usize) -> usize {
    disque.pistes.lock().unwrap()[i].load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_lecture_explicite_arrete_la_pompe_precedente_de_la_zone() {
    let zone_id = 985_791;
    let (orch, disque) = orchestrateur();
    orch.playback.play(zone_id, NowPlaying::default()).await;

    orch.resolve_stream(&piste(zone_id, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(lectures(&disque, 0) > 5, "la piste 2 est en lecture");

    // L'auditeur choisit la piste 5.
    orch.resolve_stream(&piste(zone_id, 5)).await.unwrap();
    let a_l_ouverture = lectures(&disque, 0);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        lectures(&disque, 0),
        a_l_ouverture,
        "la pompe de la piste 2 lit encore le disque après le choix de la piste 5 : \
         le bras se partage entre deux positions (fil 1945, 3 à 4 s de coupures)"
    );
    assert!(lectures(&disque, 1) > 5, "la piste 5 est lue");
    assert_eq!(
        disque.bras.pire.load(Ordering::SeqCst),
        1,
        "deux lectures simultanées sur un seul bras"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_pre_armement_gapless_laisse_finir_la_piste_qui_joue() {
    let zone_id = 985_792;
    let (orch, disque) = orchestrateur();
    orch.playback.play(zone_id, NowPlaying::default()).await;

    orch.resolve_stream(&piste(zone_id, 2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    {
        let _prewarm = orch.begin_levels_prewarm(zone_id);
        orch.resolve_stream(&piste(zone_id, 3)).await.unwrap();
    }
    let apres = lectures(&disque, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        lectures(&disque, 0) > apres,
        "le pré-armement de la piste 3 a coupé la lecture de la piste 2"
    );
}
