//! #5428 — un rapport envoyé depuis Tune porte le compte de son auteur.
//!
//! Le site accepte désormais, sur `POST /api/v1/community/bug-report`, les
//! mêmes en-têtes que le support : `Authorization: Bearer <jeton SSO>` ou, à
//! défaut, `X-License-Key` (avec `X-Hardware-Fingerprint`). Sans eux, le
//! rapport reste accepté, « non identifié ».
//!
//! # Ce que ce fichier cloue
//!
//! Observé dans les en-têtes REÇUS par un faux site local, pas déduit du code :
//!
//! 1. avec un jeton SSO, `Authorization: Bearer <jeton>` part ;
//! 2. avec une licence seule, `X-License-Key` et `X-Hardware-Fingerprint`
//!    partent, et aucun `Authorization` ;
//! 3. sans aucune identité, AUCUN de ces en-têtes ne part et le rapport part
//!    quand même (200) — pas de 412, contrairement au support ;
//! 4. avec des captures (multipart), l'en-tête d'identité part aussi.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré en cible `[[test]]` dans `tune-server/Cargo.toml`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Les en-têtes d'identité que le faux site a RÉELLEMENT reçus.
#[derive(Default)]
struct Recu {
    content_type: String,
    authorization: Option<String>,
    license_key: Option<String>,
    fingerprint: Option<String>,
}

struct FauxSite {
    base: String,
    appels: Arc<AtomicUsize>,
    dernier: Arc<Mutex<Recu>>,
}

async fn faux_site() -> FauxSite {
    let appels = Arc::new(AtomicUsize::new(0));
    let dernier = Arc::new(Mutex::new(Recu::default()));

    let a = appels.clone();
    let d = dernier.clone();
    let app = axum::Router::new().route(
        "/api/v1/community/bug-report",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, _corps: axum::body::Bytes| {
                let a = a.clone();
                let d = d.clone();
                async move {
                    a.fetch_add(1, Ordering::SeqCst);
                    let lire = |nom: &str| {
                        headers
                            .get(nom)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                    };
                    *d.lock().await = Recu {
                        content_type: lire("content-type").unwrap_or_default(),
                        authorization: lire("authorization"),
                        license_key: lire("x-license-key"),
                        fingerprint: lire("x-hardware-fingerprint"),
                    };
                    axum::Json(json!({
                        "status": "submitted",
                        "thread": { "id": 7, "slug": "bug-essai", "url": "https://exemple/forum/threads/bug-essai" },
                    }))
                }
            },
        ),
    );
    let app = app.layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    FauxSite {
        base: format!("http://127.0.0.1:{port}"),
        appels,
        dernier,
    }
}

/// Un Tune pointé sur le faux site, avec les réglages d'identité donnés.
fn app(base: &str, reglages: &[(&str, &str)]) -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base).unwrap();
    for (cle, valeur) in reglages {
        settings.set(cle, valeur).unwrap();
    }
    tune_server::routes::router(state)
}

async fn poster(app: &axum::Router, ct: &str, corps: Vec<u8>) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/system/bug-report/submit")
                .header("content-type", ct)
                .body(Body::from(corps))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn poster_json(app: &axum::Router) -> (StatusCode, Value) {
    poster(
        app,
        "application/json",
        br#"{"description":"Rapport de test #5428."}"#.to_vec(),
    )
    .await
}

// --- 1 : jeton SSO ------------------------------------------------------

#[tokio::test]
async fn avec_un_jeton_sso_le_rapport_porte_le_bearer() {
    let site = faux_site().await;
    let app = app(&site.base, &[("mozaik_access_token", "jeton-sso-essai")]);

    let (code, reponse) = poster_json(&app).await;

    assert_eq!(code, StatusCode::OK, "réponse : {reponse}");
    assert_eq!(site.appels.load(Ordering::SeqCst), 1);
    let recu = site.dernier.lock().await;
    assert_eq!(
        recu.authorization.as_deref(),
        Some("Bearer jeton-sso-essai"),
        "avec un jeton SSO, le rapport doit porter `Authorization: Bearer` \
         pour être attribué à son auteur"
    );
    assert_eq!(recu.license_key, None, "le jeton prime sur la licence");
}

/// Le jeton prime sur la licence, exactement comme pour le support.
#[tokio::test]
async fn le_jeton_prime_sur_la_licence() {
    let site = faux_site().await;
    let app = app(
        &site.base,
        &[
            ("mozaik_access_token", "jeton-sso-essai"),
            ("license_key", "TUNE-CLE-ESSAI"),
        ],
    );

    let (code, _) = poster_json(&app).await;

    assert_eq!(code, StatusCode::OK);
    let recu = site.dernier.lock().await;
    assert_eq!(
        recu.authorization.as_deref(),
        Some("Bearer jeton-sso-essai")
    );
    assert_eq!(recu.license_key, None);
    assert_eq!(recu.fingerprint, None);
}

// --- 2 : licence seule --------------------------------------------------

#[tokio::test]
async fn avec_une_licence_seule_le_rapport_porte_la_cle() {
    let site = faux_site().await;
    let app = app(
        &site.base,
        &[
            ("license_key", "TUNE-CLE-ESSAI"),
            ("hardware_fingerprint", "empreinte-essai"),
        ],
    );

    let (code, reponse) = poster_json(&app).await;

    assert_eq!(code, StatusCode::OK, "réponse : {reponse}");
    let recu = site.dernier.lock().await;
    assert_eq!(
        recu.license_key.as_deref(),
        Some("TUNE-CLE-ESSAI"),
        "sans jeton SSO, le rapport doit porter `X-License-Key`"
    );
    assert_eq!(recu.fingerprint.as_deref(), Some("empreinte-essai"));
    assert_eq!(recu.authorization, None);
}

// --- 3 : aucune identité -------------------------------------------------

/// Pas de 412 : le rapport part, sans en-tête, exactement comme avant #5428.
#[tokio::test]
async fn sans_identite_le_rapport_part_sans_en_tete() {
    let site = faux_site().await;
    let app = app(&site.base, &[]);

    let (code, reponse) = poster_json(&app).await;

    assert_eq!(
        code,
        StatusCode::OK,
        "sans identité, le rapport doit partir quand même (pas de 412) : {reponse}"
    );
    assert_eq!(site.appels.load(Ordering::SeqCst), 1);
    let recu = site.dernier.lock().await;
    assert_eq!(recu.authorization, None);
    assert_eq!(recu.license_key, None);
    assert_eq!(recu.fingerprint, None);
}

// --- 4 : multipart --------------------------------------------------------

#[tokio::test]
async fn avec_des_captures_le_multipart_porte_aussi_l_identite() {
    let site = faux_site().await;
    let app = app(&site.base, &[("mozaik_access_token", "jeton-sso-essai")]);

    let limite = "----tune5428";
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    png.resize(64, 0);
    let mut corps: Vec<u8> = format!(
        "--{limite}\r\nContent-Disposition: form-data; name=\"description\"\r\n\r\n\
         Capture jointe.\r\n--{limite}\r\nContent-Disposition: form-data; \
         name=\"images[]\"; filename=\"capture.png\"\r\nContent-Type: \
         application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    corps.extend_from_slice(&png);
    corps.extend_from_slice(format!("\r\n--{limite}--\r\n").as_bytes());

    let (code, reponse) = poster(
        &app,
        &format!("multipart/form-data; boundary={limite}"),
        corps,
    )
    .await;

    assert_eq!(code, StatusCode::OK, "réponse : {reponse}");
    let recu = site.dernier.lock().await;
    assert!(
        recu.content_type.starts_with("multipart/form-data"),
        "avec une capture, l'envoi doit être multipart : {}",
        recu.content_type
    );
    assert_eq!(
        recu.authorization.as_deref(),
        Some("Bearer jeton-sso-essai"),
        "la branche multipart doit porter la même identité que la branche JSON"
    );
}
