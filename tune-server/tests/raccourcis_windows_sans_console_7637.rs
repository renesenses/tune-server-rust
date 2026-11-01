//! #7637 — sous Windows, l'entrée principale « Tune Server » du menu Démarrer
//! et le raccourci du Bureau doivent lancer le serveur SANS fenêtre de
//! console (`--sans-console`) ; seule l'entrée « avec console, dépannage »
//! garde la console.
//!
//! Le script NSIS n'existe que dans un heredoc de `.github/workflows/release.yml`.
//! Ce test le relit tel quel. Aucun Windows n'est requis.

use std::fs;
use std::path::Path;

fn workflow() -> String {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("racine");
    fs::read_to_string(racine.join(".github/workflows/release.yml")).expect("release.yml")
}

/// Lignes `CreateShortcut` dont la cible visée par `chemin_lnk` est exactement ce raccourci.
fn raccourci(yml: &str, lnk: &str) -> Option<String> {
    yml.lines()
        .map(str::trim)
        .find(|l| l.starts_with("CreateShortcut") && l.contains(lnk))
        .map(str::to_string)
}

/// Contenu du lanceur `.bat` écrit par l'installeur (lignes `FileWrite`).
fn contenu_bat(yml: &str, nom: &str) -> String {
    let debut = yml
        .find(&format!("\\{nom}\" w"))
        .unwrap_or_else(|| panic!("lanceur {nom} absent"));
    let fin = yml[debut..].find("FileClose").expect("FileClose") + debut;
    yml[debut..fin].to_string()
}

#[test]
fn bureau_et_entree_principale_lancent_sans_console() {
    let yml = workflow();
    for lnk in [
        "Tune Server\\Tune Server.lnk",
        "$DESKTOP\\Tune Server.lnk",
    ] {
        let l = raccourci(&yml, lnk).unwrap_or_else(|| panic!("raccourci {lnk} absent"));
        assert!(
            l.contains("start-tune-server-sans-console.bat"),
            "{lnk} doit viser le lanceur sans console : {l}"
        );
        assert!(!l.contains("\\start-tune-server.bat"), "{lnk} : {l}");
    }
}

#[test]
fn entree_de_depannage_garde_la_console() {
    let yml = workflow();
    let l = raccourci(&yml, "avec console, depannage).lnk").expect("entrée de dépannage");
    assert!(l.contains("\\start-tune-server.bat"), "{l}");
    assert!(!l.contains("sans-console"), "{l}");
}

#[test]
fn contre_epreuve_les_deux_lanceurs_different_par_le_drapeau() {
    let yml = workflow();
    let sans = contenu_bat(&yml, "start-tune-server-sans-console.bat");
    let avec = contenu_bat(&yml, "start-tune-server.bat");
    assert!(sans.contains("tune-server.exe --sans-console"), "{sans}");
    assert!(!avec.contains("--sans-console"), "{avec}");
    assert!(avec.contains("tune-server.exe$"), "{avec}");
}

#[test]
fn le_journal_reste_ouvrable_et_l_ancien_raccourci_est_retire() {
    let yml = workflow();
    let j = raccourci(&yml, "Journal de Tune Server.lnk").expect("raccourci du journal");
    assert!(j.contains("notepad.exe") && j.contains("tune-server.log"), "{j}");
    assert!(
        yml.contains("Delete \"`$SMPROGRAMS\\Tune Server\\Tune Server (sans console).lnk\""),
        "l'ancien raccourci doit être supprimé à la mise à jour"
    );
    assert!(raccourci(&yml, "Tune Server (sans console).lnk").is_none());
}
