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
