//! Tune Circle (#6935) : inviter sans e-mail — lien d'invitation à usage
//! unique, invitation par pseudo du forum, blocage — relayés contre un faux
//! mozaiklabs qui suit le contrat de site-mozaiklabs (branche
//! `feat/6935-circle-inviter-sans-email`).

mod commun;

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{Value, json};

use commun::{JETON, adresse_morte, app, appel, base};

#[derive(Default)]
struct Faux {
    /// (méthode, chemin, corps JSON ou Null) de chaque appel reçu.
    recus: Vec<(String, String, Value)>,
    panne: bool,
    debit: bool,
}

type Etat = Arc<Mutex<Faux>>;

const URL_DU_LIEN: &str =
    "https://mozaiklabs.fr/circle/rejoindre/0123456789abcdef0123456789abcdef0123456789abcdef";

fn lien() -> Value {
    json!({ "id": 3, "circle_id": null, "created_at": "2026-10-24T12:00:00Z", "expires_at": "2026-10-31T12:00:00Z" })
}

async fn cloud(State(e): State<Etat>, m: Method, uri: Uri, h: HeaderMap, corps: Bytes) -> Response {
    let mut f = e.lock().unwrap();
    let chemin = uri.path().trim_start_matches("/api/v1/circle").to_string();
    let v: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    f.recus.push((m.to_string(), chemin.clone(), v.clone()));
    if f.panne {
        return (StatusCode::INTERNAL_SERVER_ERROR, "<html>oups</html>").into_response();
    }
    if h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {JETON}")) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthenticated" }))).into_response();
    }
    if f.debit {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            Json(json!({ "message": "Too Many Attempts." })),
        )
            .into_response();
    }
    let introuvable = || (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response();
    match (m.as_str(), chemin.as_str()) {
        ("POST", "/invitations/by-pseudo") => match v.get("pseudo") {
            Some(Value::String(p)) if p == "Moi-Meme" => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "self_invitation" })),
            )
                .into_response(),
            Some(Value::String(p)) => (
                StatusCode::CREATED,
                Json(json!({ "id": 5, "name_or_email": p, "created_at": "2026-10-24T12:00:00Z", "expires_at": "2026-11-23T12:00:00Z" })),
            )
                .into_response(),
            _ => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "message": "The pseudo field is required.", "errors": { "pseudo": ["The pseudo field is required."] } })),
            )
                .into_response(),
        },
        ("GET", "/invitation-links") => Json(json!({ "links": [lien()] })).into_response(),
        ("POST", "/invitation-links") => {
            if v.get("circle_id") == Some(&json!(77)) {
                return introuvable();
            }
            let mut r = lien();
            r["url"] = json!(URL_DU_LIEN);
            (StatusCode::CREATED, Json(r)).into_response()
        }
        ("DELETE", "/invitation-links/3") | ("POST", "/invitations/9/block") | ("DELETE", "/blocks/7") => {
            Json(json!({ "ok": true })).into_response()
        }
        ("GET", "/blocks") => Json(json!({ "blocks": [{ "user_id": 7, "name": "Alice", "since": "2026-10-24T12:00:00Z" }] })).into_response(),
        _ => introuvable(),
    }
}

async fn demarrer() -> (String, Etat) {
    let etat: Etat = Arc::default();
    let app = Router::new().fallback(cloud).with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (format!("http://{adresse}"), etat)
}

const ROUTES: [(&str, &str); 7] = [
    ("POST", "/invitations/by-pseudo"),
    ("GET", "/invitation-links"),
    ("POST", "/invitation-links"),
    ("DELETE", "/invitation-links/3"),
    ("POST", "/invitations/9/block"),
    ("GET", "/blocks"),
    ("DELETE", "/blocks/7"),
];

fn corps_de(m: &str, chemin: &str) -> Option<Value> {
    match (m, chemin) {
        ("POST", "/invitations/by-pseudo") => Some(json!({ "pseudo": "Bob Marley" })),
        ("POST", "/invitation-links") => Some(json!({})),
        _ => None,
    }
}

#[tokio::test]
async fn sans_session_les_routes_rendent_412_et_rien_ne_part() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, None));
    for (m, chemin) in ROUTES {
        let r = appel(&app, m, chemin, corps_de(m, chemin)).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(r.json(), json!({ "connected": false, "code": "circle.not_connected" }), "{m} {chemin}");
    }
    assert!(etat.lock().unwrap().recus.is_empty());
}

#[tokio::test]
async fn chaque_route_part_a_l_identique_et_la_reponse_revient_a_l_octet_pres() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    let r = appel(&app, "POST", "/invitations/by-pseudo", Some(json!({ "pseudo": "Bob Marley", "circle_id": 4, "email": "fuite@exemple.fr" }))).await;
    assert_eq!(r.statut, StatusCode::CREATED);
    assert_eq!(r.json()["name_or_email"], json!("Bob Marley"));

    let r = appel(&app, "GET", "/invitation-links", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "links": [lien()] }));
    assert_eq!(r.entetes.get(header::CACHE_CONTROL).unwrap(), "no-store");

    let r = appel(&app, "POST", "/invitation-links", Some(json!({ "circle_id": 4, "token": "choisi-par-le-client" }))).await;
    assert_eq!(r.statut, StatusCode::CREATED);
    assert_eq!(r.json()["url"], json!(URL_DU_LIEN));
    assert_eq!(
        r.entetes.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "l'URL du lien porte son jeton : aucun cache ne la garde"
    );
    let r = appel(&app, "POST", "/invitation-links", None).await;
    assert_eq!(r.statut, StatusCode::CREATED, "sans corps : un lien sans cercle");

    for (m, chemin) in [
        ("DELETE", "/invitation-links/3"),
        ("POST", "/invitations/9/block"),
        ("DELETE", "/blocks/7"),
    ] {
        let r = appel(&app, m, chemin, None).await;
        assert_eq!(r.statut, StatusCode::OK, "{m} {chemin}");
        assert_eq!(r.json(), json!({ "ok": true }), "{m} {chemin}");
    }
    let r = appel(&app, "GET", "/blocks", None).await;
    assert_eq!(r.json()["blocks"][0]["name"], json!("Alice"));

    let recus = etat.lock().unwrap().recus.clone();
    assert_eq!(
        recus,
        vec![
            ("POST".into(), "/invitations/by-pseudo".into(), json!({ "pseudo": "Bob Marley", "circle_id": 4 })),
            ("GET".into(), "/invitation-links".into(), Value::Null),
            ("POST".into(), "/invitation-links".into(), json!({ "circle_id": 4 })),
            ("POST".into(), "/invitation-links".into(), json!({})),
            ("DELETE".into(), "/invitation-links/3".into(), Value::Null),
            ("POST".into(), "/invitations/9/block".into(), Value::Null),
            ("DELETE".into(), "/blocks/7".into(), Value::Null),
            ("GET".into(), "/blocks".into(), Value::Null),
        ],
        "seuls les champs du contrat partent, vers le chemin du contrat"
    );
}

#[tokio::test]
async fn les_refus_du_cloud_sont_relayes_avec_leur_corps() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    let r = appel(&app, "POST", "/invitations/by-pseudo", Some(json!({ "nom": "x" }))).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "le cloud juge le pseudo absent");
    assert!(r.json()["errors"]["pseudo"][0].is_string());
    assert_eq!(etat.lock().unwrap().recus[0].2, json!({ "pseudo": null }));

    let r = appel(&app, "POST", "/invitations/by-pseudo", Some(json!({ "pseudo": "Moi-Meme" }))).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json(), json!({ "error": "self_invitation" }));

    let r = appel(&app, "POST", "/invitation-links", Some(json!({ "circle_id": 77 }))).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);
    assert_eq!(r.json(), json!({ "error": "not_found" }));

    let r = appel(&app, "DELETE", "/invitation-links/4", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);

    etat.lock().unwrap().debit = true;
    let r = appel(&app, "POST", "/invitation-links", None).await;
    assert_eq!(r.statut, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.entetes.get(header::RETRY_AFTER).unwrap(), "60");
}

#[tokio::test]
async fn cloud_en_panne_ou_injoignable_rend_503() {
    let (url, etat) = demarrer().await;
    etat.lock().unwrap().panne = true;
    let en_panne = app(base(&url, Some(JETON)));
    let morte = app(base(&adresse_morte().await, Some(JETON)));
    for (app, amont) in [(&en_panne, json!(500)), (&morte, Value::Null)] {
        for (m, chemin) in ROUTES {
            let r = appel(app, m, chemin, corps_de(m, chemin)).await;
            assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE, "{m} {chemin}");
            assert_eq!(
                r.json(),
                json!({ "connected": true, "code": "circle.cloud_unavailable", "upstream_status": amont }),
                "{m} {chemin}"
            );
        }
    }
    assert_eq!(etat.lock().unwrap().recus.len(), ROUTES.len());
}
