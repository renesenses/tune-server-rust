//! Un fichier de test posé dans `tests/` et jamais enregistré ne tourne JAMAIS.
//!
//! `tune-server` porte `autotests = false` : Cargo ne ramasse rien tout seul.
//! Un fichier n'est compilé que s'il est une cible `[[test]]` du manifeste, ou
//! s'il est déclaré `mod` par l'un des agrégateurs. Sans cela il reste sur le
//! disque, vert aux yeux de tous, et ne prouve rien.
//!
//! C'est arrivé : `notarisation_bornes.rs` est entré par la #2480 sans être
//! enregistré. Six tests qui gardent la publication du DMG Intel — pas une
//! seule fois compilés, de leur fusion jusqu'à ce garde-fou.
//!
//! Ce test relit le manifeste et les agrégateurs, et refuse tout fichier que
//! le compilateur n'atteindrait pas.
//!
//! #6376 : le garde ne voyait que la RACINE de `tests/`, et ne suivait les
//! `#[path]` que d'un cran. Un `tests/<nom>/main.rs` — la forme que Cargo
//! ramasse seule quand `autotests` est actif —, ou un fichier de plus dans
//! `tests/sendspin/`, restait hors du compilateur sans que rien ne rougisse.
//! Il descend désormais dans les sous-dossiers et suit les chaînes de
//! `#[path]` de proche en proche ; `analyse_detecte_un_orphelin_de_sous_dossier`
//! prouve, sur une arborescence factice, que les deux voient ce qu'ils doivent.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Les chemins `tests/…` déclarés en cible `[[test]]` dans le manifeste.
fn cibles_du_manifeste(manifeste: &str) -> BTreeSet<String> {
    manifeste
        .lines()
        .filter_map(|l| {
            let (_, apres) = l.split_once("path")?;
            let apres = apres.trim_start();
            let apres = apres.strip_prefix('=')?.trim();
            let valeur = apres.strip_prefix('"')?;
            let (valeur, _) = valeur.split_once('"')?;
            valeur.strip_prefix("tests/").map(str::to_owned)
        })
        .collect()
}

/// Les chemins tirés en `#[path = "…"] mod …;` par un fichier de tests.
fn chemins_declares_dans(source: &str) -> Vec<String> {
    source
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let reste = l.strip_prefix("#[path")?.trim_start();
            let reste = reste.strip_prefix('=')?.trim();
            let valeur = reste.strip_prefix('"')?;
            let (valeur, _) = valeur.split_once('"')?;
            Some(valeur.to_owned())
        })
        .collect()
}

/// `relatif` lu depuis le dossier `base` (tous deux relatifs à `tests/`).
/// `None` quand le chemin sort de `tests/` (`../../vendor/…`) : ce n'est plus
/// un fichier que ce garde recense.
fn resoudre(base: &str, relatif: &str) -> Option<String> {
    let mut morceaux: Vec<&str> = base.split('/').filter(|m| !m.is_empty()).collect();
    for morceau in relatif.split('/') {
        match morceau {
            "" | "." => {}
            ".." => {
                morceaux.pop()?;
            }
            m => morceaux.push(m),
        }
    }
    Some(morceaux.join("/"))
}

/// Tous les `.rs` sous `dossier`, sous-dossiers compris, relatifs à `tests/`.
fn fichiers_poses(dossier: &Path, prefixe: &str, poses: &mut BTreeSet<String>) {
    let entrees =
        fs::read_dir(dossier).unwrap_or_else(|e| panic!("{} illisible : {e}", dossier.display()));
    for entree in entrees.filter_map(Result::ok) {
        let Ok(nom) = entree.file_name().into_string() else {
            continue;
        };
        let relatif = format!("{prefixe}{nom}");
        let chemin = entree.path();
        if chemin.is_dir() {
            fichiers_poses(&chemin, &format!("{relatif}/"), poses);
        } else if nom.ends_with(".rs") {
            poses.insert(relatif);
        }
    }
}

/// Les fichiers de `racine/tests/` que le compilateur n'atteint pas : ni cible
/// `[[test]]` de `racine/Cargo.toml`, ni tirés — de proche en proche — par un
/// `#[path]` d'un fichier atteint.
fn orphelins_de(racine: &Path) -> Vec<String> {
    let dossier = racine.join("tests");
    let manifeste = fs::read_to_string(racine.join("Cargo.toml")).expect("Cargo.toml lisible");

    let mut poses = BTreeSet::new();
    fichiers_poses(&dossier, "", &mut poses);
    assert!(
        !poses.is_empty(),
        "tests/ ne contient aucun fichier — chemin faux ?"
    );

    let cibles = cibles_du_manifeste(&manifeste);
    assert!(
        !cibles.is_empty(),
        "aucune cible [[test]] lue dans Cargo.toml — le parseur du garde-fou est cassé"
    );

    let mut atteints = BTreeSet::new();
    let mut pile: Vec<String> = cibles.into_iter().collect();
    while let Some(fichier) = pile.pop() {
        if !atteints.insert(fichier.clone()) {
            continue;
        }
        let Ok(source) = fs::read_to_string(dossier.join(&fichier)) else {
            continue;
        };
        let base = fichier.rsplit_once('/').map_or("", |(debut, _)| debut);
        pile.extend(
            chemins_declares_dans(&source)
                .iter()
                .filter_map(|relatif| resoudre(base, relatif)),
        );
    }

    poses.difference(&atteints).cloned().collect()
}

#[test]
fn aucun_fichier_de_tests_ne_reste_hors_du_compilateur() {
    let orphelins = orphelins_de(Path::new(env!("CARGO_MANIFEST_DIR")));
    assert!(
        orphelins.is_empty(),
        "ces fichiers de tests/ ne sont compilés par personne, donc ne tournent jamais : {orphelins:?}\n\
         Enregistre-les, soit en cible [[test]] dans tune-server/Cargo.toml, soit en \
         `#[path = \"…\"] mod …;` dans un agrégateur (server_contracts.rs et consorts)."
    );
}

/// Le témoin du garde lui-même : une arborescence factice où un fichier de
/// sous-dossier est tiré par une chaîne de deux `#[path]`, et où deux autres
/// — dont un `<nom>/main.rs` — ne le sont par personne. Un garde qui cesserait
/// de descendre dans les sous-dossiers, ou de suivre les chaînes, rougit ici.
#[test]
fn analyse_detecte_un_orphelin_de_sous_dossier() {
    let racine = tune_core::test_scratch::scratch_dir("tests-orphelins-6376");
    let tests = racine.path().join("tests");
    for dossier in ["groupe", "banc_seul"] {
        fs::create_dir_all(tests.join(dossier)).expect("dossier factice");
    }
    let ecrire = |chemin: &str, contenu: &str| {
        fs::write(tests.join(chemin), contenu).expect("fichier factice");
    };
    fs::write(
        racine.path().join("Cargo.toml"),
        "[[test]]\nname = \"agregat\"\npath = \"tests/agregat.rs\"\n",
    )
    .expect("manifeste factice");
    ecrire(
        "agregat.rs",
        "#[path = \"groupe/relais.rs\"]\nmod relais;\n",
    );
    ecrire(
        "groupe/relais.rs",
        "#[path = \"feuille.rs\"]\nmod feuille;\n",
    );
    ecrire("groupe/feuille.rs", "");
    ecrire("groupe/oublie.rs", "");
    ecrire("banc_seul/main.rs", "");

    assert_eq!(
        orphelins_de(racine.path()),
        ["banc_seul/main.rs", "groupe/oublie.rs"],
        "le garde doit voir les orphelins des sous-dossiers, et suivre la chaîne \
         agregat.rs → groupe/relais.rs → groupe/feuille.rs"
    );
}
