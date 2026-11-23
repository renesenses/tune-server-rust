//! #8452 — le réglage Cover+ d'une station, par le vrai routeur. Aucun
//! réseau : la route ne fait que lire et écrire le réglage.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};

use crate::state::AppState;

async fn appel(
    app: &axum::Router,
    methode: &str,
    path: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(path);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn station() -> RadioStation {
    RadioStation {
        id: None,
        name: "Distorsión FM".into(),
        url: "https://flux.example/8452.mp3".into(),
        homepage: None,
        logo_url: Some("https://logos.example/8452.png".into()),
        country: Some("ES".into()),
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: false,
        last_played: None,
        play_count: 0,
    }
}

#[tokio::test]
async fn cover_plus_actif_par_defaut_se_coupe_et_se_retablit_par_station() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = RadioRepo::with_backend(state.backend.clone())
        .create(&station())
        .unwrap();
    let app = crate::routes::router(state.clone());
    let chemin = format!("/api/v1/radios/{id}/cover-plus");

    let (s, v) = appel(&app, "GET", &chemin, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["enabled"], true, "actif par défaut : {v}");

    let (s, v) = appel(&app, "PUT", &chemin, Some(json!({"enabled": false}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["enabled"], false);
    // Le réglage est celui que lit le sondeur radio.
    assert!(!tune_core::cover_plus::actif_pour_la_station(
        &state.backend,
        Some(id)
    ));
    let (_, v) = appel(&app, "GET", &chemin, None).await;
    assert_eq!(v["enabled"], false, "coupé : {v}");

    let (s, _) = appel(&app, "PUT", &chemin, Some(json!({"enabled": true}))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(tune_core::cover_plus::actif_pour_la_station(
        &state.backend,
        Some(id)
    ));
}

#[tokio::test]
async fn station_inconnue_404() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state);
    let (s, _) = appel(&app, "GET", "/api/v1/radios/987654/cover-plus", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = appel(
        &app,
        "PUT",
        "/api/v1/radios/987654/cover-plus",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn allume_par_defaut_l_interrupteur_du_serveur_l_eteint() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());

    let (s, v) = appel(&app, "GET", "/api/v1/radios/cover-plus", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"enabled": true, "artist_photo": true}), "allumé par défaut (rc14)");

    let (s, v) = appel(
        &app,
        "PUT",
        "/api/v1/radios/cover-plus",
        Some(json!({"enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"enabled": false, "artist_photo": true}));
    assert!(!tune_core::cover_plus::actif_globalement(&state.backend));
    let (_, v) = appel(
        &app,
        "PUT",
        "/api/v1/radios/cover-plus",
        Some(json!({"enabled": true})),
    )
    .await;
    assert_eq!(v, json!({"enabled": true, "artist_photo": true}));
    assert!(tune_core::cover_plus::actif_globalement(&state.backend));

    // Apple change sa page : on coupe le seul repli « photo ».
    let (_, v) = appel(
        &app,
        "PUT",
        "/api/v1/radios/cover-plus",
        Some(json!({"artist_photo": false})),
    )
    .await;
    assert_eq!(v, json!({"enabled": true, "artist_photo": false}));
    assert!(!tune_core::cover_plus::photo_artiste_active(&state.backend));
}
