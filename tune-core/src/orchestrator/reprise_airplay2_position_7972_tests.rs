//! #7972 — une zone AirPlay 2 rétablie après une pause où le récepteur a fermé
//! sa connexion (#7921) : la piste de la bibliothèque est rejouée à la
//! position (`retablir_a_la_position`, `seek_ms`). Le démon lit son fichier
//! depuis le début : le flux qu'on lui sert doit donc COMMENCER à la position.
//!
//! Contre-épreuve : sans `airplay2_relance_a_la_position` dans
//! `reconnaitre_les_sorties`, la piste part en passthrough, session fichier
//! servie depuis l'octet 0 — la musique repartait à 0:00.
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const SR: u32 = 44_100;
const SECONDES: u32 = 6;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Une piste FLAC de six secondes et une zone AirPlay 2 factice.
async fn montage(zone_id: i64) -> (PlaybackOrchestrator, String, tempfile::TempDir) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("piste-7972.flac");
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&vec![0u8; (SR * SECONDES * 4) as usize])
        .await
        .unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Artiste')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'Piste', 1, 1, ?, 'flac', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let appareil = format!("airplay-7972-{zone_id}");
    orch.db
        .execute(
            &format!(
                "INSERT INTO zones (id, name, output_type, output_device_id) \
                 VALUES ({zone_id}, 'Enceinte', 'airplay2', ?)"
            ),
            &[&appareil as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(&appareil, "Enceinte").with_type("airplay2"),
    ));
    (orch, appareil, dir)
}

fn requete(zone_id: i64, appareil: &str, seek_ms: Option<u64>) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(appareil.to_string()),
        track_id: Some(1),
        source: Some("local".into()),
        seek_ms,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_relance_a_la_position_sert_un_flux_qui_commence_a_la_position_7972() {
    let zone = 797_201;
    let (orch, appareil, _dir) = montage(zone).await;
    orch.play_without_history(requete(zone, &appareil, Some(3_000)))
        .await
        .expect("relance à la position");

    let np = orch
        .playback
        .get_state(zone)
        .await
        .now_playing
        .expect("piste installée");
    let sid = np.stream_id.as_deref().expect("session installée");
    let info = orch
        .streamer
        .stream_output_wire(sid)
        .await
        .expect("format réellement servi");
    assert_eq!(
        info.mime_type, "audio/wav",
        "la relance d'une zone AirPlay 2 à 0:03 doit prendre le bras qui décode \
         à partir de seek_ms, pas le passthrough servi depuis l'octet 0 (#7972)"
    );
    assert_eq!(
        info.duration_ms,
        Some(3_000),
        "le flux sert ce qui reste à partir de la position"
    );
    assert!(
        !orch.streamer.is_seekable_session(sid).await,
        "conversion en canal, qui commence à la position"
    );
    orch.stop(zone, Some(&appareil)).await;
}

/// Une lecture depuis le début ne change pas de bras : la piste part telle
/// quelle, comme avant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_lecture_depuis_le_debut_reste_en_passthrough_7972() {
    let zone = 797_202;
    let (orch, appareil, _dir) = montage(zone).await;
    orch.play_without_history(requete(zone, &appareil, None))
        .await
        .expect("lecture");
    let np = orch
        .playback
        .get_state(zone)
        .await
        .now_playing
        .expect("piste installée");
    let sid = np.stream_id.as_deref().expect("session installée");
    let info = orch
        .streamer
        .stream_output_wire(sid)
        .await
        .expect("format réellement servi");
    assert_ne!(info.mime_type, "audio/wav", "pas de décodage sans position");
    orch.stop(zone, Some(&appareil)).await;
}

/// Après la relance, l'orchestrateur dit à la sortie AirPlay 2 où commence le
/// flux : le temps publié suit ce qu'on entend, à partir de la position.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_sortie_airplay2_apprend_le_depart_du_flux_relance_7972() {
    let orch = orchestrateur();
    let appareil = "airplay-7972-depart";
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::airplay2::Airplay2Output::new(
            "Enceinte".into(),
            "127.0.0.1".into(),
            7000,
            appareil.into(),
            "00:11:22:33:44:55".into(),
        )));
    orch.seek_output_after_replay(797_203, Some(appareil), Some("airplay2"), 139_000)
        .await;
    let output = orch.outputs.lock().await.get(appareil).expect("sortie");
    let statut = output.lock().await.get_status().await.unwrap();
    assert_eq!(
        statut.position_ms, 139_000,
        "la position publiée part de l'endroit de la pause, pas de 0:00"
    );
}
