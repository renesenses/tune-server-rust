//! #7632 — l'étape « Correction de pièce » du chemin du signal. Une réponse
//! impulsionnelle réglée sur la zone (`ir_path_{zone}`) est DÉCRITE, que le
//! convolueur l'applique ou non : à la mauvaise cadence, `build_for` la
//! retire en silence (#5190), et PURE la contourne. L'étape et la clé
//! `room_correction` disent lequel des cas joue.

use super::*;
use std::sync::Arc;
use tune_core::db::sqlite::SqliteDb;
use tune_core::playback::NowPlaying;

const FIL_HZ: u32 = 96_000;

fn zone_en_lecture() -> (Arc<dyn tune_core::db::backend::DbBackend>, Zone, ZoneState) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
    let repo = ZoneRepo::with_backend(backend.clone());
    let id = repo.create("Salon", Some("dlna"), Some("dev-1")).unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    let ps = ZoneState {
        state: PlayState::Playing,
        now_playing: Some(NowPlaying {
            title: "Piste".into(),
            format: Some("flac".into()),
            sample_rate: Some(FIL_HZ),
            bit_depth: Some(24),
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
        sample_rate: FIL_HZ,
        bit_depth: 24,
        ..Default::default()
    };
    build_signal_path(ps, zone, backend, Some("Node"), "none", Some(&fil)).unwrap()
}

fn regler(backend: &Arc<dyn tune_core::db::backend::DbBackend>, cle: &str, valeur: &str) {
    SettingsRepo::with_backend(backend.clone())
        .set(cle, valeur)
        .unwrap();
}

/// Un WAV mono en flottants 32 bits (format 3), `taps` échantillons : une
/// impulsion de Dirac, la RI la plus simple que le convolueur accepte.
fn ecrire_ri(chemin: &std::path::Path, cadence: u32, taps: usize) {
    let donnees = taps * 4;
    let mut w = Vec::with_capacity(44 + donnees);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    w.extend_from_slice(&1u16.to_le_bytes()); // mono
    w.extend_from_slice(&cadence.to_le_bytes());
    w.extend_from_slice(&(cadence * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 1.0 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(chemin, w).unwrap();
}

fn etape(sp: &Value) -> Option<&Value> {
    sp["steps"]
        .as_array()
        .and_then(|s| s.iter().find(|e| e["code"] == "room_correction"))
}

#[test]
fn sans_ri_reglee_ni_etape_ni_resume_7632() {
    let (backend, zone, ps) = zone_en_lecture();
    let sp = chemin(&backend, &zone, &ps);
    assert!(sp["room_correction"].is_null(), "{}", sp["room_correction"]);
    assert!(etape(&sp).is_none(), "étape inattendue : {:?}", etape(&sp));
}

#[test]
fn une_ri_a_la_cadence_du_flux_est_dite_appliquee_7632() {
    let (backend, zone, ps) = zone_en_lecture();
    let ri = tune_core::test_scratch::scratch_file("ri-7632-ok", ".wav");
    ecrire_ri(ri.path(), FIL_HZ, 512);
    regler(
        &backend,
        &format!("ir_path_{}", zone.id.unwrap()),
        &ri.as_str(),
    );

    let sp = chemin(&backend, &zone, &ps);
    let rc = &sp["room_correction"];
    assert_eq!(rc["configured"], true, "{rc}");
    assert_eq!(rc["status"], "applied", "{rc}");
    assert_eq!(rc["applied"], true, "{rc}");
    assert_eq!(rc["taps"], 512, "{rc}");
    assert_eq!(rc["ir_sample_rate"], FIL_HZ, "{rc}");
    let e = etape(&sp).expect("l'étape Correction de pièce manque");
    assert_eq!(e["status"], "applied", "{e}");
    assert_eq!(e["bit_perfect"], false, "{e}");
}

#[test]
fn une_ri_a_la_mauvaise_cadence_est_dite_retiree_7632() {
    let (backend, zone, ps) = zone_en_lecture();
    let ri = tune_core::test_scratch::scratch_file("ri-7632-cadence", ".wav");
    ecrire_ri(ri.path(), 48_000, 256);
    regler(
        &backend,
        &format!("ir_path_{}", zone.id.unwrap()),
        &ri.as_str(),
    );

    let sp = chemin(&backend, &zone, &ps);
    let rc = &sp["room_correction"];
    assert_eq!(rc["status"], "rate_mismatch", "{rc}");
    assert_eq!(rc["applied"], false, "{rc}");
    assert_eq!(rc["ir_sample_rate"], 48_000, "{rc}");
    let e = etape(&sp).expect("l'étape doit DIRE la RI retirée");
    assert_eq!(e["status"], "rate_mismatch", "{e}");
    assert_eq!(e["bit_perfect"], true, "{e}");
    let raison = e["reason"].as_str().unwrap_or_default();
    assert!(
        raison.contains("48000") && raison.contains("96000"),
        "la raison doit nommer les deux cadences : {raison}"
    );
}

#[test]
fn sous_pure_la_ri_est_dite_contournee_7632() {
    let (backend, zone, ps) = zone_en_lecture();
    let ri = tune_core::test_scratch::scratch_file("ri-7632-pure", ".wav");
    ecrire_ri(ri.path(), FIL_HZ, 128);
    let zid = zone.id.unwrap();
    regler(&backend, &format!("ir_path_{zid}"), &ri.as_str());
    regler(
        &backend,
        &format!("zone_{zid}_audiophile"),
        r#"{"enabled":true}"#,
    );

    let sp = chemin(&backend, &zone, &ps);
    assert_eq!(sp["room_correction"]["status"], "bypassed_pure", "{sp}");
    assert_eq!(sp["room_correction"]["applied"], false);
}

#[test]
fn une_ri_illisible_est_dite_illisible_7632() {
    let (backend, zone, ps) = zone_en_lecture();
    // Un fichier qui n'est pas un WAV : le lecteur de RI le refuse.
    let ri = tune_core::test_scratch::scratch_file("ri-7632-illisible", ".wav");
    std::fs::write(ri.path(), b"pas un wav").unwrap();
    regler(
        &backend,
        &format!("ir_path_{}", zone.id.unwrap()),
        &ri.as_str(),
    );

    let sp = chemin(&backend, &zone, &ps);
    let rc = &sp["room_correction"];
    assert_eq!(rc["status"], "unreadable", "{rc}");
    assert_eq!(rc["applied"], false, "{rc}");
    assert!(rc["reason"].as_str().is_some_and(|r| !r.is_empty()), "{rc}");
}
