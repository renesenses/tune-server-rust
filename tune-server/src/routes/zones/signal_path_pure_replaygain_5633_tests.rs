//! #5633 — PURE ignore le ReplayGain (bit-perfect), et le chemin du signal
//! le DIT : `pure_replaygain_ignored` porte le gain que la piste en cours
//! recevrait hors PURE. Hors PURE, l'étape « ReplayGain » porte le même
//! nombre en `gain_db`.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone_et_piste(gain_tag: &str) -> (Arc<dyn DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(96_000);
    t.bit_depth = Some(24);
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
        .set(tid, "rg_track_gain", gain_tag)
        .unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            track_id: Some(tid),
            format: Some("flac".into()),
            sample_rate: Some(96_000),
            bit_depth: Some(24),
            stream_id: Some("sid-1".into()),
            ..Default::default()
        }),
        volume: 1.0,
        ..Default::default()
    };
    (backend, zone, ps)
}

fn chemin(backend: &Arc<dyn DbBackend>, zone: &Zone, ps: &ZoneState) -> Value {
    let fil = StreamInfo {
        format: "flac".into(),
        sample_rate: 96_000,
        bit_depth: 24,
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

#[test]
fn sous_pure_le_chemin_dit_le_replaygain_ignore_de_la_piste_5633() {
    let (backend, zone, ps) = zone_et_piste("-8.50 dB");
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    let zid = zone.id.unwrap();
    regler(
        &backend,
        &format!("zone_{zid}_audiophile"),
        r#"{"enabled":true}"#,
    );

    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["pure"], true);
    assert!(
        etape_replaygain(&sp).is_none(),
        "PURE n'applique rien : {sp}"
    );
    let ignore = &sp["pure_replaygain_ignored"];
    assert_eq!(ignore["gain_db"].as_f64(), Some(-8.5), "{sp}");
    assert_eq!(ignore["granularity"], "track");
}

#[test]
fn hors_pure_l_etape_porte_le_meme_gain_et_la_cle_est_absente_5633() {
    let (backend, zone, ps) = zone_et_piste("-8.50 dB");
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");

    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("pure_replaygain_ignored").is_none(), "{sp}");
    let etape = etape_replaygain(&sp).expect("étape ReplayGain");
    assert_eq!(etape["gain_db"].as_f64(), Some(-8.5), "{sp}");
}

#[test]
fn sous_pure_sans_replaygain_actif_rien_n_est_annonce_5633() {
    // Mode off : le réglage, pas le tag, décide — PURE n'ignore rien.
    let (backend, zone, ps) = zone_et_piste("-8.50 dB");
    let zid = zone.id.unwrap();
    regler(
        &backend,
        &format!("zone_{zid}_audiophile"),
        r#"{"enabled":true}"#,
    );
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("pure_replaygain_ignored").is_none(), "{sp}");
}
