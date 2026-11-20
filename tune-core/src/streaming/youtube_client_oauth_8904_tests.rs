//! #8904 — Google bloque la connexion YouTube présentée avec le client de
//! repli « YouTube TV » public (portée `youtube` complète). La sortie est un
//! client OAuth propre à Tune, en `youtube.readonly`, figé dans les binaires
//! publiés par la CI. Ces essais gardent les trois points qui le rendent sûr :
//!
//! 1. le journal dit quel client est employé sans livrer l'identifiant ;
//! 2. un compte déjà connecté par le repli reste rafraîchi par le repli (un
//!    jeton ne se rafraîchit que chez le client qui l'a émis) ;
//! 3. CHAQUE ligne de build qui publie un binaire reçoit les deux secrets, y
//!    compris dans le conteneur `cross` des cibles ARM.
use serde_json::json;

use super::*;

fn client_tune() -> ClientOAuth {
    ClientOAuth::resoudre((None, None), (Some("1234567890-abc.apps"), Some("s")))
}

#[test]
fn origine_du_client_execution_compilation_repli() {
    let exec = ClientOAuth::resoudre((Some("id-e"), Some("s-e")), (Some("id-c"), Some("s-c")));
    assert_eq!((exec.origine, exec.libelle()), ("execution", "tune"));
    let compil = ClientOAuth::resoudre((None, None), (Some("id-c"), Some("s-c")));
    assert_eq!((compil.origine, compil.libelle()), ("compilation", "tune"));
    assert_eq!(compil.scope, GOOGLE_SCOPE_LECTURE);
    // Secrets de CI absents : `option_env!` rend Some("") et non None.
    let vide = ClientOAuth::resoudre((None, None), (Some(""), Some("")));
    assert_eq!((vide.origine, vide.libelle()), ("repli", "repli"));
    assert_eq!(vide, ClientOAuth::repli());
}

#[test]
fn l_identifiant_abrege_ne_livre_pas_l_identifiant() {
    let c = client_tune();
    let abrege = c.id_abrege();
    assert_eq!(abrege, "12345678…");
    assert!(!abrege.contains(&c.id));
}

#[test]
fn ancienne_ligne_sans_client_est_rafraichie_par_le_repli() {
    let mut svc = YouTubeService::new();
    svc.oauth = client_tune();
    svc.client_du_jeton = svc.oauth.clone();
    assert!(svc.restore_tokens(&json!({"access_token": "at", "refresh_token": "rt"})));
    assert!(
        svc.client_du_jeton.repli,
        "jeton émis par le repli avant #8904"
    );
    // Et l'enregistrement le dit, pour le prochain démarrage.
    assert_eq!(svc.save_tokens().unwrap()["client_oauth"], "repli");
}

#[test]
fn ligne_du_client_tune_reste_au_client_tune() {
    let mut svc = YouTubeService::new();
    svc.oauth = client_tune();
    svc.client_du_jeton = ClientOAuth::repli();
    svc.restore_tokens(
        &json!({"access_token": "at", "refresh_token": "rt", "client_oauth": "tune"}),
    );
    assert_eq!(svc.client_du_jeton, svc.oauth);
    assert_eq!(svc.save_tokens().unwrap()["client_oauth"], "tune");
}

#[tokio::test]
async fn deconnexion_rend_le_client_du_processus() {
    let mut svc = YouTubeService::new();
    svc.oauth = client_tune();
    svc.restore_tokens(&json!({"access_token": "at", "refresh_token": "rt"}));
    assert!(svc.client_du_jeton.repli);
    svc.logout().await.unwrap();
    assert_eq!(svc.client_du_jeton, svc.oauth);
}

// ---------------------------------------------------------------------------
// Les lignes de build qui PUBLIENT un binaire reçoivent les secrets.
// ---------------------------------------------------------------------------

const RELEASE: &str = include_str!("../../../.github/workflows/release.yml");
const DOCKER: &str = include_str!("../../../.github/workflows/docker.yml");
const CROSS: &str = include_str!("../../../Cross.toml");

/// Pour chaque commande de build de `tune-server`, l'étape qui la porte (de
/// son `- name:` à la commande) doit poser les deux variables depuis les
/// secrets du dépôt. Rend le nombre d'étapes vérifiées.
fn etapes_de_build_avec_secrets(nom: &str, workflow: &str) -> usize {
    let lignes: Vec<&str> = workflow.lines().collect();
    let mut vues = 0;
    for (i, ligne) in lignes.iter().enumerate() {
        let t = ligne.trim_start();
        if t.starts_with('#') {
            continue;
        }
        if !(t.contains("cargo build --release --package tune-server")
            || t.contains("cross build --release --package tune-server"))
        {
            continue;
        }
        let debut = (0..i)
            .rev()
            .find(|&j| lignes[j].trim_start().starts_with("- name:"))
            .unwrap_or_else(|| panic!("{nom}:{} : build hors d'une étape nommée", i + 1));
        let etape = lignes[debut..=i].join("\n");
        for var in ["TUNE_YOUTUBE_CLIENT_ID", "TUNE_YOUTUBE_CLIENT_SECRET"] {
            let attendu = format!("{var}: ${{{{ secrets.{var} }}}}");
            assert!(
                etape.contains(&attendu),
                "{nom}:{} : l'étape « {} » ne reçoit pas {var} (#8904)",
                i + 1,
                lignes[debut].trim()
            );
        }
        vues += 1;
    }
    vues
}

#[test]
fn chaque_build_publie_recoit_le_client_oauth_de_tune() {
    // 4 dans release.yml (cross, Linux, Windows, macOS), 2 dans docker.yml.
    assert_eq!(etapes_de_build_avec_secrets("release.yml", RELEASE), 4);
    assert_eq!(etapes_de_build_avec_secrets("docker.yml", DOCKER), 2);
}

#[test]
fn le_conteneur_cross_recoit_le_client_oauth_de_tune() {
    for cible in ["aarch64-unknown-linux-gnu", "aarch64-unknown-linux-musl"] {
        let entete = format!("[target.{cible}.env]");
        let section = CROSS
            .split(&entete)
            .nth(1)
            .unwrap_or_else(|| panic!("Cross.toml : {entete} absent"));
        let section = section.split("\n[").next().unwrap();
        for var in [
            "\"TUNE_YOUTUBE_CLIENT_ID\"",
            "\"TUNE_YOUTUBE_CLIENT_SECRET\"",
        ] {
            assert!(
                section.contains(var),
                "Cross.toml {entete} : {var} absent (#8904)"
            );
        }
    }
}

#[test]
fn docker_declare_les_secrets_facultatifs() {
    // `secrets: inherit` côté release.yml ; la déclaration rend l'appel
    // explicite et NON obligatoire (un dépôt sans le secret construit quand même).
    for var in ["TUNE_YOUTUBE_CLIENT_ID:", "TUNE_YOUTUBE_CLIENT_SECRET:"] {
        let apres = DOCKER
            .split(&format!("      {var}\n"))
            .nth(1)
            .unwrap_or_else(|| panic!("docker.yml : {var} non déclaré"));
        assert!(
            apres.trim_start().starts_with("required: false"),
            "{var} doit rester facultatif"
        );
    }
}
