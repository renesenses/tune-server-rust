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
    // Ce témoin vérifie le transport DoP intact, donc à gain unité (#8813).
    orch.playback.set_volume(zone, 1.0).await;
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

/// #8813 — une consigne atténuée ne doit pas produire des marqueurs DoP :
/// la sortie locale les jouerait à plein niveau. Aucun DAC : sortie factice.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_dsd_local_a_six_pour_cent_ne_sort_pas_en_dop_8813() {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("niveau-protege.dsf");
    ecrire_dsf_temoin_7362(&fichier, 16);
    let mut piste = crate::db::models::Track::new("DSD atténué".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("dsf".into());
    piste.duration_ms = 185;
    piste.sample_rate = Some(2_822_400);
    piste.bit_depth = Some(1);
    piste.channels = 2;
    let piste_id = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .create(&piste)
        .unwrap();
    let appareil = "local:niveau-protege-8813";
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    let zone = zones
        .create("Niveau protégé", Some("local"), Some(appareil))
        .unwrap();
    zones.update_dsd_mode(zone, "dop").unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(appareil, "Sans carte son").with_type("local"),
    ));
    orch.set_volume(zone, 0.06, Some(appareil)).await.unwrap();
    orch.play(PlayRequest {
        zone_id: zone,
        output_device_id: Some(appareil.into()),
        track_id: Some(piste_id),
        source: Some("local".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let etat = orch.playback.get_state(zone).await;
    assert!((etat.volume - 0.06).abs() < 1e-6, "consigne conservée");
    let np = etat.now_playing.unwrap();
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(np.stream_id.as_deref().unwrap())
        .cloned()
        .unwrap();
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut octets = Vec::new();
        while octets.len() < 140 {
            octets.extend(session.recv_chunk().await.expect("octets audio"));
        }
        octets
    })
    .await
    .expect("flux synthétique disponible");
    assert_eq!(&octets[..4], b"RIFF");
    let marqueurs_dop = octets[44..140]
        .chunks_exact(6)
        .enumerate()
        .all(|(i, trame)| {
            let marqueur = if i % 2 == 0 { 0x05 } else { 0xfa };
            trame[2] == marqueur && trame[5] == marqueur
        });
    assert!(
        !marqueurs_dop,
        "#8813 : du DoP plein niveau est servi malgré le volume logiciel à 6 %"
    );
}

async fn verifier_baisse_dop_en_cours_8813(en_pause: bool, sourdine: bool) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("dop-en-cours.dsf");
    ecrire_dsf_temoin_7362(&fichier, 441);
    let mut piste = crate::db::models::Track::new("DSD en cours".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("dsf".into());
    piste.duration_ms = 5120;
    piste.sample_rate = Some(2_822_400);
    piste.bit_depth = Some(1);
    piste.channels = 2;
    let piste_id = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .create(&piste)
        .unwrap();
    let appareil = "local:dop-en-cours-8813";
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    let zone = zones
        .create("DoP en cours", Some("local"), Some(appareil))
        .unwrap();
    zones.update_dsd_mode(zone, "dop").unwrap();
    zones.update_volume(zone, 100.0).unwrap();
    orch.playback.set_volume(zone, 1.0).await;
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(appareil, "Sans carte son").with_type("local"),
    ));
    orch.play(PlayRequest {
        zone_id: zone,
        output_device_id: Some(appareil.into()),
        track_id: Some(piste_id),
        source: Some("local".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let sortie = orch.outputs.lock().await.get(appareil).unwrap();
    {
        let garde = sortie.lock().await;
        let factice = garde.as_any().downcast_ref::<MockOutput>().unwrap();
        factice.poser_dop_actif(true);
        factice.set_position(2000);
    }
    orch.playback.seek(zone, 2000).await;
    if en_pause {
        orch.pause(zone, Some(appareil)).await.unwrap();
    }
    if sourdine {
        orch.set_mute(zone, true, Some(appareil)).await.unwrap();
    } else {
        orch.set_volume(zone, 0.06, Some(appareil)).await.unwrap();
    }
    if en_pause {
        assert_eq!(
            orch.playback.get_state(zone).await.state,
            crate::playback::PlayState::Paused,
            "#8813 : régler le volume ne doit pas quitter la pause"
        );
        let appels = sortie
            .lock()
            .await
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_call_count()
            .await;
        assert_eq!(appels, 1, "aucune relecture pendant la pause");
        orch.resume(zone, Some(appareil)).await.unwrap();
    }
    let appels = sortie
        .lock()
        .await
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .play_call_count()
        .await;
    assert_eq!(
        appels, 2,
        "#8813/#8842 : le DoP reste ouvert malgré l’atténuation (pause={en_pause}, sourdine={sourdine})"
    );
    let etat = orch.playback.get_state(zone).await;
    if sourdine {
        assert!(etat.muted, "la sourdine reste active après la relecture");
        assert!(sortie.lock().await.get_status().await.unwrap().muted);
        assert!((etat.volume - 1.0).abs() < 1e-6);
    } else {
        assert!((etat.volume - 0.06).abs() < 1e-6);
    }
    assert!(etat.position_ms >= 2000, "la reprise garde la position");
    let np = etat.now_playing.unwrap();
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(np.stream_id.as_deref().unwrap())
        .cloned()
        .unwrap();
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut v = Vec::new();
        while v.len() < 140 {
            v.extend(session.recv_chunk().await.expect("PCM disponible"));
        }
        v
    })
    .await
    .unwrap();
    assert_eq!(&octets[..4], b"RIFF");
    let dop = octets[44..140].chunks_exact(6).enumerate().all(|(i, t)| {
        let m = if i % 2 == 0 { 0x05 } else { 0xfa };
        t[2] == m && t[5] == m
    });
    assert!(
        !dop,
        "#8813 : le flux de remplacement contient encore du DoP"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baisser_le_volume_pendant_le_dop_recree_du_pcm_8813() {
    verifier_baisse_dop_en_cours_8813(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baisser_le_volume_en_pause_ne_relance_pas_et_reprend_en_pcm_8813() {
    verifier_baisse_dop_en_cours_8813(true, false).await;
}

async fn verifier_politique_dop_8813(cas: &str, dop_attendu: bool) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("politique.dsf");
    ecrire_dsf_temoin_7362(&fichier, 16);
    let mut piste = crate::db::models::Track::new("DSD politique".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("dsf".into());
    piste.duration_ms = 185;
    piste.sample_rate = Some(2_822_400);
    piste.bit_depth = Some(1);
    piste.channels = 2;
    let repo = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone());
    let piste_id = repo.create(&piste).unwrap();
    let appareil = "local:politique-8813";
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    let zone = zones
        .create("Politique DoP", Some("local"), Some(appareil))
        .unwrap();
    zones
        .update_dsd_mode(zone, if cas == "native" { "native" } else { "dop" })
        .unwrap();
    let volume = if matches!(
        cas,
        "unite" | "replaygain" | "egaliseur" | "trim" | "trim_fixe"
    ) {
        1.0
    } else {
        0.06
    };
    zones.update_volume(zone, volume * 100.0).unwrap();
    orch.playback.set_volume(zone, volume).await;
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone());
    match cas {
        "trim_fixe" => {
            settings
                .set(&format!("zone_{zone}_gain_trim_db"), "-6.0")
                .unwrap();
            zones.update_fixed_volume(zone, true).unwrap();
        }
        "trim" => settings
            .set(&format!("zone_{zone}_gain_trim_db"), "-6.0")
            .unwrap(),
        "desactivee" => settings
            .set(
                &crate::audio::dop_ecarte_7265::cle_protection_du_niveau(zone),
                "false",
            )
            .unwrap(),
        "pure" => settings
            .set(&format!("zone_{zone}_audiophile"), r#"{"enabled":true}"#)
            .unwrap(),
        "strict" => settings
            .set(&crate::audio::bitperfect_strict::cle_de_zone(zone), "true")
            .unwrap(),
        "replaygain" => {
            settings
                .set(crate::audio::replaygain::MODE_KEY, "track")
                .unwrap();
            crate::db::track_metadata_repo::TrackMetadataRepo::with_backend(orch.db.clone())
                .set(piste_id, "rg_track_gain", "-12.0 dB")
                .unwrap();
        }
        _ => {}
    }
    if matches!(cas, "egaliseur" | "pcm_puis_dsd") {
        settings.set("plugin_equalizer_installed", "true").unwrap();
        settings.set("plugin_equalizer_enabled", "true").unwrap();
        let mut profil = crate::audio::eq::EqProfile::default();
        profil.enabled = true;
        profil.bass_gain_db = 6.0;
        let eq = crate::audio::eq::EqProcessor::new(&profil, 44_100, 2);
        assert!(eq.is_enabled());
        assert!(
            eq.preamp_db(0).unwrap() < -1.0,
            "préampli négatif réellement actif"
        );
        settings
            .set(
                &format!("zone_{zone}_eq_profile"),
                &serde_json::to_string(&profil).unwrap(),
            )
            .unwrap();
    }
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(appareil, "Sans carte son").with_type("local"),
    ));
    if cas == "pcm_puis_dsd" {
        let pcm = dir.path().join("avant.wav");
        let mut octets =
            crate::audio::wav::build_wav_header_with_duration(2, 44_100, 16, Some(100)).to_vec();
        octets.resize(44 + 17_640, 0);
        std::fs::write(&pcm, octets).unwrap();
        let mut avant = crate::db::models::Track::new("PCM avant DSD".into());
        avant.file_path = Some(pcm.to_string_lossy().into_owned());
        avant.format = Some("wav".into());
        avant.sample_rate = Some(44_100);
        avant.bit_depth = Some(16);
        avant.channels = 2;
        avant.duration_ms = 100;
        let id_pcm = repo.create(&avant).unwrap();
        orch.play(PlayRequest {
            zone_id: zone,
            output_device_id: Some(appareil.into()),
            track_id: Some(id_pcm),
            source: Some("local".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        assert!((orch.playback.get_state(zone).await.volume - 0.06).abs() < 1e-6);
    }
    orch.play(PlayRequest {
        zone_id: zone,
        output_device_id: Some(appareil.into()),
        track_id: Some(piste_id),
        source: Some("local".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let np = orch.playback.get_state(zone).await.now_playing.unwrap();
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(np.stream_id.as_deref().unwrap())
        .cloned()
        .unwrap();
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut v = Vec::new();
        while v.len() < 140 {
            v.extend(session.recv_chunk().await.expect("octets du témoin"));
        }
        v
    })
    .await
    .unwrap();
    assert_eq!(&octets[..4], b"RIFF");
    let dop = octets[44..140].chunks_exact(6).enumerate().all(|(i, t)| {
        let m = if i % 2 == 0 { 0x05 } else { 0xfa };
        t[2] == m && t[5] == m
    });
    assert_eq!(dop, dop_attendu, "#8813 : transport inattendu pour {cas}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_unite_8813() {
    verifier_politique_dop_8813("unite", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_desactivee_8813() {
    verifier_politique_dop_8813("desactivee", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_pure_8813() {
    verifier_politique_dop_8813("pure", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_strict_8813() {
    verifier_politique_dop_8813("strict", true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_native_8813() {
    verifier_politique_dop_8813("native", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_replaygain_8813() {
    verifier_politique_dop_8813("replaygain", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_egaliseur_8813() {
    verifier_politique_dop_8813("egaliseur", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_pcm_puis_dsd_8813() {
    verifier_politique_dop_8813("pcm_puis_dsd", false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_trim_8813() {
    verifier_politique_dop_8813("trim", false).await;
}

#[cfg(feature = "local-audio")]
#[tokio::test]
async fn le_preampli_garde_apres_coupure_de_l_eq_protege_aussi_le_dop_8813() {
    use crate::outputs::traits::OutputTarget;
    let orch = orchestrateur();
    // Construction des processeurs seulement : ni play, ni ouverture de DAC.
    let sortie = crate::outputs::local::LocalOutput::new("preampli-garde-8813".into());
    sortie.set_compensation_de_niveau(false);
    let mut profil = crate::audio::eq::EqProfile::default();
    profil.enabled = true;
    profil.bass_gain_db = 6.0;
    sortie.set_eq(Some(crate::audio::eq::EqProcessor::new(&profil, 44_100, 2)));
    sortie.replace_eq_live(None);
    assert!(sortie.preampli_garde().expect("préampli retenu").db < -1.0);
    let appareil = sortie.device_id().to_string();
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    let zone = zones
        .create("Préampli gardé", Some("local"), Some(&appareil))
        .unwrap();
    zones.update_volume(zone, 100.0).unwrap();
    orch.playback.set_volume(zone, 1.0).await;
    orch.outputs.lock().await.register(Box::new(sortie));
    assert!(
        orch.attenuation_logicielle_demandee_pour_dop(
            zone,
            crate::audio::replaygain::PisteRg::AUCUNE
        )
        .await,
        "#8813 : le préampli gardé dans la sortie est oublié après coupure de l’égaliseur"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn politique_dop_trim_ignore_en_volume_fixe_8813() {
    verifier_politique_dop_8813("trim_fixe", true).await;
}
#[cfg(feature = "local-audio")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn baisser_le_volume_d_un_dop_upnp_sans_extension_garde_le_format_8813() {
    let en_pause = false;
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("dop-en-cours.dsf");
    ecrire_dsf_temoin_7362(&fichier, 441);
    let octets_http = std::fs::read(&fichier).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/ressource", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/ressource",
        axum::routing::get(move || {
            let octets = octets_http.clone();
            async move { axum::body::Body::from(octets) }
        }),
    );
    let serveur = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let appareil = "local:dop-en-cours-8813";
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    let zone = zones
        .create("DoP en cours", Some("local"), Some(appareil))
        .unwrap();
    zones.update_dsd_mode(zone, "dop").unwrap();
    zones.update_volume(zone, 100.0).unwrap();
    orch.playback.set_volume(zone, 1.0).await;
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(appareil, "Sans carte son").with_type("local"),
    ));
    orch.play(PlayRequest {
        zone_id: zone,
        output_device_id: Some(appareil.into()),
        track_id: None,
        source: Some("upnp".into()),
        source_id: Some(url),
        media_format: Some("dsf".into()),
        sample_rate: Some(2_822_400),
        bit_depth: Some(1),
        duration_ms: Some(5120),
        title: Some("DSF distant sans extension".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    let sortie = orch.outputs.lock().await.get(appareil).unwrap();
    {
        let garde = sortie.lock().await;
        let factice = garde.as_any().downcast_ref::<MockOutput>().unwrap();
        factice.poser_dop_actif(true);
        factice.set_position(2000);
    }
    orch.playback.seek(zone, 2000).await;
    if en_pause {
        orch.pause(zone, Some(appareil)).await.unwrap();
    }
    orch.set_volume(zone, 0.06, Some(appareil))
        .await
        .expect("#8813 : la relecture du DSF UPnP perd son format après baisse du volume");
    if en_pause {
        assert_eq!(
            orch.playback.get_state(zone).await.state,
            crate::playback::PlayState::Paused,
            "#8813 : régler le volume ne doit pas quitter la pause"
        );
        let appels = sortie
            .lock()
            .await
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .play_call_count()
            .await;
        assert_eq!(appels, 1, "aucune relecture pendant la pause");
        orch.resume(zone, Some(appareil)).await.unwrap();
    }
    let appels = sortie
        .lock()
        .await
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .play_call_count()
        .await;
    assert_eq!(
        appels, 2,
        "#8813 : le DoP reste ouvert malgré la baisse du volume (pause={en_pause})"
    );
    let etat = orch.playback.get_state(zone).await;
    assert!((etat.volume - 0.06).abs() < 1e-6);
    assert!(etat.position_ms >= 2000, "la reprise garde la position");
    let np = etat.now_playing.unwrap();
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(
            np.stream_id
                .as_deref()
                .expect("#8813 : la relecture UPnP ne recrée pas de PCM"),
        )
        .cloned()
        .unwrap();
    let octets = tokio::time::timeout(Duration::from_secs(10), async {
        let mut v = Vec::new();
        while v.len() < 140 {
            v.extend(session.recv_chunk().await.expect("PCM disponible"));
        }
        v
    })
    .await
    .unwrap();
    assert_eq!(&octets[..4], b"RIFF");
    let dop = octets[44..140].chunks_exact(6).enumerate().all(|(i, t)| {
        let m = if i % 2 == 0 { 0x05 } else { 0xfa };
        t[2] == m && t[5] == m
    });
    assert!(
        !dop,
        "#8813 : le flux de remplacement contient encore du DoP"
    );
    serveur.abort();
}

async fn verifier_plafond_dop_local_8789(cadence: u32, strict: bool, dop_attendu: bool) {
    let orch = orchestrateur();
    let dir = tempfile::tempdir().unwrap();
    let fichier = dir.path().join("plafond-dop.dsf");
    ecrire_dsf_temoin_7362(&fichier, 16);
    let mut octets = std::fs::read(&fichier).unwrap();
    octets[56..60].copy_from_slice(&cadence.to_le_bytes());
    std::fs::write(&fichier, octets).unwrap();
    let mut piste = crate::db::models::Track::new("DSD plafond".into());
    piste.file_path = Some(fichier.to_string_lossy().into_owned());
    piste.format = Some("dsf".into());
    piste.sample_rate = Some(cadence as i32);
    piste.bit_depth = Some(1);
    piste.channels = 2;
    piste.duration_ms = i64::from(16 * 4096 * 8 * 1000u32 / cadence);
    let id = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone())
        .create(&piste)
        .unwrap();
    let zone = 8_789_000 + i64::from(cadence / 1000) + if strict { 100_000 } else { 0 };
    let appareil = format!("local:plafond-dop-8789-{zone}");
    orch.db.execute("INSERT INTO zones (id,name,output_type,output_device_id) VALUES (?, 'Banc muet', 'local', ?)", &[&zone, &appareil]).unwrap();
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone());
    zones.update_dsd_mode(zone, "dop").unwrap();
    zones.update_volume(zone, 100.0).unwrap();
    orch.playback.set_volume(zone, 1.0).await;
    if strict {
        crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
            .set(&crate::audio::bitperfect_strict::cle_de_zone(zone), "true")
            .unwrap();
    }
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(&appareil, "Sans carte son").with_type("local"),
    ));
    let resultat = orch
        .play(PlayRequest {
            zone_id: zone,
            output_device_id: Some(appareil),
            track_id: Some(id),
            source: Some("local".into()),
            ..Default::default()
        })
        .await;
    if strict {
        assert!(
            resultat
                .as_ref()
                .err()
                .is_some_and(|e| e.contains("bitperfect_strict_refused")),
            "#8789 : le strict accepte un DoP au-delà du DSD256 : {resultat:?}"
        );
        return;
    }
    resultat.expect("lecture factice");
    let np = orch.playback.get_state(zone).await.now_playing.unwrap();
    let sid = np.stream_id.as_deref().expect("session servie");
    let info = orch.streamer.stream_output_wire(sid).await.unwrap();
    let attendue = if dop_attendu { cadence / 16 } else { 352_800 };
    let bits = if dop_attendu { 24 } else { 32 };
    assert_eq!(
        (info.sample_rate, info.bit_depth),
        (attendue, bits),
        "#8789 : le DSD512 tente encore une cadence DoP impossible au lieu du PCM"
    );
    let session = orch
        .streamer
        .sessions_state()
        .lock()
        .await
        .get(sid)
        .cloned()
        .unwrap();
    let entete = tokio::time::timeout(Duration::from_secs(10), async {
        let mut v = Vec::new();
        while v.len() < 44 {
            v.extend(session.recv_chunk().await.expect("octets servis"));
        }
        v
    })
    .await
    .unwrap();
    assert_eq!(&entete[..4], b"RIFF");
    assert_eq!(
        u32::from_le_bytes(entete[24..28].try_into().unwrap()),
        attendue
    );
    assert_eq!(
        u16::from_le_bytes(entete[34..36].try_into().unwrap()),
        bits as u16
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_dsd512_local_sans_plafond_se_replie_en_pcm_8789() {
    for cadence in [22_579_200, 24_576_000] {
        verifier_plafond_dop_local_8789(cadence, false, false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_strict_refuse_le_dop_au_dela_du_dsd256_8789() {
    for cadence in [22_579_200, 24_576_000] {
        verifier_plafond_dop_local_8789(cadence, true, false).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_dop_dsd256_des_deux_familles_reste_permis_8789() {
    for cadence in [11_289_600, 12_288_000] {
        verifier_plafond_dop_local_8789(cadence, false, true).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sourdine_pendant_le_dop_recree_du_pcm_8842() {
    verifier_baisse_dop_en_cours_8813(false, true).await;
}
