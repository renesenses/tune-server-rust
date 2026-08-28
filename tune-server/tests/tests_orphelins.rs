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

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Les chemins `tests/…` déclarés en cible `[[test]]` dans le manifeste.
fn cibles_du_manifeste(racine: &Path) -> BTreeSet<String> {
    let manifeste = fs::read_to_string(racine.join("Cargo.toml")).expect("Cargo.toml lisible");
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

/// Les fichiers tirés en `#[path = "…"] mod …;` par un agrégateur.
fn fichiers_declares_par(agregateur: &Path) -> BTreeSet<String> {
    let source = match fs::read_to_string(agregateur) {
        Ok(s) => s,
        Err(_) => return BTreeSet::new(),
    };
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

#[test]
fn aucun_fichier_de_tests_ne_reste_hors_du_compilateur() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dossier = racine.join("tests");

    // Les fichiers .rs posés à la racine de tests/ — les sous-dossiers
    // (fixtures, modules d'un agrégateur) ne sont pas des cibles.
    let poses: BTreeSet<String> = fs::read_dir(&dossier)
        .expect("tests/ lisible")
        .filter_map(Result::ok)
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".rs"))
        .collect();
    assert!(
        !poses.is_empty(),
        "tests/ ne contient aucun fichier — chemin faux ?"
    );

    let cibles = cibles_du_manifeste(racine);
    assert!(
        !cibles.is_empty(),
        "aucune cible [[test]] lue dans Cargo.toml — le parseur du garde-fou est cassé"
    );

    let mut atteints = cibles.clone();
    for cible in &cibles {
        atteints.extend(fichiers_declares_par(&dossier.join(cible)));
    }

    let orphelins: Vec<&String> = poses.difference(&atteints).collect();
    assert!(
        orphelins.is_empty(),
        "ces fichiers de tests/ ne sont compilés par personne, donc ne tournent jamais : {orphelins:?}\n\
         Enregistre-les, soit en cible [[test]] dans tune-server/Cargo.toml, soit en \
         `#[path = \"…\"] mod …;` dans un agrégateur (server_contracts.rs et consorts)."
    );
}
