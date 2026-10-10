//! #6059 — de bout en bout : un FLAC de la bibliothèque joue sur une zone
//! DLNA servi tel quel (session fichier). Sur un renderer profilé « Seek
//! inopérant », avancer relance le flux NATIF à la position — une URL portant
//! `depart_ms`, une carte d'en-tête réécrit — et aucun `Seek` SOAP ne part.
//! Sur un renderer sain, rien ne change : le `Seek` part, et il est noté pour
//! que le sondeur vérifie qu'il a été exécuté.
use super::PlaybackOrchestrator;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::dlna_repli_set_uri as profil;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const SR: u32 = 44_100;
const SECONDES: u32 = 8;
const CIBLE_MS: u64 = 5_000;

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

async fn sortie(orch: &PlaybackOrchestrator, appareil: &str) -> (usize, Vec<u64>, Option<String>) {
    let arc = { orch.outputs.lock().await.get(appareil) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (
        mock.play_call_count().await,
        mock.seek_calls(),
        mock.last_play_url().await,
    )
}

/// Un FLAC de la bibliothèque qui joue sur la zone DLNA, par une session
/// FICHIER (passthrough natif, cherchable par `Range`).
async fn flac_natif_en_lecture(
    appareil: &str,
) -> (PlaybackOrchestrator, i64, String, tempfile::TempDir) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("01 Natif.flac");
    let mut x: u32 = 7;
    let pcm: Vec<u8> = (0..SR * SECONDES * 2)
        .flat_map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (((x >> 16) as i16) / 8).to_le_bytes()
        })
        .collect();
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&pcm).await.unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Cyrille')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Natif', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'Natif', 1, 1, ?, 'flac', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(appareil))
        .unwrap();
    let mock = MockOutput::new(appareil, "R-N2000A").with_type("dlna");
    orch.outputs.lock().await.register(Box::new(mock));
    let sid = orch
        .streamer
        .create_file_session(
            crate::http::streamer::StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: SR,
                bit_depth: 16,
                channels: 2,
                ..Default::default()
            },
            chemin.clone(),
            true,
        )
        .await;
    orch.playback
        .play(
            zone_id,
            crate::playback::NowPlaying {
                title: "Natif".into(),
                track_id: Some(1),
                source: "local".into(),
                stream_id: Some(sid),
                duration_ms: SECONDES as i64 * 1000,
                ..Default::default()
            },
        )
        .await;
    (orch, zone_id, chemin, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seek_inoperant_relance_le_flux_natif_a_la_position_6059() {
    let _ = profil::base_de_test();
    let appareil = "dlna:uuid-rn2000a-bout-en-bout-6059";
    let (orch, zone_id, _chemin, _dir) = flac_natif_en_lecture(appareil).await;
    profil::memoriser_seek_inoperant(appareil);

    let resultat = orch.seek(zone_id, CIBLE_MS, Some(appareil)).await;
    assert!(resultat.is_ok(), "{resultat:?}");
    // Le Seek d'après relecture part DÉTACHÉ, après une pose : l'attendre,
    // sans quoi son absence ne prouverait rien.
    tokio::time::sleep(std::time::Duration::from_millis(
        super::session::REPLAY_OUTPUT_SEEK_SETTLE_MS + 700,
    ))
    .await;

    let (lectures, seeks, url) = sortie(&orch, appareil).await;
    assert!(
        seeks.is_empty(),
        "aucun Seek SOAP vers un renderer qui les ignore : {seeks:?}"
    );
    assert_eq!(lectures, 1, "le flux est relancé (un play_media)");
    let url = url.expect("une URL posée");
    let depart = crate::outputs::dlna_depart_natif::depart_de_l_url(&url)
        .unwrap_or_else(|| panic!("l'URL porte le départ : {url}"));
    assert!(
        depart <= CIBLE_MS && CIBLE_MS - depart < 100,
        "départ {depart}"
    );
    assert!(
        url.contains(".flac"),
        "toujours du FLAC natif, pas une conversion : {url}"
    );

    let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
    let sid = np.stream_id.expect("session");
    assert_eq!(
        crate::outputs::dlna_depart_natif::depart_de_session(&sid),
        Some(depart)
    );
    let sessions = orch.streamer.sessions_state();
    let sessions = sessions.lock().await;
    let session = sessions.get(&sid).expect("session vivante").clone();
    drop(sessions);
    let carte = session
        .faststart
        .lock()
        .unwrap()
        .clone()
        .expect("la carte du flux décalé est posée");
    assert_eq!(&carte.header[..4], b"fLaC");
    assert_eq!(
        session.info.file_size,
        Some(carte.total),
        "taille annoncée = servie"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renderer_sain_le_seek_part_et_sera_verifie_6059() {
    let _ = profil::base_de_test();
    let appareil = "dlna:uuid-renderer-sain-bout-en-bout-6059";
    let (orch, zone_id, _chemin, _dir) = flac_natif_en_lecture(appareil).await;
    assert!(!profil::seek_inoperant(appareil));

    let resultat = orch.seek(zone_id, CIBLE_MS, Some(appareil)).await;
    assert!(resultat.is_ok(), "{resultat:?}");
    let (lectures, seeks, _) = sortie(&orch, appareil).await;
    assert_eq!(seeks, vec![CIBLE_MS], "le Seek ordinaire part");
    assert_eq!(lectures, 0, "aucune relance");
    // Noté pour le sondeur : trop tôt pour juger, il reste en attente.
    let statut = tune_output_api::OutputStatus {
        state: tune_output_api::TransportState::Playing,
        position_ms: 0,
        duration_ms: 0,
        volume: 0.5,
        muted: false,
        current_uri: None,
        track_title: None,
        track_artist: None,
        ended_naturally: false,
        realtime: true,
        dop_active: false,
    };
    assert_eq!(super::seek_natif_6059::constater(zone_id, &statut), None);
    assert!(
        super::seek_natif_6059::seek_en_attente(zone_id),
        "Seek noté pour vérification"
    );
}
