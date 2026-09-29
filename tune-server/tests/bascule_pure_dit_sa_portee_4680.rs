//! #4680 — la bascule PURE dit QUAND elle atteint le son, par le routeur réel.
//!
//! `POST /zones/{id}/audiophile` ne rendait que `applied_live`, un booléen à
//! plusieurs sens : `false` couvrait aussi bien « relance du flux à la
//! position courante » (zone réseau, s'entend dans l'instant) que « rien ne
//! joue » ou « piste suivante ». La barre de transport traduisait tout
//! « prendra effet à la piste suivante ». La réponse porte désormais `portee`,
//! même contrat que la route d'égaliseur ; `applied_live` reste, inchangé,
//! pour les clients qui ne lisent pas `portee`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'existe pour
//! `cargo test` que par son bloc `[[test]]` du manifeste.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

async fn poster(app: &axum::Router, zone: i64, corps: Value) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/zones/{zone}/audiophile"))
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = res.status();
    let octets = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn la_bascule_pure_publie_sa_portee_4680() {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some("dlna"), Some("dlna:uuid-4680-route"))
        .unwrap();
    let app = tune_server::routes::router(state);

    // Rien ne joue : la prochaine lecture partira en PURE. Rien à annoncer —
    // surtout pas « piste suivante ».
    let (statut, corps) = poster(&app, zone, json!({"enabled": true, "lock_volume": false})).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["portee"],
        json!("not_playing"),
        "la réponse doit dire QUAND la bascule s'entend : {corps}"
    );
    assert_eq!(
        corps["applied_live"],
        json!(false),
        "le booléen historique reste servi tel quel pour les clients d'avant : {corps}"
    );

    // Une requête qui ne touche que le verrou ne bascule rien : aucune portée.
    let (statut, corps) = poster(&app, zone, json!({"lock_volume": false})).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(
        corps.get("portee").is_some_and(Value::is_null),
        "sans bascule, `portee` est présent et `null` : {corps}"
    );
    assert_eq!(corps["applied_live"], json!(false));
}
