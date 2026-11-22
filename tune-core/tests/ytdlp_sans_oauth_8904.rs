//! #8904 — yt-dlp n'hérite pas du client OAuth de Tune.
//!
//! `tokio::process::Command` passe tout l'environnement du serveur au
//! processus fils. yt-dlp ne lit pas `TUNE_YOUTUBE_CLIENT_ID` ni
//! `TUNE_YOUTUBE_CLIENT_SECRET`, mais il ne doit pas les recevoir : la lecture
//! par yt-dlp reste séparée de la connexion au compte.
//!
//! Un seul essai dans ce binaire : il pose les deux variables dans
//! l'environnement du processus, ce qu'aucun autre essai ne doit voir.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use tune_core::streaming::youtube::{ENV_CLIENT_ID, ENV_CLIENT_SECRET, url_par_ytdlp_pour_essai};

/// Un faux yt-dlp qui recopie ce qu'il reçoit des deux variables : dans l'URL
/// (quatre lignes, comme le vrai avec `--print`), ou comme sa version.
const FAUX_YTDLP: &str = r#"#!/bin/sh
VU="${TUNE_YOUTUBE_CLIENT_ID-absent}/${TUNE_YOUTUBE_CLIENT_SECRET-absent}"
if [ "$1" = "--version" ]; then echo "$VU"; exit 0; fi
echo "http://127.0.0.1:9/flux/$VU"
echo '{"User-Agent": "Mozilla/5.0"}'
echo 'tune-format=251,opus,128.93'
echo 'tune-disponible=NA'
"#;

#[tokio::test]
async fn ytdlp_ne_recoit_pas_les_variables_du_client_oauth() {
    // SAFETY : seul essai de ce binaire, posé avant tout lancement de
    // processus ; aucun autre fil ne lit l'environnement en même temps.
    unsafe {
        std::env::set_var(ENV_CLIENT_ID, "fuite-id");
        std::env::set_var(ENV_CLIENT_SECRET, "fuite-secret");
    }
    let dossier = tempfile::tempdir().unwrap();
    let binaire = dossier.path().join("yt-dlp");
    std::fs::write(&binaire, FAUX_YTDLP).unwrap();
    std::fs::set_permissions(&binaire, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Contre-épreuve : lancé sans précaution, le fils HÉRITE des variables.
    let brut = tokio::process::Command::new(&binaire)
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&brut.stdout).trim(),
        "fuite-id/fuite-secret",
        "le faux yt-dlp doit voir l'environnement hérité"
    );

    // Épreuve : la lecture (chemin réel `extraire_par_ytdlp`)…
    let url = url_par_ytdlp_pour_essai(&binaire, "dQw4w9WgXcQ")
        .await
        .expect("le faux yt-dlp rend une URL");
    assert_eq!(url, "http://127.0.0.1:9/flux/absent/absent");

    // … et la question de version (`ytdlp::version_of`) n'en héritent pas.
    assert_eq!(
        tune_core::ytdlp::version_of(&binaire).await.as_deref(),
        Some("absent/absent")
    );

    // La commande commune le dit aussi : les deux variables y sont retirées.
    let commande = tune_core::ytdlp::commande(&binaire);
    let retirees: Vec<_> = commande
        .as_std()
        .get_envs()
        .filter(|(_, valeur)| valeur.is_none())
        .map(|(nom, _)| nom.to_string_lossy().into_owned())
        .collect();
    for variable in [ENV_CLIENT_ID, ENV_CLIENT_SECRET] {
        assert!(
            retirees.iter().any(|r| r == variable),
            "{variable} : {retirees:?}"
        );
    }
}
