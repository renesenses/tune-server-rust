//! #5395 — la radio artiste se RECHARGE par la vraie fin de file.
//!
//! `handle_track_end` sur le dernier titre d'un lot de radio : un nouveau lot
//! est ajouté et joué, même quand l'auto-lecture de la zone est coupée (la
//! radio est sans fin par décision). Et l'inverse, qui garde l'auto-lecture
//! existante : une file qui s'achève sur un titre étranger à la radio n'est
//! plus la radio — le contexte est effacé et la zone suit son réglage (ici
//! `Off` : elle s'arrête, rien n'est ajouté).
use super::*;
use crate::db::{
    backend::DbBackend,
    play_queue_repo::PlayQueueRepo,
    sqlite::SqliteDb,
    zone_repo::{AutoplayMode, ZoneRepo},
};
use crate::outputs::mock::MockOutput;
use crate::playback::radio_artiste;

struct Banc {
    db: Arc<dyn DbBackend>,
    poller: PositionPoller,
    playback: Arc<PlaybackManager>,
    services: Arc<Mutex<crate::streaming::ServiceRegistry>>,
    zone_id: i64,
    bus: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    _tmp: tempfile::TempDir,
}

async fn banc() -> Banc {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_id = repo
        .create("Radio", Some("mock"), Some("mock-radio"))
        .unwrap();
    repo.update_autoplay_mode(zone_id, AutoplayMode::Off)
        .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let mut wav =
        crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(1000)).to_vec();
    wav.resize(wav.len() + 44100 * 4, 0);
    // `tracks.file_path` est unique : un fichier par titre.
    let chemin = |id: i64| {
        let p = tmp.path().join(format!("{id}.wav"));
        std::fs::write(&p, &wav).unwrap();
        p.to_str().unwrap().to_string()
    };
    let mut id = 1i64;
    for (aid, nom) in [
        (1i64, "Graine"),
        (2, "Voisin A"),
        (3, "Voisin B"),
        (4, "Voisin C"),
        (5, "Voisin D"),
    ] {
        db.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&aid, &nom],
        )
        .unwrap();
        for n in 0..10 {
            let titre = format!("{nom} {n}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, genre, file_path, format, sample_rate, bit_depth, duration_ms) \
                 VALUES (?, ?, ?, 'Jazz', ?, 'wav', 44100, 16, 1000)",
                &[&id, &titre.as_str(), &aid, &chemin(id).as_str()],
            )
            .unwrap();
            id += 1;
        }
    }
    // Un titre hors radio, pour la contre-épreuve : d'un AUTRE artiste et
    // d'un autre genre, pour qu'aucun lot ne puisse le tirer.
    db.execute(
        "INSERT INTO artists (id, name) VALUES (6, 'Hors radio')",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO tracks (id, title, artist_id, genre, file_path, format, sample_rate, bit_depth, duration_ms) \
         VALUES (999, 'Hors radio', 6, 'Metal', ?, 'wav', 44100, 16, 1000)",
        &[&chemin(999).as_str()],
    )
    .unwrap();

    let playback = Arc::new(PlaybackManager::new());
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs
        .lock()
        .await
        .register(Box::new(MockOutput::new("mock-radio", "Radio")));
    let services = Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new()));
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        services.clone(),
        outputs.clone(),
        None,
    ));
    let bus = Arc::new(crate::event_bus::EventBus::new());
    let events = bus.subscribe();
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs,
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    )
    .with_event_bus(bus);
    Banc {
        db,
        poller,
        playback,
        services,
        zone_id,
        bus: events,
        _tmp: tmp,
    }
}

/// La file après le départ de la radio, lecture sur son DERNIER titre.
async fn file_de_radio(b: &Banc) -> (crate::playback::ZoneState, usize) {
    let lot = radio_artiste::demarrer(&b.db, &b.services, b.zone_id, "Graine", None, None).await;
    assert!(!lot.candidats.is_empty());
    let items: Vec<_> = lot
        .candidats
        .iter()
        .map(radio_artiste::Candidat::en_entree_de_file)
        .collect();
    PlayQueueRepo::with_backend(b.db.clone())
        .append(b.zone_id, &items)
        .unwrap();
    let dernier = match lot.candidats.last().unwrap() {
        radio_artiste::Candidat::Local { track_id, .. } => *track_id,
        autre => panic!("bibliothèque seule : {autre:?}"),
    };
    let n = items.len();
    let etat = crate::playback::ZoneState {
        zone_id: b.zone_id,
        queue_length: n as i64,
        queue_position: n as i64 - 1,
        now_playing: Some(crate::playback::NowPlaying {
            track_id: Some(dernier),
            title: "dernier".into(),
            source: "local".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    (etat, n)
}

#[tokio::test]
async fn la_fin_d_un_lot_recharge_la_radio_meme_auto_lecture_coupee() {
    let mut b = banc().await;
    let (etat, n) = file_de_radio(&b).await;
    b.poller.handle_track_end(b.zone_id, &etat).await;

    let queue = PlayQueueRepo::with_backend(b.db.clone());
    let total = queue.count_all(b.zone_id).unwrap() as usize;
    assert!(
        total > n,
        "un nouveau lot est ajouté : {total} lignes pour {n}"
    );
    let apres = b.playback.get_state(b.zone_id).await;
    assert_eq!(apres.state, PlayState::Playing, "{apres:?}");
    assert_eq!(
        apres.queue_position, n as i64,
        "la lecture part du lot ajouté"
    );
    let ctx = radio_artiste::lire_contexte(&b.db, b.zone_id).unwrap();
    assert_eq!(ctx.lots, 2);
    let mut annonce = None;
    while let Ok(e) = b.bus.try_recv() {
        if e.event_type == "playback.autoplay_tracks_added" {
            annonce = Some(e.data);
        }
    }
    let annonce = annonce.expect("l'ajout est annoncé");
    assert_eq!(annonce["radio_artiste"], true);
    // Aucun titre du premier lot n'est redit par le second.
    let lignes = queue.get_ordered(b.zone_id).unwrap();
    let premiers: std::collections::HashSet<_> =
        lignes[..n].iter().filter_map(|l| l.track_id).collect();
    assert!(
        lignes[n..]
            .iter()
            .all(|l| !premiers.contains(&l.track_id.unwrap()))
    );
}

#[tokio::test]
async fn une_file_finie_hors_radio_suit_le_reglage_de_la_zone() {
    let mut b = banc().await;
    let (mut etat, n) = file_de_radio(&b).await;
    // L'auditeur a ajouté un titre à lui après la radio, et il se termine.
    PlayQueueRepo::with_backend(b.db.clone())
        .append_tracks(b.zone_id, &[999])
        .unwrap();
    etat.queue_length = n as i64 + 1;
    etat.queue_position = n as i64;
    etat.now_playing.as_mut().unwrap().track_id = Some(999);
    b.poller.handle_track_end(b.zone_id, &etat).await;

    let queue = PlayQueueRepo::with_backend(b.db.clone());
    assert_eq!(
        queue.count_all(b.zone_id).unwrap() as usize,
        n + 1,
        "rien n'est ajouté"
    );
    assert_ne!(
        b.playback.get_state(b.zone_id).await.state,
        PlayState::Playing
    );
    assert!(
        radio_artiste::lire_contexte(&b.db, b.zone_id).is_none(),
        "la radio quittée est oubliée"
    );
    while let Ok(e) = b.bus.try_recv() {
        assert_ne!(e.event_type, "playback.autoplay_tracks_added");
    }
}
