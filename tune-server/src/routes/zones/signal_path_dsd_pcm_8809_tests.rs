//! #8809 — le libellé décrit le fil, même si la zone demandait du DSD.
use super::*;
use std::sync::Arc;
use tune_core::db::{backend::DbBackend, sqlite::SqliteDb};
use tune_core::playback::NowPlaying;

fn chemin(cadence: u32, mode: &str, dop: bool, fil: StreamInfo) -> Value {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let zones = ZoneRepo::with_backend(backend.clone());
    let id = zones
        .create("DAC factice", Some("local"), Some("local:asio:banc-8809"))
        .unwrap();
    zones.update_dsd_mode(id, mode).unwrap();
    let zone = zones.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "DSD témoin".into(),
            format: Some("dsf".into()),
            sample_rate: Some(cadence),
            bit_depth: Some(1),
            stream_id: Some("banc-8809".into()),
            ..Default::default()
        }),
        volume: 1.0,
        dop_active: dop,
        ..Default::default()
    };
    build_signal_path(
        &ps,
        &zone,
        &backend,
        Some("DAC factice"),
        "ASIO",
        Some(&fil),
    )
    .unwrap()
}

#[test]
fn un_dsd_decime_local_se_dit_pcm_8809() {
    for (cadence, source) in [
        (11_289_600, "DSD256 11.3 MHz"),
        (22_579_200, "DSD512 22.6 MHz"),
    ] {
        for mode in ["pcm", "native", "dop"] {
            let sp = chemin(
                cadence,
                mode,
                false,
                StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 352_800,
                    bit_depth: 32,
                    ..Default::default()
                },
            );
            let etape = sp["steps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == "Transcoder")
                .expect("conversion affichée");
            assert!(
                sp["summary"].as_str().unwrap().contains("→ PCM transcode"),
                "#8809 : résumé encore DSD : {sp}"
            );
            assert_eq!(
                etape["description"],
                format!("{source} → PCM 352kHz/32bit"),
                "#8809 : le fil PCM porte encore le nom DSD de la source (mode={mode}) : {sp}"
            );
        }
    }
}

#[test]
fn le_dop_mesure_garde_son_nom_8809() {
    let sp = chemin(
        11_289_600,
        "dop",
        true,
        StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 705_600,
            bit_depth: 24,
            ..Default::default()
        },
    );
    let etape = sp["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["code"] == "dop")
        .expect("DoP mesuré");
    assert_eq!(
        etape["description"],
        "DSD256 11.3 MHz → DSD256 over PCM (DoP)"
    );
    assert_eq!(etape["bit_perfect"], true);
}
