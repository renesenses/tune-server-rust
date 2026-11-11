//! #8082 — l'assemblage de la suite et son plan de suivi.
use super::*;
use crate::http::streamer::SourceVirtuelle;

fn wav(chemin: &Path, cadence: u32, profondeur: u16, trames: u32) {
    let octets = trames as usize * 2 * usize::from(profondeur / 8);
    let mut f =
        crate::audio::wav::build_wav_header_with_data_size(2, cadence, profondeur, octets as u32)
            .to_vec();
    f.extend((0..octets).map(|i| (i % 251) as u8));
    std::fs::write(chemin, f).unwrap();
}

#[test]
fn la_suite_s_arrete_au_premier_changement_de_format_8082() {
    let bac = crate::test_scratch::scratch_dir("flux-continu-suite-8082");
    let a = bac.join("1.wav");
    let b = bac.join("2.wav");
    let c = bac.join("3.wav");
    let d = bac.join("4.wav");
    wav(&a, 44_100, 16, 88_200);
    wav(&b, 44_100, 16, 44_100);
    wav(&c, 96_000, 24, 9_600); // autre format : la suite s'arrête là
    wav(&d, 44_100, 16, 44_100); // même format que 1, mais après la rupture
    let f = assembler(
        (7, 70, a.clone()),
        0,
        vec![(8, 80, b.clone()), (9, 90, c), (10, 100, d)],
    )
    .unwrap();
    assert_eq!(f.lignes, vec![(7, 70), (8, 80)]);
    assert_eq!(f.wav.total(), 44 + (88_200 + 44_100) * 4);
    let plan = plan_du_flux(3, "sid", &f);
    assert_eq!(
        plan.pistes
            .iter()
            .map(|p| (p.position, p.debut_ms, p.duree_ms))
            .collect::<Vec<_>>(),
        vec![(7, 0, 2_000), (8, 2_000, 1_000)]
    );
}

#[test]
fn une_relance_a_la_position_garde_les_frontieres_de_l_album_8082() {
    let bac = crate::test_scratch::scratch_dir("flux-continu-relance-8082");
    let a = bac.join("1.wav");
    let b = bac.join("2.wav");
    wav(&a, 44_100, 16, 441_000);
    wav(&b, 44_100, 16, 44_100);
    let f = assembler((1, 11, a), 4_000, vec![(2, 22, b)]).unwrap();
    assert_eq!(f.trames_avant_depart, 176_400);
    assert_eq!(
        f.wav.total(),
        44 + (441_000 - 176_400 + 44_100) * 4,
        "le flux commence à 4 s"
    );
    let plan = plan_du_flux(1, "sid", &f);
    // Les frontières restent celles de l'album : le relevé du renderer est
    // ramené au début de la première piste par `dlna_depart_natif`.
    assert_eq!(
        plan.pistes
            .iter()
            .map(|p| (p.debut_ms, p.duree_ms))
            .collect::<Vec<_>>(),
        vec![(0, 10_000), (10_000, 1_000)]
    );
}

#[test]
fn une_premiere_piste_avec_perte_n_entre_pas_8082() {
    let bac = crate::test_scratch::scratch_dir("flux-continu-perte-8082");
    let a = bac.join("1.mp3");
    std::fs::write(&a, b"ID3 pas du tout lossless").unwrap();
    let b = bac.join("2.wav");
    wav(&b, 44_100, 16, 100);
    assert!(assembler((1, 1, a), 0, vec![(2, 2, b)]).is_err());
}

// ---------------------------------------------------------------------------
// Par la porte publique (`resolve_stream`) : zone DLNA, deux FLAC de même
// format en file, le second après le premier.

mod bout_en_bout {
    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::db::zone_repo::ZoneRepo;
    use crate::orchestrator::{PlayRequest, PlaybackOrchestrator, ResolvedStream};
    use crate::outputs::mock::MockOutput;
    use std::sync::Arc;

    const FLAC: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/flac/ref_16_44100_stereo.flac"
    );

    fn orchestrateur() -> PlaybackOrchestrator {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
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

    async fn resoudre(
        renderer: &str,
        type_de_sortie: &str,
        armer: bool,
    ) -> (PlaybackOrchestrator, ResolvedStream, tempfile::TempDir) {
        let orch = orchestrateur();
        let dir = tempfile::tempdir().unwrap();
        for id in [1, 2] {
            let chemin = dir.path().join(format!("{id:02}.flac"));
            std::fs::copy(FLAC, &chemin).unwrap();
            let fichier = chemin.to_string_lossy().into_owned();
            orch.db
                .execute(
                    "INSERT INTO tracks (id, title, file_path, format, duration_ms, \
                     sample_rate, bit_depth, channels) VALUES (?, 'Piste', ?, 'flac', 400, \
                     44100, 16, 2)",
                    &[&id as &dyn ToSqlValue, &fichier as &dyn ToSqlValue],
                )
                .unwrap();
        }
        let zones = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zones
            .create("Salon", Some(type_de_sortie), Some(renderer))
            .unwrap();
        if armer {
            crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
                .set(&crate::outputs::dlna_flux_continu::cle(zone_id), "true")
                .unwrap();
        }
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new(renderer, "Salon").with_type(type_de_sortie),
        ));
        crate::db::play_queue_repo::PlayQueueRepo::with_backend(orch.db.clone())
            .set_queue(zone_id, &[1, 2])
            .unwrap();
        orch.playback.update_queue_info(zone_id, 0, 2).await;
        let resolu = orch
            .resolve_stream(&PlayRequest {
                zone_id,
                output_device_id: Some(renderer.into()),
                track_id: Some(1),
                source: Some("local".into()),
                ..Default::default()
            })
            .await
            .expect("la piste se résout");
        (orch, resolu, dir)
    }

    /// 🔴 Réglage armé : les deux pistes partent en UN WAV dont le PCM est
    /// exactement la somme des deux, et le plan de suivi est posé sur
    /// l'appareil.
    #[tokio::test]
    async fn zone_armee_les_deux_pistes_partent_en_un_seul_wav_8082() {
        let dev = "dlna-8082-arme";
        let (_orch, r, _dir) = resoudre(dev, "dlna", true).await;
        let pcm = crate::audio::decode::decode_to_pcm(FLAC, None, None, 0.0, 0.0)
            .unwrap()
            .pcm_bytes()
            .len() as u64;
        assert_eq!(r.mime_type, "audio/wav", "flux continu attendu");
        assert_eq!(r.file_size, Some(44 + 2 * pcm));
        assert!(r.url.ends_with(".wav"), "{}", r.url);
        let plan = crate::outputs::dlna_flux_continu::plan_de(dev).expect("plan posé");
        assert_eq!(Some(plan.session_id.as_str()), r.stream_id.as_deref());
        assert_eq!(
            plan.pistes
                .iter()
                .map(|p| (p.position, p.track_id))
                .collect::<Vec<_>>(),
            vec![(0, 1), (1, 2)]
        );
        assert_eq!(
            crate::outputs::dlna_flux_continu::duree_de_la_premiere_piste(r.stream_id.as_deref()),
            Some(400),
            "l'écran parle de la première piste, pas du flux"
        );
    }

    /// Réglage éteint (défaut) : rien ne change, le FLAC part tel quel.
    #[tokio::test]
    async fn zone_non_armee_rien_ne_change_8082() {
        let dev = "dlna-8082-eteint";
        let (_orch, r, _dir) = resoudre(dev, "dlna", false).await;
        assert_eq!(r.mime_type, "audio/flac");
        assert!(crate::outputs::dlna_flux_continu::plan_de(dev).is_none());
    }
}
