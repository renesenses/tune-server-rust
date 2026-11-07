//! #8074 — une sortie locale née au balayage à chaud reçoit la graine de sa
//! zone AVANT d'être enregistrée.
//!
//! Un testeur sous Windows, ASIO choisi, zone locale sur la sortie WASAPI
//! « Haut-parleurs » réglée vers 30 % (loi `db60`) : le premier morceau après
//! le démarrage est parti à pleine échelle. Avec ASIO, le démarrage
//! n'enregistre que les noms ASIO ; les sorties WASAPI naissent dans
//! `appliquer_le_parc_local`, qui ne posait aucune graine : la sortie gardait
//! son gain de naissance (l'unité) jusqu'au premier geste sur le volume.
//!
//! Le banc donne à `appliquer_le_parc_local` un parc fabriqué : aucun
//! matériel n'est sondé, aucun son n'est joué.
use super::*;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::local::{AudioDevice, LocalOutput};

fn appareil(nom: &str, hote: &str) -> AudioDevice {
    AudioDevice {
        name: nom.into(),
        endpoint_id: String::new(),
        is_default: false,
        max_channels: 2,
        sample_rates: vec![44100, 48000],
        sample_rates_measured: true,
        availability: None,
        backend: hote.into(),
        hardware_detail: None,
    }
}

/// Le gain que rendent les rappels de la sortie `device_id`, rapporté à
/// celui d'une sortie neuve (l'unité) : indépendant de l'unité des atomiques.
async fn gain_rendu(state: &AppState, device_id: &str) -> (f64, bool) {
    let unite = LocalOutput::new("étalon".into())
        .gain_de_rendu()
        .load(std::sync::atomic::Ordering::SeqCst) as f64;
    let arc = state
        .outputs
        .lock()
        .await
        .get(device_id)
        .expect("la sortie doit être enregistrée par le balayage");
    let sortie = arc.lock().await;
    let locale = sortie
        .as_any()
        .downcast_ref::<LocalOutput>()
        .expect("une LocalOutput");
    let g = locale
        .gain_de_rendu()
        .load(std::sync::atomic::Ordering::SeqCst) as f64;
    (g / unite, locale.volume_est_pose())
}

fn en_db(g: f64) -> f64 {
    20.0 * g.max(1e-12).log10()
}

/// LE démarrage du rapport : ASIO choisi, zone « Haut-parleurs » à 30 % du
/// curseur `db60` en base, la sortie WASAPI naît au balayage à chaud.
#[tokio::test]
async fn sortie_wasapi_nee_au_balayage_sous_asio_part_au_gain_de_sa_zone_8074() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = "local:Haut-parleurs";
    let zone = repo
        .create("Zone 1", Some("local"), Some(id))
        .expect("zone");
    let trente = tune_core::audio::volume_scale::gain_perceptif(0.30);
    // La base garde le GAIN × 100 (`appliquer_volume`).
    repo.update_volume(zone, trente * 100.0).expect("volume");

    appliquer_le_parc_local(&state, &[appareil("Haut-parleurs", "WASAPI")], "asio", true).await;

    let (gain, pose) = gain_rendu(&state, id).await;
    assert!(
        pose && (en_db(gain) - en_db(trente)).abs() < 0.1,
        "#8074 : sortie WASAPI née au balayage à chaud sous ASIO, zone à 30 % db60 \
         ({:.1} dB) : la sortie est enregistrée à {:.1} dB (gain {gain:.4}, volume posé : \
         {pose}). Sans la graine de zone, le premier morceau part à pleine échelle",
        en_db(trente),
        en_db(gain),
    );
}

/// Sans zone en base pour l'appareil, la sortie née au balayage part BAS
/// (30 % du curseur `db60`), jamais à pleine échelle.
#[tokio::test]
async fn sortie_nee_au_balayage_sans_zone_part_bas_8074() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("état");
    let id = "local:Haut-parleurs";

    appliquer_le_parc_local(&state, &[appareil("Haut-parleurs", "WASAPI")], "asio", true).await;

    let (gain, pose) = gain_rendu(&state, id).await;
    assert!(
        pose && (en_db(gain) - (-42.0)).abs() < 0.1,
        "#8074 : sortie sans zone née au balayage : attendu −42 dB, enregistrée à \
         {:.1} dB (volume posé : {pose})",
        en_db(gain),
    );
}
