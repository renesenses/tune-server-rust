//! #2117 — `uptime_seconds` doit mesurer CE processus, et le dire sans détour.
//!
//! Le champ est le premier qu'on regarde pour répondre à « le serveur a-t-il
//! redémarré ? ». Deux propriétés le rendent utilisable, et aucune n'était
//! tenue par un test :
//!
//! 1. il repart de zéro à chaque démarrage — un état neuf est un processus
//!    neuf, et son compteur ne doit rien hériter du précédent ;
//! 2. la réponse porte un horodatage ABSOLU du démarrage, pour qu'un
//!    redémarrage se CONSTATE au lieu de se déduire d'un écart de compteur.
//!
//! Un `AppState` fraîchement construit joue le rôle du redémarrage : c'est
//! exactement ce que fait un processus qui repart.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn parse_ancrage(body: &Value, route: &str) -> OffsetDateTime {
    let brut = body["process_started_at"]
        .as_str()
        .unwrap_or_else(|| panic!("{route} ne publie pas process_started_at : {body}"));
    OffsetDateTime::parse(brut, &Rfc3339).unwrap_or_else(|e| {
        panic!("{route} : process_started_at n'est pas du RFC 3339 ({brut}) : {e}")
    })
}

/// Les trois charges qui publient `uptime_seconds` publient aussi l'ancrage.
///
/// Elles doivent rendre la MÊME date pour un même processus : si `/diagnostics`
/// et `/profile` divergeaient, comparer deux appels ne prouverait plus rien.
#[tokio::test]
async fn les_routes_de_diagnostic_publient_l_ancrage_absolu() {
    let app = make_app();

    let (status, diag) = get_json(&app, "/api/v1/system/diagnostics").await;
    assert_eq!(status, StatusCode::OK);
    let ancrage_diag = parse_ancrage(&diag, "/system/diagnostics");
    assert!(
        diag["uptime_seconds"].is_number(),
        "le compteur relatif reste publié : ne rien casser chez ses lecteurs"
    );

    let (status, profil) = get_json(&app, "/api/v1/system/profile").await;
    assert_eq!(status, StatusCode::OK);
    let ancrage_profil = parse_ancrage(&profil["server"], "/system/profile");

    let (status, rapport) = get_json(&app, "/api/v1/system/bug-report").await;
    assert_eq!(status, StatusCode::OK);
    let ancrage_rapport = parse_ancrage(&rapport, "/system/bug-report");

    assert_eq!(
        ancrage_diag, ancrage_profil,
        "deux routes du même processus donnent deux dates de démarrage"
    );
    assert_eq!(
        ancrage_diag, ancrage_rapport,
        "deux routes du même processus donnent deux dates de démarrage"
    );
}

/// Après un « redémarrage » simulé, le compteur repart de zéro et l'ancrage
/// avance. C'est la propriété que le rapport #2117 croyait violée.
#[tokio::test]
async fn un_redemarrage_remet_le_compteur_a_zero_et_avance_l_ancrage() {
    let ancien = make_app();
    let (_, avant) = get_json(&ancien, "/api/v1/system/diagnostics").await;
    let ancrage_avant = parse_ancrage(&avant, "/system/diagnostics");

    // Laisser le compteur du premier processus décoller : sans cela, « il est
    // reparti de zéro » et « il n'a jamais bougé » sont indiscernables.
    tokio::time::sleep(std::time::Duration::from_millis(1_100)).await;

    let (_, encore) = get_json(&ancien, "/api/v1/system/diagnostics").await;
    let uptime_ancien = encore["uptime_seconds"].as_u64().expect("uptime_seconds");
    assert!(
        uptime_ancien >= 1,
        "le compteur du processus vivant n'avance pas : {uptime_ancien}"
    );

    // Redémarrage : un état neuf, donc un processus neuf.
    let nouveau = make_app();
    let (_, apres) = get_json(&nouveau, "/api/v1/system/diagnostics").await;

    let uptime_nouveau = apres["uptime_seconds"].as_u64().expect("uptime_seconds");
    assert!(
        uptime_nouveau < uptime_ancien,
        "uptime_seconds a survécu au redémarrage : {uptime_ancien} -> {uptime_nouveau}"
    );

    let ancrage_apres = parse_ancrage(&apres, "/system/diagnostics");
    assert!(
        ancrage_apres > ancrage_avant,
        "process_started_at a survécu au redémarrage : {ancrage_avant} -> {ancrage_apres}"
    );
}
