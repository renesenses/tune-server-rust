//! #8689 — témoins du registre des gains publiés, sur des réponses simulées
//! (aucun appel réseau).

use super::*;
use serde_json::json;

#[test]
fn qobuz_audio_info_donne_le_gain_et_le_pic_de_piste() {
    let item = json!({
        "id": 12345,
        "audio_info": {"replaygain_track_gain": -8.21, "replaygain_track_peak": 0.988}
    });
    let g = GainsDuService::depuis_qobuz(&item).expect("gain Qobuz lu");
    assert_eq!(g.piste_db, Some(-8.21));
    assert_eq!(g.piste_pic, Some(0.988));
    assert_eq!(g.album_db, None);
}

#[test]
fn qobuz_sans_audio_info_ne_donne_rien() {
    assert_eq!(GainsDuService::depuis_qobuz(&json!({"id": 1})), None);
    // Un pic seul n'est pas un gain.
    assert_eq!(
        GainsDuService::depuis_qobuz(&json!({"audio_info": {"replaygain_track_peak": 1.0}})),
        None
    );
}

#[test]
fn tidal_v2_et_v1_donnent_piste_et_album() {
    let v2 = json!({
        "albumAudioNormalizationData": {"replayGain": -8.1, "peakAmplitude": 0.99},
        "trackAudioNormalizationData": {"replayGain": -7.9, "peakAmplitude": 0.98}
    });
    let g = GainsDuService::depuis_tidal_v2(&v2).unwrap();
    assert_eq!((g.piste_db, g.piste_pic), (Some(-7.9), Some(0.98)));
    assert_eq!((g.album_db, g.album_pic), (Some(-8.1), Some(0.99)));

    let v1 = json!({
        "trackReplayGain": -6.5, "trackPeakAmplitude": 1.0,
        "albumReplayGain": -7.0, "albumPeakAmplitude": 1.0
    });
    let g = GainsDuService::depuis_tidal_lecture(&v1).unwrap();
    assert_eq!(g.piste_db, Some(-6.5));
    assert_eq!(g.album_db, Some(-7.0));
}

#[test]
fn valeurs_hors_echelle_ignorees() {
    let g = GainsDuService::depuis_tidal_lecture(&json!({
        "trackReplayGain": -6.0, "trackPeakAmplitude": 32768.0
    }))
    .unwrap();
    assert_eq!(
        g.piste_pic, None,
        "un pic à l'échelle de l'échantillon n'est pas un pic"
    );
    assert_eq!(
        GainsDuService::depuis_tidal_lecture(&json!({"trackReplayGain": 999.0})),
        None
    );
}

#[test]
fn mode_album_retombe_sur_la_piste() {
    let g = GainsDuService {
        piste_db: Some(-5.0),
        piste_pic: Some(0.9),
        album_db: None,
        album_pic: None,
    };
    let (gain, granularite, nature) = g.choisir(ReplayGainMode::Album).unwrap();
    assert_eq!(gain.gain_db, -5.0);
    assert_eq!(granularite, ReplayGainMode::Track);
    assert_eq!(nature, PeakKind::SamplePeak);
    assert!(g.choisir(ReplayGainMode::Off).is_none());
}

#[test]
fn le_registre_complete_sans_effacer() {
    noter(
        "TIDAL",
        "services-tests-1",
        GainsDuService {
            piste_db: Some(-3.0),
            piste_pic: None,
            album_db: Some(-4.0),
            album_pic: Some(0.97),
        },
    );
    // L'objet piste ne porte que le gain de piste : l'album reste.
    noter(
        "tidal",
        "services-tests-1",
        GainsDuService {
            piste_db: Some(-3.5),
            piste_pic: Some(0.95),
            album_db: None,
            album_pic: None,
        },
    );
    let g = lire("tidal", "services-tests-1").unwrap();
    assert_eq!(g.piste_db, Some(-3.5));
    assert_eq!(g.album_db, Some(-4.0));
    assert_eq!(lire("qobuz", "services-tests-1"), None);
}

#[test]
fn piste_rg_la_bibliotheque_prime_et_les_autres_services_sont_ignores() {
    let p = PisteRg::new(Some(7), Some("qobuz"), Some("123"));
    assert_eq!(p.service, None);
    assert_eq!(p.track_id, Some(7));
    let p = PisteRg::new(None, Some("qobuz"), Some("123"));
    assert_eq!(p.service, Some(("qobuz", "123")));
    let p = PisteRg::new(None, Some("youtube"), Some("abc"));
    assert!(!p.est_identifiee());
}

// ── Le facteur de lecture, comme la lecture le calcule ────────────────────

use crate::audio::replaygain::{MODE_KEY, PREAMP_KEY, facteur_de_lecture, gain_de_piste};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use std::sync::Arc;

fn base(mode: &str, preamp: &str) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let s = SettingsRepo::with_backend(db.clone());
    s.set(super::super::SERVICES_ENABLED_KEY, "true").unwrap();
    s.set(MODE_KEY, mode).unwrap();
    s.set(PREAMP_KEY, preamp).unwrap();
    db
}

/// 🔴 LE TÉMOIN DE #8689 : un titre Qobuz dont le service publie −8 dB joue
/// à −8 dB (+ pré-gain), et non plus à 1,0. Les gains arrivent par le
/// chemin de production (`map_track` → `noter`) : ici leur lecteur
/// `depuis_qobuz` sur une réponse `track/get` simulée.
#[test]
fn un_titre_qobuz_recoit_le_gain_publie_et_le_pre_gain() {
    let reponse = json!({
        "id": 880_001,
        "audio_info": {"replaygain_track_gain": -8.0, "replaygain_track_peak": 0.5}
    });
    noter(
        "qobuz",
        "880001",
        GainsDuService::depuis_qobuz(&reponse).unwrap(),
    );
    let db = base("track", "2");
    let piste = PisteRg::new(None, Some("qobuz"), Some("880001"));
    let f = facteur_de_lecture(&db, piste);
    let attendu = 10f64.powf(-6.0 / 20.0);
    assert!(
        (f - attendu).abs() < 1e-12,
        "titre Qobuz : facteur {f}, attendu {attendu} (−8 dB publiés + 2 dB de pré-gain)"
    );
}

/// Mode album : le gain d'album TIDAL ; et l'anti-écrêtage tire sur le pic
/// publié (réserve de pic d'échantillon comprise), comme pour un fichier.
#[test]
fn un_titre_tidal_suit_le_mode_album_et_l_anti_ecretage() {
    let v2 = json!({
        "albumAudioNormalizationData": {"replayGain": 6.0, "peakAmplitude": 0.9},
        "trackAudioNormalizationData": {"replayGain": -3.0, "peakAmplitude": 0.9}
    });
    noter(
        "tidal",
        "880002",
        GainsDuService::depuis_tidal_v2(&v2).unwrap(),
    );
    let db = base("album", "0");
    let piste = PisteRg::new(None, Some("tidal"), Some("880002"));
    let (g, granularite, _) = gain_de_piste(&db, piste, ReplayGainMode::Album).unwrap();
    assert_eq!((g.gain_db, granularite), (6.0, ReplayGainMode::Album));
    let f = facteur_de_lecture(&db, piste);
    // +6 dB demandés, pic 0,9 relevé de 3 dB de réserve : borné sous l'unité
    // de crête.
    let borne = 1.0 / (0.9 * 10f64.powf(3.0 / 20.0));
    assert!((f - borne).abs() < 1e-12, "facteur {f}, borne {borne}");
}

/// Sans donnée publiée, ou ReplayGain coupé : 1,0 — le comportement d'avant.
#[test]
fn sans_donnee_ou_mode_off_rien_ne_change() {
    let db = base("track", "6");
    let inconnue = PisteRg::new(None, Some("qobuz"), Some("880099"));
    assert_eq!(facteur_de_lecture(&db, inconnue), 1.0);
    noter(
        "qobuz",
        "880003",
        GainsDuService {
            piste_db: Some(-9.0),
            piste_pic: Some(1.0),
            album_db: None,
            album_pic: None,
        },
    );
    let off = base("off", "6");
    assert_eq!(
        facteur_de_lecture(&off, PisteRg::new(None, Some("qobuz"), Some("880003"))),
        1.0
    );
    assert_eq!(facteur_de_lecture(&db, PisteRg::AUCUNE), 1.0);
}

#[test]
fn les_services_8689_restent_desactives_par_defaut_et_se_desactivent() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let settings = SettingsRepo::with_backend(db.clone());
    settings.set(MODE_KEY, "track").unwrap();
    noter(
        "qobuz",
        "8689-opt-in",
        GainsDuService {
            piste_db: Some(-6.0),
            piste_pic: Some(0.8),
            ..Default::default()
        },
    );
    let piste = PisteRg::new(None, Some("qobuz"), Some("8689-opt-in"));
    assert_eq!(facteur_de_lecture(&db, piste), 1.0, "absent = désactivé");
    settings
        .set(super::super::SERVICES_ENABLED_KEY, "true")
        .unwrap();
    assert!(
        facteur_de_lecture(&db, piste) < 0.6,
        "activation volontaire"
    );
    settings
        .set(super::super::SERVICES_ENABLED_KEY, "false")
        .unwrap();
    assert_eq!(facteur_de_lecture(&db, piste), 1.0, "désactivation");
}

/// Valeurs techniques relevées sur une réponse Qobuz réelle le 15/11,
/// sans donnée de compte ni URL de lecture.
#[test]
fn qobuz_reponse_reelle_8689_gain_positif_et_pic_restent_proteges() {
    let gains = GainsDuService::depuis_qobuz(&json!({
        "audio_info": { "replaygain_track_gain": 2.53, "replaygain_track_peak": 0.666412 }
    }))
    .unwrap();
    noter("qobuz", "8689-reponse-reelle", gains);
    let db = base("track", "0");
    let f = facteur_de_lecture(
        &db,
        PisteRg::new(None, Some("qobuz"), Some("8689-reponse-reelle")),
    );
    let borne = 1.0 / (0.666412 * 10f64.powf(3.0 / 20.0));
    assert!(
        (f - borne).abs() < 1e-12,
        "le gain publié doit respecter le pic et la réserve"
    );
}
