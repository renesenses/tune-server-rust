//! Contrat #6902 contre un faux site : octets, corps, curseur, erreurs et SSO.
mod commun;

use axum::{
    Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use commun::{JETON, app, appel, base};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Cloud {
    statut: StatusCode,
    corps: &'static str,
    appels: Vec<(Method, String, Value, String)>,
}

type Etat = Arc<Mutex<Cloud>>;
async fn recevoir(
    State(e): State<Etat>,
    OriginalUri(uri): OriginalUri,
    methode: Method,
    entetes: HeaderMap,
    corps: Bytes,
) -> Response {
    let mut e = e.lock().unwrap();
    e.appels.push((
        methode,
        uri.to_string(),
        if corps.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&corps).unwrap()
        },
        entetes
            .get(header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
    ));
    (
        e.statut,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::RETRY_AFTER, "42"),
        ],
        e.corps,
    )
        .into_response()
}
async fn demarrer() -> (String, Etat) {
    let e = Arc::new(Mutex::new(Cloud {
        statut: StatusCode::OK,
        corps: "{ \"conversations\" : [] }",
        appels: vec![],
    }));
    let app = Router::new().fallback(recevoir).with_state(e.clone());
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (url, e)
}

#[tokio::test]
async fn messagerie_6902_relaie_les_sept_gestes_et_le_curseur_sans_cache() {
    let (url, e) = demarrer().await;
    let a = app(base(&url, Some(JETON)));
    let carte = json!({"body":"Écoute ça", "card":{"type":"album","title":"Kind of Blue","qobuz_id":"123"}});
    for (methode, chemin, corps) in [
        ("GET", "/conversations", None),
        ("POST", "/contacts/7/conversation", None),
        (
            "GET",
            "/conversations/12/messages?cursor=abc%2Bdef%3D&limit=50",
            None,
        ),
        ("POST", "/conversations/12/messages", Some(carte.clone())),
        ("POST", "/conversations/12/read", Some(json!({"up_to":81}))),
        ("DELETE", "/conversations/12/messages/81", None),
        (
            "POST",
            "/conversations/12/reports",
            Some(json!({"message_ids":[81],"reason":"spam","comment":"signalement"})),
        ),
    ] {
        let r = appel(&a, methode, chemin, corps.clone()).await;
        assert_eq!(
            r.statut,
            StatusCode::OK,
            "route manquante : {methode} {chemin}"
        );
        assert_eq!(
            r.entetes[header::CACHE_CONTROL],
            "no-store",
            "la messagerie ne doit jamais être mise en cache"
        );
        assert_eq!(
            r.octets, b"{ \"conversations\" : [] }",
            "les octets du site doivent rester intacts"
        );
        let c = e.lock().unwrap();
        let (m, u, b, auth) = c.appels.last().unwrap();
        assert_eq!(m.as_str(), methode);
        assert_eq!(
            u,
            &format!("/api/v1/circle{chemin}"),
            "le curseur opaque doit parvenir inchangé au site"
        );
        assert_eq!(b, &corps.unwrap_or(Value::Null));
        assert_eq!(auth, &format!("Bearer {JETON}"));
    }
}

#[tokio::test]
async fn messagerie_6902_relaie_les_refus_et_le_204_du_site() {
    let (url, e) = demarrer().await;
    let db = base(&url, Some(JETON));
    // Ce banc porte le refus final, pas le rafraîchissement testé dans relais.rs.
    tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .set("mozaik_refresh_token", "")
        .unwrap();
    let a = app(db);
    for (amont, attendu, corps) in [
        (
            StatusCode::NOT_FOUND,
            StatusCode::NOT_FOUND,
            "{\"error\":\"not_found\"}",
        ),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{\"message\":\"invalid\",\"errors\":{\"body\":[\"invalid\"]}}",
        ),
        (
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::TOO_MANY_REQUESTS,
            "{\"message\":\"Too Many Attempts.\"}",
        ),
        (StatusCode::NO_CONTENT, StatusCode::NO_CONTENT, ""),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
        ),
        (
            StatusCode::UNAUTHORIZED,
            StatusCode::PRECONDITION_FAILED,
            "{}",
        ),
    ] {
        {
            let mut e = e.lock().unwrap();
            e.statut = amont;
            e.corps = corps;
        }
        let r = appel(&a, "DELETE", "/conversations/12/messages/81", None).await;
        assert_eq!(r.statut, attendu);
        assert_eq!(r.entetes[header::CACHE_CONTROL], "no-store");
        if amont == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(r.entetes[header::RETRY_AFTER], "42");
        }
        if amont.as_u16() < 500 && amont != StatusCode::UNAUTHORIZED {
            assert_eq!(r.octets, corps.as_bytes());
        }
    }
}

#[tokio::test]
async fn messagerie_6902_sans_sso_aucun_message_ne_part() {
    let (url, e) = demarrer().await;
    let a = app(base(&url, None));
    for (m, u, b) in [
        ("GET", "/conversations", None),
        ("POST", "/contacts/7/conversation", None),
        ("GET", "/conversations/12/messages", None),
        (
            "POST",
            "/conversations/12/messages",
            Some(json!({"body":"privé"})),
        ),
        ("POST", "/conversations/12/read", None),
        ("DELETE", "/conversations/12/messages/81", None),
        (
            "POST",
            "/conversations/12/reports",
            Some(json!({"message_ids":[81],"reason":"spam"})),
        ),
    ] {
        let r = appel(&a, m, u, b).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED);
        assert_eq!(r.json()["code"], "circle.not_connected");
        assert_eq!(r.entetes[header::CACHE_CONTROL], "no-store");
    }
    assert!(
        e.lock().unwrap().appels.is_empty(),
        "aucun contenu privé ne doit partir sans session SSO"
    );
}

#[tokio::test]
async fn messagerie_6902_refuse_un_identifiant_hors_contrat_avant_le_site() {
    let (url, e) = demarrer().await;
    let a = app(base(&url, Some(JETON)));
    for u in [
        "/conversations/abc/messages",
        "/conversations/1234567890123456789/messages",
    ] {
        let r = appel(&a, "GET", u, None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND);
        assert_eq!(r.entetes[header::CACHE_CONTROL], "no-store");
    }
    let r = appel(&a, "POST", "/conversations/12/messages", None).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["code"], "circle.invalid_body");
    assert_eq!(r.entetes[header::CACHE_CONTROL], "no-store");
    assert!(e.lock().unwrap().appels.is_empty());
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn messagerie_6902_ne_journalise_ni_message_carte_commentaire_ni_curseur() {
    let capture = Capture::default();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(capture.clone())
        .init();
    tracing::info!("capteur_messagerie_actif");
    let (url, _) = demarrer().await;
    let a = app(base(&url, Some(JETON)));
    for (m, u, b) in [
        (
            "POST",
            "/conversations/12/messages",
            Some(
                json!({"body":"TEXTE-PRIVE-6902","card":{"type":"album","title":"CARTE-PRIVEE-6902"}}),
            ),
        ),
        (
            "POST",
            "/conversations/12/reports",
            Some(json!({"message_ids":[81],"reason":"other","comment":"COMMENTAIRE-PRIVE-6902"})),
        ),
        (
            "GET",
            "/conversations/12/messages?cursor=CURSEUR-PRIVE-6902",
            None,
        ),
    ] {
        assert_eq!(appel(&a, m, u, b).await.statut, StatusCode::OK);
    }
    let journal = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(journal.contains("capteur_messagerie_actif"));
    assert!(
        journal.contains("circle_relai"),
        "le capteur doit voir les appels réels du relais"
    );
    for interdit in [
        "TEXTE-PRIVE-6902",
        "CARTE-PRIVEE-6902",
        "COMMENTAIRE-PRIVE-6902",
        "CURSEUR-PRIVE-6902",
        JETON,
    ] {
        assert!(
            !journal.contains(interdit),
            "le journal ne doit contenir aucun contenu privé : {interdit}"
        );
    }
}

#[tokio::test]
async fn messagerie_cercle_relaie_adhesion_ouverture_et_consentement() {
    let (url, e) = demarrer().await;
    let a = app(base(&url, Some(JETON)));
    for (method, path, body) in [
        ("GET", "/general-circles", None),
        ("POST", "/general-circles/8/membership", None),
        ("DELETE", "/general-circles/8/membership", None),
        ("POST", "/general-circles/8/conversation", None),
        (
            "POST",
            "/conversations/12/name-consent",
            Some(json!({"accepted":true})),
        ),
    ] {
        let r = appel(&a, method, path, body.clone()).await;
        assert_eq!(
            r.statut,
            StatusCode::OK,
            "route cercle absente : {method} {path}"
        );
        assert_eq!(r.entetes[header::CACHE_CONTROL], "no-store");
        assert_eq!(r.octets, b"{ \"conversations\" : [] }");
        let c = e.lock().unwrap();
        let (m, u, b, auth) = c.appels.last().unwrap();
        assert_eq!(m.as_str(), method);
        assert_eq!(u, &format!("/api/v1/circle{path}"));
        assert_eq!(b, &body.unwrap_or(Value::Null));
        assert_eq!(auth, &format!("Bearer {JETON}"));
    }
}

#[tokio::test]
async fn messagerie_cercle_refus_cloud_et_consentement_invalide() {
    let (url, e) = demarrer().await;
    let a = app(base(&url, Some(JETON)));
    {
        let mut c = e.lock().unwrap();
        c.statut = StatusCode::UNPROCESSABLE_ENTITY;
        c.corps = "{\"error\":\"circle.name_consent_required\"}";
    }
    let r = appel(
        &a,
        "POST",
        "/conversations/12/messages",
        Some(json!({"body":"x"})),
    )
    .await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.octets, b"{\"error\":\"circle.name_consent_required\"}");
    let before = e.lock().unwrap().appels.len();
    assert_eq!(
        appel(&a, "POST", "/general-circles/not-an-id/conversation", None)
            .await
            .statut,
        StatusCode::NOT_FOUND
    );
    assert_eq!(e.lock().unwrap().appels.len(), before);
}
