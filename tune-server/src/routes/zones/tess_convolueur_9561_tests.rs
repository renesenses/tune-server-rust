//! Revue de #9561 — la convolution que TESS pose sur la sortie locale
//! (`rafraichir`) suit le headroom de la chaîne. Mesure : un sinus à
//! −1 dBFS joué par le moteur que la sortie a retenu, filtre de +6,02 dB.
//!
//! - défaut 1 : cocher la convolution en marge MANUELLE ne lui donne pas la
//!   marge Auto en plus de M (l'ancien `set_convolver_ir`, `from_wav`, la
//!   calculait toujours en Auto : −6,5 dB de trop) ;
//! - défaut 2 : passer de manuel à Auto, convolution déjà posée, lui rend sa
//!   marge (sans quoi elle restait sans marge et le signal saturait).

use super::*;
use tune_core::outputs::local::LocalOutput;
use tune_core::tess::chaine::{ChaineTess, Headroom, MaillonGreffon};

const TAUX: u32 = 48_000;

/// Un filtre plat de +6,02 dB (Dirac de 2), WAV flottant mono.
fn ri_plus_6(chemin: &std::path::Path) {
    let taps = 256usize;
    let donnees = taps * 4;
    let mut w = Vec::with_capacity(44 + donnees);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&TAUX.to_le_bytes());
    w.extend_from_slice(&(TAUX * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 2.0 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(chemin, w).unwrap();
}

/// La crête (dBFS) d'un sinus à −1 dBFS rendu par le moteur que la sortie a
/// retenu pour la piste suivante.
async fn crete_du_moteur_retenu(state: &AppState) -> f64 {
    let arc = state.outputs.lock().await.get("local:DAC").unwrap();
    let sortie = arc.lock().await;
    let config = sortie
        .as_any()
        .downcast_ref::<LocalOutput>()
        .expect("sortie locale")
        .config_de_convolution()
        .expect("un jeu de convolution retenu");
    let mut moteur = config.build_for(1024, TAUX, 2).unwrap();
    let a = 10f64.powf(-1.0 / 20.0);
    let mut y: Vec<f64> = (0..TAUX as usize)
        .flat_map(|n| {
            let v = a * (2.0 * std::f64::consts::PI * 997.0 * n as f64 / f64::from(TAUX)).sin();
            [v, v]
        })
        .collect();
    moteur.process_interleaved_f64(&mut y);
    let c = y[y.len() / 2..].iter().fold(0.0f64, |m, x| m.max(x.abs()));
    20.0 * c.log10()
}

#[tokio::test]
async fn la_convolution_posee_par_tess_suit_le_headroom_9561() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    for id in ["equalizer", "room-correction"] {
        settings
            .set(&format!("plugin_{id}_installed"), "true")
            .unwrap();
        settings
            .set(&format!("plugin_{id}_enabled"), "true")
            .unwrap();
    }
    let id = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(id)
        .unwrap()
        .unwrap();
    state
        .outputs
        .lock()
        .await
        .register(Box::new(LocalOutput::new("DAC".to_string())));
    let d = tune_core::test_scratch::scratch_dir("ri-9561-tess");
    let ri = d.path().join("plus6.wav");
    ri_plus_6(&ri);

    // Défaut 1 — convolution cochée, marge MANUELLE −8 dB, sortie locale
    // sans égaliseur : M est le gain fixe de l'étage des égaliseurs, le
    // filtre n'en reçoit pas d'autre (+6,02 dB : −1 → +5,02 dBFS au
    // convolueur, l'étage des égaliseurs pose les −8 dB).
    let mut chaine = ChaineTess {
        plugins: vec![MaillonGreffon {
            id: "convolution".into(),
            enabled: true,
            settings: serde_json::json!({ "ir_path": ri.to_str().unwrap() }),
        }],
        ..ChaineTess::default()
    };
    chaine.fixed.headroom = Headroom::Manual { db: -8.0 };
    let bilan = tune_core::tess::migration::ecrire_chaine(&settings, id, &chaine).unwrap();
    assert!(
        bilan.cles_modifiees.contains(&Partie::Convolution.cle(id)),
        "{bilan:?}"
    );
    rafraichir(&state, &zone, &bilan.cles_modifiees).await;
    let c = crete_du_moteur_retenu(&state).await;
    assert!(
        (c - (-1.0 + 6.02)).abs() < 0.05,
        "manuel : aucune marge Auto de plus au filtre ({c:.2} dBFS)"
    );

    // Défaut 2 — manuel → Auto, convolution inchangée : le filtre reprend
    // sa marge −(6,02 + 0,5) dB, plus de saturation.
    chaine.fixed.headroom = Headroom::Auto {};
    let bilan = tune_core::tess::migration::ecrire_chaine(&settings, id, &chaine).unwrap();
    assert!(
        !bilan.cles_modifiees.contains(&Partie::Convolution.cle(id)),
        "la convolution n'a pas changé : {bilan:?}"
    );
    rafraichir(&state, &zone, &bilan.cles_modifiees).await;
    let c = crete_du_moteur_retenu(&state).await;
    assert!(c <= 0.0, "Auto : saturation ({c:.2} dBFS)");
    assert!((c - (-1.0 - 0.5)).abs() < 0.05, "{c:.2} dBFS");

    // Et retour au manuel : plus de marge au filtre.
    chaine.fixed.headroom = Headroom::Manual { db: -8.0 };
    let bilan = tune_core::tess::migration::ecrire_chaine(&settings, id, &chaine).unwrap();
    rafraichir(&state, &zone, &bilan.cles_modifiees).await;
    let c = crete_du_moteur_retenu(&state).await;
    assert!((c - (-1.0 + 6.02)).abs() < 0.05, "{c:.2} dBFS");
}
