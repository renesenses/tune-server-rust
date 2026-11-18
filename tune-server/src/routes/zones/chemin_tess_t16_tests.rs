//! TESS T16 — le bloc `tess` croisé avec le VRAI chemin du signal.
//!
//! Chaque témoin construit le chemin historique avec `build_signal_path`
//! (sonde de sortie comprise), puis la vue TESS par-dessus, et vérifie
//! qu'elles ne se contredisent jamais : « Bit-perfect » côté TESS exige un
//! chemin historique intact et aucun traitement en service.

use super::chemin_tess_t16::{chemin_tess_de_la_zone, en_service_d_apres_le_chemin};
use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::traits::{OutputSampleTransport, OutputSignalPathStatus};
use tune_core::playback::NowPlaying;
use tune_core::tess::chemin::GreffonsEnService;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn zone(backend: &Arc<dyn DbBackend>, output_type: &str, device: &str, volume: f64) -> Zone {
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Z", Some(output_type), Some(device)).unwrap();
    let mut z = repo.get(id).unwrap().unwrap();
    z.volume = volume;
    z
}

fn en_lecture(dsp: OutputDspState, volume: OutputVolumeState) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            channels: Some(2),
            ..Default::default()
        }),
        volume: 1.0,
        output_signal_path: Some(OutputSignalPathStatus {
            bit_perfect: dsp != OutputDspState::Applied,
            sample_transport: OutputSampleTransport::NativeInteger,
            dsp,
            volume,
            reasons: vec![],
        }),
        ..Default::default()
    }
}

fn armer_l_eq(backend: &Arc<dyn DbBackend>, zone_id: i64) {
    let profile = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![tune_core::audio::eq::EqBandSpec {
            gain: 6.0,
            ..Default::default()
        }],
        ..Default::default()
    };
    let s = SettingsRepo::with_backend(backend.clone());
    s.set(
        &format!("zone_{zone_id}_eq_profile"),
        &serde_json::to_string(&profile).unwrap(),
    )
    .unwrap();
    s.set("plugin_equalizer_installed", "true").unwrap();
}

/// Le chemin historique puis la vue TESS, et leur cohérence.
fn les_deux(backend: &Arc<dyn DbBackend>, z: &Zone, ps: &ZoneState) -> (Value, Value) {
    let sp = build_signal_path(ps, z, backend, Some("DAC"), "ALSA", None).expect("chemin");
    let tess = chemin_tess_de_la_zone(backend, z, ps, &sp, true).expect("bloc tess");
    if tess["badge"] == "bit_perfect" {
        assert_eq!(
            sp["bit_perfect"], true,
            "TESS dit Bit-perfect sur un chemin altéré : {sp}"
        );
        let effectifs: Vec<_> = tess["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["effective"] == true)
            .map(|e| e["code"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(effectifs, ["source", "output"], "{tess}");
    }
    (sp, tess)
}

fn etape<'a>(tess: &'a Value, code: &str) -> &'a Value {
    tess["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == code)
        .unwrap()
}

fn greffons_effectifs(tess: &Value) -> Vec<String> {
    tess["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["code"] == "plugin" && e["effective"] == true)
        .map(|e| e["plugin_id"].as_str().unwrap_or("DSP").to_owned())
        .collect()
}

#[test]
fn eq_qui_tourne_traite_par_tess_et_nomme() {
    let b = base();
    let z = zone(&b, "local", "local:dac", 100.0);
    armer_l_eq(&b, z.id.unwrap());
    let (sp, tess) = les_deux(
        &b,
        &z,
        &en_lecture(OutputDspState::Applied, OutputVolumeState::Unity),
    );
    assert_eq!(sp["bit_perfect"], false);
    assert_eq!(tess["badge"], "processed_by_tess");
    let g = greffons_effectifs(&tess);
    assert_eq!(g.len(), 1, "{tess}");
    assert!(g[0].starts_with("equalizer-"), "{g:?}");
    let eq = tess["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["plugin_id"] == g[0].as_str())
        .unwrap();
    assert_eq!(eq["params"]["bands"], 1);
    assert_eq!(etape(&tess, "quantization")["effective"], true);
    assert_eq!(etape(&tess, "quantization")["name"], "Dither");
}

#[test]
fn eq_arme_que_la_sortie_n_applique_pas_n_est_pas_annonce() {
    // L'égaliseur est réglé, mais la sonde de la sortie dit qu'aucun
    // traitement n'a touché les échantillons : la vue TESS le dit aussi.
    let b = base();
    let z = zone(&b, "local", "local:dac", 100.0);
    armer_l_eq(&b, z.id.unwrap());
    let (_, tess) = les_deux(
        &b,
        &z,
        &en_lecture(OutputDspState::Inactive, OutputVolumeState::Unity),
    );
    assert!(greffons_effectifs(&tess).is_empty(), "{tess}");
    let eq = tess["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == "plugin")
        .expect("le greffon reste listé");
    assert_eq!(eq["reason"], "not_in_service");
    assert!(
        tess["discrepancies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["kind"] == "requested_not_running")
    );
}

#[test]
fn volume_logiciel_seul_traite_par_tess_mais_reste_bit_perfect_pour_l_historique() {
    let b = base();
    let z = zone(&b, "local", "local:dac", 50.0);
    let (sp, tess) = les_deux(
        &b,
        &z,
        &en_lecture(OutputDspState::Inactive, OutputVolumeState::Applied),
    );
    // L'historique tient le volume pour une préférence ; TESS le compte
    // (décision T07 : « Bit-perfect » seulement à 0 dB).
    assert_eq!(sp["bit_perfect"], true);
    assert_eq!(tess["badge"], "processed_by_tess");
    assert_eq!(etape(&tess, "volume")["effective"], true);
    assert_eq!(tess["volume"]["placement"], "at_render");
    let db = tess["volume"]["db"].as_f64().unwrap();
    assert!((db - 20.0 * 0.5f64.log10()).abs() < 1e-9, "{db}");
}

#[test]
fn rien_ne_tourne_volume_a_l_unite_bit_perfect() {
    let b = base();
    let z = zone(&b, "local", "local:dac", 100.0);
    let (_, tess) = les_deux(
        &b,
        &z,
        &en_lecture(OutputDspState::Inactive, OutputVolumeState::Unity),
    );
    assert_eq!(tess["badge"], "bit_perfect");
    assert_eq!(
        etape(&tess, "quantization")["reason"],
        "nothing_to_quantize"
    );
}

#[test]
fn pure_verrouille_tient_le_volume_a_0_db_et_redevient_bit_perfect() {
    let b = base();
    let z = zone(&b, "local", "local:dac", 50.0);
    let zid = z.id.unwrap();
    let s = SettingsRepo::with_backend(b.clone());
    s.set(&format!("zone_{zid}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    // Pure sans verrou : le volume logiciel reste effectif.
    let (_, tess) = les_deux(
        &b,
        &z,
        &en_lecture(OutputDspState::PureSansObjet, OutputVolumeState::Applied),
    );
    assert_eq!(tess["badge"], "processed_by_tess");
    // Avec le verrou : tenu à 100 %, bit-perfect.
    s.set(tune_core::audio::audiophile::SETTING_LOCK_VOLUME, "true")
        .unwrap();
    let (_, tess) = les_deux(
        &b,
        &z,
        &en_lecture(OutputDspState::PureSansObjet, OutputVolumeState::Unity),
    );
    assert_eq!(tess["badge"], "bit_perfect", "{tess}");
}

#[test]
fn zone_hqplayer_hors_tess() {
    let b = base();
    let z = zone(&b, "hqplayer", "hqp-1", 100.0);
    let mut ps = en_lecture(OutputDspState::Inactive, OutputVolumeState::Unity);
    ps.output_signal_path = None;
    let sp = build_signal_path(&ps, &z, &b, Some("HQPlayer"), "", None).expect("chemin");
    let tess = chemin_tess_de_la_zone(&b, &z, &ps, &sp, true).unwrap();
    assert_eq!(tess["badge"], "outside_tess");
}

#[test]
fn chaine_illisible_pas_de_bloc_plutot_qu_une_chaine_inventee() {
    let b = base();
    let z = zone(&b, "local", "local:dac", 100.0);
    SettingsRepo::with_backend(b.clone())
        .set(
            &tune_core::tess::cle_chaine_zone(z.id.unwrap()),
            "{pas du json",
        )
        .unwrap();
    let ps = en_lecture(OutputDspState::Inactive, OutputVolumeState::Unity);
    let sp = build_signal_path(&ps, &z, &b, Some("DAC"), "ALSA", None).unwrap();
    assert!(chemin_tess_de_la_zone(&b, &z, &ps, &sp, true).is_none());
}

#[test]
fn la_sonde_du_chemin_historique() {
    let sp = json!({
        "steps": [
            { "name": "DSP", "bit_perfect": false },
            { "name": "Correction de pièce", "code": "room_correction", "applied": false }
        ],
        "room_correction": { "applied": false },
        "upsampling": { "active": true }
    });
    let s = en_service_d_apres_le_chemin(&sp, true);
    assert_eq!(
        s.greffons,
        GreffonsEnService::Sonde {
            actif: true,
            hors_service: vec!["convolution".into(), "room-correction".into()]
        }
    );
    assert!(s.upsampling && s.volume_logiciel && s.headroom_db.is_none());
    // Une étape DSP intacte (contournée) ne compte pas.
    let sp = json!({ "steps": [ { "name": "DSP", "bit_perfect": true } ] });
    assert_eq!(
        en_service_d_apres_le_chemin(&sp, false).greffons,
        GreffonsEnService::Sonde {
            actif: false,
            hors_service: vec![]
        }
    );
}
