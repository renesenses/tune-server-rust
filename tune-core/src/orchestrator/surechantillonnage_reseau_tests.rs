use super::*;

#[test]
fn cadences_lues_par_format() {
    let sink: Vec<String> = [
        "http-get:*:audio/flac;rate=44100:*",
        "http-get:*:audio/x-flac;rate=192000:*",
        "http-get:*:audio/L24;rate=96000;channels=2:*",
        "http-get:*:audio/L16;rate=48000;channels=2:DLNA.ORG_PN=LPCM",
        "http-get:*:audio/wav:*",
        "http-get:*:audio/mpeg:*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(cadences_annoncees(&sink, true), vec![44_100, 192_000]);
    assert_eq!(cadences_annoncees(&sink, false), vec![48_000, 96_000]);
    assert!(cadences_annoncees(&["http-get:*:audio/flac:*".into()], true).is_empty());
}

#[test]
fn seuls_dlna_et_openhome_ont_une_sonde() {
    assert!(type_avec_sonde_de_cadences(Some("dlna")));
    assert!(type_avec_sonde_de_cadences(Some("openhome")));
    for t in ["bluos", "chromecast", "squeezebox", "local"] {
        assert!(!type_avec_sonde_de_cadences(Some(t)), "{t}");
    }
}

// ── Bancs : zone DLNA, greffon installé, machine mesurée ─────────────────

use crate::audio::surechantillonnage as su;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use std::sync::Arc;

/// Lecteur qui annonce la famille complète jusqu'à 192 kHz (FLAC et L24).
const SINK_192: &[&str] = &[
    "http-get:*:audio/flac;rate=44100:*",
    "http-get:*:audio/flac;rate=48000:*",
    "http-get:*:audio/flac;rate=88200:*",
    "http-get:*:audio/flac;rate=96000:*",
    "http-get:*:audio/flac;rate=176400:*",
    "http-get:*:audio/flac;rate=192000:*",
];
/// Lecteur limité à 96 kHz.
const SINK_96: &[&str] = &[
    "http-get:*:audio/flac;rate=44100:*",
    "http-get:*:audio/flac;rate=48000:*",
    "http-get:*:audio/flac;rate=88200:*",
    "http-get:*:audio/flac;rate=96000:*",
];
/// Lecteur qui lit le FLAC sans dire à quelle cadence.
const SINK_MUET: &[&str] = &["http-get:*:audio/flac:*", "http-get:*:audio/L16:*"];

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

fn verdict() -> su::VerdictMachine {
    let mut results = Vec::new();
    for f in su::greffon::OFFERED_FILTERS {
        for (de, vers) in [
            (44_100, 352_800),
            (48_000, 384_000),
            (44_100, 705_600),
            (48_000, 768_000),
        ] {
            results.push(su::MesureMachine {
                filter: f.to_string(),
                from_hz: de,
                to_hz: vers,
                realtime_factor: 10.0,
                admitted: true,
            });
        }
    }
    su::VerdictMachine {
        simd_path: "test".into(),
        margin: 3.0,
        results,
        measured_at: 0,
    }
}

/// Une zone DLNA dont le lecteur annonce `sink`, suréchantillonneur ACTIVÉ.
fn zone_dlna(orch: &PlaybackOrchestrator, uuid: &str, sink: &[&str]) -> i64 {
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon UPnP", Some("dlna"), Some(uuid))
        .unwrap();
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    su::enregistrer_le_verdict(&s, &verdict()).unwrap();
    su::enregistrer_les_reglages(
        &s,
        zone_id,
        &serde_json::json!({ "enabled": true, "filter": "polyvalent" }),
    )
    .unwrap();
    poser_le_sink_de_test(uuid, sink);
    zone_id
}

fn piste(orch: &PlaybackOrchestrator, id: i64, format: &str, cadence: u32) {
    orch.db
        .execute(
            "INSERT OR IGNORE INTO artists (id, name) VALUES (1, 'Banc')",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT OR IGNORE INTO albums (id, title, artist_id) VALUES (1, 'Banc', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                 duration_ms, sample_rate, bit_depth, channels) \
                 VALUES ({id}, 'Piste', 1, 1, '/m/banc/piste-{id}.{format}', '{format}', \
                 300000, {cadence}, 16, 2)"
            ),
            &[],
        )
        .unwrap();
}

fn requete(zone_id: i64, track_id: i64) -> super::super::PlayRequest {
    super::super::PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: Some(track_id),
        source: Some("local".into()),
        source_id: None,
        title: None,
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

/// Un lecteur qui annonce 192 kHz reçoit une piste 44,1 kHz à 176,4 kHz
/// (multiple entier), par le fichier refait, en FLAC.
#[tokio::test]
async fn un_lecteur_a_192_khz_recoit_176_4_khz() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-192", SINK_192);
    piste(&orch, 1, "flac", 44_100);
    let f = orch
        .format_de_sortie_pour_test(&requete(zone_id, 1))
        .await
        .unwrap();
    assert!(f.use_file_transcode, "le fichier refait porte l'étage");
    assert_eq!(f.target_format_str, "flac");
    assert_eq!(f.out_sr, 176_400, "cible = multiple entier ≤ 192 kHz");
    assert_eq!(f.info.sample_rate, 176_400, "annoncée au lecteur (DIDL)");
    let p = f.surech.as_ref().expect("étage posé");
    assert_eq!((p.de, p.vers), (44_100, 176_400));
    let etat = orch.surechantillonnage_reseau_de_la_zone(zone_id).unwrap();
    assert!(etat.active);
    assert_eq!(etat.to_hz, Some(176_400));
    assert_eq!(
        etat.offered_rates_hz,
        vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000]
    );
    // Une piste 48 kHz va à 192 kHz.
    piste(&orch, 2, "flac", 48_000);
    let f = orch
        .format_de_sortie_pour_test(&requete(zone_id, 2))
        .await
        .unwrap();
    assert_eq!(f.out_sr, 192_000);
}

#[tokio::test]
async fn un_lecteur_limite_a_96_khz_recoit_88_2_khz() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-96", SINK_96);
    piste(&orch, 1, "flac", 44_100);
    let f = orch
        .format_de_sortie_pour_test(&requete(zone_id, 1))
        .await
        .unwrap();
    assert_eq!(f.out_sr, 88_200);
    // Le réglage borne aussi : ×2 au plus vers un lecteur à 192 kHz.
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-x2", SINK_192);
    su::enregistrer_les_reglages(
        &SettingsRepo::with_backend(orch.db.clone()),
        zone_id,
        &serde_json::json!({ "enabled": true, "filter": "polyvalent", "max_ratio": 2 }),
    )
    .unwrap();
    piste(&orch, 1, "flac", 44_100);
    let f = orch
        .format_de_sortie_pour_test(&requete(zone_id, 1))
        .await
        .unwrap();
    assert_eq!(f.out_sr, 88_200);
}

/// Lecteur qui n'annonce aucune cadence : rien n'est suréchantillonné, et le
/// refus le dit. La piste part telle quelle (passthrough).
#[tokio::test]
async fn un_lecteur_sans_cadence_annoncee_refuse_avec_motif() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-muet", SINK_MUET);
    piste(&orch, 1, "flac", 44_100);
    let f = orch.format_de_sortie_pour_test(&requete(zone_id, 1)).await;
    if let Ok(f) = f {
        assert!(f.surech.is_none());
        assert_eq!(f.out_sr, 44_100);
    }
    let etat = orch.surechantillonnage_reseau_de_la_zone(zone_id).unwrap();
    assert!(!etat.active);
    assert_eq!(
        etat.reason.as_deref(),
        Some(su::refus_reseau::CADENCE_INCONNUE)
    );
}

/// PURE : refus « pure », et la piste part bit-perfect (aucun transcodage).
#[tokio::test]
async fn pure_refuse_et_reste_bit_perfect() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-pure", SINK_192);
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    piste(&orch, 1, "flac", 44_100);
    let f = orch
        .format_de_sortie_pour_test(&requete(zone_id, 1))
        .await
        .unwrap();
    assert!(f.surech.is_none(), "PURE : aucun étage");
    assert_eq!(f.out_sr, 44_100, "PURE : la cadence de la source");
    let etat = orch.surechantillonnage_reseau_de_la_zone(zone_id).unwrap();
    assert_eq!(etat.reason.as_deref(), Some(su::refus_reseau::PURE));
}

/// DSD : refus motivé (le DoP en UPnP n'est pas dans ce périmètre).
#[tokio::test]
async fn une_source_dsd_refuse_avec_motif() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-dsd", SINK_192);
    let d = DemandeSurechReseau {
        zone_id,
        type_de_zone: Some("dlna"),
        device_id: "uuid:banc-surech-dsd",
        source_format: Some(AudioFormat::Dsd),
        source_hz: 2_822_400,
        plafond_zone: None,
        flac: true,
    };
    assert!(orch.decider_le_surechantillonnage_reseau(d).await.is_none());
    let etat = orch.surechantillonnage_reseau_de_la_zone(zone_id).unwrap();
    assert_eq!(etat.reason.as_deref(), Some(su::refus_reseau::DSD));
}

/// BluOS, Chromecast, Squeezebox : pas de sonde de cadences, refus motivé.
#[tokio::test]
async fn un_lecteur_sans_sonde_refuse_avec_motif() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-bluos", SINK_192);
    for t in ["bluos", "chromecast", "squeezebox"] {
        let d = DemandeSurechReseau {
            zone_id,
            type_de_zone: Some(t),
            device_id: "uuid:banc-surech-bluos",
            source_format: Some(AudioFormat::Flac),
            source_hz: 44_100,
            plafond_zone: None,
            flac: true,
        };
        assert!(orch.decider_le_surechantillonnage_reseau(d).await.is_none());
        assert_eq!(
            orch.surechantillonnage_reseau_de_la_zone(zone_id)
                .unwrap()
                .reason
                .as_deref(),
            Some(su::refus_reseau::LECTEUR_SANS_SONDE),
            "{t}"
        );
    }
}

/// Désactivé : aucune ligne, aucune conversion (le passthrough d'avant).
#[tokio::test]
async fn desactive_rien_ne_change() {
    let orch = orchestrateur();
    let zone_id = zone_dlna(&orch, "uuid:banc-surech-off", SINK_192);
    su::enregistrer_les_reglages(
        &SettingsRepo::with_backend(orch.db.clone()),
        zone_id,
        &serde_json::json!({ "enabled": false }),
    )
    .unwrap();
    piste(&orch, 1, "flac", 44_100);
    let f = orch
        .format_de_sortie_pour_test(&requete(zone_id, 1))
        .await
        .unwrap();
    assert!(f.surech.is_none());
    assert_eq!(f.out_sr, 44_100);
    assert!(orch.surechantillonnage_reseau_de_la_zone(zone_id).is_none());
}

// ── Mesures sur le flux servi ────────────────────────────────────────────

/// Une sinusoïde stéréo 16 bits à 44,1 kHz : `f` Hz, amplitude `a` (pleine
/// échelle = 1), phase `phi`.
fn wav_sinus(chemin: &std::path::Path, trames: usize, f: f64, a: f64, phi: f64) {
    let mut octets = Vec::with_capacity(44 + trames * 4);
    octets.extend_from_slice(&crate::audio::wav::build_wav_header_with_data_size(
        2,
        44_100,
        16,
        (trames * 4) as u32,
    ));
    for n in 0..trames {
        let x = a * (std::f64::consts::TAU * f * n as f64 / 44_100.0 + phi).sin();
        let mot = (x * 32_767.0).round().clamp(-32_768.0, 32_767.0) as i16;
        octets.extend_from_slice(&mot.to_le_bytes());
        octets.extend_from_slice(&mot.to_le_bytes());
    }
    std::fs::write(chemin, octets).unwrap();
}

fn reglages() -> su::UpsamplerSettings {
    su::UpsamplerSettings {
        enabled: true,
        ..Default::default()
    }
}

/// Le flux servi : 44,1 kHz → 176,4 kHz, exactement 4·n trames (latence du
/// noyau retirée, queue rendue), aucune crête à 0 dBFS — fs/4 à 45° à
/// −0,001 dBFS, dont la reconstruction culmine à +3 dBFS entre les
/// échantillons. Et le FLAC 176,4/24 se relit.
#[tokio::test]
async fn le_flux_servi_est_a_176_4_khz_longueur_exacte_sans_crete() {
    let bac = crate::test_scratch::scratch_dir("tune-surech-reseau");
    let source = bac.join("source.wav");
    let n = 44_100usize;
    // Échantillons à ±32 762 (−0,001 dBFS) ; reconstruction à +3 dBFS.
    wav_sinus(&source, n, 11_025.0, 1.414, std::f64::consts::FRAC_PI_4);
    let dest = bac.join("servi.flac").to_string_lossy().into_owned();
    let (_, pcm, bd) = super::super::transcodage::transcode_source_to_file_avec_crossfeed(
        source.to_string_lossy().into_owned(),
        44_100,
        2,
        24,
        "flac".into(),
        None,
        None,
        None,
        dest.clone(),
        None,
        None,
        None,
        None,
        None,
        Some((reglages(), 176_400)),
    )
    .await
    .unwrap();
    assert_eq!(bd, 24);
    assert_eq!(pcm.len(), 4 * n * 2 * 3, "exactement 4·n trames de 24 bits");
    let d = crate::audio::decode::decode_to_pcm(&dest, None, None, 0.0, 0.0).unwrap();
    assert_eq!(d.sample_rate, 176_400, "FLAC relu à 176,4 kHz");
    assert_eq!(d.channels, 2);
    assert_eq!(d.samples_i32.len(), 4 * n * 2, "FLAC relu : 4·n trames");
    let plein = f64::from(1u32 << (d.bit_depth - 1));
    let crete = d
        .samples_i32
        .iter()
        .fold(0.0f64, |m, s| m.max(f64::from(*s).abs() / plein));
    assert!(crete < 1.0, "crête {crete} : jamais 0 dBFS");
    assert!(
        crete <= tune_plugin_audio_support::limiteur::PLAFOND + 1e-4,
        "crête {crete} au-dessus du plafond du limiteur"
    );
    assert!(crete > 0.9, "crête {crete} : le niveau ne s'effondre pas");
}

/// Contre-épreuve : SANS l'étage (le correctif retiré), la même source sort
/// à 44,1 kHz, n trames.
#[tokio::test]
async fn sans_l_etage_le_flux_reste_a_44_1_khz() {
    let bac = crate::test_scratch::scratch_dir("tune-surech-reseau-sans");
    let source = bac.join("source.wav");
    let n = 4_410usize;
    wav_sinus(&source, n, 1_000.0, 0.5, 0.0);
    let dest = bac.join("servi.flac").to_string_lossy().into_owned();
    let (_, pcm, _) = super::super::transcodage::transcode_source_to_file_avec_crossfeed(
        source.to_string_lossy().into_owned(),
        44_100,
        2,
        24,
        "flac".into(),
        None,
        None,
        None,
        dest.clone(),
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(pcm.len(), n * 2 * 3);
    let d = crate::audio::decode::decode_to_pcm(&dest, None, None, 0.0, 0.0).unwrap();
    assert_eq!(d.sample_rate, 44_100);
}

/// Le contenu est le bon : une sinusoïde à 1 kHz reste à 1 kHz (corrélation
/// avec la référence analytique à 176,4 kHz), alignée sur la source (latence
/// retirée), au niveau de la source moins la réserve du filtre.
#[test]
fn la_chaine_reseau_surechantillonne_alignee_au_bon_niveau() {
    let n = 8_820usize;
    let a = 0.5;
    let mut pcm = Vec::with_capacity(n * 6);
    for i in 0..n {
        let x = a * (std::f64::consts::TAU * 1_000.0 * i as f64 / 44_100.0).sin();
        let mot = (x * 8_388_607.0).round() as i32;
        let b = mot.to_le_bytes();
        for _ in 0..2 {
            pcm.extend_from_slice(&b[..3]);
        }
    }
    let etage = su::SurechantillonneurF64::nouveau(&reglages(), 44_100, 176_400, 2).unwrap();
    let reserve = 10f64.powf(etage.reserve_db() / 20.0);
    assert!(reserve <= 1.0, "gain jamais supérieur à 1");
    let mut dsp = super::super::StreamingDsp {
        channels: 2,
        sample_rate: 44_100,
        surechantillonneur: Some(super::super::chaine_reseau_tess::SurechReseau::nouveau(
            etage,
        )),
        ..Default::default()
    };
    assert!(dsp.is_active());
    dsp.process_piste(&mut pcm, 24);
    assert_eq!(pcm.len(), 4 * n * 6);
    // Au milieu (loin des bords), comparer à la référence à 176,4 kHz. Le
    // noyau, de longueur paire, a son centre ENTRE deux échantillons : on
    // cherche le meilleur décalage fractionnaire, qui doit rester sous une
    // demi-trame de sortie (2,8 µs).
    let snr = |decalage: f64| {
        let mut err = 0.0f64;
        let mut energie = 0.0f64;
        for k in (n / 2)..(3 * n) {
            let o = &pcm[k * 6..k * 6 + 3];
            let v = f64::from(i32::from_le_bytes([0, o[0], o[1], o[2]]) >> 8) / 8_388_608.0;
            let t = (k as f64 - decalage) / 176_400.0;
            let r = a * reserve * (std::f64::consts::TAU * 1_000.0 * t).sin();
            err += (v - r).powi(2);
            energie += r.powi(2);
        }
        10.0 * (energie / err).log10()
    };
    let (meilleur, snr_db) = (-8..=8)
        .map(|i| f64::from(i) / 8.0)
        .map(|d| (d, snr(d)))
        .fold((0.0, f64::MIN), |m, x| if x.1 > m.1 { x } else { m });
    assert!(
        meilleur.abs() <= 0.5,
        "décalage {meilleur} trame : la latence du noyau n'est pas retirée"
    );
    assert!(
        snr_db > 60.0,
        "écart à la référence : {snr_db:.1} dB (décalage {meilleur})"
    );
}

/// Charge : coût CPU du suréchantillonnage d'une piste de 10 minutes,
/// 44,1 → 176,4 kHz, 16 → 24 bits, FLAC, sur CETTE machine — le délai avant
/// la première note du fichier refait. Lancer en `--release -- --ignored`.
#[tokio::test]
#[ignore = "banc de charge : cargo test --release -- --ignored"]
async fn banc_charge_piste_de_10_minutes_44_1_vers_176_4() {
    let bac = crate::test_scratch::scratch_dir("tune-surech-reseau-banc");
    let source = bac.join("dix-minutes.wav");
    let n = 44_100usize * 600;
    wav_sinus(&source, n, 997.0, 0.5, 0.0);
    for (filtre, vers, cible) in [
        ("polyvalent", 176_400u32, "flac"),
        ("precision", 176_400, "flac"),
        ("polyvalent", 352_800, "flac"),
        ("aucun", 44_100, "flac"),
    ] {
        let dest = bac.join(format!("servi-{filtre}-{vers}.{cible}"));
        let surech = (filtre != "aucun").then(|| {
            (
                su::UpsamplerSettings {
                    enabled: true,
                    filter: filtre.into(),
                    ..Default::default()
                },
                vers,
            )
        });
        let t0 = std::time::Instant::now();
        let (taille, _, _) = super::super::transcodage::transcode_source_to_file_avec_crossfeed(
            source.to_string_lossy().into_owned(),
            44_100,
            2,
            24,
            cible.into(),
            None,
            None,
            None,
            dest.to_string_lossy().into_owned(),
            None,
            None,
            None,
            None,
            None,
            surech,
        )
        .await
        .unwrap();
        let s = t0.elapsed().as_secs_f64();
        eprintln!(
            "BANC_SURECH filtre={filtre} vers={vers} cible={cible} duree_piste_s=600 \
             premiere_note_s={s:.2} temps_reel=x{:.1} octets={taille}",
            600.0 / s
        );
        let _ = std::fs::remove_file(&dest);
    }
}

/// Charge, sans fichier : le seul étage de suréchantillonnage (chaîne TESS +
/// quantification), 60 s de musique. Lancer en `--release -- --ignored`.
#[test]
#[ignore = "banc de charge : cargo test --release -- --ignored"]
fn banc_charge_etage_seul() {
    let n = 44_100usize * 60;
    let mut source = Vec::with_capacity(n * 6);
    for i in 0..n {
        let x = 0.5 * (std::f64::consts::TAU * 997.0 * i as f64 / 44_100.0).sin();
        let b = ((x * 8_388_607.0) as i32).to_le_bytes();
        for _ in 0..2 {
            source.extend_from_slice(&b[..3]);
        }
    }
    for (filtre, vers) in [
        ("polyvalent", 176_400u32),
        ("precision", 176_400),
        ("resolution", 176_400),
        ("polyvalent", 352_800),
    ] {
        let r = su::UpsamplerSettings {
            enabled: true,
            filter: filtre.into(),
            ..Default::default()
        };
        let etage = su::SurechantillonneurF64::nouveau(&r, 44_100, vers, 2).unwrap();
        let mut dsp = super::super::StreamingDsp {
            channels: 2,
            sample_rate: 44_100,
            surechantillonneur: Some(super::super::chaine_reseau_tess::SurechReseau::nouveau(
                etage,
            )),
            ..Default::default()
        };
        let mut pcm = source.clone();
        let t0 = std::time::Instant::now();
        dsp.process_piste(&mut pcm, 24);
        let s = t0.elapsed().as_secs_f64();
        eprintln!(
            "BANC_ETAGE filtre={filtre} vers={vers} musique_s=60 calcul_s={s:.2} temps_reel=x{:.1}",
            60.0 / s
        );
    }
}
