//! Tune Circle rc8 (#6900) : le résumé de bibliothèque d'un contact et le
//! lieu opt-in, relayés contre un faux mozaiklabs qui implémente le contrat
//! rc8 (`coordination/contrat-tune-circle-rc8.md`).
//!
//! Faux cloud propre à ce fichier : celui de `commun` route déjà
//! `/contacts/{user_id}/library/{quoi}` vers le catalogue T2.

mod commun;

use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use commun::{JETON, adresse_morte, app, appel, base};

/// Le contact qui partage sa bibliothèque avec moi.
const QUI_PARTAGE: &str = "7";
/// Le contact accepté qui ne partage pas : 403.
const QUI_NE_PARTAGE_PAS: &str = "8";
/// Le contact qui partage, mais dont le cloud n'a pas encore les chiffres : 204.
const SANS_CHIFFRES: &str = "9";

#[derive(Default)]
struct Globe {
    appels: usize,
    panne: bool,
    lieu: Option<Value>,
    corps_recus: Vec<Value>,
    retire_de_la_carte: bool,
    corps_du_retrait: Vec<Bytes>,
}

type Etat = Arc<Mutex<Globe>>;

/// Le refus du faux cloud, ou `None` si l'appel passe.
fn autorise(e: &Etat, h: &HeaderMap) -> Option<Response> {
    let mut g = e.lock().unwrap();
    g.appels += 1;
    if g.panne {
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

fn resume_attendu() -> Value {
    json!({ "albums": 1200, "genres": [{ "genre": "Jazz", "albums": 140 }, { "genre": "Rock", "albums": 99 }] })
}

async fn resume(State(e): State<Etat>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    match id.as_str() {
        QUI_PARTAGE => Json(resume_attendu()).into_response(),
        QUI_NE_PARTAGE_PAS => (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "library_not_shared" })),
        )
            .into_response(),
        SANS_CHIFFRES => StatusCode::NO_CONTENT.into_response(),
        _ => (StatusCode::NOT_FOUND, Json(json!({ "error": "not_found" }))).into_response(),
    }
}

async fn lire_lieu(State(e): State<Etat>, h: HeaderMap) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    match e.lock().unwrap().lieu.clone() {
        Some(l) => Json(l).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn poser_lieu(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    let v: Value = serde_json::from_slice(&corps).unwrap();
    let mut g = e.lock().unwrap();
    g.corps_recus.push(v.clone());
    g.lieu = Some(v.clone());
    Json(v).into_response()
}

async fn retirer_lieu(State(e): State<Etat>, h: HeaderMap) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    e.lock().unwrap().lieu = None;
    StatusCode::NO_CONTENT.into_response()
}

fn lieux_attendus() -> Value {
    json!([{ "contact_id": 7, "label": "Budapest", "country_code": "HU", "lat": 47.5, "lon": 19.0 }])
}

async fn lieux(State(e): State<Etat>, h: HeaderMap) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    Json(lieux_attendus()).into_response()
}

/// rc9 (#7122) : la carte publique, telle que le cloud l'anonymise.
fn points_attendus() -> Value {
    json!([{ "lat": 46.0, "lon": 5.0 }, { "lat": 48.0, "lon": 2.0 }])
}

async fn points(State(e): State<Etat>, h: HeaderMap) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    Json(points_attendus()).into_response()
}

async fn lire_retrait(State(e): State<Etat>, h: HeaderMap) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    let v = e.lock().unwrap().retire_de_la_carte;
    Json(json!({ "public_map_opt_out": v })).into_response()
}

async fn poser_retrait(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    let mut g = e.lock().unwrap();
    g.corps_du_retrait.push(corps);
    g.retire_de_la_carte = true;
    Json(json!({ "public_map_opt_out": true })).into_response()
}

async fn lever_retrait(State(e): State<Etat>, h: HeaderMap, corps: Bytes) -> Response {
    if let Some(r) = autorise(&e, &h) {
        return r;
    }
    let mut g = e.lock().unwrap();
    g.corps_du_retrait.push(corps);
    g.retire_de_la_carte = false;
    Json(json!({ "public_map_opt_out": false })).into_response()
}

async fn demarrer() -> (String, Etat) {
    let etat: Etat = Arc::default();
    let app = Router::new()
        .route(
            "/api/v1/circle/contacts/{user_id}/library/summary",
            get(resume),
        )
        .route(
            "/api/v1/circle/me/location",
            get(lire_lieu).put(poser_lieu).delete(retirer_lieu),
        )
        .route("/api/v1/circle/contacts/locations", get(lieux))
        .route("/api/v1/circle/world/points", get(points))
        .route(
            "/api/v1/circle/me/public-map-opt-out",
            get(lire_retrait).put(poser_retrait).delete(lever_retrait),
        )
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (format!("http://{adresse}"), etat)
}

const ROUTES: [(&str, &str); 9] = [
    ("GET", "/contacts/7/library/summary"),
    ("GET", "/me/location"),
    ("PUT", "/me/location"),
    ("DELETE", "/me/location"),
    ("GET", "/contacts/locations"),
    ("GET", "/world/points"),
    ("GET", "/me/public-map-opt-out"),
    ("PUT", "/me/public-map-opt-out"),
    ("DELETE", "/me/public-map-opt-out"),
];

fn lieu_valide() -> Value {
    json!({ "label": "Talant", "country_code": "FR", "lat": 47.3361, "lon": 4.9969 })
}

#[tokio::test]
async fn sans_session_les_routes_rendent_412_et_rien_ne_part() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, None));
    for (m, chemin) in ROUTES {
        let corps = (m == "PUT" && chemin == "/me/location").then(lieu_valide);
        let r = appel(&app, m, chemin, corps).await;
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
async fn le_resume_est_relaye_avec_les_codes_du_cloud() {
    let (url, _etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    let r = appel(&app, "GET", "/contacts/7/library/summary", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), resume_attendu());

    let r = appel(&app, "GET", "/contacts/8/library/summary", None).await;
    assert_eq!(
        r.statut,
        StatusCode::FORBIDDEN,
        "pas de partage : 403 relayé"
    );
    assert_eq!(r.json(), json!({ "error": "library_not_shared" }));

    let r = appel(&app, "GET", "/contacts/42/library/summary", None).await;
    assert_eq!(
        r.statut,
        StatusCode::NOT_FOUND,
        "pas un contact : 404 relayé"
    );
    assert_eq!(r.json(), json!({ "error": "not_found" }));

    let r = appel(&app, "GET", "/contacts/9/library/summary", None).await;
    assert_eq!(
        r.statut,
        StatusCode::NO_CONTENT,
        "pas de chiffres : 204 relayé"
    );
    assert!(r.octets.is_empty());
}

#[tokio::test]
async fn le_lieu_se_pose_se_lit_et_se_retire() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    let r = appel(&app, "GET", "/me/location", None).await;
    assert_eq!(r.statut, StatusCode::NO_CONTENT, "désactivé par défaut");

    let mut envoi = lieu_valide();
    // Ce que le contrat ne prévoit pas ne part pas.
    envoi["address"] = json!("12 rue des Vignes");
    envoi["accuracy_m"] = json!(5);
    let r = appel(&app, "PUT", "/me/location", Some(envoi)).await;
    assert_eq!(r.statut, StatusCode::OK);
    let attendu = json!({ "label": "Talant", "country_code": "FR", "lat": 47.5, "lon": 5.0 });
    assert_eq!(r.json(), attendu);
    assert_eq!(
        etat.lock().unwrap().corps_recus,
        vec![attendu.clone()],
        "seuls les quatre champs du contrat partent, déjà arrondis à 0,5°"
    );

    let r = appel(&app, "GET", "/me/location", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), attendu);
    assert_eq!(
        r.entetes.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "un lieu ne reste dans aucun cache"
    );

    let r = appel(&app, "DELETE", "/me/location", None).await;
    assert_eq!(r.statut, StatusCode::NO_CONTENT);
    let r = appel(&app, "GET", "/me/location", None).await;
    assert_eq!(
        r.statut,
        StatusCode::NO_CONTENT,
        "le retrait efface le lieu"
    );
}

#[tokio::test]
async fn un_lieu_hors_bornes_est_refuse_sans_appel() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));
    for (corps, champ) in [
        (json!({ "label": "x", "lat": 91, "lon": 0 }), "lat"),
        (json!({ "label": "x", "lat": 0, "lon": -180.01 }), "lon"),
        (
            json!({ "label": "x".repeat(61), "lat": 0, "lon": 0 }),
            "label",
        ),
        (json!({ "label": "", "lat": 0, "lon": 0 }), "label"),
        (
            json!({ "label": "x", "lat": 0, "lon": 0, "country_code": "FRA" }),
            "country_code",
        ),
        (json!("Talant"), "body"),
    ] {
        let r = appel(&app, "PUT", "/me/location", Some(corps.clone())).await;
        assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "{corps}");
        // La forme de validation du cloud (Laravel), plus le code nommé.
        let v = r.json();
        assert_eq!(v["code"], json!("circle.invalid_body"), "{corps}");
        assert!(v["message"].is_string(), "{corps}");
        assert!(v["errors"][champ][0].is_string(), "{corps} → {v}");
        assert_eq!(v["errors"].as_object().unwrap().len(), 1, "{corps}");
    }
    let r = appel(&app, "PUT", "/me/location", None).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY, "corps vide");
    assert_eq!(
        etat.lock().unwrap().appels,
        0,
        "un corps refusé ne part pas"
    );
}

#[tokio::test]
async fn les_lieux_des_contacts_sont_relayes() {
    let (url, _etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));
    let r = appel(&app, "GET", "/contacts/locations", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), lieux_attendus());
}

#[tokio::test]
async fn cloud_en_panne_ou_injoignable_rend_503() {
    let (url, etat) = demarrer().await;
    etat.lock().unwrap().panne = true;
    let en_panne = app(base(&url, Some(JETON)));
    let morte = app(base(&adresse_morte().await, Some(JETON)));
    for (app, amont) in [(&en_panne, json!(500)), (&morte, Value::Null)] {
        for (m, chemin) in ROUTES {
            let corps = (m == "PUT" && chemin == "/me/location").then(lieu_valide);
            let r = appel(app, m, chemin, corps).await;
            assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE, "{m} {chemin}");
            assert_eq!(
                r.json(),
                json!({
                    "connected": true,
                    "code": "circle.cloud_unavailable",
                    "upstream_status": amont,
                }),
                "{m} {chemin}"
            );
        }
    }
    assert_eq!(etat.lock().unwrap().appels, ROUTES.len());
}

#[tokio::test]
async fn la_carte_publique_est_relayee_telle_quelle_et_sans_cache() {
    let (url, _etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));
    let r = appel(&app, "GET", "/world/points", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), points_attendus());
    assert_eq!(
        r.entetes.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "la carte publique ne reste dans aucun cache"
    );
}

#[tokio::test]
async fn l_interrupteur_de_retrait_est_relaye_sans_corps() {
    let (url, etat) = demarrer().await;
    let app = app(base(&url, Some(JETON)));

    let r = appel(&app, "GET", "/me/public-map-opt-out", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "public_map_opt_out": false }));

    // Le client pourrait envoyer n'importe quoi : rien n'en part.
    let r = appel(
        &app,
        "PUT",
        "/me/public-map-opt-out",
        Some(json!({ "lat": 47.3, "nom": "Alice" })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "public_map_opt_out": true }));
    assert_eq!(r.entetes.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let r = appel(&app, "GET", "/me/public-map-opt-out", None).await;
    assert_eq!(
        r.json(),
        json!({ "public_map_opt_out": true }),
        "effectif tout de suite"
    );

    let r = appel(&app, "DELETE", "/me/public-map-opt-out", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "public_map_opt_out": false }));

    let recus = etat.lock().unwrap().corps_du_retrait.clone();
    assert_eq!(recus.len(), 2);
    assert!(
        recus.iter().all(|c| c.is_empty()),
        "aucun corps ne part vers le cloud : {recus:?}"
    );
}
