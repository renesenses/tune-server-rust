//! #7362 : la longueur annoncée DoP doit rejoindre les octets réellement
//! produits après retour au bloc DSF précédent. Aucun octet, marqueur ou
//! comportement de déplacement du lecteur n'est modifié par ces témoins.
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

fn ecrire_dsf_temoin_7362(path: &std::path::Path, blocs_par_canal: usize) {
    const BLOC: usize = 4096;
    const CANAUX: usize = 2;
    let mut data = Vec::with_capacity(blocs_par_canal * BLOC * CANAUX);
    // Disposition DSF : bloc du canal 0, bloc du canal 1, bloc suivant du
    // canal 0… Les deux canaux portent le même carré.
    for indice_bloc in 0..blocs_par_canal * CANAUX {
        let octet: u8 = if (indice_bloc / CANAUX).is_multiple_of(2) {
            0xFF
        } else {
            0x00
        };
        data.extend(std::iter::repeat_n(octet, BLOC));
    }
    let total_samples = (blocs_par_canal * BLOC * 8) as u64;

    let mut buf = Vec::with_capacity(92 + data.len());
    buf.extend_from_slice(b"DSD ");
    buf.extend_from_slice(&28u64.to_le_bytes());
    buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&0u64.to_le_bytes()); // pas de métadonnées
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&52u64.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes()); // version
    buf.extend_from_slice(&0u32.to_le_bytes()); // format = DSD brut
    buf.extend_from_slice(&2u32.to_le_bytes()); // type de canaux = stéréo
    buf.extend_from_slice(&(CANAUX as u32).to_le_bytes());
    buf.extend_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
    buf.extend_from_slice(&1u32.to_le_bytes()); // bits par échantillon
    buf.extend_from_slice(&total_samples.to_le_bytes());
    buf.extend_from_slice(&(BLOC as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // réservé
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
    buf.extend_from_slice(&data);
    std::fs::write(path, &buf).unwrap();
}

async fn verifier_dsf_dop(zone: i64, depart_ms: u64) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("dop-synthetique.dsf");
    // 441 blocs de 4096 octets/canal à DSD64 : exactement 5120 ms.
    // Les déplacements à1280/2560ms visent un quart/une moitié de bloc.
    ecrire_dsf_temoin_7362(&fichier, 441);
    let mut piste = crate::db::models::Track::new("DoP synthétique".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("dsf".into());
    piste.duration_ms = 5120;
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
    .expect("fin réelle du producteur DSF");
    assert_eq!(info.mime_type, "audio/wav");
    assert_eq!(info.sample_rate, 176_400);
    assert_eq!(info.bit_depth, 24);
    assert_eq!(info.channels, 2);
    let cible_par_canal = 352_800u64 * depart_ms / 1000;
    let atteint_par_canal = cible_par_canal / 4096 * 4096;
    let attendue = 44 + (441 * 4096 - atteint_par_canal) / 2 * 2 * 3;
    assert_eq!(
        octets.len() as u64,
        attendue,
        "le producteur conserve le retour au bloc précédent, contrat du lecteur DSF"
    );
    assert_eq!(np.duration_ms, 5120, "la durée client DoP reste entière");
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
        np.duration_ms, 5120,
        "la piste DoP reste entière, sans lui rajouter le déplacement une seconde fois"
    );
    assert_eq!(
        crate::outputs::dlna_depart_natif::depart_de_session(sid),
        None
    );
    orch.stop(zone, Some(&appareil)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depuis_le_debut_annonce_et_corps_dsf_dop_coincident_7362() {
    verifier_dsf_dop(736201, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_un_quart_de_bloc_annonce_et_corps_dsf_dop_coincident_7362() {
    verifier_dsf_dop(736202, 1280).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_un_demi_bloc_annonce_et_corps_dsf_dop_coincident_7362() {
    verifier_dsf_dop(736203, 2560).await;
}
