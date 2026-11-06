//! Complément #6630 : durée restante servie après déplacement, durée entière
//! de la piste annoncée au client. FLAC synthétique et sortie locale factice,
//! aucun périphérique. Identifiants de zones propres à chaque témoin.
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::mock::MockOutput;
use std::sync::Arc;
use std::time::Duration;

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

pub(super) async fn montage(
    zone_id: i64,
) -> (PlaybackOrchestrator, i64, String, tempfile::TempDir) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("1-02 Is It Like Today.flac");
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, 16, 2);
    enc.start().await.unwrap();
    enc.write(&vec![0u8; (SR * SECONDES * 4) as usize])
        .await
        .unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'World Party')",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Bang!', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'Is It Like Today?', 1, 1, ?, 'flac', {}, {SR}, 16, 2)",
                SECONDES as i64 * 1000
            ),
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let appareil = format!("local:duree-convertie-6630-{zone_id}");
    orch.db.execute(
        &format!("INSERT INTO zones (id, name, output_type, output_device_id) VALUES ({zone_id}, 'Fausse sortie', 'local', ?)"),
        &[&appareil as &dyn crate::db::backend::ToSqlValue],
    ).unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(&appareil, "Fausse sortie").with_type("local"),
    ));
    orch.play(PlayRequest {
        zone_id,
        output_device_id: Some(appareil.clone()),
        track_id: Some(1),
        source: Some("local".into()),
        ..Default::default()
    })
    .await
    .expect("lecture factice préparée");
    (orch, zone_id, appareil, dir)
}

pub(super) async fn constater(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
) -> (
    crate::playback::NowPlaying,
    crate::http::streamer::StreamInfo,
    Vec<u8>,
    i64,
) {
    let etat = orch.playback.get_state(zone_id).await;
    let np = etat.now_playing.expect("piste installée");
    let sid = np.stream_id.as_deref().expect("session installée");
    assert!(
        !orch.streamer.is_seekable_session(sid).await,
        "prémisse : conversion en canal, pas fichier entier cherchable"
    );
    let info = orch
        .streamer
        .stream_output_wire(sid)
        .await
        .expect("format réellement servi");
    assert_eq!(info.mime_type, "audio/wav");
    let sessions = orch.streamer.sessions_state();
    let session = sessions
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session présente");
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut octets = Vec::new();
        while let Some(bloc) = session.recv_chunk().await {
            octets.extend_from_slice(&bloc);
        }
        octets
    })
    .await
    .expect("le décodeur finit le fichier synthétique");
    assert_eq!(&octets[..4], b"RIFF");
    assert_eq!(&octets[8..12], b"WAVE");
    (np, info, octets, etat.position_ms)
}

fn longueur_reelle(info: &crate::http::streamer::StreamInfo, octets: &[u8], duree_ms: u64) {
    assert_eq!(
        info.duration_ms,
        Some(duree_ms),
        "la session décrit le temps restant à servir"
    );
    let attendue = 44
        + duree_ms
            * u64::from(info.sample_rate)
            * u64::from(info.channels)
            * u64::from(info.bit_depth / 8)
            / 1000;
    assert_eq!(info.wav_content_length(), Some(attendue));
    assert_eq!(
        octets.len() as u64,
        attendue,
        "le décodeur sert exactement la longueur annoncée"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depuis_le_debut_le_flux_et_la_piste_gardent_six_secondes_6630() {
    let (orch, zone, appareil, _dir) = montage(663001).await;
    let (np, info, octets, _) = constater(&orch, zone).await;
    longueur_reelle(&info, &octets, 6000);
    assert_eq!(np.duration_ms, 6000);
    orch.stop(zone, Some(&appareil)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_deplacement_trois_secondes_servies_mais_six_annoncees_6630() {
    let (orch, zone, appareil, _dir) = montage(663002).await;
    orch.seek(zone, 3000, Some(&appareil))
        .await
        .expect("premier déplacement");
    let (np, info, octets, position) = constater(&orch, zone).await;
    longueur_reelle(&info, &octets, 3000);
    assert_eq!(position, 3000);
    assert_eq!(
        np.duration_ms, 6000,
        "la piste reste entière, même si le flux ne sert plus que trois secondes"
    );
    orch.stop(zone, Some(&appareil)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_second_deplacement_peut_encore_atteindre_cinq_secondes_6630() {
    let (orch, zone, appareil, _dir) = montage(663003).await;
    orch.seek(zone, 3000, Some(&appareil))
        .await
        .expect("premier déplacement");
    let _ = constater(&orch, zone).await;
    orch.seek(zone, 5000, Some(&appareil))
        .await
        .expect("second déplacement");
    let (np, info, octets, position) = constater(&orch, zone).await;
    assert_eq!(
        position, 5000,
        "la seconde cible n'est pas bornée par la durée restante du premier flux"
    );
    longueur_reelle(&info, &octets, 1000);
    assert_eq!(np.duration_ms, 6000);
    orch.stop(zone, Some(&appareil)).await;
}

#[test]
fn la_duree_decodee_ne_change_pas_les_regles_de_seek_natif_7360() {
    let session = "temoin-duree-pcm-isole-7360";
    super::noter_depart_pcm_decode(session, 3000);
    assert_eq!(
        super::duree_piste_pcm_decodee(3000, Some(session)),
        Some(6000)
    );
    assert_eq!(super::duree_piste_pcm_decodee(0, Some(session)), None);
    assert_eq!(super::duree_piste_pcm_decodee(3000, None), None);
    assert_eq!(
        crate::outputs::dlna_depart_natif::depart_de_session(session),
        None,
        "une conversion PCM ne devient pas un départ natif qui ferait sauter le Seek de l'agent"
    );
    assert_eq!(
        crate::outputs::dlna_depart_natif::duree_de_la_piste(3000, Some(session)),
        3000
    );
}
