//! Garde de non-régression du découpeur d'artistes, contre un corpus RÉEL.
//!
//! Corpus : les 594 noms d'artistes uniques du Core Roon d'un testeur, relevés
//! le 12/09/2026 sur une bibliothèque de 15 196 morceaux.
//!
//! Roon a **déjà** consolidé ces noms — c'est son travail d'identification.
//! L'attendu est donc sans ambiguïté : **aucun ne doit être découpé**. Chaque
//! découpage est un faux positif, et chacun fabrique dans Tune une ligne
//! d'artiste orpheline, sans MBID ni image.
//!
//! Mesure d'origine (main @ 826d5667) :
//!   - `split_risky = false` → 0 / 594 — mais **sans valeur de preuve** : ce mode
//!     ne traite que `;` et `feat`, absents du corpus. Voir le premier test.
//!   - `split_risky = true`  → **8 / 594** ← c'est là que se joue la garde.

use tune_core::metadata::artist_split::analyze_artist_credit;

const CORPUS: &str = include_str!("fixtures/corpus_artistes_roon.json");

/// Les huit noms que le mode risqué découpe aujourd'hui. Deux sont défendables
/// (`Jimmy Page & Robert Plant`, `Dave Gahan & Soulsavers` sont de vraies
/// collaborations) ; les six autres sont des noms de groupe indivisibles.
/// Cette liste est un CLIQUET : elle ne doit que rétrécir.
const RISQUE_CONNUS: &[&str] = &[
    "Arms and Sleepers",
    "Dave Gahan & Soulsavers",
    "Eko & Vinda Folio",
    "Grover Washington, Jr.",
    "Iron & Wine",
    "Jimmy Page & Robert Plant",
    "Medeski, Martin & Wood",
    "Secos & Molhados",
];

fn noms() -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(CORPUS).expect("corpus illisible");
    let mut out = Vec::new();
    for cle in ["propres", "a_separateur_mais_entiers"] {
        for n in v[cle].as_array().expect("tableau attendu") {
            out.push(n.as_str().expect("chaîne attendue").to_string());
        }
    }
    assert_eq!(out.len(), 594, "le corpus a changé de taille");
    out
}

/// ⚠️ CE TEST NE GARDE PAS LE DÉCOUPEUR — il décrit le CORPUS.
///
/// En mode par défaut (`split_risky = false`) le découpeur ne traite que `;` et
/// les marqueurs `feat` ; il ne touche ni la virgule ni l'esperluette. Or ce
/// corpus n'en contient aucun. Un test « le mode par défaut ne découpe rien »
/// serait donc vert PAR CONSTRUCTION — vérifié par contre-épreuve : en
/// neutralisant `is_allowlisted`, il restait vert quand l'autre tombait.
///
/// On affirme donc ce qui est vrai et utile : le corpus est exempt de
/// séparateurs forts. Si un jour il en gagne un, la garde du mode risqué
/// ci-dessous cesse d'être le seul juge, et il faudra y revenir.
#[test]
fn le_corpus_ne_contient_aucun_separateur_fort() {
    let fautifs: Vec<String> = noms()
        .into_iter()
        .filter(|n| n.contains(';') || n.to_lowercase().contains("feat"))
        .collect();

    assert!(
        fautifs.is_empty(),
        "le corpus contient {} nom(s) à séparateur fort : {:#?}",
        fautifs.len(),
        fautifs
    );
}

/// ⭐ LA garde. Elle exerce virgule, esperluette et liste blanche sur 594 noms
/// réels. Contre-épreuve faite : en neutralisant `is_allowlisted`, elle tombe.
#[test]
fn mode_risque_ne_regresse_pas() {
    let fautifs: Vec<String> = noms()
        .into_iter()
        .filter(|n| analyze_artist_credit(n, &[], true).would_split())
        .collect();

    let nouveaux: Vec<&String> = fautifs
        .iter()
        .filter(|n| !RISQUE_CONNUS.contains(&n.as_str()))
        .collect();

    assert!(
        nouveaux.is_empty(),
        "le mode risqué découpe {} nom(s) de plus qu'avant : {:#?}",
        nouveaux.len(),
        nouveaux
    );

    // Le cliquet se resserre : si un cas connu est corrigé, retirer son entrée.
    let disparus: Vec<&&str> = RISQUE_CONNUS
        .iter()
        .filter(|c| !fautifs.iter().any(|n| n == *c))
        .collect();
    assert!(
        disparus.is_empty(),
        "ces cas ne sont plus découpés : retirez-les de RISQUE_CONNUS — {:#?}",
        disparus
    );
}
