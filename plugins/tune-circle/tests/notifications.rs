//! Tune Circle rc9 (#7130) : les notifications de Tune web, relayées contre un
//! faux mozaiklabs qui implémente le contrat cloud (site-mozaiklabs#384,
//! `coordination/contrat-tune-circle-rc8.md`, « Notifications »).

mod commun;

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use commun::{JETON, adresse_morte, app, appel, base};

#[derive(Default)]
struct Cloud {
    appels: usize,
    panne: bool,
    lues: Vec<String>,
    corps_recus: Vec<Bytes>,
    reglages: Option<Value>,
}

type Etat = Arc<Mutex<Cloud>>;

fn refuse(e: &Etat, h: &HeaderMap, corps: &Bytes) -> Option<Response> {
    let mut c = e.lock().unwrap();
    c.appels += 1;
    c.corps_recus.push(corps.clone());
    if c.panne {
        return Some((StatusCode::INTERNAL_SERVER_ERROR, "<html>oups</html>").into_response());
    }
    if h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {JETON}"))
    {
        return Some(
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "unauthenticated" })),
            )
                .into_response(),
        );
    }
    None
}

fn liste_attendue() -> Value {
    json!({
        "unread_count": 1,
        "notifications": [{
            "id": 17, "kind": "playlist_shared", "read": false, "at": "2026-10-26T20:00:00Z",
            "actor": { "user_id": 3, "name": "Alice" }, "circle_name": "Jazz du jeudi",
            "playlist": { "id": 9, "name": "Blue Note" }, "conversation_id": null,
            "invitation_id": null, "count": 1
        }]
    })
}

fn reglages_par_defaut() -> Value {
    json!({ "playlist_shared": true, "circle_invitation": true, "message": true, "contact": true })
}

async fn liste(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = refuse(&e, &h, &corps) {
        return r;
    }
    Json(liste_attendue()).into_response()
}

async fn lue(
    State(e): State<Etat>,
    h: HeaderMap,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if let Some(r) = refuse(&e, &h, &corps) {
        return r;
    }
    if id != "17" {
        return (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response();
    }
    e.lock().unwrap().lues.push(id);
    Json(json!({ "unread_count": 0 })).into_response()
}

async fn tout_lu(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = refuse(&e, &h, &corps) {
        return r;
    }
    Json(json!({ "unread_count": 0 })).into_response()
}

async fn lire_reglages(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = refuse(&e, &h, &corps) {
        return r;
    }
    Json(
        e.lock()
            .unwrap()
            .reglages
            .clone()
            .unwrap_or_else(reglages_par_defaut),
    )
    .into_response()
}

async fn poser_reglages(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = refuse(&e, &h, &corps) {
        return r;
    }
    let changes: Value = serde_json::from_slice(&corps).unwrap();
    let mut c = e.lock().unwrap();
    let mut v = c.reglages.clone().unwrap_or_else(reglages_par_defaut);
    for (k, b) in changes.as_object().unwrap() {
        v[k] = b.clone();
    }
    c.reglages = Some(v.clone());
    Json(v).into_response()
}

async fn demarrer() -> (String, Etat) {
    let etat: Etat = Arc::default();
    let app = Router::new()
        .route("/api/v1/circle/notifications", get(liste))
        .route("/api/v1/circle/notifications/read-all", post(tout_lu))
        .route("/api/v1/circle/notifications/{id}/read", post(lue))
        .route(
            "/api/v1/circle/notifications/preferences",
            get(lire_reglages).put(poser_reglages),
        )
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (format!("http://{adresse}"), etat)
}

const ROUTES: [(&str, &str); 5] = [
    ("GET", "/notifications"),
    ("POST", "/notifications/17/read"),
    ("POST", "/notifications/read-all"),
    ("GET", "/notifications/preferences"),
    ("PUT", "/notifications/preferences"),
];

fn corps_de(m: &str, chemin: &str) -> Option<Value> {
    (m == "PUT" && chemin == "/notifications/preferences").then(|| json!({ "message": false }))
}

#[tokio::test]
async fn sans_session_les_routes_rendent_412_et_rien_ne_part() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, None));
    for (m, chemin) in ROUTES {
        let r = appel(&app, m, chemin, corps_de(m, chemin)).await;
        assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED, "{m} {chemin}");
        assert_eq!(
            r.json(),
            json!({ "connected": false, "code": "circle.not_connected" }),
            "{m} {chemin}"
        );
    }
    assert_eq!(etat.lock().unwrap().appels, 0);
}

#[tokio::test]
async fn la_liste_est_relayee_telle_quelle_et_sans_cache() {
    let (url, _etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));
    let r = appel(&app, "GET", "/notifications", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), liste_attendue());
    assert_eq!(
        r.entetes.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "une notification ne reste dans aucun cache"
    );
}

#[tokio::test]
async fn marquer_lu_part_sans_corps_et_le_404_du_cloud_est_relaye() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    // Le client pourrait envoyer n'importe quoi : rien n'en part.
    let r = appel(
        &app,
        "POST",
        "/notifications/17/read",
        Some(json!({ "seen_by": "Alice" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "unread_count": 0 }));
    assert_eq!(r.entetes.get(header::CACHE_CONTROL).unwrap(), "no-store");

    let r = appel(
        &app,
        "POST",
        "/notifications/read-all",
        Some(json!({ "x": 1 })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "unread_count": 0 }));

    let r = appel(&app, "POST", "/notifications/18/read", None).await;
    assert_eq!(
        r.statut,
        StatusCode::NOT_FOUND,
        "celle d'un autre : 404 du cloud relayé"
    );
    assert_eq!(r.json(), json!({ "error": "not_found" }));

    let c = etat.lock().unwrap();
    assert_eq!(c.lues, vec!["17".to_string()]);
    assert!(
        c.corps_recus.iter().all(|b| b.is_empty()),
        "aucun corps ne part : {:?}",
        c.corps_recus
    );
}

#[tokio::test]
async fn un_identifiant_mal_forme_est_un_404_sans_appel() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));
    for id in ["abc", "1a", "-3", &"9".repeat(19)] {
        let r = appel(&app, "POST", &format!("/notifications/{id}/read"), None).await;
        assert_eq!(r.statut, StatusCode::NOT_FOUND, "{id}");
    }
    assert_eq!(etat.lock().unwrap().appels, 0);
}

#[tokio::test]
async fn les_reglages_se_lisent_et_seules_les_quatre_cles_partent() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    let r = appel(&app, "GET", "/notifications/preferences", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), reglages_par_defaut());

    let r = appel(
        &app,
        "PUT",
        "/notifications/preferences",
        Some(json!({ "message": false })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json()["message"], json!(false));
    assert_eq!(r.entetes.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let envoye: Value =
        serde_json::from_slice(etat.lock().unwrap().corps_recus.last().unwrap()).unwrap();
    assert_eq!(envoye, json!({ "message": false }));

    let avant = etat.lock().unwrap().appels;
    for (corps, champ) in [
        (json!({}), "preferences"),
        (json!({ "message": "non" }), "message"),
        (json!({ "push": true }), "push"),
        (json!({ "contact": true, "email": "a@b.c" }), "email"),
    ] {
        let r = appel(
            &app,
            "PUT",
            "/notifications/preferences",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
        let v = r.json();
        assert_eq!(v["code"], json!("circle.invalid_body"), "{corps}");
        assert!(v["errors"][champ][0].is_string(), "{corps} → {v}");
    }
    assert_eq!(
        etat.lock().unwrap().appels,
        avant,
        "un corps refusé ne part pas"
    );
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
    assert_eq!(etat.lock().unwrap().appels, ROUTES.len());
}
