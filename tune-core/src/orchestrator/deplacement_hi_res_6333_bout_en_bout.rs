//! #6333 — de bout en bout : un FLAC de la bibliothèque joue sur une zone
//! DLNA servi tel quel (session fichier), comme « Daydreaming » (24 bits /
//! 48 kHz) chez FabienM. Sur le Devialet, avancer dans une piste Hi-Res
//! relance le flux NATIF à la position — une URL portant `depart_ms`, une
//! carte d'en-tête réécrit — et aucun `Seek` SOAP ne part. En 16 bits, le
//! `Seek` ordinaire part toujours.
//!
//! Contre-épreuve : forcer `relance_hi_res` à `false` dans
//! `deplacer_en_flux_natif_6059` fait tomber
//! `devialet_hi_res_l_avance_relance_le_flux_natif_a_la_position_6333`.
use super::PlaybackOrchestrator;
use super::deplacement_hi_res_6333::cle;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::dlna_repli_set_uri as profil;
use crate::outputs::mock::MockOutput;
use std::sync::Arc;

const SR: u32 = 48_000;
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

/// Un FLAC de `profondeur` bits de la bibliothèque qui joue sur la zone DLNA
/// « Salon », par une session FICHIER (passthrough natif, cherchable par
/// `Range`). La zone arme la règle par son réglage : la sortie factice n'a
/// pas l'adresse de contrôle d'un Devialet.
async fn flac_natif_en_lecture(
    appareil: &str,
    profondeur: u32,
) -> (PlaybackOrchestrator, i64, tempfile::TempDir) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("04 Daydreaming.flac");
    let octets = profondeur.div_ceil(8) as usize;
    let mut x: u32 = 7;
    let pcm: Vec<u8> = (0..SR * SECONDES * 2)
        .flat_map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let echantillon = ((x as i32) >> 3).to_le_bytes();
            echantillon[4 - octets..].to_vec()
        })
        .collect();
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", SR, profondeur, 2);
    enc.start().await.unwrap();
    enc.write(&pcm).await.unwrap();
    std::fs::write(&piste, enc.finish().await.unwrap()).unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Radiohead')",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'A Moon Shaped Pool', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES (1, 'Daydreaming', 1, 1, ?, 'flac', {}, {SR}, {profondeur}, 2)",
                SECONDES as i64 * 1000
            ),
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(appareil))
        .unwrap();
    SettingsRepo::with_backend(orch.db.clone())
        .set(&cle(zone_id), "true")
        .unwrap();
    let mock = MockOutput::new(appareil, "Salon").with_type("dlna");
    orch.outputs.lock().await.register(Box::new(mock));
    let sid = orch
        .streamer
        .create_file_session(
            crate::http::streamer::StreamInfo {
                format: "flac".into(),
                mime_type: "audio/flac".into(),
                sample_rate: SR,
                bit_depth: profondeur as u16,
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
                title: "Daydreaming".into(),
                track_id: Some(1),
                source: "local".into(),
                stream_id: Some(sid),
                duration_ms: SECONDES as i64 * 1000,
                sample_rate: Some(SR),
                bit_depth: Some(profondeur),
                ..Default::default()
            },
        )
        .await;
    (orch, zone_id, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devialet_hi_res_l_avance_relance_le_flux_natif_a_la_position_6333() {
    let _ = profil::base_de_test();
    let appareil = "dlna:uuid-devialet-salon-hi-res-6333";
    let (orch, zone_id, _dir) = flac_natif_en_lecture(appareil, 24).await;
    assert!(!profil::seek_inoperant(appareil), "pas le profil de #6059");

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
        "avance en 24 bits sur le Devialet : aucun Seek à l'octet, le flux \
         doit être relancé à la position : {seeks:?}"
    );
    assert_eq!(lectures, 1, "le flux est relancé (un play_media)");
    let url = url.expect("une URL posée");
    let depart = crate::outputs::dlna_depart_natif::depart_de_l_url(&url)
        .unwrap_or_else(|| panic!("l'URL porte le départ : {url}"));
    assert!(
        depart <= CIBLE_MS && CIBLE_MS - depart < 200,
        "départ {depart}"
    );
    assert!(
        url.contains(".flac"),
        "toujours du FLAC natif, pas une conversion : {url}"
    );

    let np = orch.playback.get_state(zone_id).await.now_playing.unwrap();
    let sid = np.stream_id.expect("session");
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
async fn devialet_16_bits_l_avance_garde_le_seek_6333() {
    let _ = profil::base_de_test();
    let appareil = "dlna:uuid-devialet-salon-cd-6333";
    let (orch, zone_id, _dir) = flac_natif_en_lecture(appareil, 16).await;

    let resultat = orch.seek(zone_id, CIBLE_MS, Some(appareil)).await;
    assert!(resultat.is_ok(), "{resultat:?}");
    let (lectures, seeks, _) = sortie(&orch, appareil).await;
    assert_eq!(seeks, vec![CIBLE_MS], "le Seek ordinaire part, il marche");
    assert_eq!(lectures, 0, "aucune relance");
}

#[tokio::test]
async fn le_devialet_est_reconnu_a_son_adresse_de_controle_6333() {
    let orch = orchestrateur();
    let devialet = "dlna:uuid-devialet-adresse-6333";
    let autre = "dlna:uuid-autre-renderer-6333";
    for (did, chemin) in [
        (devialet, "Control/DvltRygelRendererPlugin"),
        (autre, "AVTransport"),
    ] {
        let sortie = crate::outputs::dlna::DlnaOutput::new(
            "Salon".into(),
            did.into(),
            "http://192.0.2.39:43403".into(),
            format!("http://192.0.2.39:43403/{chemin}/RygelAVTransport"),
            format!("http://192.0.2.39:43403/{chemin}/RygelRenderingControl"),
            None,
        );
        orch.outputs.lock().await.register(Box::new(sortie));
    }
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let salon = zones.create("Salon", Some("dlna"), Some(devialet)).unwrap();
    let autre_zone = zones.create("Bureau", Some("dlna"), Some(autre)).unwrap();

    assert!(
        orch.deplacement_hi_res_par_nouveau_flux(salon, devialet, Some(24))
            .await,
        "le Devialet en 24 bits, sans réglage"
    );
    assert!(
        !orch
            .deplacement_hi_res_par_nouveau_flux(salon, devialet, Some(16))
            .await
    );
    assert!(
        !orch
            .deplacement_hi_res_par_nouveau_flux(autre_zone, autre, Some(24))
            .await,
        "un autre renderer ne change pas"
    );
    SettingsRepo::with_backend(orch.db.clone())
        .set(&cle(salon), "false")
        .unwrap();
    assert!(
        !orch
            .deplacement_hi_res_par_nouveau_flux(salon, devialet, Some(24))
            .await,
        "désarmé à la main"
    );
}
