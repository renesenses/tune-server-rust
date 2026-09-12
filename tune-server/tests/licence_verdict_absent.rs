//! #3673 (même motif, autre question) — « ce compte a-t-il le droit ? » était
//! répondu à **quatre** endroits pour la validation de licence, avec des
//! défauts opposés sur le champ manquant.
//!
//! Les quatre lisaient le corps rendu par `POST /api/v1/license/validate` :
//! `validate_stored_license` et `revalider_la_cle` défaillaient à `false`, le
//! battement de cœur à `true` **mais sous une garde sur `license_tier`**, et la
//! route `POST /cloud/license/validate` — celle du bouton « Valider » du
//! panneau — à `true` **sans cette garde**.
//!
//! Conséquence de cette quatrième lecture : un corps rendu en 200 sans champ de
//! licence (point d'accès redirigé par `mozaik_base_url`, enveloppe d'erreur
//! rendue en 200, schéma qui a bougé) valait `license_valid = true`, puis
//! `license_tier` absent valait `"free"` — donc `update_from_server(Free)`
//! **persisté** sur un compte premium, et une réponse `status:"validated"`.
//! Le bouton « Valider » rétrogradait un payeur en annonçant un succès.
//!
//! Ces témoins exercent la ROUTE, pas la fonction : ils passent par le routeur
//! monté. Le serveur de licences est bouchonné en local (`mozaik_base_url`),
//! jamais mozaiklabs.fr. Les deux sens sont couverts — ne pas rétrograder un
//! premium, et ne pas promouvoir un gratuit.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "TUNE-LIFETIME-3673";
const ACTIVATE: &str = "/api/v1/cloud/license/activate";
const VALIDATE: &str = "/api/v1/cloud/license/validate";
const STATUS: &str = "/api/v1/cloud/license/status";

/// Le corps que rend le faux serveur, échangeable en cours de test.
type Corps = Arc<Mutex<Value>>;

fn confirmation_premium() -> Value {
    json!({
        "license_valid": true,
        "license_tier": "premium",
        "license_expires_at": "2099-01-01T00:00:00Z",
    })
}

/// Faux mozaiklabs.fr : un vrai serveur HTTP tenu par `axum::serve` — un
/// bouchon qui coupe la connexion rend un RST et fabrique un test instable.
/// Le corps rendu est partagé, pour le changer entre deux appels.
async fn faux_serveur_de_licences(initial: Value) -> (String, Corps) {
    let corps: Corps = Arc::new(Mutex::new(initial));
    let partage = corps.clone();
    let app = axum::Router::new().route(
        "/api/v1/license/validate",
        axum::routing::post(move || {
            let partage = partage.clone();
            async move {
                let v = partage.lock().unwrap().clone();
                axum::Json(v)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), corps)
}

fn serveur_froid(base_url: &str) -> (axum::Router, tune_core::test_scratch::ScratchDir) {
    let base = tune_core::test_scratch::scratch_dir("tune-i3673-verdict");
    let db_path = base.join("library.db");
    let state = AppState::new(db_path.to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base_url).unwrap();
    (tune_server::routes::router(state), base)
}

async fn appeler(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn poster(app: &axum::Router, route: &str, corps: Value) -> (StatusCode, Value) {
    let req = Request::post(route)
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    appeler(app, req).await
}

async fn palier_courant(app: &axum::Router) -> Value {
    let (_, body) = appeler(app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    body["tier"].clone()
}

/// Le défaut : un compte premium qui actionne « Valider » pendant que le point
/// d'accès rend un 200 **sans verdict** retombait en Free, et la réponse
/// annonçait `status:"validated"`.
#[tokio::test]
async fn un_verdict_absent_ne_retrograde_pas_un_compte_premium() {
    let (base, corps) = faux_serveur_de_licences(confirmation_premium()).await;
    let (app, _garde) = serveur_froid(&base);

    let (status, body) = poster(&app, ACTIVATE, json!({ "license_key": CLE })).await;
    assert_eq!(status, StatusCode::OK, "activation : {body}");
    assert_eq!(
        body["tier"], "premium",
        "état de départ non premium : {body}"
    );

    // Le point d'accès répond désormais 200 avec un corps lisible mais SANS
    // aucun champ de licence — l'enveloppe que rend un relais ou une route
    // d'erreur qui n'a pas su rendre son code.
    *corps.lock().unwrap() = json!({ "ok": true });

    let (status, body) = poster(&app, VALIDATE, json!({})).await;
    assert_eq!(status, StatusCode::OK, "validation : {body}");
    assert_ne!(
        body["status"], "validated",
        "une validation sans verdict s'annonce comme un succès : {body}"
    );
    assert_eq!(
        body["tier"], "premium",
        "compte premium rétrogradé par un corps sans verdict : {body}"
    );

    assert_eq!(
        palier_courant(&app).await,
        "premium",
        "le palier PERSISTÉ est retombé en gratuit"
    );
}

/// La contrepartie, qui doit rester vraie : un verdict absent ne promeut rien
/// non plus. Un correctif qui ouvrirait le premium serait pire que le défaut.
#[tokio::test]
async fn un_verdict_absent_ne_promeut_pas_un_compte_gratuit() {
    // La clé est refusée : le serveur reste gratuit, la clé en attente.
    let (base, corps) = faux_serveur_de_licences(json!({ "license_valid": false })).await;
    let (app, _garde) = serveur_froid(&base);

    let (_, body) = poster(&app, ACTIVATE, json!({ "license_key": CLE })).await;
    assert_eq!(body["tier"], "free", "état de départ non gratuit : {body}");

    // Un corps qui NOMME un palier premium sans rendre de verdict ne doit pas
    // suffire à l'accorder.
    *corps.lock().unwrap() = json!({ "license_tier": "premium" });

    let (status, body) = poster(&app, VALIDATE, json!({})).await;
    assert_eq!(status, StatusCode::OK, "validation : {body}");
    assert_eq!(
        body["tier"], "free",
        "premium accordé sans verdict du serveur : {body}"
    );
    assert_eq!(
        palier_courant(&app).await,
        "free",
        "le palier PERSISTÉ est passé en premium sans verdict"
    );
}

/// Le vrai positif reste vrai : une confirmation en bonne et due forme valide
/// toujours, et s'annonce `validated`.
#[tokio::test]
async fn une_confirmation_en_regle_valide_toujours() {
    let (base, corps) = faux_serveur_de_licences(json!({ "license_valid": false })).await;
    let (app, _garde) = serveur_froid(&base);

    let (_, body) = poster(&app, ACTIVATE, json!({ "license_key": CLE })).await;
    assert_eq!(body["tier"], "free", "état de départ non gratuit : {body}");

    *corps.lock().unwrap() = confirmation_premium();

    let (status, body) = poster(&app, VALIDATE, json!({})).await;
    assert_eq!(status, StatusCode::OK, "validation : {body}");
    assert_eq!(body["status"], "validated", "réponse : {body}");
    assert_eq!(body["tier"], "premium", "réponse : {body}");
    assert_eq!(palier_courant(&app).await, "premium");
}
