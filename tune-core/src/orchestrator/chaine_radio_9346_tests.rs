//! #9346 — une radio servie à un renderer DLNA, à OAAT ou à un onglet porte la
//! convolution de la zone. Une « station » sur 127.0.0.1 sert le MP3 de
//! `tests/fixtures/test.mp3` ; la zone a pour seule correction une réponse
//! impulsionnelle Dirac de gain 0,25. Le PCM que la session sert doit donc
//! valoir le quart de celui d'une zone sans correction.
use super::super::*;
use std::sync::Arc;
use std::time::Duration;

const MP3: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test.mp3"
));

/// Une station qui sert le MP3 à chaque connexion.
fn station() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(MP3);
            let _ = s.flush();
        }
    });
    format!("http://{addr}/live.mp3")
}

/// Orchestrateur Premium : la convolution est un droit Premium (#7632).
fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.license = Some(crate::license::licence_premium_pour_test(db));
    orch
}

/// Une RI WAV mono flottante à 44,1 kHz (la cadence de la station), Dirac de
/// gain 0,25, réglée sur la zone.
fn regler_ri(orch: &PlaybackOrchestrator, zone_id: i64) -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-9346-radio", ".wav");
    let taps = 32usize;
    let donnees = taps * 4;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&44_100u32.to_le_bytes());
    w.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 0.25 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), w).unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("ir_path_{zone_id}"), &f.as_str())
        .unwrap();
    f
}

fn demande(zone_id: i64, sortie: Option<&str>, station: &str) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: sortie.map(str::to_owned),
        track_id: None,
        source: Some("radio".into()),
        source_id: Some(station.into()),
        title: Some("Station 9346".into()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
    }
}

/// Le niveau moyen (|échantillon| 16 bits) des `octets` premiers octets que
/// la session sert à la sortie.
async fn niveau_servi(orch: &PlaybackOrchestrator, req: &PlayRequest, octets: usize) -> f64 {
    let resolu = orch.resolve_direct_url(req).await.expect("résolution");
    let sid = resolu
        .stream_id
        .expect("une radio servie passe par une session Tune");
    assert_eq!(resolu.mime_type, "audio/wav");
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(&sid)
        .cloned()
        .expect("session");
    let mut pcm = Vec::new();
    let fin = tokio::time::Instant::now() + Duration::from_secs(20);
    while pcm.len() < octets {
        match tokio::time::timeout_at(fin, session.recv_chunk()).await {
            Ok(Some(bloc)) => pcm.extend_from_slice(&bloc),
            _ => break,
        }
    }
    assert!(pcm.len() >= octets, "PCM servi trop court : {}", pcm.len());
    let n = octets / 2;
    pcm[..n * 2]
        .chunks_exact(2)
        .map(|b| f64::from(i16::from_le_bytes([b[0], b[1]]).unsigned_abs()))
        .sum::<f64>()
        / n as f64
}

async fn la_convolution_est_appliquee(type_de_sortie: &str, sortie: Option<&str>) {
    let station = station();
    let orch = orchestrateur();
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let seche = zones
        .create("Sans correction", Some(type_de_sortie), sortie)
        .unwrap();
    let corrigee = zones
        .create("Avec correction", Some(type_de_sortie), sortie)
        .unwrap();
    let _ri = regler_ri(&orch, corrigee);
    assert!(orch.zone_has_active_ir(corrigee));

    let octets = 64 * 1024;
    let sec = niveau_servi(&orch, &demande(seche, sortie, &station), octets).await;
    let corrige = niveau_servi(&orch, &demande(corrigee, sortie, &station), octets).await;
    assert!(
        sec > 100.0,
        "{type_de_sortie} : la station est muette ({sec})"
    );
    let rapport = corrige / sec;
    assert!(
        (0.15..0.35).contains(&rapport),
        "{type_de_sortie} : la RI (gain 0,25) n'est pas appliquée à la radio servie : \
         niveau corrigé / sec = {rapport:.3}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_radio_dlna_avec_la_convolution_seule_est_convoluee_9346() {
    la_convolution_est_appliquee("dlna", Some("dlna:uuid-9346")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_radio_oaat_avec_la_convolution_seule_est_convoluee_9346() {
    la_convolution_est_appliquee("oaat", Some("oaat:9346")).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_radio_au_navigateur_avec_la_convolution_seule_est_convoluee_9346() {
    la_convolution_est_appliquee("browser", None).await;
}

/// Une station `.mp3` sans appareil désigné passait telle quelle quand
/// seule la convolution était réglée : le passage direct n'était interdit que
/// par l'égaliseur.
#[tokio::test]
async fn la_convolution_seule_interdit_le_passage_direct_9346() {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Réseau", Some("dlna"), None)
        .unwrap();
    let _ri = regler_ri(&orch, zone_id);
    // Port 9 (discard) : aucun flux réel, seule la décision compte.
    let station = "http://127.0.0.1:9/station.mp3";
    let resolu = orch
        .resolve_direct_url(&demande(zone_id, None, station))
        .await
        .unwrap();
    assert!(
        resolu.stream_id.is_some(),
        "la convolution doit interdire le passage direct de la station"
    );
    assert_ne!(resolu.url, station);
}

/// PURE : aucune chaîne, la station garde son chemin d'avant.
#[test]
fn en_pure_aucune_chaine_radio_9346() {
    let orch = orchestrateur();
    let _ri = regler_ri(&orch, 1);
    assert!(orch.preparer_la_chaine_radio(1).is_some());
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set("zone_1_audiophile", r#"{"enabled":true}"#)
        .unwrap();
    assert!(orch.zone_audiophile(1));
    assert!(orch.preparer_la_chaine_radio(1).is_none());
}
