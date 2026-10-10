//! #6044 — le chemin du signal dit la réaffectation des canaux, et seulement
//! quand l'étage local l'applique vraiment.

use super::*;
use tune_core::audio::reaffectation_canaux as rc;
use tune_core::outputs::traits::{AudioSpec, FormatOuvert};

fn etat_avec(zone: i64, reglage: Option<rc::ChannelRemapSettings>) -> crate::state::AppState {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    tune_core::audio::premium_plugins::migrate(&settings).unwrap();
    settings
        .set("plugin_channel-remap_installed", "true")
        .unwrap();
    if let Some(r) = reglage {
        settings
            .set(&rc::cle_de_zone(zone), &serde_json::to_string(&r).unwrap())
            .unwrap();
    }
    state
}

fn reel(entree: u16, ouvert: u16) -> TransformationsReelles {
    TransformationsReelles::nouvelles(
        AudioSpec::depuis_entete(48_000, 24, entree).unwrap(),
        FormatOuvert::new(48_000, ouvert),
        true,
    )
}

#[test]
fn quad_vers_5_1_sur_une_sortie_locale_ouverte_en_6_voies_est_annonce() {
    let state = etat_avec(7, rc::prereglage("quad_to_5_1"));
    let r = reel(4, 6);
    let etape = zone_reaffectation_step(&state.backend, 7, "local", None, Some(&r))
        .expect("la matrice 4 → 6 s'applique : l'étape doit apparaître");
    assert!(etape.contains("4 → 6"), "{etape}");
    assert!(etape.contains("quad_to_5_1"), "{etape}");
}

#[test]
fn rien_a_annoncer_quand_l_etage_ne_l_applique_pas() {
    let state = etat_avec(7, rc::prereglage("quad_to_5_1"));
    // Ouvert en 8 voies : la matrice 4 → 6 ne correspond pas.
    assert!(zone_reaffectation_step(&state.backend, 7, "local", None, Some(&reel(4, 8))).is_none());
    // Source 5.1 : la règle 4.0 ne la concerne pas.
    assert!(zone_reaffectation_step(&state.backend, 7, "local", None, Some(&reel(6, 6))).is_none());
    // Zone réseau : aucune réaffectation sur ce chemin.
    assert!(zone_reaffectation_step(&state.backend, 7, "dlna", None, Some(&reel(4, 6))).is_none());
    // Aucun réglage.
    let vide = etat_avec(7, None);
    assert!(zone_reaffectation_step(&vide.backend, 7, "local", None, Some(&reel(4, 6))).is_none());
}

#[test]
fn greffon_desinstalle_rien_n_est_annonce() {
    let state = etat_avec(7, rc::prereglage("quad_to_5_1"));
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_channel-remap_installed", "false")
        .unwrap();
    assert!(zone_reaffectation_step(&state.backend, 7, "local", None, Some(&reel(4, 6))).is_none());
}

#[test]
fn pure_l_emporte_sur_la_reaffectation() {
    let state = etat_avec(7, rc::prereglage("swap_lr"));
    assert!(zone_reaffectation_step(&state.backend, 7, "local", None, Some(&reel(2, 2))).is_some());
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("zone_7_audiophile", r#"{"enabled": true}"#)
        .unwrap();
    assert!(zone_reaffectation_step(&state.backend, 7, "local", None, Some(&reel(2, 2))).is_none());
}
