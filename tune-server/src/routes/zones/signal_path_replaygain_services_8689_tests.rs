//! #8689 — un titre Qobuz / TIDAL dont le service publie un ReplayGain : le
//! chemin du signal montre l'étape, avec sa provenance « données du
//! service » ; PURE l'écarte ; sans donnée publiée, rien ne change.
//! Les gains entrent par le registre, comme le ferait la lecture d'une
//! réponse `track/get` (aucun appel réseau).

use super::*;
use std::sync::Arc;
use tune_core::audio::replaygain::services::{GainsDuService, noter};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone_et_titre(source: &str, source_id: &str) -> (Arc<dyn DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Titre".into(),
            track_id: None,
            source: source.into(),
            source_id: Some(source_id.into()),
            format: Some("flac".into()),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            stream_id: Some("sid-8689".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    regler(
        &backend,
        tune_core::audio::replaygain::SERVICES_ENABLED_KEY,
        "true",
    );
    (backend, zone, ps)
}

fn chemin(backend: &Arc<dyn DbBackend>, zone: &Zone, ps: &ZoneState) -> Value {
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        ..Default::default()
    };
    build_signal_path(ps, zone, backend, Some("Node"), "none", Some(&fil)).unwrap()
}

fn regler(backend: &Arc<dyn DbBackend>, cle: &str, valeur: &str) {
    SettingsRepo::with_backend(backend.clone())
        .set(cle, valeur)
        .unwrap();
}

fn etape_replaygain(sp: &Value) -> Option<&Value> {
    sp["steps"]
        .as_array()
        .and_then(|s| s.iter().find(|e| e["name"] == "ReplayGain"))
}

fn publier(source: &str, id: &str, gain_db: f64) {
    noter(
        source,
        id,
        GainsDuService {
            piste_db: Some(gain_db),
            piste_pic: Some(0.5),
            album_db: None,
            album_pic: None,
        },
    );
}

#[test]
fn le_chemin_montre_le_replaygain_publie_par_qobuz_8689() {
    publier("qobuz", "8689101", -8.0);
    let (backend, zone, ps) = zone_et_titre("qobuz", "8689101");
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");

    let sp = chemin(&backend, &zone, &ps);
    let etape = etape_replaygain(&sp)
        .unwrap_or_else(|| panic!("#8689 : aucune étape ReplayGain pour un titre Qobuz : {sp}"));
    assert_eq!(etape["gain_db"].as_f64(), Some(-8.0), "{sp}");
    assert_eq!(etape["gain_source"], "streaming_service", "{sp}");
}

#[test]
fn sous_pure_le_gain_publie_par_tidal_est_ecarte_et_dit_8689() {
    publier("tidal", "8689102", -6.0);
    let (backend, zone, ps) = zone_et_titre("tidal", "8689102");
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    let zid = zone.id.unwrap();
    regler(
        &backend,
        &format!("zone_{zid}_audiophile"),
        r#"{"enabled":true}"#,
    );
    let sp = chemin(&backend, &zone, &ps);
    assert!(
        etape_replaygain(&sp).is_none(),
        "PURE n'applique rien : {sp}"
    );
    assert_eq!(
        sp["pure_replaygain_ignored"]["gain_db"].as_f64(),
        Some(-6.0),
        "{sp}"
    );
}

#[test]
fn sans_donnee_publiee_rien_ne_change_8689() {
    let (backend, zone, ps) = zone_et_titre("qobuz", "8689199");
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    let sp = chemin(&backend, &zone, &ps);
    assert!(etape_replaygain(&sp).is_none(), "{sp}");
}

#[test]
fn sous_bitperfect_strict_le_gain_de_service_n_est_pas_annonce_applique_8689() {
    publier("qobuz", "8689103", -6.0);
    let (backend, zone, ps) = zone_et_titre("qobuz", "8689103");
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    regler(
        &backend,
        &tune_core::audio::bitperfect_strict::cle_de_zone(zone.id.unwrap()),
        "true",
    );
    let sp = chemin(&backend, &zone, &ps);
    assert!(
        etape_replaygain(&sp).is_none(),
        "le chemin strict ne doit pas annoncer un gain : {sp}"
    );
}
