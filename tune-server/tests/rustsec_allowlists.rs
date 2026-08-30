//! Garde-fou : un avis RustSec ne peut plus apparaitre en silence (#2251).
//!
//! Le depot tient TROIS listes d'exceptions RustSec, lues par trois outils
//! differents :
//!
//! | fichier | lu par |
//! |---|---|
//! | `deny.toml` | `cargo deny check advisories` (preflight de release) |
//! | `.cargo/audit.toml` | `cargo audit` nu (preflight de release) |
//! | `.github/workflows/ci.yml` | l'action `actions-rust-lang/audit` (CI de PR) |
//!
//! Elles ont derive, et la derive a coute exactement ce qu'elle promettait :
//! RUSTSEC-2026-0150 (`audiopus_sys` abandonne, 21/05/2026) n'existait que dans
//! `deny.toml`. Les deux autres outils ne le connaissaient pas — et ne le
//! signalaient pas non plus, parce qu'un avis `informational = "unmaintained"`
//! est, par defaut, affiche puis sorti en 0. Mesure sur `release/v0.9`
//! (e608df16) : « warning: 3 allowed warnings found », code de sortie 0.
//!
//! Ce fichier verrouille les deux moities du correctif :
//!
//! 1. les trois listes contiennent exactement les memes identifiants ;
//! 2. les deux portes `cargo audit` refusent bien les avis informatifs
//!    (`deny = ["warnings"]` et `denyWarnings: true`) — sans quoi la liste
//!    d'exceptions ne servirait a rien, puisque rien ne bloquerait.
//!
//! Contre-epreuve faite a l'ecriture : en retirant RUSTSEC-2026-0150 de
//! `.cargo/audit.toml`, `cargo audit` rend « error: 1 denied warning found! »
//! et sort en 1.
//!
//! Suite : RUSTSEC-2026-0150 a depuis quitte les trois listes, non par
//! exception mais parce que la dependance a disparu — `audiopus`/`audiopus_sys`
//! remplaces par `opus`/`opusic-sys`, maintenus (#2251). Ce test-ci n'en est
//! pas affecte : il compare les listes entre elles, quel qu'en soit le
//! contenu.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn racine() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("tune-server a un parent")
        .to_path_buf()
}

fn lire(chemin: &str) -> String {
    let p = racine().join(chemin);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{} illisible : {e}", p.display()))
}

/// Identifiants RustSec cites dans une ligne, hors commentaire.
///
/// Les trois fichiers citent des identifiants DANS LEURS COMMENTAIRES — pour
/// justifier une exception, ou pour rappeler qu'un avis a ete resolu et retire
/// (RUSTSEC-2026-0222/0223 dans `deny.toml`). Les ramasser rendrait ce test
/// vert sur une liste fausse : on coupe donc chaque ligne a son `#` avant de
/// chercher.
fn ids_hors_commentaire(ligne: &str, prefixe_commentaire: char) -> Vec<String> {
    let code = match ligne.find(prefixe_commentaire) {
        Some(i) => &ligne[..i],
        None => ligne,
    };
    let mut trouves = Vec::new();
    let mut i = 0;
    while let Some(pos) = code[i..].find("RUSTSEC-") {
        let debut = i + pos;
        // RUSTSEC-AAAA-NNNN : 8 + 4 + 1 + 4 = 17 octets, tous ASCII. Le
        // `is_char_boundary` protege le decoupage quand un accent suit
        // immediatement (les commentaires de deny.toml en sont pleins).
        let fin = debut + 17;
        if fin <= code.len() && code.is_char_boundary(fin) {
            let candidat = &code[debut..fin];
            let reste: Vec<char> = candidat.chars().skip(8).collect();
            let bien_forme = reste.len() == 9
                && reste[4] == '-'
                && reste
                    .iter()
                    .enumerate()
                    .all(|(k, c)| k == 4 || c.is_ascii_digit());
            if bien_forme {
                trouves.push(candidat.to_string());
            }
        }
        i = debut + 8;
    }
    trouves
}

/// Retire les commentaires ligne a ligne.
///
/// Indispensable AVANT de chercher `[advisories]` : les deux fichiers TOML
/// citent cette section dans leur en-tete explicative, et un lecteur naif
/// s'ancre sur le commentaire puis ne trouve plus rien — il rend alors une
/// liste vide, c'est-a-dire un faux verdict. (Aucune chaine TOML de ces deux
/// fichiers ne contient de `#`.)
fn sans_commentaires(source: &str) -> String {
    source
        .lines()
        .map(|l| match l.find('#') {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Contenu du tableau `ignore = [ ... ]` de la section `[advisories]`.
fn ignore_toml(source: &str, fichier: &str) -> BTreeSet<String> {
    let source = &sans_commentaires(source);
    let debut_section = source
        .find("[advisories]")
        .unwrap_or_else(|| panic!("{fichier} : section [advisories] absente"));
    let reste = &source[debut_section..];
    let debut_tableau = reste
        .find("ignore")
        .unwrap_or_else(|| panic!("{fichier} : clef `ignore` absente de [advisories]"));
    let apres = &reste[debut_tableau..];
    let fin = apres
        .find(']')
        .unwrap_or_else(|| panic!("{fichier} : tableau `ignore` non ferme"));
    apres[..fin]
        .lines()
        .flat_map(|l| ids_hors_commentaire(l, '#'))
        .collect()
}

/// Contenu de l'entree `ignore:` du job `audit` de `ci.yml`.
fn ignore_ci(source: &str) -> BTreeSet<String> {
    let debut = source
        .find("actions-rust-lang/audit@")
        .expect("ci.yml : l'etape actions-rust-lang/audit a disparu");
    let bloc = &source[debut..];
    let ligne = bloc
        .lines()
        .find(|l| l.trim_start().starts_with("ignore:"))
        .expect("ci.yml : l'etape audit n'a plus d'entree `ignore:`");
    ids_hors_commentaire(ligne, '#').into_iter().collect()
}

#[test]
fn les_trois_listes_dexceptions_rustsec_sont_identiques() {
    let deny = ignore_toml(&lire("deny.toml"), "deny.toml");
    let audit = ignore_toml(&lire(".cargo/audit.toml"), ".cargo/audit.toml");
    let ci = ignore_ci(&lire(".github/workflows/ci.yml"));

    assert!(
        !deny.is_empty(),
        "deny.toml : liste `ignore` vide — le lecteur de ce test s'est casse, \
         pas la configuration"
    );

    for (nom_a, a, nom_b, b) in [
        ("deny.toml", &deny, ".cargo/audit.toml", &audit),
        ("deny.toml", &deny, "ci.yml", &ci),
    ] {
        assert_eq!(
            a,
            b,
            "les listes d'exceptions RustSec ont derive.\n\
             {nom_a} : {a:?}\n\
             {nom_b} : {b:?}\n\
             Manquant dans {nom_b} : {:?}\n\
             En trop dans {nom_b} : {:?}\n\
             C'est exactement cette derive qui a rendu RUSTSEC-2026-0150 \
             invisible de mai a aout 2026 (#2251). Toute exception s'inscrit \
             dans les TROIS fichiers, avec sa raison et sa date.",
            a.difference(b).collect::<Vec<_>>(),
            b.difference(a).collect::<Vec<_>>(),
        );
    }
}

#[test]
fn les_deux_portes_cargo_audit_refusent_les_avis_informatifs() {
    let sans_commentaires = sans_commentaires(&lire(".cargo/audit.toml"));
    assert!(
        sans_commentaires.contains("[output]"),
        ".cargo/audit.toml : section [output] absente — sans elle `cargo audit` \
         affiche les avis « unmaintained » puis sort en 0, et la liste \
         d'exceptions ne bloque rien (#2251)."
    );
    assert!(
        sans_commentaires.contains("deny") && sans_commentaires.contains("\"warnings\""),
        ".cargo/audit.toml : `deny = [\"warnings\"]` a disparu de [output]. \
         Le preflight de release lance `cargo audit` nu : sans cette ligne, un \
         nouvel avis informatif repasse en silence (#2251)."
    );

    let ci = lire(".github/workflows/ci.yml");
    let debut = ci
        .find("actions-rust-lang/audit@")
        .expect("ci.yml : l'etape actions-rust-lang/audit a disparu");
    // L'etape s'arrete au prochain job (une clef a deux espaces d'indentation).
    let bloc = &ci[debut..];
    let fin = bloc
        .find("\n  ffi:")
        .unwrap_or_else(|| bloc.len().min(1_500));
    let etape = &bloc[..fin];
    assert!(
        etape.contains("denyWarnings: true"),
        "ci.yml : l'etape `audit` n'a plus `denyWarnings: true`. Par defaut \
         l'action laisse passer les avis « unmaintained » — c'est ainsi que \
         RUSTSEC-2026-0150 est reste invisible trois mois (#2251)."
    );
}
