//! #5068 — « Signaler un bogue » : la limite d'envoi du site se dit.
//!
//! Mesuré le 25/09/2026 dans le journal nginx de mozaiklabs.fr :
//! `POST /api/v1/community/bug-report` → 429 (5 refus, 6 acceptés depuis la
//! même adresse). La route du site est limitée à 5 envois par heure et par IP
//! (`throttle:5,60`). Le relais rendait `{"error":"cloud rejected the report",
//! "status":429}` : l'écran ne disait ni qu'il fallait attendre, ni combien.
//!
//! Ce fichier cloue, contre un faux site local (aucun réseau réel) :
//!
//! 1. ⭐ sur 429 AVEC `Retry-After`, le relais rend le code stable
//!    `rate_limited`, le délai du site sous `retry_after`, garde le statut 429
//!    et réémet l'en-tête `Retry-After` ;
//! 2. sur 429 SANS en-tête exploitable, `retry_after` est absent : aucun délai
//!    inventé ;
//! 3. hors 429, rien ne change : 502, et le code du site sous `status`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré en cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Un faux site qui refuse chaque envoi avec `statut`, et pose `Retry-After`
/// si on le lui donne. Il rend ce que Laravel rend sur `throttle`.
async fn faux_site(statut: u16, retry_after: Option<&'static str>) -> String {
    let app = axum::Router::new().route(
        "/api/v1/community/bug-report",
        axum::routing::post(move || async move {
            let mut resp = axum::response::IntoResponse::into_response((
                StatusCode::from_u16(statut).unwrap(),
                axum::Json(json!({ "message": "Too Many Attempts." })),
            ));
            if let Some(v) = retry_after {
                resp.headers_mut()
                    .insert("retry-after", axum::http::HeaderValue::from_static(v));
            }
            resp
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://127.0.0.1:{port}")
}

async fn envoyer(base: &str) -> (StatusCode, HeaderMap, Value) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", base)
        .unwrap();
    let app = tune_server::routes::router(state);
    let resp = app
        .oneshot(
            Request::post("/api/v1/system/bug-report/submit")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"description":"La liste saute."}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

#[tokio::test]
async fn limite_du_site_rend_rate_limited_et_le_delai() {
    let base = faux_site(429, Some("1740")).await;
    let (code, headers, corps) = envoyer(&base).await;

    assert_eq!(
        corps["error"],
        json!("rate_limited"),
        "⭐ #5068 : un 429 du site doit rendre le code stable `rate_limited`, \
         pas « cloud rejected the report » — sinon l'écran ne peut pas dire \
         que la limite d'envoi est atteinte. Corps rendu : {corps}"
    );
    assert_eq!(
        corps["retry_after"],
        json!(1740),
        "#5068 : le délai du `Retry-After` du site doit arriver sous \
         `retry_after`. Corps rendu : {corps}"
    );
    assert_eq!(
        code,
        StatusCode::TOO_MANY_REQUESTS,
        "#5068 : la limite n'est pas une panne ; un 5xx lèverait un bandeau \
         « Server error » dans l'interface. Corps rendu : {corps}"
    );
    assert_eq!(
        headers.get("retry-after").and_then(|v| v.to_str().ok()),
        Some("1740"),
        "le délai doit aussi partir sous sa forme standard `Retry-After`"
    );
}

#[tokio::test]
async fn limite_sans_retry_after_n_invente_aucun_delai() {
    let base = faux_site(429, None).await;
    let (code, headers, corps) = envoyer(&base).await;

    assert_eq!(corps["error"], json!("rate_limited"), "corps : {corps}");
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        corps.get("retry_after").is_none(),
        "sans `Retry-After` du site, aucun délai ne doit être inventé : {corps}"
    );
    assert!(headers.get("retry-after").is_none());
}

#[tokio::test]
async fn autre_refus_garde_son_code_http() {
    let base = faux_site(422, None).await;
    let (code, _headers, corps) = envoyer(&base).await;

    assert_eq!(code, StatusCode::BAD_GATEWAY, "corps : {corps}");
    assert_eq!(
        corps,
        json!({ "error": "cloud rejected the report", "status": 422 }),
        "hors 429, le refus garde sa forme et le code HTTP du site"
    );
}
