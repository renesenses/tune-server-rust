//! #6285 — les routes des faux hi-res, contre le vrai routeur et une base en
//! mémoire.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

type Etat = crate::state::AppState;

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let r = app.clone().oneshot(req).await.unwrap();
    let s = r.status();
    let b = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

fn banc() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute_batch(
            "INSERT INTO albums (id, title) VALUES (5, 'Kind of Blue'), (6, 'Vide');
             INSERT INTO tracks (id, title, album_id, file_path, format, duration_ms) VALUES
               (51, 'So What', 5, '/m/51.flac', 'flac', 60000),
               (52, 'Blue in Green', 5, '/m/52.flac', 'flac', 60000),
               (53, 'Flamenco Sketches', 5, '/m/53.flac', 'flac', 60000);
             INSERT INTO hires_verifications (track_id, verdict, raison, coupure_hz, bits_effectifs, sample_rate, bits_declares)
               VALUES (51, 'verifie', NULL, NULL, 24, 96000, 24),
                      (52, 'douteux', 'bande_de_cd', 21000.0, 24, 96000, 24);",
        )
        .unwrap();
    let app = crate::routes::router(state.clone());
    (app, state)
}

#[tokio::test]
async fn le_verdict_d_album_et_de_ses_pistes_6285() {
    let (app, _) = banc();
    let (s, c) = appel(
        &app,
        Request::get("/api/v1/library/albums/5/hires")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(
        c["verdict"], "douteux",
        "une piste douteuse rend l'album douteux — {c}"
    );
    assert_eq!(c["raison"], "bande_de_cd", "{c}");
    assert_eq!(
        c["pistes"].as_array().unwrap().len(),
        2,
        "la piste 53 n'est pas mesurée — {c}"
    );
    assert_eq!(c["pistes"][1]["coupure_hz"], 21000.0, "{c}");

    let (_, c) = appel(
        &app,
        Request::get("/api/v1/library/albums/6/hires")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(c["verdict"], "non_analyse", "{c}");
}

#[tokio::test]
async fn la_passe_est_desactivee_par_defaut_et_s_arme_6285() {
    let (app, _) = banc();
    let (_, c) = appel(
        &app,
        Request::get("/api/v1/library/hires-check")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(c["active"], false, "désactivée par défaut — {c}");
    assert_eq!(c["restantes"], 1, "seule la piste 53 reste à mesurer — {c}");
    assert_eq!(c["rangees"], 2, "{c}");
    let (s, c) = appel(
        &app,
        Request::post("/api/v1/library/hires-check")
            .header("content-type", "application/json")
            .body(Body::from(json!({ "active": true }).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["active"], true, "{c}");
}
