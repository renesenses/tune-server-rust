//! #5081 — l'étape crossfeed du chemin du signal montre l'ombre de la tête.

use super::*;

fn ombre() -> Option<tune_core::audio::crossfeed::OmbreDeTete> {
    Some(tune_core::audio::crossfeed::OmbreDeTete {
        cutoff_hz: 700.0,
        slope_db_per_octave: 4.5,
    })
}

/// Allumée, l'étape « Crossfeed » porte la coupure et la pente, en champs et
/// dans sa description ; les autres étapes n'en disent rien.
#[test]
fn l_etape_crossfeed_montre_la_coupure_et_la_pente_5081() {
    let mut steps = vec![
        json!({"name": "DSP", "description": "EQ actif", "bit_perfect": false}),
        json!({"name": "Crossfeed", "code": "crossfeed",
               "description": "Crossfeed casque dans le flux", "bit_perfect": false}),
    ];
    annoter_l_ombre_du_crossfeed(&mut steps, ombre());
    assert_eq!(
        steps[1]["description"],
        "Crossfeed casque dans le flux · ombre de la tête 700 Hz, 4.5 dB/octave"
    );
    assert_eq!(steps[1]["head_shadow"]["cutoff_hz"], 700.0);
    assert_eq!(steps[1]["head_shadow"]["slope_db_per_octave"], 4.5);
    assert!(steps[0].get("head_shadow").is_none());
}

/// Éteinte, rien ne change ; et sans étape crossfeed, aucune n'est créée.
#[test]
fn eteinte_ou_sans_etape_crossfeed_rien_ne_change_5081() {
    let avant = vec![json!({"name": "Crossfeed", "code": "crossfeed", "description": "x"})];
    let mut steps = avant.clone();
    annoter_l_ombre_du_crossfeed(&mut steps, None);
    assert_eq!(steps, avant);
    let mut sans = vec![json!({"name": "DSP", "description": "EQ actif"})];
    annoter_l_ombre_du_crossfeed(&mut sans, ombre());
    assert_eq!(sans.len(), 1);
    assert!(sans[0].get("head_shadow").is_none());
}

/// La lecture du réglage : crossfeed coché ET interrupteur coché, sinon rien
/// — un réglage d'avant #5081 compris.
#[test]
fn la_zone_n_a_d_ombre_que_crossfeed_et_filtre_coches_5081() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    for (reglage, attendu) in [
        (json!({"enabled": true, "amount": 0.3}), None),
        (
            json!({"enabled": false, "head_shadow_enabled": true, "cutoff_hz": 700.0}),
            None,
        ),
        (
            json!({"enabled": true, "head_shadow_enabled": true, "cutoff_hz": 700.0,
                   "slope_db_per_octave": 4.5}),
            ombre(),
        ),
    ] {
        settings
            .set("zone_7_crossfeed", &reglage.to_string())
            .unwrap();
        assert_eq!(
            zone_crossfeed_ombre(&state.backend, 7),
            attendu,
            "{reglage}"
        );
    }
}
