//! #8904 — Google bloque la connexion YouTube présentée avec le client de
//! repli « YouTube TV » public (portée `youtube` complète). La sortie est un
//! client OAuth propre à Tune, en `youtube.readonly`, figé dans les binaires
//! publiés par la CI ; le repli est RETIRÉ (décision du 17/11). Ces essais
//! gardent :
//!
//! 1. le journal dit quel client est employé sans livrer l'identifiant ;
//! 2. sans client de Tune, la connexion est refusée par un message clair et
//!    AUCUN appel ne part vers Google ;
//! 3. un compte connecté par le repli passe « à reconnecter » au premier
//!    rafraîchissement, sans appel à Google, et sa ligne est effacée ;
//! 4. CHAQUE ligne de build qui publie un binaire reçoit les deux secrets, y
//!    compris dans le conteneur `cross` des cibles ARM.
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;

const ID_TUNE: &str = "1234567890-abc.apps";

fn client_tune() -> ClientOAuth {
    ClientOAuth::resoudre((None, None), (Some(ID_TUNE), Some("s"))).expect("client de Tune")
}

/// Les corps des requêtes reçues par le faux Google.
type Appels = Arc<Mutex<Vec<String>>>;

/// Un faux Google OAuth local (`/device/code`, `/token`) qui note chaque
/// requête reçue. Rend le service branché dessus et le carnet d'appels.
async fn service_contre_un_faux_google(oauth: Option<ClientOAuth>) -> (YouTubeService, Appels) {
    use axum::routing::post;
    let appels: Appels = Arc::default();
    let note = |appels: Appels, reponse: serde_json::Value| {
        move |corps: String| {
            let appels = appels.clone();
            let reponse = reponse.clone();
            async move {
                appels.lock().unwrap().push(corps);
                axum::Json(reponse)
            }
        }
    };
    let app = axum::Router::new()
        .route(
            "/device/code",
            post(note(
                appels.clone(),
                json!({"device_code": "dc", "user_code": "UC-8904",
                       "verification_url": "https://www.google.com/device",
                       "expires_in": 1800, "interval": 5}),
            )),
        )
        .route(
            "/token",
            post(note(
                appels.clone(),
                json!({"access_token": "at-neuf", "expires_in": 3600}),
            )),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ecoute, app).await });

    let mut svc = YouTubeService::new();
    svc.oauth = oauth;
    svc.google_oauth_base = PointsOAuthGoogle {
        device_code: format!("http://{adresse}/device/code"),
        token: format!("http://{adresse}/token"),
        // Pas de route : 404, l'adresse du compte reste inconnue, sans réseau.
        userinfo: format!("http://{adresse}/userinfo"),
    };
    (svc, appels)
}

fn nombre(appels: &Appels) -> usize {
    appels.lock().unwrap().len()
}

#[test]
fn origine_du_client_execution_compilation_sinon_aucun() {
    let exec =
        ClientOAuth::resoudre((Some("id-e"), Some("s-e")), (Some("id-c"), Some("s-c"))).unwrap();
    assert_eq!(exec.origine, "execution");
    let compil = ClientOAuth::resoudre((None, None), (Some("id-c"), Some("s-c"))).unwrap();
    assert_eq!(compil.origine, "compilation");
    assert_eq!(compil.scope, GOOGLE_SCOPE_LECTURE);
    // Secrets de CI absents : `option_env!` rend Some("") et non None. Plus de
    // repli : aucun client.
    assert_eq!(
        ClientOAuth::resoudre((None, None), (Some(""), Some(""))),
        None
    );
}

#[test]
fn les_identifiants_du_repli_ont_quitte_le_code() {
    // Ni l'identifiant ni le secret du client « YouTube TV », ni la portée
    // complète, nulle part dans le module (essais compris).
    const SOURCE: &str = include_str!("youtube.rs");
    for retire in [
        concat!("43212718137", "-t3p59j0tov6ak6jdsuj9leqpdmmmqpt4"),
        concat!("GOCSPX-", "Zrl1xIm-Bf63fMFIOh2IDK1AyAFQ"),
        concat!("\"https://www.googleapis.com/auth/", "youtube\""),
    ] {
        assert!(!SOURCE.contains(retire), "{retire} encore dans youtube.rs");
    }
}

#[test]
fn l_identifiant_abrege_ne_livre_pas_l_identifiant() {
    let c = client_tune();
    let abrege = c.id_abrege();
    assert_eq!(abrege, "12345678…");
    assert!(!abrege.contains(&c.id));
}

/// Épreuve 1 : sans client de Tune, la connexion est refusée par le message
/// « non configuré » et AUCUNE requête ne part vers Google.
#[tokio::test]
async fn sans_client_de_tune_la_connexion_est_refusee_sans_appel() {
    let (mut svc, appels) = service_contre_un_faux_google(None).await;
    let refus = svc
        .authenticate(&json!({"device_flow": true}))
        .await
        .expect_err("connexion refusée");
    match &refus {
        TuneError::Refused { code, message } => {
            assert_eq!(*code, CODE_CLIENT_OAUTH_ABSENT);
            assert!(
                message.contains("non configuré dans cette version"),
                "{message}"
            );
        }
        autre => panic!("refus attendu, reçu {autre:?}"),
    }
    assert_eq!(nombre(&appels), 0, "un appel est parti vers Google");
    assert!(svc.pending_device_auth.is_none());
}

/// Contre-épreuve de l'épreuve 1 : avec le client de Tune, la même demande
/// part bien vers Google, avec l'identifiant de TUNE et la portée lecture.
#[tokio::test]
async fn avec_le_client_de_tune_la_connexion_part_avec_son_identifiant() {
    let (mut svc, appels) = service_contre_un_faux_google(Some(client_tune())).await;
    let etat = svc
        .authenticate(&json!({"device_flow": true}))
        .await
        .expect("flux ouvert");
    assert_eq!(etat.user_code.as_deref(), Some("UC-8904"));
    let appels = appels.lock().unwrap();
    assert_eq!(appels.len(), 1);
    assert!(
        appels[0].starts_with(&format!("client_id={ID_TUNE}&")),
        "{}",
        appels[0]
    );
    assert!(appels[0].ends_with("youtube.readonly"), "{}", appels[0]);
}

/// Épreuve 2 : une ligne d'avant #8914 (sans `client_oauth`), donc émise par
/// le repli, passe « à reconnecter » au premier rafraîchissement : jetons
/// effacés, aucun appel à Google, ligne effacée par le registre.
#[tokio::test]
async fn un_jeton_du_repli_passe_a_reconnecter_sans_appel() {
    let (mut svc, appels) = service_contre_un_faux_google(Some(client_tune())).await;
    assert!(svc.restore_tokens(&json!({
        "access_token": "at-repli", "refresh_token": "rt-repli", "email": "a@example.invalid",
    })));
    assert_eq!(svc.raison_de_reconnexion(), None);
    assert!(!svc.session_expired());

    svc.post_restore().await;

    assert_eq!(
        nombre(&appels),
        0,
        "le jeton du repli est parti vers Google"
    );
    assert!(svc.access_token.is_none() && svc.refresh_token.is_none());
    assert!(!svc.is_authenticated());
    assert!(!svc.auth_status().await.authenticated);
    assert_eq!(svc.raison_de_reconnexion(), Some(MESSAGE_RECONNEXION_REPLI));
    assert!(
        svc.session_expired(),
        "le registre doit effacer la ligne périmée"
    );
    assert_eq!(svc.save_tokens(), Some(json!({})), "la ligne est écrasée");
    // Rien ne plante ensuite : un rafraîchissement de plus ne fait rien.
    assert!(!svc.refresh_if_needed().await.unwrap());
    assert_eq!(nombre(&appels), 0);

    // La reconnexion passe par le client de Tune et lève l'état.
    svc.authenticate(&json!({"device_flow": true}))
        .await
        .unwrap();
    assert!(appels.lock().unwrap()[0].starts_with(&format!("client_id={ID_TUNE}&")));
    svc.authenticate(&json!({"poll": true})).await.unwrap();
    assert!(svc.is_authenticated());
    assert_eq!(svc.raison_de_reconnexion(), None);
    assert_eq!(svc.save_tokens().unwrap()["client_oauth"], "tune");
}

/// Épreuve 2 bis : le rafraîchissement périodique (et non celui du démarrage)
/// aboutit au même état.
#[tokio::test]
async fn le_rafraichissement_periodique_d_un_jeton_du_repli_n_appelle_pas_google() {
    let (mut svc, appels) = service_contre_un_faux_google(Some(client_tune())).await;
    svc.restore_tokens(
        &json!({"access_token": "at", "refresh_token": "rt", "client_oauth": "repli"}),
    );
    assert!(!svc.refresh_if_needed().await.unwrap());
    assert_eq!(nombre(&appels), 0);
    assert_eq!(svc.raison_de_reconnexion(), Some(MESSAGE_RECONNEXION_REPLI));
}

/// Contre-épreuve de l'épreuve 2 : une ligne du client de Tune est rafraîchie
/// par lui, et le compte reste connecté.
#[tokio::test]
async fn un_jeton_du_client_de_tune_reste_connecte() {
    let (mut svc, appels) = service_contre_un_faux_google(Some(client_tune())).await;
    assert!(svc.restore_tokens(&json!({
        "access_token": "at", "refresh_token": "rt", "client_oauth": "tune",
    })));
    svc.post_restore().await;
    let appels = appels.lock().unwrap();
    assert_eq!(appels.len(), 1);
    assert!(
        appels[0].starts_with(&format!("client_id={ID_TUNE}&")),
        "{}",
        appels[0]
    );
    assert_eq!(svc.access_token.as_deref(), Some("at-neuf"));
    assert!(svc.is_authenticated());
    assert_eq!(svc.raison_de_reconnexion(), None);
    assert!(!svc.session_expired());
    assert_eq!(svc.save_tokens().unwrap()["client_oauth"], "tune");
}

/// Une ligne du client de Tune relue par une version SANS client (fork) : à
/// reconnecter avec le message « non configuré », sans appel.
#[tokio::test]
async fn un_jeton_de_tune_sans_client_passe_a_reconnecter() {
    let (mut svc, appels) = service_contre_un_faux_google(None).await;
    svc.restore_tokens(
        &json!({"access_token": "at", "refresh_token": "rt", "client_oauth": "tune"}),
    );
    svc.post_restore().await;
    assert_eq!(nombre(&appels), 0);
    assert_eq!(
        svc.raison_de_reconnexion(),
        Some(MESSAGE_CLIENT_OAUTH_ABSENT)
    );
    assert!(svc.session_expired());
}

/// Un flux de connexion ouvert par le repli (sans marque) ne se reprend pas
/// au redémarrage ; celui du client de Tune, si.
#[test]
fn un_flux_de_connexion_du_repli_ne_se_reprend_pas() {
    let ligne = |marque: Option<&str>| {
        let mut l = json!({"pending_device_code": "dc", "pending_user_code": "UC",
                           "pending_elapsed_secs": 10});
        if let Some(m) = marque {
            l["pending_client_oauth"] = json!(m);
        }
        l
    };
    let mut svc = YouTubeService::new();
    svc.oauth = Some(client_tune());
    svc.restore_tokens(&ligne(None));
    assert!(svc.pending_device_auth.is_none());
    svc.restore_tokens(&ligne(Some("tune")));
    assert!(svc.pending_device_auth.is_some());
    assert_eq!(svc.save_tokens().unwrap()["pending_client_oauth"], "tune");
}

#[tokio::test]
async fn deconnexion_leve_l_etat_a_reconnecter() {
    let mut svc = YouTubeService::new();
    svc.oauth = Some(client_tune());
    svc.restore_tokens(&json!({"access_token": "at", "refresh_token": "rt"}));
    assert!(svc.jeton_du_repli);
    svc.logout().await.unwrap();
    assert!(!svc.jeton_du_repli);
    assert_eq!(svc.raison_de_reconnexion(), None);
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
