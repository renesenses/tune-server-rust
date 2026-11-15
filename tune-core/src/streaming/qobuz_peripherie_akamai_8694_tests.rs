//! #8694 — la périphérie Akamai de Qobuz refuse le réseau de l'utilisateur
//! (`403 <HTML>…Access Denied…`, `errors.edgesuite.net`) : le relais doit être
//! essayé, connexion comprise, et si lui aussi échoue l'écran doit dire que
//! c'est le RÉSEAU qui est refusé, pas le mot de passe.
//!
//! Faux Qobuz direct et faux relais sur la boucle locale : aucun appel réel,
//! identifiants factices.

use super::*;
use axum::response::IntoResponse;
use serde_json::json;
use std::sync::{Arc, Mutex as StdMutex};

/// La page servie par Akamai au testeur du fil 2803 (abrégée).
const PAGE_AKAMAI: &str = "<HTML><HEAD>\n<TITLE>Access Denied</TITLE>\n</HEAD><BODY>\n\
<H1>Access Denied</H1>\nYou don't have permission to access \"http&#58;&#47;&#47;www&#46;qobuz&#46;com&#47;api&#46;json&#47;0&#46;2&#47;user&#47;login\" on this server.<P>\n\
Reference&#32;&#35;18&#46;5c3e1002&#46;1762952149&#46;2a1f\n\
<P>https&#58;&#47;&#47;errors&#46;edgesuite&#46;net&#47;18&#46;5c3e1002\n</BODY>\n</HTML>\n";

/// Les chemins reçus par un faux serveur.
type Journal = Arc<StdMutex<Vec<String>>>;

async fn ecouter(app: axum::Router) -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    format!("http://{adresse}")
}

/// L'API directe derrière une périphérie Akamai qui refuse tout, sauf la
/// configuration distante (servie ailleurs en production).
async fn direct_bloque_par_akamai(journal: Journal) -> String {
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let journal = journal.clone();
        async move {
            journal.lock().unwrap().push(uri.path().to_string());
            match uri.path() {
                "/config" => axum::Json(json!({"qobuz": {
                    "app_id": "app-essai",
                    "app_secret": "secret-essai",
                }}))
                .into_response(),
                _ => (
                    axum::http::StatusCode::FORBIDDEN,
                    [(axum::http::header::CONTENT_TYPE, "text/html")],
                    PAGE_AKAMAI,
                )
                    .into_response(),
            }
        }
    });
    ecouter(app).await
}

/// Le relais mozaiklabs, qui passe : il accepte la connexion et les lectures.
async fn relais_qui_passe(journal: Journal) -> String {
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let journal = journal.clone();
        async move {
            journal.lock().unwrap().push(uri.path().to_string());
            match uri.path() {
                "/user/login" => axum::Json(json!({
                    "user_auth_token": "jeton-relais",
                    "user": {"display_name": "Testeur", "credential": {"label": "Studio"}},
                }))
                .into_response(),
                "/album/get" => axum::Json(json!({"id": "a1", "title": "Album"})).into_response(),
                _ => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    ecouter(app).await
}

/// Une adresse où personne n'écoute : le relais injoignable.
async fn relais_injoignable() -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    drop(ecoute);
    format!("http://{adresse}")
}

fn service(direct: &str, relais: &str) -> QobuzService {
    QobuzService::avec_bases_de_test(
        "app-essai".into(),
        "secret-essai".into(),
        BasesQobuz {
            api: direct.to_string(),
            proxy: relais.to_string(),
            config_distante: format!("{direct}/config"),
        },
    )
}

fn identifiants() -> serde_json::Value {
    json!({"username": "testeur@example.org", "password": "mot-de-passe-juste"})
}

#[test]
fn la_page_akamai_est_un_refus_de_peripherie_et_le_403_json_non() {
    let akamai = AttemptError::Http {
        status: 403,
        body: PAGE_AKAMAI.into(),
    };
    assert!(akamai.refus_de_peripherie());
    assert!(
        akamai.transient(),
        "la page Akamai doit faire essayer le relais"
    );

    let api = AttemptError::Http {
        status: 403,
        body: r#"{"status":"error","code":403,"message":"User authentication is required"}"#.into(),
    };
    assert!(!api.refus_de_peripherie());
    assert!(!api.transient(), "un 403 JSON de l'API reste définitif");

    let html_401 = AttemptError::Http {
        status: 401,
        body: PAGE_AKAMAI.into(),
    };
    assert!(!html_401.refus_de_peripherie(), "seul le 403 est reconnu");
}

#[tokio::test]
async fn la_connexion_passe_par_le_relais_quand_akamai_refuse_le_direct() {
    let direct_recus: Journal = Default::default();
    let relais_recus: Journal = Default::default();
    let direct = direct_bloque_par_akamai(direct_recus.clone()).await;
    let relais = relais_qui_passe(relais_recus.clone()).await;
    let mut svc = service(&direct, &relais);

    let statut = svc
        .authenticate(&identifiants())
        .await
        .expect("le relais doit prendre la connexion quand Akamai refuse le direct");
    assert!(statut.authenticated);
    assert!(
        direct_recus
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "/user/login")
    );
    assert!(
        relais_recus
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "/user/login"),
        "le relais n'a jamais été essayé pour user/login"
    );
}

#[tokio::test]
async fn les_lectures_passent_par_le_relais_quand_akamai_refuse_le_direct() {
    let relais_recus: Journal = Default::default();
    let direct = direct_bloque_par_akamai(Default::default()).await;
    let relais = relais_qui_passe(relais_recus.clone()).await;
    let svc = service(&direct, &relais);

    let album = svc
        .api_get("/album/get", &[("album_id", "a1")])
        .await
        .expect("la lecture doit passer par le relais");
    assert_eq!(album["title"], "Album");
    assert!(
        relais_recus
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == "/album/get")
    );
}

#[tokio::test]
async fn relais_injoignable_le_refus_dit_que_c_est_le_reseau() {
    let direct = direct_bloque_par_akamai(Default::default()).await;
    let relais = relais_injoignable().await;
    let mut svc = service(&direct, &relais);

    let err = svc
        .authenticate(&identifiants())
        .await
        .expect_err("ni le direct ni le relais ne passent");
    match err {
        TuneError::Refused { code, message } => {
            assert_eq!(code, CODE_RESEAU_REFUSE);
            assert_eq!(code, "service_network_refused");
            assert!(message.contains("network"), "{message}");
            assert!(
                !message.contains("<HTML>"),
                "la page HTML ne sort pas : {message}"
            );
        }
        autre => panic!("attendu le refus de réseau, obtenu {autre:?}"),
    }
}

#[tokio::test]
async fn un_vrai_refus_d_identifiants_au_relais_reste_un_refus_d_identifiants() {
    let direct = direct_bloque_par_akamai(Default::default()).await;
    let app = axum::Router::new().fallback(|| async {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            axum::Json(json!({"status": "error", "code": 401,
                "message": "Invalid username/email and password combination"})),
        )
            .into_response()
    });
    let relais = ecouter(app).await;
    let mut svc = service(&direct, &relais);

    let err = svc
        .authenticate(&identifiants())
        .await
        .expect_err("le relais refuse les identifiants");
    assert!(matches!(err, TuneError::Credentials(_)), "{err:?}");
}
