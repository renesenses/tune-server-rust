//! #8944 (suite) — un démon qui n'a pas suivi la mise à jour se lit dans le
//! rapport, sans déduction.
use super::*;
use std::time::{Duration, SystemTime};

fn poser(dir: &Path, nom: &str, contenu: &[u8], modifie: SystemTime) {
    let chemin = dir.join(nom);
    std::fs::write(&chemin, contenu).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&chemin)
        .unwrap()
        .set_modified(modifie)
        .unwrap();
}

/// ⭐ Le cas du fil 2878 : `tune-server` neuf, `airplay-daemon` de la première
/// installation. Le rapport le signale, avec sa date et son empreinte.
#[test]
fn un_demon_plus_ancien_que_le_serveur_est_signale_8944() {
    let dir = tune_core::test_scratch::scratch_dir("executables-livres-8944-ancien");
    let maintenant = SystemTime::now();
    poser(&dir, "tune-server", b"serveur neuf", maintenant);
    poser(
        &dir,
        "airplay-daemon",
        b"demon d'avant #8347",
        maintenant - Duration::from_secs(40 * 86_400),
    );

    let releves = relever(&dir.join("tune-server"), &dir, "");
    let md = section_markdown(&releves, None);

    let ligne = md
        .lines()
        .find(|l| l.starts_with("- airplay-daemon:"))
        .expect("ligne airplay-daemon");
    assert!(
        ligne.contains("older than tune-server by 40 day(s)"),
        "#8944 : un démon de 40 jours plus ancien que le serveur doit être signalé : {ligne}"
    );
    // L'empreinte est celle du contenu (comparable à l'archive de release).
    let attendue: String = Sha256::digest(b"demon d'avant #8347")
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(ligne.contains(&format!("sha256 {attendue}")), "{ligne}");
    // Seize chiffres : le filet de confidentialité ne la prend pas pour une
    // adresse MAC (douze chiffres en fin de mot).
    let mut rapport = serde_json::json!({ "markdown": md.clone() });
    tune_core::confidentialite::anonymiser_json(&mut rapport);
    assert!(
        rapport["markdown"].as_str().unwrap().contains(&attendue),
        "#8944 : l'empreinte doit survivre à l'anonymisation du rapport"
    );
    // Un exécutable livré absent est dit absent, pas omis.
    assert!(md.contains("- librespot: absent"), "{md}");
    assert!(md.contains("- ffmpeg: absent"), "{md}");
}

/// Contre-épreuve : un démon posé avec le serveur (même mise à jour) n'est
/// pas signalé — le ⚠ n'est pas collé à toute ligne.
#[test]
fn un_demon_pose_avec_le_serveur_n_est_pas_signale_8944() {
    let dir = tune_core::test_scratch::scratch_dir("executables-livres-8944-a-jour");
    let maintenant = SystemTime::now();
    poser(&dir, "tune-server", b"serveur", maintenant);
    poser(
        &dir,
        "airplay-daemon",
        b"demon",
        maintenant - Duration::from_secs(600),
    );
    poser(&dir, "librespot", b"librespot", maintenant);

    let releves = relever(&dir.join("tune-server"), &dir, "");
    let md = section_markdown(&releves, None);

    assert!(!md.contains('⚠'), "#8944 : rien à signaler ici : {md}");
    assert!(md.contains("- airplay-daemon: "), "{md}");
    assert!(md.contains("- librespot: "), "{md}");
    let j = json(&releves, None);
    assert_eq!(j["files"][1]["name"], "airplay-daemon");
    assert_eq!(j["files"][1]["present"], true);
    assert_eq!(j["files"][2]["name"], "librespot");
}

/// Le démon réellement lancé, quand ce n'est pas celui d'à côté, est nommé.
#[test]
fn le_demon_lance_ailleurs_est_nomme_8944() {
    let dir = tune_core::test_scratch::scratch_dir("executables-livres-8944-ailleurs");
    let maintenant = SystemTime::now();
    poser(&dir, "tune-server", b"serveur", maintenant);
    let ailleurs = dir.join("usr-local-bin");
    std::fs::create_dir_all(&ailleurs).unwrap();
    poser(
        &ailleurs,
        "airplay-daemon",
        b"vieux demon",
        maintenant - Duration::from_secs(3 * 86_400),
    );

    let releves = relever(&dir.join("tune-server"), &dir, "");
    let lance = relever_un("airplay-daemon", &ailleurs.join("airplay-daemon"));
    let md = section_markdown(&releves, Some(&lance));

    assert!(md.contains("- airplay-daemon: absent"), "{md}");
    assert!(
        md.contains("airplay-daemon the server would launch: "),
        "{md}"
    );
    assert!(md.contains("older than tune-server by 3 day(s)"), "{md}");
}
