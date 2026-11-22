//! Faux service direct et relais locaux : aucun compte ni réseau externe.
use super::*;
use axum::response::IntoResponse;
use serde_json::json;

async fn endpoint(status: u16, body: serde_json::Value) -> String {
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let body = body.clone();
        async move {
            if uri.path() == "/config" {
                axum::Json(json!({"qobuz":{"app_id":"essai", "app_secret":"factice"}}))
                    .into_response()
            } else {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(body),
                )
                    .into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn service(direct: &str, proxy: &str, proxy_first: bool) -> QobuzService {
    let mut svc = QobuzService::avec_bases_de_test(
        "essai".into(),
        "factice".into(),
        BasesQobuz {
            api: direct.into(),
            proxy: proxy.into(),
            config_distante: format!("{direct}/config"),
        },
    );
    svc.proxy_first = proxy_first;
    svc
}
fn credentials() -> serde_json::Value {
    json!({"username":"personne@example.invalid", "password":"factice"})
}
fn code(error: TuneError) -> &'static str {
    match error {
        TuneError::Refused { code, .. } => code,
        TuneError::Credentials(_) => "invalid_credentials",
        _ => "autre",
    }
}

#[tokio::test]
async fn connexion_8867_sans_jeton_ne_devient_pas_un_succes() {
    for token in [serde_json::Value::Null, json!(""), json!("   "), json!(42)] {
        let direct = endpoint(
            200,
            json!({"user_auth_token":token,"user":{"display_name":"Essai"}}),
        )
        .await;
        let mut svc = service(&direct, &direct, false);
        let error = svc
            .authenticate(&credentials())
            .await
            .expect_err("200 sans jeton utilisable doit être une connexion incomplète");
        assert_eq!(code(error), "qobuz_login_incomplete");
        assert!(!svc.auth_status_internal().authenticated);
    }
}

#[tokio::test]
async fn connexion_8867_jeton_absent_preserve_la_session_precedente() {
    let direct = endpoint(200, json!({"user":{}})).await;
    let mut svc = service(&direct, &direct, false);
    svc.user_auth_token = Some("ancien-jeton-factice".into());
    assert!(
        svc.authenticate(&credentials()).await.is_err(),
        "la réponse incomplète ne doit pas valider une session"
    );
    assert_eq!(svc.user_auth_token.as_deref(), Some("ancien-jeton-factice"));
}

#[tokio::test]
async fn connexion_8867_401_direct_reste_identifiants() {
    let direct = endpoint(
        401,
        json!({"message":"Invalid username/email and password combination"}),
    )
    .await;
    let mut svc = service(&direct, &direct, false);
    assert_eq!(
        code(svc.authenticate(&credentials()).await.unwrap_err()),
        "invalid_credentials"
    );
}

#[tokio::test]
async fn connexion_8867_401_relais_nommee_sans_accuser_le_compte() {
    let direct = endpoint(200, json!({"user_auth_token":"factice"})).await;
    let proxy = endpoint(401, json!({"message":"Unauthorized"})).await;
    let mut svc = service(&direct, &proxy, true);
    assert_eq!(
        code(svc.authenticate(&credentials()).await.unwrap_err()),
        "qobuz_relay_login_refused",
        "le 401 du relais doit être distingué du 401 direct"
    );
}

#[tokio::test]
async fn connexion_8867_secours_relais_401_garde_son_origine() {
    let direct = endpoint(503, json!({"message":"indisponible"})).await;
    let proxy = endpoint(401, json!({"message":"Unauthorized"})).await;
    let mut svc = service(&direct, &proxy, false);
    assert_eq!(
        code(svc.authenticate(&credentials()).await.unwrap_err()),
        "qobuz_relay_login_refused"
    );
}

#[tokio::test]
async fn connexion_8867_secours_direct_401_reste_direct() {
    let direct = endpoint(
        401,
        json!({"message":"Invalid username/email and password combination"}),
    )
    .await;
    let proxy = endpoint(503, json!({"message":"indisponible"})).await;
    let mut svc = service(&direct, &proxy, true);
    assert_eq!(
        code(svc.authenticate(&credentials()).await.unwrap_err()),
        "invalid_credentials"
    );
}

#[tokio::test]
async fn connexion_8867_jeton_valide_authentifie() {
    let direct = endpoint(
        200,
        json!({"user_auth_token":"jeton-factice", "user":{"display_name":"Essai"}}),
    )
    .await;
    let mut svc = service(&direct, &direct, false);
    assert!(
        svc.authenticate(&credentials())
            .await
            .unwrap()
            .authenticated
    );
    assert_eq!(svc.user_auth_token.as_deref(), Some("jeton-factice"));
}

#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn connexion_8867_journal_nomme_origine_et_jeton_absent() {
    use tracing::instrument::WithSubscriber;
    for (status, proxy_first, expected, nature) in [
        (401, false, "direct", "identifiants_refuses"),
        (401, true, "relais", "identifiants_refuses"),
        (200, false, "direct", "jeton_absent"),
    ] {
        let endpoint = endpoint(status, json!({"message":"Unauthorized"})).await;
        let mut svc = service(&endpoint, &endpoint, proxy_first);
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.clone())
            .finish();
        let _ = svc
            .authenticate(&credentials())
            .with_subscriber(subscriber)
            .await;
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let events: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let event = events
            .iter()
            .find(|e| e["fields"]["message"] == "qobuz_login_failed")
            .expect("un échec doit être journalisé");
        assert_eq!(
            event["fields"]["origine"], expected,
            "le journal doit nommer direct ou relais"
        );
        assert_eq!(event["fields"]["nature"], nature);
        assert!(
            !logs.contains("qobuz_authenticated"),
            "aucun succès sans jeton"
        );
    }
}
