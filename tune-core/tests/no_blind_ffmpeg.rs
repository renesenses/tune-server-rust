//! Garde-fou #1477 : plus jamais d'appel AVEUGLE à ffmpeg.
//!
//! ffmpeg a été retiré du projet en v0.8.46. Un `Command::new("ffmpeg")` nu
//! cherche un binaire système qui n'existe pas sur une installation neuve —
//! typiquement Windows — et le symptôme n'est pas une erreur : c'est du
//! SILENCE (`state=playing`, position figée, rien dans les logs ; vécu sur
//! .42 le 2026-08-10). Les deux derniers appels ont été sortis par #1485 ;
//! ce test empêche qu'un futur correctif en réintroduise un par réflexe.
//!
//! Le chemin **sanctionné** reste permis : le convertisseur (#1534) résout
//! d'abord le ffmpeg minimal LIVRÉ avec la release, ne retombe sur le PATH
//! qu'ensuite, et échoue avec une erreur nommée quand rien n'existe — voir
//! `tune-server/src/routes/converter.rs` (`run_command(&ffmpeg, …)` sur un
//! chemin résolu). Ce test n'interdit que l'invocation par NOM nu, celle qui
//! rend muet sans rien dire.

use std::path::Path;

/// L'invocation interdite, en deux morceaux pour que ce fichier-ci ne se
/// signale pas lui-même.
fn forbidden() -> String {
    format!("Command::new({}ffmpeg{})", '"', '"')
}

fn scan_dir(dir: &Path, needle: &str, hits: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, needle, hits);
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(src) = std::fs::read_to_string(&path)
        {
            for (i, line) in src.lines().enumerate() {
                if line.contains(needle) {
                    hits.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
    }
}

#[test]
fn no_blind_ffmpeg_invocation_in_sources() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = manifest.parent().expect("tune-core has a parent");
    let needle = forbidden();

    let mut hits = Vec::new();
    for crate_src in ["tune-core/src", "tune-server/src", "tune-cli/src"] {
        scan_dir(&workspace.join(crate_src), &needle, &mut hits);
    }

    assert!(
        hits.is_empty(),
        "appel ffmpeg AVEUGLE réintroduit (#1477) — il rend muet sans erreur \
         sur toute installation sans binaire système. Passer par le ffmpeg \
         minimal livré, résolu par chemin (routes/converter.rs), ou décoder \
         en natif. Sites : {hits:?}"
    );
}
