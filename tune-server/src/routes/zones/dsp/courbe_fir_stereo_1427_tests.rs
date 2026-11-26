//! web#1427 (GgB, fil 1671) — avec une IR stéréo, la courbe du canal droit
//! n'était jamais calculée : seul le canal gauche s'affichait.

use super::*;

/// Un Dirac (plat, 0 dB) à gauche, un Dirac à −6 dB à droite.
fn ir_stereo_aux_canaux_differents() -> Vec<Vec<f32>> {
    let mut gauche = vec![0.0f32; 64];
    gauche[0] = 1.0;
    let mut droit = vec![0.0f32; 64];
    droit[0] = 0.5;
    vec![gauche, droit]
}

#[test]
fn la_reponse_porte_une_courbe_par_canal_1427() {
    let corps = reponse_du_convolueur(&ir_stereo_aux_canaux_differents(), 48_000).unwrap();
    let canaux = corps["channels"]
        .as_array()
        .expect("`channels` doit être publié");
    assert_eq!(
        canaux.len(),
        2,
        "une IR stéréo doit rendre DEUX courbes (seul le canal gauche s'affichait)"
    );
    let db = |canal: &Value| canal["points"][10]["db"].as_f64().unwrap();
    assert!(db(&canaux[0]).abs() < 0.1, "gauche : Dirac plat à 0 dB");
    assert!(
        (db(&canaux[1]) + 6.02).abs() < 0.1,
        "droit : −6 dB, et non la courbe du canal gauche recopiée"
    );
    assert_eq!(canaux[1]["channel"], 1);
}

#[test]
fn points_reste_le_canal_gauche_pour_les_anciens_clients_1427() {
    let corps = reponse_du_convolueur(&ir_stereo_aux_canaux_differents(), 48_000).unwrap();
    assert_eq!(corps["points"], corps["channels"][0]["points"]);
    assert_eq!(corps["taps"], 64);
    assert_eq!(corps["loaded"], true);
}

#[test]
fn une_ir_mono_rend_un_seul_canal_1427() {
    let corps = reponse_du_convolueur(&[vec![1.0f32; 1]], 44_100).unwrap();
    assert_eq!(corps["channels"].as_array().unwrap().len(), 1);
    assert!(reponse_du_convolueur(&[], 44_100).is_err());
    assert!(reponse_du_convolueur(&[vec![1.0f32]], 0).is_err());
}

/// rc14 (Rémi) — 512 points par FFT ; une impulsion retardée différemment
/// sur chaque canal : chaque canal publie SON retard, et sa phase déroulée
/// hors retard est nulle (phase linéaire pure).
#[test]
fn la_courbe_fft_publie_la_phase_deroulee_hors_retard_par_canal_rc14() {
    let mut gauche = vec![0.0f32; 2048];
    gauche[10] = 1.0;
    let mut droit = vec![0.0f32; 2048];
    droit[480] = -0.5;
    let corps = reponse_du_convolueur(&[gauche, droit], 48_000).unwrap();
    assert_eq!(corps["grid_points"], 512);
    let canaux = corps["channels"].as_array().unwrap();
    for (canal, retard) in [(0usize, 10u64), (1, 480)] {
        let c = &canaux[canal];
        assert_eq!(c["phase_delay_samples"], retard);
        assert_eq!(
            c["phase_delay_ms"],
            (retard as f64 / 48.0 * 100.0).round() / 100.0
        );
        let points = c["points"].as_array().unwrap();
        assert_eq!(points.len(), 512);
        for p in points {
            let ex = p["phase_unwrapped_deg"].as_f64().expect("phase publiée");
            // Le droit est inversé (−0,5) : ±180° hors retard, constant.
            let attendu = if canal == 0 { 0.0 } else { 180.0 };
            let ecart = (ex.abs() - attendu).abs();
            assert!(ecart < 0.01, "canal {canal}, {} Hz : {ex}°", p["f"]);
        }
    }
    assert!((canaux[1]["points"][100]["db"].as_f64().unwrap() + 6.02).abs() < 0.01);
}
