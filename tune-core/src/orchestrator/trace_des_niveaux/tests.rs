//! #5104 — les témoins CAPTURENT le journal au niveau INFO, celui des exports
//! de terrain, et lisent ce qu'un testeur enverrait.
//!
//! 🔴 La capture passe par `set_default`, local au fil : les tests tournent
//! sur un runtime `current_thread`, où les tâches du forwarder s'exécutent
//! sur le fil du test. Et le cache d'intérêt de `tracing` est neutralisé en
//! tête de chaque test (voir [`fiabiliser_la_capture`]).

use std::sync::{Arc, Mutex, OnceLock};

use super::super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

/// #5440 — sans cela, un fil voisin qui atteint le PREMIER l'un de nos points
/// d'appel sans abonné le fige à `never` pour tout le processus, et la ligne
/// n'arrive jamais au nôtre. Deux abonnés inertes gardés en vie baissent pour
/// de bon le raccourci `has_just_one` de tracing-core. C'est le remède de
/// `journal_de_test::fiabiliser_la_capture()` (PR #5452), qui n'est pas
/// encore sur `main` : à remplacer par lui quand il y sera.
fn fiabiliser_la_capture() {
    static TEMOINS: OnceLock<[tracing::Dispatch; 2]> = OnceLock::new();
    TEMOINS.get_or_init(|| {
        [
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
            tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()),
        ]
    });
}

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Journal {
    fn write(&mut self, octets: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(octets);
        Ok(octets.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn lignes(&self, evenement: &str) -> Vec<String> {
        self.texte()
            .lines()
            .filter(|l| l.contains(evenement))
            .map(str::to_owned)
            .collect()
    }
    /// Attend, en laissant tourner les tâches, qu'une ligne `evenement`
    /// apparaisse ; rend les lignes vues.
    async fn attendre(&self, evenement: &str) -> Vec<String> {
        for _ in 0..500 {
            let vues = self.lignes(evenement);
            if !vues.is_empty() {
                return vues;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Vec::new()
    }
}

fn capturer() -> (Journal, tracing::subscriber::DefaultGuard) {
    fiabiliser_la_capture();
    let journal = Journal::default();
    let garde = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .finish(),
    );
    (journal, garde)
}

/// Une trame de PCM : 4096 octets de silence 16 bits stéréo.
fn une_fenetre(tx: &tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>) {
    crate::audio::tap::send_windowed_pcm(tx, &[0u8; 4096], 16, 2, 44_100);
}

/// **Le témoin.** La lecture a changé avant que le forwarder ne publie : il
/// meurt, et le journal dit pourquoi.
#[tokio::test]
async fn un_forwarder_mort_sans_rien_publier_dit_son_motif() {
    let (journal, _garde) = capturer();
    let zone_id = 5_104_001;
    let playback = Arc::new(PlaybackManager::new());
    playback.play(zone_id, NowPlaying::default()).await;
    let perime = playback.current_play_seq(zone_id).await;
    playback.bump_generation(zone_id).await;
    playback.play(zone_id, NowPlaying::default()).await;
    assert_ne!(playback.current_play_seq(zone_id).await, perime);

    let bus = Arc::new(EventBus::new());
    let tx = super::super::spawn_paced_levels_forwarder(bus, playback, zone_id, perime, 0);
    une_fenetre(&tx);

    let lignes = journal
        .attendre("levels_forwarder_stopped_unpublished")
        .await;
    assert_eq!(
        lignes.len(),
        1,
        "un forwarder mort sans rien publier doit l'écrire en INFO, une fois : {}",
        journal.texte()
    );
    let ligne = &lignes[0];
    assert!(ligne.contains(" INFO "), "niveau INFO attendu : {ligne}");
    assert!(ligne.contains("play_seq_change"), "motif absent : {ligne}");
    assert!(
        ligne.contains("fenetres_recues=1"),
        "fenêtres reçues absentes : {ligne}"
    );
    assert!(
        ligne.contains(&format!("zone_id={zone_id}")),
        "zone absente : {ligne}"
    );
    drop(tx);
}

/// Débit limité : deux forwarders de la MÊME piste qui meurent muets ne
/// font qu'une ligne. Une autre piste en refait une.
#[tokio::test]
async fn une_ligne_par_piste_au_plus() {
    let (journal, _garde) = capturer();
    let zone_id = 5_104_002;
    let playback = Arc::new(PlaybackManager::new());
    let play_seq = playback.current_play_seq(zone_id).await;
    let bus = Arc::new(EventBus::new());

    for _ in 0..3 {
        let tx = super::super::spawn_paced_levels_forwarder(
            bus.clone(),
            playback.clone(),
            zone_id,
            play_seq,
            0,
        );
        drop(tx); // flux clos sans une fenêtre
    }
    journal
        .attendre("levels_forwarder_stopped_unpublished")
        .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let lignes = journal.lignes("levels_forwarder_stopped_unpublished");
    assert_eq!(
        lignes.len(),
        1,
        "trois morts sur la même piste : {lignes:?}"
    );
    assert!(lignes[0].contains("flux_clos"), "{}", lignes[0]);

    // Piste suivante (génération bumpée par l'avance) : une nouvelle ligne.
    playback.bump_levels_gen(zone_id);
    drop(super::super::spawn_paced_levels_forwarder(
        bus, playback, zone_id, play_seq, 0,
    ));
    for _ in 0..500 {
        if journal.lignes("levels_forwarder_stopped_unpublished").len() > 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        journal.lignes("levels_forwarder_stopped_unpublished").len(),
        2,
        "une autre piste doit pouvoir le dire à son tour"
    );
}

/// Un forwarder qui a publié ne dit rien en mourant : c'est la vie normale
/// d'une piste, et le journal ne doit pas s'en remplir.
#[tokio::test]
async fn un_forwarder_qui_a_publie_se_tait() {
    let (journal, _garde) = capturer();
    let zone_id = 5_104_003;
    let playback = Arc::new(PlaybackManager::new());
    playback.play(zone_id, NowPlaying::default()).await;
    let play_seq = playback.current_play_seq(zone_id).await;
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    let tx = super::super::spawn_paced_levels_forwarder(
        bus.clone(),
        playback.clone(),
        zone_id,
        play_seq,
        0,
    );
    une_fenetre(&tx);
    let publie = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(ev) = rx.recv().await
                && ev.event_type == "playback.audio_levels"
            {
                break;
            }
        }
    })
    .await;
    assert!(publie.is_ok(), "le forwarder aurait dû publier une trame");
    drop(tx);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        journal
            .lignes("levels_forwarder_stopped_unpublished")
            .is_empty(),
        "un forwarder qui a publié ne doit rien écrire : {}",
        journal.texte()
    );
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let mut orch = PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(ServiceRegistry::new())),
        Arc::new(tokio::sync::Mutex::new(OutputRegistry::new())),
        None,
    );
    orch.event_bus = Some(Arc::new(EventBus::new()));
    orch
}

/// **De bout en bout, le chemin de Didier.** L'avance gapless vers une piste
/// locale écrit qu'elle lance la mesure ; le fichier est illisible, et les
/// deux sorties jusqu'ici muettes (décodage en échec, forwarder mort sans
/// rien publier) sont au journal, en INFO.
#[tokio::test]
async fn l_enchainement_dit_s_il_lance_la_mesure_et_pourquoi_elle_meurt() {
    let (journal, _garde) = capturer();
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Zone 5104", Some("local"), None)
        .unwrap();
    let pistes = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone());
    let mut ids = Vec::new();
    for i in 1..=2 {
        let mut piste = crate::db::models::Track::new(format!("Piste {i}"));
        piste.file_path = Some(format!("/aucun/chemin/5104/piste{i}.flac"));
        piste.track_number = i;
        piste.duration_ms = 180_000;
        ids.push(pistes.create(&piste).unwrap());
    }
    PlayQueueRepo::with_backend(orch.db.clone())
        .set_queue(zone_id, &ids)
        .unwrap();

    // La table de débit est celle du PROCESSUS : d'autres tests de la lib
    // font avancer eux aussi une « zone 1 » neuve (même `play_seq`, même
    // génération), et leur ligne passerait pour la nôtre. Une génération
    // propre à ce test ; l'avance la bumpe comme d'habitude.
    orch.playback
        .levels_gen(zone_id)
        .store(5_104_000, std::sync::atomic::Ordering::Relaxed);
    orch.advance_queue_metadata(zone_id, 1).await.unwrap();

    let avance = journal.lignes("gapless_levels_after_advance");
    assert_eq!(avance.len(), 1, "journal : {}", journal.texte());
    assert!(avance[0].contains(" INFO "), "{}", avance[0]);
    assert!(avance[0].contains("decodage_du_fichier"), "{}", avance[0]);
    assert!(avance[0].contains("generation=5104001"), "{}", avance[0]);
    assert!(
        avance[0].contains(&format!("track_id={}", ids[1])),
        "{}",
        avance[0]
    );

    let echec = journal.attendre("gapless_levels_decode_failed").await;
    assert_eq!(echec.len(), 1, "journal : {}", journal.texte());
    assert!(echec[0].contains(" INFO "), "{}", echec[0]);

    let arret = journal
        .attendre("levels_forwarder_stopped_unpublished")
        .await;
    assert_eq!(arret.len(), 1, "journal : {}", journal.texte());
    assert!(arret[0].contains("flux_clos"), "{}", arret[0]);
    assert!(arret[0].contains("fenetres_recues=0"), "{}", arret[0]);
}
