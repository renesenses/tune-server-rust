//! La CI et le SDK exécutent la même vérification du dépôt.
use std::{path::Path, process::Command};

#[test]
fn catalogue_prive_absent_du_depot() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python = if cfg!(windows) { "python" } else { "python3" };
    let statut = Command::new(python)
        .current_dir(racine)
        .arg("scripts/verifier-catalogue.py")
        .status()
        .expect("exécuter la garde du catalogue");
    assert!(statut.success());
}
