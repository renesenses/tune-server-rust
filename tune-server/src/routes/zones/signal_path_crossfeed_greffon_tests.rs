//! L'étape crossfeed du chemin du signal porte le nom du greffon natif tiers
//! de la famille du crossfeed quand c'est lui qui traite.

use super::*;

fn etapes() -> Vec<Value> {
    vec![
        json!({"name": "DSP", "description": "EQ actif", "bit_perfect": false}),
        json!({"name": "Crossfeed", "code": "crossfeed",
               "description": "Crossfeed casque dans le flux (voies gauche et droite croisées)",
               "bit_perfect": false}),
    ]
}

#[test]
fn l_etape_prend_le_nom_du_greffon_de_crossfeed() {
    let mut steps = etapes();
    nommer_le_crossfeed_du_greffon(&mut steps, Some("crossfeed-essai"));
    assert_eq!(steps[1]["name"], "Crossfeed Essai");
    assert!(
        steps[1]["description"]
            .as_str()
            .is_some_and(|d| d.starts_with("Crossfeed Essai ")),
        "{}",
        steps[1]
    );
    assert_eq!(steps[1]["plugin"], "crossfeed-essai");
    assert_eq!(
        steps[1]["code"], "crossfeed",
        "le code reste lisible par les clients"
    );
    assert_eq!(steps[1]["bit_perfect"], false);
    assert_eq!(steps[0], etapes()[0], "les autres étapes ne bougent pas");
}

#[test]
fn sans_greffon_l_etape_du_crossfeed_integre_ne_change_pas() {
    let mut steps = etapes();
    nommer_le_crossfeed_du_greffon(&mut steps, None);
    assert_eq!(steps, etapes());
    let mut sans = vec![json!({"name": "DSP", "description": "EQ actif"})];
    nommer_le_crossfeed_du_greffon(&mut sans, Some("crossfeed-essai"));
    assert_eq!(
        sans,
        vec![json!({"name": "DSP", "description": "EQ actif"})]
    );
}
