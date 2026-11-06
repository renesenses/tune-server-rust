//! #7632 — la convolution par réponse impulsionnelle est un droit Premium
//! (`RoomCorrection`, décision du 04/11/2026), tenu par l'orchestrateur pour
//! le flux transcodé vers une sortie réseau (`load_convolver`) et pour tout ce
//! qui lit `chemin_ir_configure` (empreinte du flux, verdict PURE, bras
//! transcodé). ⚠️ Sans licence fournie : pas de droit.

use super::*;
use std::sync::Arc;

fn orchestrateur(licence: Option<&str>) -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let license = match licence {
        None => None,
        Some("gratuit") => Some(Arc::new(crate::license::LicenseManager::new(db.clone()))),
        Some(_) => Some(crate::license::licence_premium_pour_test(db.clone())),
    };
    let mut orch = PlaybackOrchestrator::new(
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
    );
    orch.license = license;
    orch
}

/// Une RI WAV mono flottante, Dirac de gain 0,25, réglée sur la zone 1.
fn regler_ri(orch: &PlaybackOrchestrator) -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-7632-orchestrateur", ".wav");
    let taps = 32usize;
    let donnees = taps * 4;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&48_000u32.to_le_bytes());
    w.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 0.25 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), w).unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set("ir_path_1", &f.as_str())
        .unwrap();
    f
}

#[test]
fn sans_premium_aucun_convolveur_pour_le_flux_transcode_7632() {
    for cas in [None, Some("gratuit")] {
        let orch = orchestrateur(cas);
        let _ri = regler_ri(&orch);
        assert!(!orch.correction_ir_autorisee(), "{cas:?}");
        assert!(
            orch.load_convolver(1, 48_000, 2).is_none(),
            "{cas:?} : un convolveur a été bâti sans le droit Premium"
        );
        assert!(!orch.zone_has_active_ir(1), "{cas:?}");
        assert!(orch.chemin_ir_configure(1).is_none(), "{cas:?}");
        // Ce que l'utilisateur a réglé reste lisible.
        assert!(orch.chemin_ir_enregistre(1).is_some());
    }
}

#[test]
fn avec_premium_le_flux_transcode_est_convolue_7632() {
    let orch = orchestrateur(Some("premium"));
    let _ri = regler_ri(&orch);
    assert!(orch.correction_ir_autorisee());
    let mut conv = orch
        .load_convolver(1, 48_000, 2)
        .expect("Premium : le convolveur doit être bâti");
    let entree: Vec<f32> = (0..4_096)
        .map(|n| ((n as f32) * 0.01).sin() * 0.5)
        .collect();
    let mut pcm = entree.clone();
    conv.process_interleaved(&mut pcm);
    assert_ne!(pcm, entree, "Premium : la RI doit être appliquée");
    assert!(orch.zone_has_active_ir(1));
}
