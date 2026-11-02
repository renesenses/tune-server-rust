//! Le format d'un thème « Mon thème » et ses deux juges : la FORME (format,
//! version, nom, les 20 couleurs) et la LISIBILITÉ (WCAG AA).
//!
//! Le même format sert au rangement par profil, à l'export et à l'import :
//!
//! ```json
//! { "format": "tune-theme", "version": 1, "name": "Ma nuit bleue",
//!   "colors": { "--v2-bg": "#0B1020", … } }
//! ```
//!
//! `colors` porte EXACTEMENT les jetons de [`JETONS`] — ceux de `V2_JETONS`
//! (tune-web-client, `src/lib/v2Jetons.ts`, #2520) moins `--v2-danger-soft`,
//! qui est un `rgba` dérivé de `--v2-danger` par le client. Une clé manquante
//! ou inconnue refuse le thème entier : un thème à moitié peint laisserait des
//! éléments sans couleur sur tous les appareils du profil.
//!
//! Les règles de lisibilité sont celles de la garde du client
//! (`themeMozaiklabsLogo2520.test.ts`) et de `src/lib/monTheme.ts` : le serveur
//! les rejoue pour qu'un client ancien ou un fichier importé à la main ne
//! puisse pas ranger un thème illisible.

use serde::Serialize;
use serde_json::{Map, Value, json};

/// Valeur de `format`.
pub const FORMAT: &str = "tune-theme";
/// Seule version connue.
pub const VERSION: u64 = 1;
/// Longueur maximale du nom, en caractères.
pub const NOM_MAX: usize = 60;

/// Les 5 fonds sur lesquels le texte doit se lire.
pub const FONDS: [&str; 5] = [
    "--v2-bg",
    "--v2-bg2",
    "--v2-surface",
    "--v2-surface2",
    "--v2-hover",
];

/// Les 20 jetons qu'un thème choisit, dans l'ordre de `V2_JETONS`.
pub const JETONS: [&str; 20] = [
    "--v2-bg",
    "--v2-bg2",
    "--v2-surface",
    "--v2-surface2",
    "--v2-hover",
    "--v2-txt",
    "--v2-txt2",
    "--v2-txt3",
    "--v2-acc1",
    "--v2-acc2",
    "--v2-acc-tint",
    "--v2-on-acc",
    "--v2-line",
    "--v2-line2",
    "--v2-active1",
    "--v2-active2",
    "--v2-player1",
    "--v2-player2",
    "--v2-danger",
    "--v2-danger-bd",
];

/// Ce qui se lit comme du texte : ≥ 4,5:1 sur chacun des [`FONDS`].
pub const TEXTES: [&str; 6] = [
    "--v2-txt",
    "--v2-txt2",
    "--v2-txt3",
    "--v2-acc1",
    "--v2-acc-tint",
    "--v2-danger",
];

/// Seuil du texte (WCAG 1.4.3, AA).
pub const MIN_TEXTE: f64 = 4.5;
/// Seuil d'un remplissage (WCAG 1.4.11).
pub const MIN_REMPLISSAGE: f64 = 3.0;

/// Une paire qui ne se lit pas.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paire {
    pub texte: &'static str,
    pub fond: &'static str,
    /// Le rapport mesuré, arrondi au centième.
    pub ratio: f64,
    pub minimum: f64,
}

/// Pourquoi un thème est refusé.
#[derive(Debug, Clone, PartialEq)]
pub enum Refus {
    /// La forme : `champ` nomme ce qui ne va pas (`format`, `version`,
    /// `name`, `colors`, ou le jeton fautif).
    Invalide { champ: String },
    /// La lisibilité : les paires sous leur seuil.
    Illisible { paires: Vec<Paire> },
}

impl Refus {
    /// Le corps de la réponse 422 : un code stable, jamais une phrase.
    pub fn corps(&self) -> Value {
        match self {
            Refus::Invalide { champ } => json!({"error": "mon_theme.invalide", "champ": champ}),
            Refus::Illisible { paires } => {
                json!({"error": "mon_theme.illisible", "paires": paires})
            }
        }
    }
}

fn invalide(champ: &str) -> Refus {
    Refus::Invalide {
        champ: champ.to_string(),
    }
}

/// `#RRGGBB` → `#RRGGBB` en capitales ; rien d'autre n'est une couleur ici
/// (ni `rgb()`, ni nom, ni forme courte) : la valeur part telle quelle dans
/// une feuille de style chez le client, elle ne doit rien pouvoir y ajouter.
pub fn couleur(v: &Value) -> Option<String> {
    let s = v.as_str()?;
    let hex = s.strip_prefix('#')?;
    if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(format!("#{}", hex.to_ascii_uppercase()))
    } else {
        None
    }
}

/// Le nom, caractères de contrôle retirés et espaces repliés ; 1 à
/// [`NOM_MAX`] caractères.
fn nom(v: &Value) -> Option<String> {
    let brut = v.as_str()?;
    let propre = brut
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let n = propre.chars().count();
    (1..=NOM_MAX).contains(&n).then_some(propre)
}

fn luminance(hex: &str) -> f64 {
    let v = hex.trim_start_matches('#');
    let canal = |i: usize| {
        let x = f64::from(u8::from_str_radix(&v[i..i + 2], 16).unwrap_or(0)) / 255.0;
        if x <= 0.03928 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * canal(0) + 0.7152 * canal(2) + 0.0722 * canal(4)
}

/// Le rapport de contraste WCAG entre deux `#RRGGBB`.
pub fn contraste(a: &str, b: &str) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (h, l) = if la >= lb { (la, lb) } else { (lb, la) };
    (h + 0.05) / (l + 0.05)
}

/// Les paires sous leur seuil. Vide : le thème se lit.
pub fn paires_illisibles(couleurs: &Map<String, Value>) -> Vec<Paire> {
    let c = |n: &str| couleurs.get(n).and_then(Value::as_str).unwrap_or("#000000");
    let mut echecs = Vec::new();
    let mut juger = |texte: &'static str, fond: &'static str, minimum: f64| {
        let r = contraste(c(texte), c(fond));
        if r < minimum {
            echecs.push(Paire {
                texte,
                fond,
                ratio: (r * 100.0).floor() / 100.0,
                minimum,
            });
        }
    };
    for t in TEXTES {
        for f in FONDS {
            juger(t, f, MIN_TEXTE);
        }
    }
    for f in FONDS {
        juger("--v2-acc2", f, MIN_REMPLISSAGE);
    }
    juger("--v2-on-acc", "--v2-acc1", MIN_TEXTE);
    juger("--v2-on-acc", "--v2-acc2", MIN_TEXTE);
    echecs
}

/// Juge un thème et rend sa forme normalisée (couleurs en capitales, nom
/// nettoyé, aucune autre clé).
pub fn valider(v: &Value) -> Result<Value, Refus> {
    let o = v.as_object().ok_or_else(|| invalide("theme"))?;
    if o.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(invalide("format"));
    }
    if o.get("version").and_then(Value::as_u64) != Some(VERSION) {
        return Err(invalide("version"));
    }
    let nom = o
        .get("name")
        .and_then(nom)
        .ok_or_else(|| invalide("name"))?;
    let brutes = o
        .get("colors")
        .and_then(Value::as_object)
        .ok_or_else(|| invalide("colors"))?;
    if let Some(inconnue) = brutes.keys().find(|k| !JETONS.contains(&k.as_str())) {
        return Err(invalide(inconnue));
    }
    let mut couleurs = Map::new();
    for j in JETONS {
        let c = brutes.get(j).and_then(couleur).ok_or_else(|| invalide(j))?;
        couleurs.insert(j.to_string(), Value::String(c));
    }
    let paires = paires_illisibles(&couleurs);
    if !paires.is_empty() {
        return Err(Refus::Illisible { paires });
    }
    Ok(json!({"format": FORMAT, "version": VERSION, "name": nom, "colors": couleurs}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le thème « Mozaiklabs (logo) » de #2520, lisible par construction.
    pub(crate) fn lisible() -> Value {
        json!({
            "format": "tune-theme", "version": 1, "name": "  Logo\u{7}  noir ",
            "colors": {
                "--v2-bg": "#000000", "--v2-bg2": "#080A09", "--v2-surface": "#1F2321",
                "--v2-surface2": "#151817", "--v2-hover": "#101312",
                "--v2-txt": "#ffffff", "--v2-txt2": "#D2D8D5", "--v2-txt3": "#A9B1AD",
                "--v2-acc1": "#2A9F5B", "--v2-acc2": "#28BA9B", "--v2-acc-tint": "#E4F9D8",
                "--v2-on-acc": "#000000", "--v2-line": "#2B302D", "--v2-line2": "#3A413D",
                "--v2-active1": "#0F2A1B", "--v2-active2": "#0A1A12", "--v2-player1": "#050606",
                "--v2-player2": "#000000", "--v2-danger": "#FF8F87", "--v2-danger-bd": "#FF6B6B"
            }
        })
    }

    #[test]
    fn contraste_noir_blanc_vaut_21() {
        assert!((contraste("#000000", "#FFFFFF") - 21.0).abs() < 1e-9);
        assert!((contraste("#777777", "#777777") - 1.0).abs() < 1e-9);
    }

    #[test]
    fn un_theme_lisible_est_normalise() {
        let t = valider(&lisible()).unwrap();
        assert_eq!(t["name"], "Logo noir");
        assert_eq!(t["colors"]["--v2-txt"], "#FFFFFF");
        assert_eq!(t["colors"].as_object().unwrap().len(), JETONS.len());
    }

    #[test]
    fn texte_gris_sur_fond_gris_est_refuse_avec_ses_paires() {
        let mut t = lisible();
        t["colors"]["--v2-txt3"] = json!("#333333");
        match valider(&t) {
            Err(Refus::Illisible { paires }) => {
                assert!(paires.iter().all(|p| p.texte == "--v2-txt3"));
                assert_eq!(paires.len(), FONDS.len());
                assert!(paires.iter().all(|p| p.ratio < MIN_TEXTE));
            }
            autre => panic!("attendu illisible, obtenu {autre:?}"),
        }
    }

    #[test]
    fn le_texte_pose_sur_l_accent_est_juge() {
        let mut t = lisible();
        t["colors"]["--v2-on-acc"] = json!("#2A9F5B");
        let Err(Refus::Illisible { paires }) = valider(&t) else {
            panic!("attendu illisible")
        };
        assert!(
            paires
                .iter()
                .any(|p| p.texte == "--v2-on-acc" && p.fond == "--v2-acc1")
        );
    }

    #[test]
    fn la_forme_est_jugee_avant_la_lisibilite() {
        let cas: [(&str, Value); 6] = [
            ("format", json!("autre")),
            ("version", json!(2)),
            ("name", json!("")),
            ("name", json!("x".repeat(NOM_MAX + 1))),
            ("colors", json!([])),
            ("format", Value::Null),
        ];
        for (champ, valeur) in cas {
            let mut t = lisible();
            t[champ] = valeur;
            assert_eq!(valider(&t), Err(invalide(champ)), "{champ}");
        }
    }

    #[test]
    fn une_couleur_qui_n_est_pas_rrggbb_est_refusee_par_son_jeton() {
        for mauvaise in [
            json!("#FFF"),
            json!("red"),
            json!("#12345G"),
            json!("#000000;}body{x:y"),
            json!(0),
        ] {
            let mut t = lisible();
            t["colors"]["--v2-bg2"] = mauvaise.clone();
            assert_eq!(valider(&t), Err(invalide("--v2-bg2")), "{mauvaise}");
        }
    }

    #[test]
    fn un_jeton_manquant_ou_inconnu_refuse_le_theme() {
        let mut t = lisible();
        t["colors"].as_object_mut().unwrap().remove("--v2-line");
        assert_eq!(valider(&t), Err(invalide("--v2-line")));
        let mut t = lisible();
        t["colors"]["--v2-danger-soft"] = json!("#FF0000");
        assert_eq!(valider(&t), Err(invalide("--v2-danger-soft")));
    }

    #[test]
    fn les_cles_en_trop_ne_sont_pas_rangees() {
        let mut t = lisible();
        t["autre"] = json!("x");
        let n = valider(&t).unwrap();
        assert!(n.get("autre").is_none());
    }
}
