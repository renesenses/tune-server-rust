//! Témoins de [`super`] : série, ordre, marge unique, bornes de gain.

use super::*;
use crate::audio::eq::EqProcessor;
use crate::tess::migration::{GRILLE_15, courbe_db};
use serde_json::json;

fn bande(freq: f64, gain: f64, q: f64, t: &str) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        channel: None,
    }
}

/// Un graphique 10 bandes (grille ISO, Q 1) en « sourire ».
pub(crate) fn graphique() -> EqProfile {
    let gains = [6.0, 4.0, 2.0, 0.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0];
    EqProfile {
        enabled: true,
        bands: crate::audio::eq_presets::GRILLE_10
            .iter()
            .zip(gains)
            .map(|(&f, g)| bande(f, g, 1.0, "peak"))
            .collect(),
        ..Default::default()
    }
}

/// Un paramétrique « à la Rémi » : un creux profond et un plateau.
pub(crate) fn parametrique() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![
            bande(3150.0, -18.0, 6.0, "peak"),
            bande(120.0, -4.0, 0.7, "low_shelf"),
        ],
        ..Default::default()
    }
}

fn reglages(p: &EqProfile) -> Value {
    let mut v = serde_json::to_value(p).unwrap();
    v.as_object_mut().unwrap().remove("enabled");
    v
}

pub(crate) fn maillon(id: &str, enabled: bool, p: &EqProfile) -> MaillonGreffon {
    MaillonGreffon {
        id: id.into(),
        enabled,
        settings: reglages(p),
    }
}

fn frequences() -> Vec<f64> {
    let mut f: Vec<f64> = GRILLE_15.to_vec();
    f.extend([3150.0, 3000.0, 3300.0, 120.0, 60.0]);
    f
}

/// La réponse en fréquence de la série est la SOMME (en dB) des réponses de
/// chaque égaliseur, à chaque fréquence : les deux s'entendent.
#[test]
fn la_reponse_combinee_est_la_somme_des_reponses() {
    let (g, p) = (graphique(), parametrique());
    let c = combiner_en_serie(&[g.clone(), p.clone()]).unwrap();
    let f = frequences();
    let (rg, rp, rc) = (courbe_db(&g, &f), courbe_db(&p, &f), courbe_db(&c, &f));
    for canal in 0..2 {
        for i in 0..f.len() {
            let attendu = rg[canal][i] + rp[canal][i];
            assert!(
                (rc[canal][i] - attendu).abs() < 1e-6,
                "{} Hz : {} au lieu de {attendu}",
                f[i],
                rc[canal][i]
            );
        }
    }
    // Le creux du paramétrique est bien là, sous le graphique.
    let i = f.iter().position(|&x| x == 3150.0).unwrap();
    assert!(rc[0][i] < rg[0][i] - 15.0, "{} vs {}", rc[0][i], rg[0][i]);
}

/// L'ordre de la chaîne est celui des bandes : graphique puis paramétrique,
/// ou l'inverse si la chaîne les inverse.
#[test]
fn l_ordre_de_la_chaine_est_l_ordre_des_bandes() {
    let (g, p) = (graphique(), parametrique());
    let mut chaine = ChaineTess {
        plugins: vec![
            maillon("equalizer-graphic", true, &g),
            MaillonGreffon {
                id: "crossfeed".into(),
                enabled: true,
                settings: Value::Null,
            },
            maillon("equalizer-parametric", true, &p),
        ],
        ..ChaineTess::default()
    };
    let c = profil_de_la_chaine(&chaine, None).unwrap();
    let attendu: Vec<f64> = g.bands.iter().chain(&p.bands).map(|b| b.freq).collect();
    assert_eq!(c.bands.iter().map(|b| b.freq).collect::<Vec<_>>(), attendu);
    chaine.plugins.reverse();
    let c = profil_de_la_chaine(&chaine, None).unwrap();
    let attendu: Vec<f64> = p.bands.iter().chain(&g.bands).map(|b| b.freq).collect();
    assert_eq!(c.bands.iter().map(|b| b.freq).collect::<Vec<_>>(), attendu);
    // Une façade décochée ne joue pas.
    chaine.plugins[0].enabled = false;
    let c = profil_de_la_chaine(&chaine, None).unwrap();
    assert_eq!(c.bands.len(), g.bands.len());
}

/// Les trois curseurs d'un graphique sans bandes jouent leur cascade (celle
/// du moteur) une fois mis en série.
#[test]
fn les_curseurs_jouent_aussi_en_serie() {
    let curseurs = EqProfile {
        enabled: true,
        bass_gain_db: 5.0,
        treble_gain_db: -3.0,
        ..Default::default()
    };
    let p = parametrique();
    let c = combiner_en_serie(&[curseurs.clone(), p.clone()]).unwrap();
    let f = frequences();
    let (ra, rp, rc) = (
        courbe_db(&curseurs, &f),
        courbe_db(&p, &f),
        courbe_db(&c, &f),
    );
    for i in 0..f.len() {
        assert!((rc[0][i] - ra[0][i] - rp[0][i]).abs() < 1e-6, "{} Hz", f[i]);
    }
}

/// UNE marge, celle de la cascade entière : jamais la somme des réserves de
/// chaque égaliseur. Avec un headroom manuel, c'est lui seul.
#[test]
fn une_seule_marge_rc14() {
    let (g, p) = (graphique(), parametrique());
    let pre = |x: &EqProfile| EqProcessor::new(x, 48_000, 2).preamp_db(0).unwrap();
    let c = combiner_en_serie(&[g.clone(), p.clone()]).unwrap();
    assert!(
        pre(&c) >= pre(&g) + pre(&p) - 1e-9,
        "{} < {} + {}",
        pre(&c),
        pre(&g),
        pre(&p)
    );
    let mut chaine = ChaineTess {
        plugins: vec![
            maillon("equalizer-graphic", true, &g),
            maillon("equalizer-parametric", true, &p),
        ],
        ..ChaineTess::default()
    };
    chaine.fixed.headroom = Headroom::Manual { db: -4.5 };
    let c = profil_de_la_chaine(&chaine, None).unwrap();
    assert_eq!(c.reserve_imposee_db, Some(-4.5));
    assert!((pre(&c) + 4.5).abs() < 1e-12, "{}", pre(&c));
    // Jamais écrite : le profil se relit comme avant.
    let texte = serde_json::to_string(&c).unwrap();
    assert!(!texte.contains("reserve_imposee"), "{texte}");
}

/// −30 dB atteint le moteur (il bornait à −24) ; la hausse garde sa borne.
#[test]
fn les_gains_extremes_atteignent_le_moteur_rc14() {
    let creux = EqProfile {
        enabled: true,
        bands: vec![bande(1000.0, -30.0, 2.0, "peak")],
        ..Default::default()
    };
    let r = courbe_db(&creux, &[1000.0]);
    assert!((r[0][0] + 30.0).abs() < 0.2, "{}", r[0][0]);
    let pic = EqProfile {
        enabled: true,
        bands: vec![bande(1000.0, 40.0, 2.0, "peak")],
        ..Default::default()
    };
    let r = courbe_db(&pic, &[1000.0]);
    assert!((r[0][0] - 24.0).abs() < 0.2, "{}", r[0][0]);
}

#[test]
fn les_gains_sont_bornes_au_put_rc14() {
    let avec = |id: &str, g: f64| ChaineTess {
        plugins: vec![MaillonGreffon {
            id: id.into(),
            enabled: true,
            settings: json!({"bands": [{"freq": 1000.0, "gain": g, "q": 1.0, "type": "peak"}]}),
        }],
        ..ChaineTess::default()
    };
    let vide = ChaineTess::default();
    for (id, ok, refuse) in [
        ("equalizer-parametric", [-30.0, 15.0], [-30.1, 15.1]),
        ("equalizer-headphone", [-30.0, 15.0], [-31.0, 16.0]),
        ("equalizer-graphic", [-12.0, 12.0], [-12.5, 12.5]),
    ] {
        for g in ok {
            verifier_les_gains(&avec(id, g), &vide).unwrap();
        }
        for g in refuse {
            let e = verifier_les_gains(&avec(id, g), &vide).unwrap_err();
            assert_eq!(e.code(), "invalid_settings", "{id} {g}");
            // Inchangé depuis l'enregistrement (ancienne route) : gardé.
            verifier_les_gains(&avec(id, g), &avec(id, g)).unwrap();
        }
    }
    // Le gain d'un passe-haut n'est pas lu.
    let ph = ChaineTess {
        plugins: vec![MaillonGreffon {
            id: "equalizer-parametric".into(),
            enabled: true,
            settings: json!({"bands": [{"freq": 30.0, "gain": -60.0, "q": 0.7, "type": "high_pass"}]}),
        }],
        ..ChaineTess::default()
    };
    verifier_les_gains(&ph, &vide).unwrap();
}

#[test]
fn le_schema_publie_les_bornes_rc14() {
    let mut s: Value = serde_json::from_str(include_str!(
        "../../../sdk/tune-plugin-equalizer/schemas/config.json"
    ))
    .unwrap();
    borner_le_schema("equalizer-parametric", &mut s);
    let g = &s["properties"]["bands"]["items"]["properties"]["gain"];
    assert_eq!(g["minimum"], json!(-30.0));
    assert_eq!(g["maximum"], json!(15.0));
    assert_eq!(
        s["properties"]["bands"]["items"]["required"],
        json!(["freq"])
    );
    let mut s2 = s.clone();
    borner_le_schema("equalizer-graphic", &mut s2);
    let g = &s2["properties"]["bands"]["items"]["properties"]["gain"];
    assert_eq!(
        (g["minimum"].clone(), g["maximum"].clone()),
        (json!(-12.0), json!(12.0))
    );
    let mut cf = json!({"type": "object"});
    borner_le_schema("crossfeed", &mut cf);
    assert_eq!(cf, json!({"type": "object"}));
}

/// L'ordre des familles ne change qu'au déplacement : cocher le repli mono
/// ou un second égaliseur à sa place par défaut ne le change pas.
#[test]
fn l_ordre_des_familles_ne_change_qu_au_deplacement_rc14() {
    let c = |ids: &[(&str, bool)]| ChaineTess {
        plugins: ids
            .iter()
            .map(|(id, on)| MaillonGreffon {
                id: (*id).into(),
                enabled: *on,
                settings: Value::Null,
            })
            .collect(),
        ..ChaineTess::default()
    };
    let defaut = ordre_des_coches(&c(&[("equalizer-graphic", true), ("crossfeed", true)]));
    // Sans convolution cochée, sa place ne compte pas (aucun moteur) :
    // même règle que `OrdreLocal::depuis_ids`.
    assert_eq!(defaut, [0, 2, 1]);
    assert_eq!(
        ordre_des_coches(&c(&[
            ("equalizer-graphic", true),
            ("equalizer-parametric", true),
            ("crossfeed", true),
            ("mono-downmix", true),
        ])),
        defaut
    );
    assert_eq!(ordre_des_coches(&c(&[("crossfeed", false)])), [0, 1, 2]);
    assert_eq!(
        ordre_des_coches(&c(&[("crossfeed", true), ("equalizer-graphic", true)])),
        [2, 0, 1]
    );
}
