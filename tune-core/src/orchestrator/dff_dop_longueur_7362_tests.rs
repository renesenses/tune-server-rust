//! Contrôle privé DFF après #7362 : comparer les annonces au corps réel.
use crate::orchestrator::{PlayRequest, PlaybackOrchestrator};
use crate::outputs::mock::MockOutput;
use std::sync::Arc;
use std::time::Duration;

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

const CADENCE: u32 = 2_822_400;
fn ecrire_dff(chemin: &std::path::Path, octets_par_canal: usize) {
    let mut data = Vec::new();
    for i in 0..octets_par_canal {
        for canal in 0..2 {
            data.push(if (i + canal) % 2 == 0 { 0xFF } else { 0x00 });
        }
    }
    let mut fver = Vec::new();
    fver.extend_from_slice(b"FVER");
    fver.extend_from_slice(&4u64.to_be_bytes());
    fver.extend_from_slice(&0x0105_0000u32.to_be_bytes());
    let mut prop = Vec::new();
    prop.extend_from_slice(b"SND ");
    prop.extend_from_slice(b"FS  ");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(&CADENCE.to_be_bytes());
    prop.extend_from_slice(b"CHNL");
    prop.extend_from_slice(&(2 + 4 * 2u64).to_be_bytes());
    prop.extend_from_slice(&2u16.to_be_bytes());
    prop.extend_from_slice(b"SLFT");
    prop.extend_from_slice(b"SRGT");
    prop.extend_from_slice(b"CMPR");
    prop.extend_from_slice(&4u64.to_be_bytes());
    prop.extend_from_slice(b"DSD ");
    let frm8_size = 4 + fver.len() + 12 + prop.len() + 12 + data.len();
    let mut buf = Vec::new();
    buf.extend_from_slice(b"FRM8");
    buf.extend_from_slice(&(frm8_size as u64).to_be_bytes());
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&fver);
    buf.extend_from_slice(b"PROP");
    buf.extend_from_slice(&(prop.len() as u64).to_be_bytes());
    buf.extend_from_slice(&prop);
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&(data.len() as u64).to_be_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(chemin, &buf).unwrap();
}

async fn verifier_dff_dop(zone: i64, depart_ms: u64) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("dop-synthetique.dff");
    // DFF brut stéréo de trois secondes ; départ entre trames DoP.
    ecrire_dff(&fichier, 3 * 352_800);
    let mut piste = crate::db::models::Track::new("DoP synthétique".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("dff".into());
    piste.duration_ms = 3000;
    piste.sample_rate = Some(2_822_400);
    piste.bit_depth = Some(1);
    piste.channels = 2;
    let piste_id = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .create(&piste)
        .unwrap();
    let appareil = format!("local:dop-synthetique-7362-{zone}");
    orch.db.execute(
        &format!("INSERT INTO zones (id, name, output_type, output_device_id) VALUES ({zone}, 'DoP factice', 'local', ?)"),
        &[&appareil as &dyn crate::db::backend::ToSqlValue],
    ).unwrap();
    crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone())
        .update_dsd_mode(zone, "dop")
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(&appareil, "DoP factice").with_type("local"),
    ));
    orch.play(PlayRequest {
        zone_id: zone,
        output_device_id: Some(appareil.clone()),
        track_id: Some(piste_id),
        source: Some("local".into()),
        seek_ms: Some(depart_ms),
        ..Default::default()
    })
    .await
    .expect("résolution et lecture factices DoP");
    let np = orch
        .playback
        .get_state(zone)
        .await
        .now_playing
        .expect("piste installée");
    let sid = np.stream_id.as_deref().expect("session DoP");
    let info = orch
        .streamer
        .stream_output_wire(sid)
        .await
        .expect("format DoP servi");
    let sessions = orch.streamer.sessions_state();
    let session = sessions
        .lock()
        .await
        .get(sid)
        .cloned()
        .expect("session présente");
    // Retirer seulement les deux sentinelles de session. Le producteur DoP
    // conserve son propre Sender jusqu'à sa vraie fin : aucun octet tronqué.
    session.close_sender().await;
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut v = Vec::new();
        while let Some(b) = session.recv_chunk().await {
            v.extend_from_slice(&b);
        }
        v
    })
    .await
    .expect("fin réelle du producteur DFF");
    assert_eq!(info.mime_type, "audio/wav");
    assert_eq!(info.sample_rate, 176_400);
    assert_eq!(info.bit_depth, 24);
    assert_eq!(info.channels, 2);
    let cible_par_canal = 352_800u64 * depart_ms / 1000;
    let atteint_par_canal = cible_par_canal;
    let attendue = 44 + (3 * 352_800 - atteint_par_canal) / 2 * 2 * 3;
    assert_eq!(
        octets.len() as u64,
        attendue,
        "le producteur respecte les trames DoP complètes après le départ DFF"
    );
    assert_eq!(np.duration_ms, 3000, "la durée client DoP reste entière");
    assert_eq!(&octets[..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(octets[24..28].try_into().unwrap()),
        176_400
    );
    assert_eq!(
        u32::from_le_bytes(octets[40..44].try_into().unwrap()) as u64 + 44,
        attendue,
        "l'en-tête WAV annonce la même longueur que les octets réellement servis"
    );
    assert_eq!(info.wav_content_length(), Some(attendue));
    assert_eq!(
        np.duration_ms, 3000,
        "la piste DoP reste entière, sans lui rajouter le déplacement une seconde fois"
    );
    assert_eq!(
        crate::outputs::dlna_depart_natif::depart_de_session(sid),
        None
    );
    orch.stop(zone, Some(&appareil)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dff_dop_depart_zero_annonce_corps() {
    verifier_dff_dop(736211, 0).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dff_dop_depart_une_ms_annonce_corps() {
    verifier_dff_dop(736212, 1).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dff_dop_depart_deux_ms_annonce_corps() {
    verifier_dff_dop(736213, 2).await;
}
