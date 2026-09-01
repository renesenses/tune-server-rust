//! Un 429 du nuage doit arriver à l'écran en disant la limite et le délai (#2178).
//!
//! ## Le défaut
//!
//! Le relais du support a été appris à lire un 429 — motif `rate_limited`,
//! délai `retry_after`, message traduit (#2650, #2835). Les **autres** appelants
//! du nuage sont restés nus : chacun bâtissait sa chaîne
//! (`"artist image report failed: 429 Too Many Requests"`), la route en faisait
//! `{"error": "<cette chaîne>"}` sous un statut de son cru — 502 ici, 500 pour
//! le Playlist Hub, **200** pour Concerts / Nouveautés / Recommandations. Le
//! 429 et son `Retry-After` disparaissaient là, et l'écran ne pouvait dire que
//! « Une erreur est survenue ».
//!
//! ## Ce que ce fichier éprouve
//!
//! Le contrat rendu au client, sur **deux modules distincts** du nuage —
//! `cloud::community` et `cloud::plugins` — pris par leurs vraies routes :
//!
//! 1. face à un 429 portant `Retry-After: 30`, la charge utile **nomme la
//!    limite** (`error: "rate_limited"`), **porte le délai** (`retry_after: 30`,
//!    en-tête `Retry-After: 30`) et un message lisible dans la langue de
//!    l'interface — jamais un code HTTP seul ;
//! 2. sans en-tête, aucun délai n'est fabriqué ;
//! 3. **témoin** : un 503 repart exactement comme avant — même statut, même
//!    texte. Le correctif ne se paie pas d'une régression sur les autres refus.
//!
//! Le distant est un vrai serveur HTTP monté dans le test, qui répond
//! proprement le statut voulu : jamais de connexion coupée (RST), source
//! d'intermittence mesurée dans ce dépôt.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

/// Serveur de test qui refuse TOUT avec `status`.
///
/// `retry_after` pose l'en-tête quand il est fourni ; sinon le refus est muet
/// sur le délai, comme un limiteur mal configuré. Le corps reprend le seul
/// texte que sait produire le limiteur de Laravel.
async fn distant_refusant(status: u16, retry_after: Option<&'static str>) -> String {
    let app = axum::Router::new().fallback(move || async move {
        let mut resp = axum::response::IntoResponse::into_response((
            StatusCode::from_u16(status).unwrap(),
            axum::Json(json!({ "message": "Too Many Attempts." })),
        ));
        if let Some(secs) = retry_after {
            resp.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from_static(secs),
            );
        }
        resp
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    format!("http://127.0.0.1:{port}")
}

/// Routeur complet + réglage `mozaik_base_url` pointé sur le distant de test.
fn app_vers(base_url: &str) -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", base_url)
        .unwrap();
    tune_server::routes::router(state)
}

/// Envoie un POST JSON et rend (statut, en-tête `Retry-After`, corps).
async fn poster(
    app: &axum::Router,
    chemin: &str,
    corps: Value,
    langue: Option<&str>,
) -> (StatusCode, Option<String>, Value) {
    let mut req = Request::post(chemin).header("Content-Type", "application/json");
    if let Some(l) = langue {
        req = req.header("Accept-Language", l);
    }
    let reponse = app
        .clone()
        .oneshot(req.body(Body::from(corps.to_string())).unwrap())
        .await
        .unwrap();
    let status = reponse.status();
    let retry = reponse
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        retry,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// Les deux surfaces éprouvées : chemin de route et corps attendu par la route.
fn surfaces() -> Vec<(&'static str, Value)> {
    vec![
        // cloud::community — « signaler cette image d'artiste »
        (
            "/api/v1/cloud/community/artist-image",
            json!({ "mbid": "11111111-2222-3333-4444-555555555555",
                    "image_url": "https://exemple.test/a.jpg" }),
        ),
        // cloud::plugins — vote dans le magasin de greffons
        ("/api/v1/cloud/plugins/tune-dj/vote", json!({ "up": true })),
    ]
}

// ---------------------------------------------------------------------------
// 1. Le 429 arrive entier : motif, délai, en-tête, message
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_429_nomme_la_limite_et_porte_le_delai() {
    let base = distant_refusant(429, Some("30")).await;
    let app = app_vers(&base);

    for (chemin, corps) in surfaces() {
        let (status, retry, body) = poster(&app, chemin, corps, Some("fr-FR,fr;q=0.9")).await;

        // Le statut d'amont n'est plus écrasé : 429, pas 502.
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "chemin = {chemin}, corps = {body}"
        );

        // Le motif, code machine stable — pas une chaîne à deviner.
        assert_eq!(body["error"], json!("rate_limited"), "chemin = {chemin}");

        // Le délai, aux deux endroits qui comptent.
        assert_eq!(body["retry_after"], json!(30), "chemin = {chemin}");
        assert_eq!(retry.as_deref(), Some("30"), "chemin = {chemin}");

        // Et un texte que l'écran peut afficher tel quel.
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("trop de requêtes"),
            "chemin = {chemin}, message = {message}"
        );
        assert!(
            message.contains('1'),
            "30 s doivent s'annoncer « 1 min » : chemin = {chemin}, message = {message}"
        );

        // Le texte amont n'est pas perdu, il est déplacé.
        assert_eq!(
            body["upstream_message"],
            json!("Too Many Attempts."),
            "chemin = {chemin}"
        );
    }
}

#[tokio::test]
async fn le_message_suit_la_langue_de_l_interface() {
    let base = distant_refusant(429, Some("30")).await;
    let app = app_vers(&base);
    let (chemin, corps) = surfaces().remove(0);

    let (_, _, fr) = poster(&app, chemin, corps.clone(), Some("fr")).await;
    let (_, _, en) = poster(&app, chemin, corps, Some("en-US,en;q=0.9")).await;

    assert!(
        en["message"]
            .as_str()
            .unwrap()
            .contains("too many requests")
    );
    assert_ne!(fr["message"], en["message"]);
}

// ---------------------------------------------------------------------------
// 2. Aucun délai n'est fabriqué
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_429_sans_entete_ne_fabrique_aucun_delai() {
    let base = distant_refusant(429, None).await;
    let app = app_vers(&base);
    let (chemin, corps) = surfaces().remove(0);

    let (status, retry, body) = poster(&app, chemin, corps, Some("fr")).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "corps = {body}");
    assert_eq!(body["error"], json!("rate_limited"));
    assert!(body.get("retry_after").is_none(), "délai inventé : {body}");
    assert_eq!(retry, None, "en-tête inventé");
    // Le message existe quand même, il tait seulement l'attente.
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("trop de requêtes"), "message = {message}");
}

// ---------------------------------------------------------------------------
// 3. Témoin : les autres refus ne bougent pas d'un caractère
// ---------------------------------------------------------------------------

#[tokio::test]
async fn temoin_un_503_repart_comme_avant() {
    let base = distant_refusant(503, Some("30")).await;
    let app = app_vers(&base);

    let attendus = [
        "artist image report failed: 503 Service Unavailable",
        "plugin vote failed: 503 Service Unavailable",
    ];

    for ((chemin, corps), attendu) in surfaces().into_iter().zip(attendus) {
        let (status, retry, body) = poster(&app, chemin, corps, Some("fr")).await;

        assert_eq!(
            status,
            StatusCode::BAD_GATEWAY,
            "chemin = {chemin}, corps = {body}"
        );
        assert_eq!(body["error"], json!(attendu), "chemin = {chemin}");
        assert!(body.get("retry_after").is_none(), "chemin = {chemin}");
        assert_eq!(retry, None, "chemin = {chemin}");
    }
}
