use super::*;
use crate::db::zone_repo::{AutoplayMode, ZoneRepo};
use crate::playback::{NowPlaying, PlayState, PlaybackManager};

/// Fin du flux confié par Windows : la bibliothèque du Tune destinataire
/// contient encore un album, et son autoplay est activé. Aucune sortie réelle.
async fn fin_de_flux(
    source: &str,
    mode: AutoplayMode,
    track_id: Option<i64>,
    suivante: bool,
) -> (usize, PlayState) {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let zones = ZoneRepo::with_backend(db.clone());
    let zone = zones
        .create("Façade", Some("mock"), Some("mock:6634"))
        .unwrap();
    zones.update_autoplay_mode(zone, mode).unwrap();
    let fichiers = tempfile::tempdir().unwrap();
    let fichier = fichiers.path().join("piano.wav");
    let mut wav =
        crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(1000)).to_vec();
    wav.resize(wav.len() + 44100 * 4, 0);
    std::fs::write(&fichier, wav).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'M Uchida');
        INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'Piano', 1, 2000);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, duration_ms)
        VALUES (1, 'Piano', 1, 1, ?, 'wav', 1000)",
        &[&fichier.to_str().unwrap()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO queue_items (zone_id, position, source, source_id, title, duration_ms)
        VALUES (?, 0, 'upnp', 'http://127.0.0.1/controleur.flac', 'Inperayritz', 345000)",
        &[&zone],
    )
    .unwrap();
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::mock::MockOutput::new(
            "mock:6634",
            "Sans son",
        )));
    let playback = Arc::new(PlaybackManager::new());
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    playback
        .play(
            zone,
            NowPlaying {
                track_id,
                title: "Inperayritz".into(),
                source: source.into(),
                source_id: Some("http://127.0.0.1/controleur.flac".into()),
                duration_ms: 345000,
                ..Default::default()
            },
        )
        .await;
    if suivante {
        db.execute(
            "INSERT INTO queue_items (zone_id, position, track_id, source)
            VALUES (?, 1, 1, 'local')",
            &[&zone],
        )
        .unwrap();
    }
    playback
        .update_queue_info(zone, 0, if suivante { 2 } else { 1 })
        .await;
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs,
        db.clone(),
        Arc::new(Mutex::new(HashMap::new())),
    );
    let state = playback.get_state(zone).await;
    poller.handle_track_end(zone, &state).await;
    let rows = db
        .query_many("SELECT id FROM queue_items WHERE zone_id = ?", &[&zone])
        .unwrap();
    (rows.len(), playback.get_state(zone).await.state)
}

#[tokio::test]
async fn fin_upnp_6634_ne_lance_pas_le_piano_local() {
    for mode in [
        AutoplayMode::RandomAlbum,
        AutoplayMode::RandomArtist,
        AutoplayMode::RandomYear,
        AutoplayMode::RandomTracks,
    ] {
        let (taille, etat) = fin_de_flux("upnp", mode, None, false).await;
        assert_eq!(
            taille, 1,
            "fin UPnP : autoplay a ajouté du piano local ({mode:?})"
        );
        assert_eq!(
            etat,
            PlayState::Stopped,
            "le contrôleur doit décider de la suite"
        );
    }
}

#[tokio::test]
async fn fin_upnp_6634_preserve_autoplay_d_une_lecture_tune() {
    let (taille, _) = fin_de_flux("local", AutoplayMode::RandomAlbum, None, false).await;
    assert_eq!(taille, 2, "une lecture Tune garde sa continuation locale");
}

/// Diagnostic du second symptôme : le vrai sondeur reçoit PLAYING/0,
/// puis PLAYING/2000 du renderer. Cela ne reproduit pas le renderer du testeur.
#[tokio::test]
async fn fin_upnp_6634_position_du_renderer_publiee_sans_zero_force() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Position", Some("mock"), Some("mock:position6634"))
        .unwrap();
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    let sortie = crate::outputs::mock::MockOutput::new("mock:position6634", "Sans son");
    sortie.set_state(TransportState::Playing).await;
    sortie.set_duration(345000);
    outputs.lock().await.register(Box::new(sortie));
    let playback = Arc::new(PlaybackManager::new());
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    playback
        .play(
            zone,
            NowPlaying {
                source: "upnp".into(),
                title: "O Virgo splendens".into(),
                duration_ms: 345000,
                ..Default::default()
            },
        )
        .await;
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs.clone(),
        db,
        Arc::new(Mutex::new(HashMap::new())),
    );
    let mut polls = HashMap::new();
    let mut idle = HashMap::new();
    for position in [0_u64, 2000] {
        outputs
            .lock()
            .await
            .get("mock:position6634")
            .unwrap()
            .lock()
            .await
            .seek(position)
            .await
            .unwrap();
        poller.tick(&mut polls, &mut idle, &Instant::now()).await;
        let state = playback.get_state(zone).await;
        assert_eq!(
            state.position_ms, position as i64,
            "la position du renderer est perdue avant la façade"
        );
        let response =
            crate::upnp_renderer::position_info_response(&crate::upnp_renderer::RendererSnapshot {
                transport_state: "PLAYING",
                position_ms: state.position_ms,
                duration_ms: 345000,
                uri: String::new(),
                next_uri: String::new(),
                next_metadata: String::new(),
                volume: 50,
                muted: false,
            });
        let expected = if position == 0 {
            "<RelTime>0:00:00</RelTime>"
        } else {
            "<RelTime>0:00:02</RelTime>"
        };
        assert!(
            response.contains(expected),
            "position SOAP perdue : {response}"
        );
    }
}

#[tokio::test]
async fn fin_upnp_6634_preserve_autoplay_d_un_media_server_indexe() {
    let (taille, _) = fin_de_flux("upnp", AutoplayMode::RandomAlbum, Some(1), false).await;
    assert_eq!(taille, 2, "une piste indexée garde sa continuation Tune");
}

#[tokio::test]
async fn fin_upnp_6634_joue_encore_la_suivante_explicitement_confiee() {
    let (taille, etat) = fin_de_flux("upnp", AutoplayMode::RandomAlbum, None, true).await;
    assert_eq!(taille, 2, "la suivante confiée ne doit pas être effacée");
    assert_eq!(
        etat,
        PlayState::Playing,
        "une suivante déjà confiée doit jouer"
    );
}
