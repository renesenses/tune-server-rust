//! Garde-fou : `tune-core` doit rester constructible HORS de ce dépôt.
//!
//! # Le défaut, et pourquoi il était invisible ici
//!
//! Le manifeste racine portait :
//!
//! ```toml
//! [patch.crates-io]
//! rust_cast = { path = "vendor/rust_cast" }
//! ```
//!
//! Or **un `[patch]` n'est honoré que depuis le manifeste racine du workspace
//! qu'on CONSTRUIT.** Chez nous, tout compile : on construit depuis la racine.
//! Chez un consommateur externe — un binaire composeur qui dépend de
//! `tune-core` par git ou par chemin, c'est-à-dire exactement le montage que
//! `docs/plugins/README.md` recommande — cargo ignore ce bloc et résout le vrai
//! `rust_cast` 0.21.0 de crates.io. Celui-ci n'a pas
//! `connect_without_host_verification_with_deadline`, que `chromecast.rs`
//! appelle trois fois, et le build casse sur un `E0599` qui ne nomme ni le
//! patch ni le vendor.
//!
//! Mesuré le 07/09/2026 : le partenaire Diretta est resté bloqué dessus de la
//! **0.9.114 à la 0.9.140**, en croyant à un problème de son côté. Le défaut
//! n'était pas propre à Diretta : il cassait la voie documentée pour TOUT
//! fournisseur de sortie hors dépôt.
//!
//! # Pourquoi un test sur le manifeste, et pas sur le code
//!
//! Aucune porte de ce dépôt ne pouvait voir le défaut : `cargo check`,
//! `clippy`, `cargo test` s'exécutent tous depuis la racine, là où le `[patch]`
//! s'applique. Le seul endroit où le défaut existe est **hors** du dépôt.
//!
//! La reproduction fidèle demanderait de construire une caisse externe dans un
//! test — un clone, un réseau, plusieurs minutes. Ce test lit la CAUSE à la
//! place : un `[patch.crates-io]` sur une caisse dont `tune-core` dépend. Elle
//! est textuelle, elle tient dans le manifeste, et elle a un motif de panne
//! unique.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use std::fs;
use std::path::{Path, PathBuf};

fn racine() -> PathBuf {
    // CARGO_MANIFEST_DIR = tune-server/, quel que soit le répertoire courant.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn lire(chemin: &Path) -> String {
    fs::read_to_string(chemin)
        .unwrap_or_else(|e| panic!("lecture de {} impossible : {e}", chemin.display()))
}

/// Les noms de caisses cités dans `[patch.crates-io]` du manifeste racine.
///
/// Lecture bornée à CETTE table : un `[patch."https://…"]` viserait une autre
/// source et ne se comporte pas pareil, et les tables qui suivent
/// (`[profile.dev]`…) n'ont rien à voir.
fn caisses_patchees(manifeste: &str) -> Vec<String> {
    let mut dans_patch = false;
    let mut noms = Vec::new();
    for ligne in manifeste.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            dans_patch = t == "[patch.crates-io]";
            continue;
        }
        if !dans_patch || t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((nom, _)) = t.split_once('=') {
            noms.push(nom.trim().trim_matches('"').to_string());
        }
    }
    noms
}

/// ⭐ LA GARDE — aucune dépendance de `tune-core` ne doit reposer sur un
/// `[patch.crates-io]` de la racine.
///
/// Un `[patch]` est légitime pour un outil, un test, une dépendance de
/// développement : rien de tout cela n'est reconstruit par un consommateur
/// externe. Il ne l'est PAS pour une caisse que `tune-core` compile, parce que
/// `tune-core` est, lui, une dépendance publique de nos binaires composeurs.
#[test]
fn aucune_dependance_de_tune_core_ne_depend_d_un_patch_racine() {
    let racine = racine();
    let manifeste_racine = lire(&racine.join("Cargo.toml"));
    let manifeste_core = lire(&racine.join("tune-core/Cargo.toml"));

    let patchees = caisses_patchees(&manifeste_racine);

    for caisse in &patchees {
        // `tune-core` cite la caisse soit directement, soit par héritage
        // (`nom = { workspace = true }`) — dans les deux cas son nom apparaît
        // en début de ligne dans son manifeste.
        let citee = manifeste_core
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .any(|l| {
                l.strip_prefix(caisse.as_str())
                    .is_some_and(|reste| reste.trim_start().starts_with('='))
            });
        assert!(
            !citee,
            "`{caisse}` est corrigée par `[patch.crates-io]` à la racine ET \
             compilée par tune-core.\n\
             \n\
             Un `[patch]` n'est honoré que depuis la racine du workspace qu'on \
             CONSTRUIT. Un binaire composeur qui dépend de tune-core par git ou \
             par chemin — la voie que docs/plugins/README.md recommande — ne le \
             voit pas, résout la version de crates.io, et casse sur une méthode \
             manquante.\n\
             \n\
             Le remède est une dépendance `path` INTERNE au dépôt : elle, le \
             consommateur la résout, puisqu'il a cloné tout l'arbre.\n\
             \n\
             Bloqué le partenaire Diretta de la 0.9.114 à la 0.9.140."
        );
    }
}

/// La déclaration d'une caisse dans une table nommée du manifeste.
///
/// La table COMPTE, et c'est tout le sujet : `rust_cast = { path = … }` est
/// juste sous `[workspace.dependencies]` et faux sous `[patch.crates-io]`,
/// alors que le texte est identique au caractère près. Une recherche par
/// `contains` sur le manifeste entier confondrait les deux — et resterait
/// verte contre exactement le défaut qu'on corrige ici.
fn declaration_dans(manifeste: &str, table: &str, caisse: &str) -> Option<String> {
    let mut dans_table = false;
    for ligne in manifeste.lines() {
        let t = ligne.trim();
        if t.starts_with('[') {
            dans_table = t == table;
            continue;
        }
        if !dans_table || t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(reste) = t.strip_prefix(caisse)
            && reste.trim_start().starts_with('=')
        {
            return Some(t.to_string());
        }
    }
    None
}

/// TÉMOIN — le fork rust_cast est toujours atteint, et par un chemin déclaré
/// dans `[workspace.dependencies]`.
///
/// Sans lui, la garde ci-dessus deviendrait vraie **en supprimant le fork** :
/// plus de `[patch]`, plus rien à vérifier, et le correctif #1185 (certificats
/// X.509 v1 des vieux Chromecast) disparaîtrait en silence. Une garde qu'on
/// satisfait en retirant la fonctionnalité n'en est pas une.
#[test]
fn le_fork_rust_cast_est_atteint_par_un_chemin_interne() {
    let racine = racine();
    let manifeste = lire(&racine.join("Cargo.toml"));

    let declaration = declaration_dans(&manifeste, "[workspace.dependencies]", "rust_cast").expect(
        "rust_cast n'est plus déclarée dans `[workspace.dependencies]` : \
             le correctif #1185 (certificats X.509 v1) ne serait plus compilé, \
             ou serait revenu à un `[patch]` invisible hors du dépôt",
    );
    assert!(
        declaration.contains(r#"path = "vendor/rust_cast""#),
        "rust_cast n'est plus atteinte par un chemin interne — c'est LUI, et \
         non un `[patch]`, qu'un consommateur hors dépôt sait résoudre.\n\
         déclaration : {declaration}"
    );
    assert!(
        racine.join("vendor/rust_cast/src/lib.rs").exists(),
        "vendor/rust_cast a disparu de l'arbre"
    );

    // Et la méthode qui n'existe QUE dans le fork est toujours celle que le
    // code appelle : c'est elle qui fabriquait le `E0599` chez le partenaire.
    let fork = lire(&racine.join("vendor/rust_cast/src/lib.rs"));
    let chromecast = lire(&racine.join("tune-core/src/outputs/chromecast.rs"));
    const METHODE: &str = "connect_without_host_verification_with_deadline";
    assert!(
        fork.contains(METHODE),
        "le fork ne porte plus `{METHODE}` — crates.io ne l'a pas non plus"
    );
    assert!(
        chromecast.contains(METHODE),
        "chromecast.rs n'appelle plus `{METHODE}` : si le fork n'est plus \
         nécessaire, retirez-le entièrement plutôt que de le laisser dériver"
    );
}

/// `vendor/rust_cast` reste HORS du workspace.
///
/// Une dépendance `path` interne devient membre du workspace toute seule — le
/// manifeste de la CI le dit de `tune-output-api`. Membre, ce fork tiers
/// passerait sous `cargo fmt --all` (réécriture d'un code qui n'est pas le
/// nôtre, et diff permanent avec l'amont) et sous la porte clippy, que la
/// garde `toute_caisse_du_workspace_est_nommee_par_une_porte_clippy`
/// exigerait alors de compléter.
#[test]
fn le_fork_tiers_reste_exclu_du_workspace() {
    let manifeste = lire(&racine().join("Cargo.toml"));
    let ligne = manifeste
        .lines()
        .find(|l| l.trim_start().starts_with("exclude"))
        .expect("le manifeste racine doit porter une clé `exclude`");
    assert!(
        ligne.contains("vendor/rust_cast"),
        "vendor/rust_cast n'est plus exclu : il entre dans `cargo fmt --all` \
         et dans la porte clippy, sur du code tiers.\nligne : {ligne}"
    );
}
