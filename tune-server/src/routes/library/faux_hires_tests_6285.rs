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

async fn montrer_douteux(app: &axum::Router, visible: bool) {
    let (s, c) = appel(
        app,
        Request::post("/api/v1/library/hires-check")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({ "douteux_visible": visible }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["douteux_visible"], visible, "{c}");
}

async fn get(app: &axum::Router, chemin: &str) -> Value {
    let (s, c) = appel(app, Request::get(chemin).body(Body::empty()).unwrap()).await;
    assert_eq!(s, StatusCode::OK, "{chemin} : {c}");
    c
}

/// Deuxième étape : « douteux » est masqué par défaut, sur la fiche aussi —
/// c'est elle que lit le badge de la fiche album du client web.
#[tokio::test]
async fn la_fiche_masque_douteux_par_defaut_6285() {
    let (app, _) = banc();
    let c = get(&app, "/api/v1/library/albums/5/hires").await;
    assert_eq!(
        c["verdict"], "non_analyse",
        "« douteux » ne doit pas apparaître par défaut — {c}"
    );
    assert_eq!(c["raison"], Value::Null, "{c}");
    assert_eq!(c["pistes"][1]["verdict"], "non_analyse", "{c}");
    assert_eq!(c["pistes"][1]["raison"], Value::Null, "{c}");
    assert_eq!(
        c["pistes"][1]["coupure_hz"], 21000.0,
        "la mesure brute reste rendue — {c}"
    );
    assert_eq!(c["pistes"][0]["verdict"], "verifie", "{c}");
}

#[tokio::test]
async fn le_verdict_d_album_et_de_ses_pistes_6285() {
    let (app, _) = banc();
    montrer_douteux(&app, true).await;
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
    assert_eq!(
        c["douteux_visible"], false,
        "« douteux » masqué par défaut — {c}"
    );
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
    assert_eq!(
        c["douteux_visible"], false,
        "un champ absent garde sa valeur — {c}"
    );
    montrer_douteux(&app, true).await;
    let c = get(&app, "/api/v1/library/hires-check").await;
    assert_eq!(c["active"], true, "{c}");
    assert_eq!(c["douteux_visible"], true, "{c}");
}

/// La liste des albums avec un album de chaque verdict : 5 douteux (une piste
/// vérifiée, une douteuse), 6 sans mesure, 7 vérifié, 8 conforme.
fn banc_de_la_liste() -> (axum::Router, Etat) {
    let (app, state) = banc();
    state
        .backend
        .execute_batch(
            "INSERT INTO albums (id, title) VALUES (7, 'Vérifié'), (8, 'Conforme');
             INSERT INTO tracks (id, title, album_id, file_path, format, duration_ms) VALUES
               (71, 'a', 7, '/m/71.flac', 'flac', 60000),
               (81, 'b', 8, '/m/81.flac', 'flac', 60000);
             INSERT INTO hires_verifications (track_id, verdict) VALUES
               (71, 'verifie'), (81, 'conforme');",
        )
        .unwrap();
    (app, state)
}

/// `id → (hires_verdict, hires_raison)` des lignes d'une page.
fn verdicts(page: &Value) -> Vec<(i64, Value, Value)> {
    let mut v: Vec<(i64, Value, Value)> = page["items"]
        .as_array()
        .unwrap_or_else(|| panic!("items attendu : {page}"))
        .iter()
        .map(|a| {
            (
                a["id"].as_i64().unwrap(),
                a["hires_verdict"].clone(),
                a["hires_raison"].clone(),
            )
        })
        .collect();
    v.sort_by_key(|(id, _, _)| *id);
    v
}

fn ids(page: &Value) -> Vec<i64> {
    verdicts(page).into_iter().map(|(id, _, _)| id).collect()
}

/// 🔴 La liste des albums porte le verdict publié de chaque album, et
/// « douteux » n'y apparaît pas par défaut.
#[tokio::test]
async fn la_liste_des_albums_porte_le_verdict_hires_6285() {
    let (app, _) = banc_de_la_liste();
    let page = get(&app, "/api/v1/library/albums?limit=50").await;
    assert_eq!(
        verdicts(&page),
        vec![
            (5, json!("non_analyse"), Value::Null),
            (6, json!("non_analyse"), Value::Null),
            (7, json!("verifie"), Value::Null),
            (8, json!("conforme"), Value::Null),
        ],
        "« douteux » masqué par défaut — {page}"
    );
    montrer_douteux(&app, true).await;
    let page = get(&app, "/api/v1/library/albums?limit=50").await;
    assert_eq!(
        verdicts(&page)[0],
        (5, json!("douteux"), json!("bande_de_cd")),
        "{page}"
    );
    // La recherche passe par le même corps de page.
    let page = get(&app, "/api/v1/library/albums?q=kind").await;
    assert_eq!(
        verdicts(&page),
        vec![(5, json!("douteux"), json!("bande_de_cd"))],
        "{page}"
    );
}

/// 🔴 Le filtre `?hires_verdict=` rend les albums du badge, son total est
/// celui des albums filtrés, et « douteux » masqué ne rend rien.
#[tokio::test]
async fn le_filtre_de_la_liste_sur_le_verdict_hires_6285() {
    let (app, _) = banc_de_la_liste();
    let page = get(&app, "/api/v1/library/albums?hires_verdict=verifie").await;
    assert_eq!(ids(&page), vec![7], "{page}");
    assert_eq!(
        page["total"], 1,
        "le total est celui des albums filtrés — {page}"
    );
    let page = get(&app, "/api/v1/library/albums?hires_verdict=conforme").await;
    assert_eq!(ids(&page), vec![8], "{page}");
    let page = get(&app, "/api/v1/library/albums?hires_verdict=douteux").await;
    assert_eq!(ids(&page), Vec::<i64>::new(), "masqué par défaut — {page}");
    assert_eq!(page["total"], 0, "{page}");
    let page = get(&app, "/api/v1/library/albums?hires_verdict=non_analyse").await;
    assert_eq!(
        ids(&page),
        vec![5, 6],
        "masqué, douteux est non analysé — {page}"
    );
    let page = get(&app, "/api/v1/library/albums?hires_verdict=n_importe").await;
    assert_eq!(
        ids(&page),
        Vec::<i64>::new(),
        "un mot inconnu ne rend rien — {page}"
    );

    montrer_douteux(&app, true).await;
    let page = get(&app, "/api/v1/library/albums?hires_verdict=douteux").await;
    assert_eq!(ids(&page), vec![5], "{page}");
    let page = get(&app, "/api/v1/library/albums?hires_verdict=non_analyse").await;
    assert_eq!(ids(&page), vec![6], "{page}");
    // Au-delà de la fin : la page est vide, le total reste celui du filtre.
    let page = get(
        &app,
        "/api/v1/library/albums?hires_verdict=douteux&offset=5",
    )
    .await;
    assert_eq!(ids(&page), Vec::<i64>::new(), "{page}");
    assert_eq!(page["total"], 1, "{page}");
    // Combiné à la recherche.
    let page = get(&app, "/api/v1/library/albums?hires_verdict=verifie&q=kind").await;
    assert_eq!(ids(&page), Vec::<i64>::new(), "{page}");
    // Sans filtre : toute la bibliothèque, comme avant.
    let page = get(&app, "/api/v1/library/albums").await;
    assert_eq!(ids(&page), vec![5, 6, 7, 8], "{page}");
}
