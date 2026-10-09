//! #4384 — le chemin du signal dit ce que le gain devient :
//! - `replaygain_untagged` : ReplayGain armé, piste sans gain stocké, donc
//!   préampli non appliqué ;
//! - `applied_in` sur l'étape ReplayGain : composé avec le volume d'une sortie
//!   locale, ou cuit dans le flux d'un rendu réseau.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone_et_piste(
    output_type: &str,
    gain_tag: Option<&str>,
) -> (Arc<dyn DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("Salon", Some(output_type), Some("dev-1"))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let mut t = tune_core::db::models::Track::new("Piste".into());
    t.format = Some("flac".into());
    t.sample_rate = Some(96_000);
    t.bit_depth = Some(24);
    let tid = TrackRepo::with_backend(backend.clone()).create(&t).unwrap();
    if let Some(tag) = gain_tag {
        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone())
            .set(tid, "rg_track_gain", tag)
            .unwrap();
    }
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
fn piste_sans_gain_tague_le_chemin_dit_que_le_preampli_n_est_pas_applique_4384() {
    let (backend, zone, ps) = zone_et_piste("dlna", None);
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    regler(&backend, tune_core::audio::replaygain::PREAMP_KEY, "6");

    let sp = chemin(&backend, &zone, &ps);
    assert!(
        etape_replaygain(&sp).is_none(),
        "aucun gain, aucune étape : {sp}"
    );
    let rg = &sp["replaygain_untagged"];
    assert_eq!(rg["preamp_db"].as_f64(), Some(6.0), "{sp}");
    assert_eq!(rg["mode"], "track", "{sp}");
}

#[test]
fn piste_taguee_ou_replaygain_off_la_cle_est_absente_4384() {
    let (backend, zone, ps) = zone_et_piste("dlna", Some("-4.00 dB"));
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    regler(&backend, tune_core::audio::replaygain::PREAMP_KEY, "6");
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("replaygain_untagged").is_none(), "{sp}");

    let (backend, zone, ps) = zone_et_piste("dlna", None);
    regler(&backend, tune_core::audio::replaygain::PREAMP_KEY, "6");
    let sp = chemin(&backend, &zone, &ps);
    assert!(
        sp.get("replaygain_untagged").is_none(),
        "mode off : rien n'est annoncé : {sp}"
    );
}

#[test]
fn sous_pure_la_piste_sans_gain_n_annonce_rien_4384() {
    let (backend, zone, ps) = zone_et_piste("dlna", None);
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    regler(&backend, tune_core::audio::replaygain::PREAMP_KEY, "6");
    let zid = zone.id.unwrap();
    regler(
        &backend,
        &format!("zone_{zid}_audiophile"),
        r#"{"enabled":true}"#,
    );
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp.get("replaygain_untagged").is_none(), "{sp}");
}

#[test]
fn l_etape_replaygain_dit_ou_le_gain_s_applique_4384() {
    let (backend, zone, ps) = zone_et_piste("dlna", Some("-4.00 dB"));
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    let sp = chemin(&backend, &zone, &ps);
    let etape = etape_replaygain(&sp).expect("étape ReplayGain");
    assert_eq!(etape["applied_in"], "stream", "rendu réseau : {sp}");

    let (backend, zone, ps) = zone_et_piste("local", Some("-4.00 dB"));
    regler(&backend, tune_core::audio::replaygain::MODE_KEY, "track");
    let sp = chemin(&backend, &zone, &ps);
    let etape = etape_replaygain(&sp).expect("étape ReplayGain");
    assert_eq!(etape["applied_in"], "local_output", "sortie locale : {sp}");
}
