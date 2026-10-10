//! #2029 (forum fil 2107, Didier, Windows ASIO vers un SMSL SU-8) — un DSD
//! qui part EN DoP s'affichait « DSD64 2.8 MHz → DSD64 176kHz/24bit » :
//! on lisait une décimation en PCM 176,4 kHz / 24 bits, alors que les bits DSD
//! voyagent intacts dans des trames PCM. Libellé demandé par le testeur :
//! « DSD64 2.8MHz > DSD64 over PCM (DoP) ».
//!
//! Le DoP se lit sur `ZoneState::dop_active`, que la sortie DÉTECTE sur les
//! octets (`is_dop_pcm`), jamais sur `dsd_mode` — le réglage dit ce qui a été
//! demandé, pas ce qui part (#1595).

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

fn zone(output_type: &str, device: &str) -> (Arc<dyn DbBackend>, Zone) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo
        .create("SU-8", Some(output_type), Some(device))
        .unwrap();
    (backend.clone(), repo.get(id).unwrap().unwrap())
}

fn dsd64_playing(dop_active: bool) -> ZoneState {
    ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste DSD64".into(),
            format: Some("dsf".into()),
            sample_rate: Some(2_822_400),
            bit_depth: Some(1),
            stream_id: Some("sid-dop".into()),
            ..Default::default()
        }),
        volume: 1.0,
        dop_active,
        ..Default::default()
    }
}

/// Le porteur DoP d'un DSD64 : du « WAV » 176,4 kHz / 24 bits.
fn fil_dop() -> StreamInfo {
    StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 176_400,
        bit_depth: 24,
        ..Default::default()
    }
}

fn transcoder(sp: &Value) -> Option<Value> {
    sp.get("steps")?
        .as_array()?
        .iter()
        .find(|s| s["name"] == "Transcoder")
        .cloned()
}

/// ⭐ Le cas du signalement : sortie locale ASIO, DoP mesuré sur le fil.
#[test]
fn un_dsd_en_dop_se_dit_over_pcm_dop_2029() {
    let (backend, zone) = zone("local", "local:asio:SMSL SU-8");
    let sp = build_signal_path(
        &dsd64_playing(true),
        &zone,
        &backend,
        Some("SMSL SU-8"),
        "ASIO",
        Some(&fil_dop()),
    )
    .unwrap();
    let etape = transcoder(&sp).expect("l'étage d'emballage DoP doit rester affiché");
    assert_eq!(
        etape["description"].as_str(),
        Some("DSD64 2.8 MHz \u{2192} DSD64 over PCM (DoP)"),
        "le DoP n'est pas une conversion en 176 kHz / 24 bits : {etape}"
    );
    assert_eq!(etape["code"], "dop", "{etape}");
    assert_eq!(
        etape["bit_perfect"], true,
        "le DoP emballe les bits DSD sans les toucher : {etape}"
    );
}

/// La contre-épreuve : même source, même fil, mais la sortie n'a PAS vu de
/// DoP (décimation réelle en PCM) — le libellé PCM mesuré reste, en rouge.
#[test]
fn sans_dop_mesure_le_libelle_pcm_reste_2029() {
    let (backend, zone) = zone("local", "local:asio:SMSL SU-8");
    let sp = build_signal_path(
        &dsd64_playing(false),
        &zone,
        &backend,
        Some("SMSL SU-8"),
        "ASIO",
        Some(&fil_dop()),
    )
    .unwrap();
    let etape = transcoder(&sp).expect("témoin : ce cas transcode");
    let desc = etape["description"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !desc.contains("DoP"),
        "aucun DoP mesuré : rien ne doit l'annoncer ({desc})"
    );
    assert!(desc.contains("176kHz/24bit"), "{desc}");
    assert_eq!(etape["bit_perfect"], false, "{etape}");
}

/// Même règle en réseau : un renderer DLNA qui reçoit du DoP le dit aussi.
#[test]
fn un_dop_reseau_mesure_se_dit_aussi_2029() {
    let (backend, zone) = zone("dlna", "dev-1");
    let sp = build_signal_path(
        &dsd64_playing(true),
        &zone,
        &backend,
        Some("Wiim Pro"),
        "none",
        Some(&fil_dop()),
    )
    .unwrap();
    let etape = transcoder(&sp).expect("étage présent");
    assert_eq!(
        etape["description"].as_str(),
        Some("DSD64 2.8 MHz \u{2192} DSD64 over PCM (DoP)"),
        "{etape}"
    );
}
