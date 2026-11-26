//! Témoins des réglages du greffon « Casque » tirés d'OPRA (rc14).

use super::*;
use crate::audio::opra_base::{analyser_base, correction_de};

fn hd650() -> CorrectionOpra {
    let base = analyser_base(crate::audio::opra_base::tests::EXTRAIT);
    correction_de(&base, "sennheiser:hd650::oratory1990_harman_target").unwrap()
}

#[test]
fn les_reglages_portent_la_reference_et_les_bandes_du_profil() {
    let r = reglages_du_casque(&hd650(), NIVEAU_PLEIN, None);
    // Le maillon porte `enabled`, pas ses réglages.
    assert!(r.get("enabled").is_none());
    assert_eq!(r["listening"], "headphones");
    assert_eq!(r["headroom_mode"], "realistic");
    assert_eq!(r["bands"].as_array().unwrap().len(), 5);
    let reference = profil_reconnu(&r).expect("profil reconnu");
    assert_eq!(reference.id, "sennheiser:hd650::oratory1990_harman_target");
    assert_eq!(reference.product_id, "sennheiser::hd650");
    assert_eq!(reference.vendor, "Sennheiser");
    assert_eq!(reference.product, "HD650");
    assert_eq!(reference.author, "oratory1990");
    assert_eq!(reference.details, "Harman Target");
    assert_eq!(reference.license, "CC BY-SA 4.0");
    assert_eq!(reference.level, 100);
    // Les réglages restent un profil du moteur (le schéma de la caisse).
    let mut p = r.clone();
    p.as_object_mut()
        .unwrap()
        .insert("enabled".into(), true.into());
    serde_json::from_value::<EqProfile>(p).expect("EqProfile");
}

#[test]
fn la_reference_survit_a_l_aller_retour_json() {
    let r = reglages_du_casque(&hd650(), 70, None);
    let relu: Value = serde_json::from_str(&r.to_string()).unwrap();
    assert_eq!(profil_reconnu(&relu).unwrap().level, 70);
}

#[test]
fn des_bandes_retouchees_ne_se_disent_plus_reconnues() {
    let mut r = reglages_du_casque(&hd650(), NIVEAU_PLEIN, None);
    r["bands"][0]["gain"] = Value::from(9.0);
    assert!(profil_reconnu(&r).is_none());
    // Un centième de dB suffit.
    let mut r2 = reglages_du_casque(&hd650(), NIVEAU_PLEIN, None);
    r2["bands"][1]["gain"] = Value::from(-2.49);
    assert!(profil_reconnu(&r2).is_none());
    // Contre-épreuve : sans retouche, reconnu.
    assert!(profil_reconnu(&reglages_du_casque(&hd650(), NIVEAU_PLEIN, None)).is_some());
    // Des bandes libres, sans référence : pas de profil.
    let mut libre = r.clone();
    libre.as_object_mut().unwrap().remove(CHAMP_OPRA);
    assert!(profil_reconnu(&libre).is_none());
}

#[test]
fn le_niveau_d_effet_ne_touche_que_les_gains() {
    let c = hd650();
    let moitie = bandes_au_niveau(&c.profil.bandes, 50);
    for (b, m) in c.profil.bandes.iter().zip(&moitie) {
        assert_eq!((b.freq, b.q, &b.band_type), (m.freq, m.q, &m.band_type));
        if b.band_type == "high_pass" {
            assert_eq!(m.gain, b.gain);
        } else {
            assert!(
                (m.gain - b.gain / 2.0).abs() < 0.005 + 1e-12,
                "{b:?} → {m:?}"
            );
        }
    }
    // Au-delà de 100 % : borné à la correction publiée.
    assert_eq!(
        serde_json::to_value(bandes_au_niveau(&c.profil.bandes, 250)).unwrap(),
        serde_json::to_value(&c.profil.bandes).unwrap()
    );
    // À 0 %, aucune cloche ni plateau ne pousse.
    assert!(
        bandes_au_niveau(&c.profil.bandes, 0)
            .iter()
            .filter(|b| b.band_type != "high_pass")
            .all(|b| b.gain == 0.0)
    );
}

#[test]
fn le_preampli_automatique_couvre_le_maximum_de_la_courbe() {
    let r = reglages_du_casque(&hd650(), NIVEAU_PLEIN, None);
    let preampli = preampli_auto_db(&r);
    let hz = frequences_d_affichage(256);
    let courbe = courbe_db(&r, &hz);
    let max = courbe.iter().copied().fold(f64::MIN, f64::max);
    assert!(max > 3.0, "la correction pousse le grave : {max}");
    // Le préampli compense au moins le maximum de la courbe : pas d'écrêtage.
    assert!(
        preampli <= -max + 1e-6,
        "préampli {preampli}, maximum {max}"
    );
    // Moins d'effet, moins de réserve.
    let doux = preampli_auto_db(&reglages_du_casque(&hd650(), 30, None));
    assert!(doux > preampli, "{doux} > {preampli}");
}

#[test]
fn la_courbe_suit_la_correction() {
    let r = reglages_du_casque(&hd650(), NIVEAU_PLEIN, None);
    let hz = frequences_d_affichage(64);
    assert_eq!(hz.len(), 64);
    assert!((hz[0] - 20.0).abs() < 1e-9 && (hz[63] - 20_000.0).abs() < 1e-6);
    let db = courbe_db(&r, &[40.0, 1000.0, 16_000.0]);
    // Plateau grave +5,5 dB, plateau aigu −3 dB : le grave monte, l'aigu
    // descend, le médium reste près de 0.
    assert!(db[0] > 2.0, "{db:?}");
    assert!(db[1].abs() < 1.5, "{db:?}");
    assert!(db[2] < -1.5, "{db:?}");
}

#[test]
fn les_champs_inconnus_des_reglages_precedents_sont_gardes() {
    let precedents = serde_json::json!({ "futur": 1, "bass_gain_db": 6.0, "enabled": true });
    let r = reglages_du_casque(&hd650(), NIVEAU_PLEIN, Some(&precedents));
    assert_eq!(r["futur"], 1);
    // Ce que la correction décide est écrasé.
    assert_eq!(r["bass_gain_db"], 0.0);
    assert!(r.get("enabled").is_none());
}
