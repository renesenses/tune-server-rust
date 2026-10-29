//! #6982 : le FLAC servi après l'avance contient le PCM à la position,
//! avec sa profondeur, et ne reçoit pas un second Seek SOAP.
use super::*;
use crate::db::{settings_repo::SettingsRepo, zone_repo::ZoneRepo};
use crate::outputs::mock::MockOutput;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

async fn cas(bits: u16, arme: bool) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let original = dir.path().join("original.flac");
    let cadence = 48_000;
    let octets = usize::from(bits / 8);
    let mut x = 7u32;
    let pcm: Vec<u8> = (0..cadence * 8 * 2)
        .flat_map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            x.to_le_bytes()[4 - octets..].to_vec()
        })
        .collect();
    let mut enc = crate::audio::encoder::AudioEncoder::new("flac", cadence, u32::from(bits), 2);
    enc.start().await.unwrap();
    enc.write(&pcm).await.unwrap();
    let flac = enc.finish().await.unwrap();
    std::fs::write(&original, &flac).unwrap();
    let app = axum::Router::new().route(
        "/piste.flac",
        axum::routing::get(move || {
            let flac = flac.clone();
            async move { ([("Content-Type", "audio/flac")], flac) }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/piste.flac", ecoute.local_addr().unwrap());
    let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    let did = "dlna:6982-salon";
    let zone = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), Some(did))
        .unwrap();
    SettingsRepo::with_backend(orch.db.clone())
        .set(
            &super::deplacement_hi_res_6333::cle(zone),
            if arme { "true" } else { "false" },
        )
        .unwrap();
    let mut req = super::service_wav_progressif_5080::requete_qobuz(zone);
    req.seek_ms = Some(5_000);
    req.duration_ms = Some(8_000);
    let flux = super::service_wav_progressif_5080::flux_qobuz(url, cadence, bits);
    let (url, sid, mime, _) = orch
        .relayer_le_flux(
            &req,
            "6982",
            "qobuz",
            &flux,
            super::service_wav_progressif_5080::info_amont(&flux),
            false,
            "flac".into(),
            None,
            None,
        )
        .await
        .unwrap();
    let sid = sid.unwrap();
    let sessions = orch.streamer.sessions_state();
    let session = sessions.lock().await.get(&sid).unwrap().clone();
    let chemin = session.file_path.lock().await.clone();
    if bits > 16 && arme {
        let chemin=chemin.expect("#6982 : l'avance Qobuz Hi-Res doit créer un FLAC qui commence à la position, pas garder le proxy à Seek");
        assert_eq!(mime, "audio/flac");
        let depart =
            crate::outputs::dlna_depart_natif::depart_de_l_url(&url).expect("départ inscrit");
        assert!(depart <= 5_000 && 5_000 - depart < 200);
        let carte = session
            .faststart
            .lock()
            .unwrap()
            .clone()
            .expect("#6982 : carte FLAC native");
        let source = std::fs::read(&chemin).unwrap();
        let mut virtuel = carte.header.clone();
        virtuel.extend_from_slice(
            &source
                [carte.body_src_start as usize..(carte.body_src_start + carte.body_len) as usize],
        );
        let servi = dir.path().join("servi.flac");
        std::fs::write(&servi, &virtuel).unwrap();
        let rendu =
            crate::audio::decode::decode_to_pcm(servi.to_str().unwrap(), None, Some(2), 0.0, 0.0)
                .unwrap();
        let attendu = crate::audio::decode::decode_to_pcm(
            original.to_str().unwrap(),
            None,
            Some(2),
            0.0,
            0.0,
        )
        .unwrap();
        assert_eq!(
            (rendu.sample_rate, rendu.bit_depth),
            (cadence, bits),
            "#6982 : Hi-Res conservé"
        );
        let a = rendu.pcm_bytes();
        let b = attendu.pcm_bytes();
        assert!(!a.is_empty());
        let offset = b
            .windows(a.len())
            .position(|w| w == a.as_slice())
            .expect("#6982 : les trames natives doivent conserver le PCM de la source");
        assert_eq!(
            depart,
            offset as u64 * 1000 / (u64::from(cadence) * u64::from(bits / 8) * 2)
        );
        orch.outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new(did, "Salon").with_type("dlna")));
        orch.playback
            .play(
                zone,
                crate::playback::NowPlaying {
                    source: "qobuz".into(),
                    stream_id: Some(sid.clone()),
                    ..Default::default()
                },
            )
            .await;
        orch.seek_output_after_replay(zone, Some(did), Some("dlna"), 5_000)
            .await;
        let arc = orch.outputs.lock().await.get(did).unwrap();
        let guard = arc.lock().await;
        assert!(
            guard
                .as_any()
                .downcast_ref::<MockOutput>()
                .unwrap()
                .seek_calls()
                .is_empty(),
            "#6982 : aucun Seek ajouté au FLAC déjà décalé"
        );
    } else {
        assert!(
            chemin.is_none(),
            "#6982 : le CD et les autres renderers gardent le proxy"
        );
        assert_eq!(
            crate::outputs::dlna_depart_natif::depart_de_l_url(&url),
            None
        );
    }
    orch.streamer.remove_session(&sid).await;
    serveur.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_qobuz_hi_res_est_servi_a_la_position_sans_second_seek_6982() {
    cas(24, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_qobuz_cd_garde_son_proxy_6982() {
    cas(16, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn les_autres_renderers_hi_res_gardent_leur_proxy_6982() {
    cas(24, false).await;
}
