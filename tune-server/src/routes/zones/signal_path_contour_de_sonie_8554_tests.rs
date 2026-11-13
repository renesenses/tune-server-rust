//! #8554 — l'étape « Contour de loudness » du chemin du signal : absente
//! quand le réglage est éteint, active (et non bit-perfect) sous la
//! référence, intacte au niveau de référence, contournée sous PURE, dite
//! « pas encore appliquée » sur une sortie réseau.

use super::*;

fn base_et_zone(
    sortie: &str,
    type_de_sortie: &str,
) -> (std::sync::Arc<dyn tune_core::db::backend::DbBackend>, Zone) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Casque", Some(type_de_sortie), Some(sortie))
        .unwrap();
    let zone = repo.get(id).unwrap().unwrap();
    (state.backend.clone(), zone)
}

fn allumer(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>, zone: &Zone) {
    SettingsRepo::with_backend(backend.clone())
        .set(
            &tune_core::audio::contour_de_sonie::cle(zone.id.unwrap()),
            r#"{"enabled":true}"#,
        )
        .unwrap();
}

#[tokio::test]
async fn eteint_aucune_etape() {
    let (backend, zone) = base_et_zone("local:DAC", "local");
    assert!(etape_du_contour_de_sonie("fr", &zone, &backend, "local").is_none());
}

#[tokio::test]
async fn sous_la_reference_l_etape_dit_le_contour_et_n_est_pas_bit_perfect() {
    let (backend, mut zone) = base_et_zone("local:DAC", "local");
    allumer(&backend, &zone);
    zone.volume = 0.1; // −20 dB
    let etape = etape_du_contour_de_sonie("fr", &zone, &backend, "local").unwrap();
    assert_eq!(etape["code"], "loudness_contour");
    assert_eq!(etape["status"], "active");
    assert_eq!(etape["bit_perfect"], false, "{etape}");
    assert_eq!(etape["attenuation_db"], 20.0);
    let d = etape["description"].as_str().unwrap();
    assert!(d.contains("+7.6 dB à 63 Hz"), "{d}");
}

#[tokio::test]
async fn a_la_reference_l_etape_reste_bit_perfect() {
    let (backend, mut zone) = base_et_zone("local:DAC", "local");
    allumer(&backend, &zone);
    zone.volume = 1.0;
    let etape = etape_du_contour_de_sonie("en", &zone, &backend, "local").unwrap();
    assert_eq!(etape["status"], "active");
    assert_eq!(etape["bit_perfect"], true, "{etape}");
}

#[tokio::test]
async fn sous_pure_le_contour_est_contourne() {
    let (backend, mut zone) = base_et_zone("local:DAC", "local");
    allumer(&backend, &zone);
    SettingsRepo::with_backend(backend.clone())
        .set(
            &format!("zone_{}_audiophile", zone.id.unwrap()),
            r#"{"enabled":true}"#,
        )
        .unwrap();
    zone.volume = 0.1;
    let etape = etape_du_contour_de_sonie("fr", &zone, &backend, "local").unwrap();
    assert_eq!(etape["status"], "bypassed_pure");
    assert_eq!(etape["bit_perfect"], true);
}

#[tokio::test]
async fn sur_une_sortie_reseau_il_n_est_pas_encore_applique() {
    let (backend, mut zone) = base_et_zone("dlna:uuid-1", "dlna");
    allumer(&backend, &zone);
    zone.volume = 0.1;
    let etape = etape_du_contour_de_sonie("fr", &zone, &backend, "dlna").unwrap();
    assert_eq!(etape["status"], "output_not_supported");
    assert_eq!(etape["bit_perfect"], true);
}
