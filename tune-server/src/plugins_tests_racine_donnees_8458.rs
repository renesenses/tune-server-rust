//! #8458 — « Ma collection physique » : « Le serveur n'a pas pu enregistrer
//! les données » (fil 2660). Les greffons rangeaient leur état sous
//! `plugins/data` RELATIF au répertoire courant ; l'application macOS lance
//! le serveur depuis `/`, en lecture seule.

use std::path::{Path, PathBuf};

use super::racine_des_donnees_de_greffons as racine;

/// Un répertoire courant en lecture seule, comme `/` pour un service lancé
/// par launchd : le symptôme du fil 2660.
#[cfg(unix)]
#[test]
fn un_repertoire_courant_en_lecture_seule_ne_prive_plus_les_greffons_d_ecriture_8458() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("racine-en-lecture-seule");
    std::fs::create_dir(&cwd).unwrap();
    let donnees = tmp.path().join("Application Support").join("Tune");
    std::fs::create_dir_all(&donnees).unwrap();
    let base = donnees.join("tune.db");
    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o555)).unwrap();

    let r = racine(None, None, None, &base.to_string_lossy(), &cwd);
    let dossier_du_greffon = r.join("collection-physique");
    let ecrit = std::fs::create_dir_all(&dossier_du_greffon)
        .and_then(|_| std::fs::write(dossier_du_greffon.join("memoire.json"), b"{}"));

    std::fs::set_permissions(&cwd, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        ecrit.is_ok(),
        "#8458 — le greffon doit pouvoir écrire son état ({}) : {ecrit:?}",
        dossier_du_greffon.display()
    );
    assert_eq!(r, donnees.join("plugins-data"));
}

#[test]
fn sans_variable_la_racine_suit_le_dossier_de_la_base_8458() {
    let tmp = tempfile::tempdir().unwrap();
    let cwd = tmp.path().join("ailleurs");
    std::fs::create_dir(&cwd).unwrap();
    let base = tmp.path().join("donnees").join("tune.db");
    assert_eq!(
        racine(None, None, None, &base.to_string_lossy(), &cwd),
        tmp.path().join("donnees").join("plugins-data")
    );
    // `TUNE_DATA_DIR` décide du dossier de données comme pour la base.
    let d = tmp.path().join("var-tune");
    assert_eq!(
        racine(
            None,
            None,
            Some(d.to_string_lossy().into_owned()),
            &base.to_string_lossy(),
            &cwd
        ),
        d.join("plugins-data")
    );
}

#[test]
fn un_plugins_data_existant_dans_le_repertoire_courant_est_garde_8458() {
    let tmp = tempfile::tempdir().unwrap();
    let historique = tmp.path().join("plugins").join("data");
    std::fs::create_dir_all(&historique).unwrap();
    let base = tmp.path().join("autre").join("tune.db");
    assert_eq!(
        racine(None, None, None, &base.to_string_lossy(), tmp.path()),
        historique,
        "une installation qui range déjà son état là ne le perd pas"
    );
}

#[test]
fn les_variables_d_environnement_priment_8458() {
    let cwd = Path::new("/nulle-part");
    assert_eq!(
        racine(
            Some("/opt/d".into()),
            Some("/opt/p".into()),
            None,
            "/x/tune.db",
            cwd
        ),
        PathBuf::from("/opt/d")
    );
    assert_eq!(
        racine(None, Some("/opt/p".into()), None, "/x/tune.db", cwd),
        PathBuf::from("/opt/p/data")
    );
}

#[test]
fn une_base_en_memoire_garde_l_ancien_dossier_8458() {
    let cwd = Path::new("/nulle-part");
    assert_eq!(
        racine(None, None, None, ":memory:", cwd),
        cwd.join("plugins").join("data")
    );
    assert_eq!(
        racine(None, None, None, "", cwd),
        cwd.join("plugins").join("data")
    );
}
