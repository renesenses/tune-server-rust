//! #7614 — le suréchantillonneur dans le chemin du signal : l'étape « Upsampler »
//! (filtre, réserve) quand il porte la cadence, la ligne « inactif (mode PURE) »
//! quand PURE l'écarte, et rien quand il est désactivé.

use super::*;
use std::sync::Arc;
use tune_core::db::sqlite::SqliteDb;
use tune_core::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
use tune_core::playback::NowPlaying;

fn zone_locale() -> (Arc<dyn tune_core::db::backend::DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    (backend, zone, ps)
}

fn chemin(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    zone: &Zone,
    ps: &ZoneState,
) -> Value {
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    build_signal_path(ps, zone, backend, Some("DAC"), "CPAL", Some(&fil)).unwrap()
}

fn activer(backend: &Arc<dyn tune_core::db::backend::DbBackend>, zone_id: i64) {
    tune_core::audio::surechantillonnage::enregistrer_les_reglages(
        &SettingsRepo::with_backend(backend.clone()),
        zone_id,
        &serde_json::json!({"enabled": true}),
    )
    .unwrap();
}

fn etape<'a>(sp: &'a Value, code: &str) -> Option<&'a Value> {
    sp["steps"].as_array()?.iter().find(|e| e["code"] == code)
}

#[test]
fn desactive_rien_n_apparait() {
    let (backend, zone, ps) = zone_locale();
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("upsampling").is_none(), "{sp}");
    assert!(etape(&sp, "upsampling").is_none());
}

#[test]
fn actif_l_etape_nomme_le_filtre_et_la_reserve_et_le_bit_perfect_tombe() {
    let (backend, zone, mut ps) = zone_locale();
    activer(&backend, zone.id.unwrap());
    ps.transformations_reelles = Some(
        TransformationsReelles::nouvelles(
            AudioSpec::nouvelle(44_100, ProfondeurPcm::Entier16, 2).unwrap(),
            FormatOuvert::new(352_800, 2),
            false,
        )
        .avec_surechantillonnage("polyvalent", -8.55),
    );
    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["bit_perfect"], false);
    let e = etape(&sp, "upsampling").expect("l'étape du suréchantillonneur");
    assert_eq!(e["from_hz"], 44_100);
    assert_eq!(e["to_hz"], 352_800);
    assert_eq!(e["filter"], "polyvalent");
    assert_eq!(e["bit_perfect"], false);
    let d = e["description"].as_str().unwrap();
    assert!(
        d.contains("44.1 kHz") && d.contains("352.8 kHz") && d.contains("-8.55"),
        "{d}"
    );
    assert_eq!(sp["upsampling"]["active"], true);
    assert_eq!(sp["upsampling"]["headroom_db"], -8.55);
    assert!(
        etape(&sp, "rate_conversion").is_none(),
        "une seule étape de cadence"
    );
}

#[test]
fn sous_pure_la_ligne_inactif_mode_pure() {
    let (backend, zone, ps) = zone_locale();
    let id = zone.id.unwrap();
    activer(&backend, id);
    SettingsRepo::with_backend(backend.clone())
        .set(&format!("zone_{id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["upsampling"]["active"], false);
    assert_eq!(sp["upsampling"]["reason"], "pure");
    let e = etape(&sp, "upsampling").expect("la ligne PURE");
    assert_eq!(e["bit_perfect"], true);
    assert!(e["description"].as_str().unwrap().contains("PURE"));
}
