//! #4770, point 4 — deux fabrications d'image sur un même hôte ne partagent
//! plus leur dossier de travail (`/tmp/tune-os-build`, vidé par `rm -rf` au
//! démarrage). Les scripts d'image sont du bash : le témoin est
//! `image/test-tune-os-work-dir.sh`, que ce test fait tourner sur toute PR Rust.

#[cfg(unix)]
#[test]
fn les_scripts_d_image_n_ont_plus_de_dossier_de_travail_fixe() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("image")
        .join("test-tune-os-work-dir.sh");
    assert!(script.is_file(), "témoin absent : {}", script.display());
    let sortie = std::process::Command::new("bash")
        .arg(&script)
        .env_remove("TUNE_OS_WORK_DIR")
        .output()
        .expect("bash introuvable");
    assert!(
        sortie.status.success(),
        "#4770 : {}\n{}",
        String::from_utf8_lossy(&sortie.stderr),
        String::from_utf8_lossy(&sortie.stdout)
    );
}
