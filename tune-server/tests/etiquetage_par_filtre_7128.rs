//! #7128 (fil 2465 de r5yk5) — poser une étiquette sur TOUT le résultat d'un
//! filtre d'Oxygen, en un geste : `GET /library/tag-by-filter/preview` puis
//! `POST /library/tag-by-filter`.
//!
//! Le même scénario tourne sur SQLite (base de FICHIER, jamais `:memory:`,
//! #5043) et sur PostgreSQL (`TUNE_TEST_PG_URL`), et exige les mêmes
//! résultats. Il garde quatre propriétés :
//!
//! 1. seuls les objets du filtre reçoivent l'étiquette ;
//! 2. relancer la pose ne crée aucun doublon (`changed: 0`) ;
//! 3. un filtre vide est refusé, rien n'est écrit — pas de « tout taguer »
//!    par accident ;
//! 4. au-delà du plafond, rien n'est écrit non plus.
//!
//! Les identifiants et les genres sont propres à ce fichier (`…7128`) : sur
//! la base PostgreSQL partagée de la CI, aucune autre ligne ne répond au
//! filtre.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

const FILTRE_ROB: &str = "genre=Jazz7128&genre=Vocal7128&all_of=genre";

async fn appel(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
    let app = tune_server::routes::router(state.clone());
    let rep = app.oneshot(req).await.unwrap();
    let st = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

async fn apercu(state: &AppState, query: &str) -> (StatusCode, Value) {
    let uri = format!("/api/v1/library/tag-by-filter/preview?{query}");
    appel(state, Request::get(uri).body(Body::empty()).unwrap()).await
}

async fn poser(state: &AppState, query: &str, corps: Value) -> (StatusCode, Value) {
    let uri = format!("/api/v1/library/tag-by-filter?{query}");
    appel(
        state,
        Request::post(uri)
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

fn executer(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
}

fn ids(state: &AppState, sql: &str) -> Vec<i64> {
    state
        .backend
        .query_many(sql, &[])
        .unwrap_or_else(|e| panic!("{sql}\n{e}"))
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

fn poses(state: &AppState, tag: i64, item_type: &str) -> Vec<i64> {
    ids(
        state,
        &format!(
            "SELECT item_id FROM item_tags WHERE tag_id = {tag} AND item_type = '{item_type}' \
             ORDER BY item_id"
        ),
    )
}

fn nettoyer(state: &AppState) {
    executer(
        state,
        "DELETE FROM item_tags WHERE tag_id IN (SELECT id FROM tags WHERE name LIKE 'Vocal Jazz 7128%')",
    );
    executer(state, "DELETE FROM tags WHERE name LIKE 'Vocal Jazz 7128%'");
    executer(
        state,
        "DELETE FROM tracks WHERE (id BETWEEN 7128001 AND 7128099) OR genre = 'Masse7128'",
    );
    executer(
        state,
        "DELETE FROM albums WHERE id BETWEEN 7128101 AND 7128199",
    );
}

/// Deux albums, cinq pistes. Le filtre de Rob (Jazz ET Vocal) désigne les
/// pistes 1, 2 et 5, donc les albums 101 et 102. La piste 3 (Jazz seul) et
/// la piste 4 (Vocal seul) sont les témoins qui ne doivent RIEN recevoir.
fn semer(state: &AppState) {
    nettoyer(state);
    executer(
        state,
        "INSERT INTO albums (id, title, artist_id) VALUES (7128101, 'Ella 7128', NULL)",
    );
    executer(
        state,
        "INSERT INTO albums (id, title, artist_id) VALUES (7128102, 'Miles 7128', NULL)",
    );
    for (id, album, genre, genres) in [
        (
            7128001,
            "7128101",
            "Jazz7128",
            r#"["Jazz7128","Vocal7128"]"#,
        ),
        (
            7128002,
            "7128101",
            "Jazz7128",
            r#"["Jazz7128","Vocal7128"]"#,
        ),
        (7128003, "7128102", "Jazz7128", r#"["Jazz7128"]"#),
        (7128004, "NULL", "Vocal7128", r#"["Vocal7128"]"#),
        (
            7128005,
            "7128102",
            "Vocal7128",
            r#"["Jazz7128","Vocal7128"]"#,
        ),
    ] {
        executer(
            state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
                 format, genre, genres, sample_rate) \
                 VALUES ({id}, 'Piste {id}', {album}, NULL, '/music/7128/{id}.flac', 200000, \
                 'flac', '{genre}', '{genres}', 44100)"
            ),
        );
    }
}

async fn creer_tag(state: &AppState, nom: &str) -> i64 {
    let (st, v) = appel(
        state,
        Request::post("/api/v1/tags")
            .header("Content-Type", "application/json")
            .body(Body::from(json!({ "name": nom }).to_string()))
            .unwrap(),
    )
    .await;
    assert!(st.is_success(), "création de l'étiquette : {st} {v}");
    v["id"].as_i64().expect("id de l'étiquette")
}

async fn scenario(state: &AppState, moteur: &str) {
    semer(state);
    let tag = creer_tag(state, "Vocal Jazz 7128").await;

    // Aperçu : 3 morceaux, 2 albums.
    let (st, v) = apercu(state, &format!("item_type=track&{FILTRE_ROB}")).await;
    assert_eq!(st, StatusCode::OK, "{moteur} aperçu : {v}");
    assert_eq!(v["count"], 3, "{moteur} aperçu pistes : {v}");
    assert_eq!(v["over_limit"], false, "{moteur} : {v}");
    let (_, v) = apercu(state, &format!("item_type=album&{FILTRE_ROB}")).await;
    assert_eq!(v["count"], 2, "{moteur} aperçu albums : {v}");

    // 1. Seuls les morceaux filtrés reçoivent l'étiquette.
    let (st, v) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": tag, "item_type": "track" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{moteur} pose : {v}");
    assert_eq!(
        (v["matched"].as_i64(), v["changed"].as_i64()),
        (Some(3), Some(3)),
        "{moteur} : {v}"
    );
    assert_eq!(
        poses(state, tag, "track"),
        vec![7128001, 7128002, 7128005],
        "{moteur} : seules les pistes Jazz ET Vocal doivent porter l'étiquette"
    );

    // 2. Relancer ne crée aucun doublon.
    let (st, v) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": tag, "item_type": "track" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{moteur} relance : {v}");
    assert_eq!(
        (v["matched"].as_i64(), v["changed"].as_i64()),
        (Some(3), Some(0)),
        "{moteur} relance : {v}"
    );
    assert_eq!(
        ids(
            state,
            &format!("SELECT COUNT(*) FROM item_tags WHERE tag_id = {tag}")
        ),
        vec![3],
        "{moteur} : la relance a créé des doublons"
    );

    // 3. Un filtre vide est refusé, et rien n'est écrit.
    let autre = creer_tag(state, "Vocal Jazz 7128 vide").await;
    for vide in [
        "",
        "genre=",
        "limit=50&offset=0",
        "genre=&label=&folder=",
        "all_of=genre",
    ] {
        let (st, v) = poser(
            state,
            vide,
            json!({ "tag_id": autre, "item_type": "track" }),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "{moteur} filtre vide {vide:?} : {v}"
        );
        assert_eq!(v["code"], "empty_filter", "{moteur} {vide:?} : {v}");
        let (st, v) = apercu(state, &format!("item_type=track&{vide}")).await;
        assert_eq!(
            st,
            StatusCode::BAD_REQUEST,
            "{moteur} aperçu vide {vide:?} : {v}"
        );
    }
    assert!(
        poses(state, autre, "track").is_empty(),
        "{moteur} : un filtre vide a étiqueté des pistes"
    );

    // Albums : les deux albums du résultat, pas un de plus.
    let (st, v) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": tag, "item_type": "album" }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{moteur} albums : {v}");
    assert_eq!(
        poses(state, tag, "album"),
        vec![7128101, 7128102],
        "{moteur} albums"
    );

    // Retrait : le même geste, en sens inverse, idempotent lui aussi.
    let (st, v) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": tag, "item_type": "track", "remove": true }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{moteur} retrait : {v}");
    assert_eq!(v["changed"], 3, "{moteur} retrait : {v}");
    assert!(poses(state, tag, "track").is_empty(), "{moteur} retrait");
    assert_eq!(
        poses(state, tag, "album").len(),
        2,
        "{moteur} : le retrait des pistes a touché les albums"
    );
    let (_, v) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": tag, "item_type": "track", "remove": true }),
    )
    .await;
    assert_eq!(v["changed"], 0, "{moteur} second retrait : {v}");

    // Étiquette inconnue, type inconnu.
    let (st, _) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": 987_654_321, "item_type": "track" }),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{moteur} étiquette inconnue");
    let (st, _) = poser(
        state,
        FILTRE_ROB,
        json!({ "tag_id": tag, "item_type": "albums" }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{moteur} type inconnu");

    // 4. Au-delà du plafond, rien n'est écrit.
    let plafond = tune_core::db::track_repo::PLAFOND_ETIQUETAGE_PAR_FILTRE;
    executer(
        state,
        &format!(
            "INSERT INTO tracks (title, file_path, duration_ms, format, genre, sample_rate) \
             WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {}) \
             SELECT 'Masse', '/music/m7128/' || CAST(i AS TEXT) || '.flac', 1000, 'flac', \
             'Masse7128', 44100 FROM n",
            plafond + 1
        ),
    );
    let (st, v) = apercu(state, "item_type=track&genre=Masse7128").await;
    assert_eq!(st, StatusCode::OK, "{moteur} aperçu masse : {v}");
    assert_eq!(v["count"].as_i64(), Some(plafond + 1), "{moteur} : {v}");
    assert_eq!(v["over_limit"], true, "{moteur} : {v}");
    let (st, v) = poser(
        state,
        "genre=Masse7128",
        json!({ "tag_id": autre, "item_type": "track" }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{moteur} plafond : {v}"
    );
    assert_eq!(v["code"], "too_many_items", "{moteur} : {v}");
    assert!(
        poses(state, autre, "track").is_empty(),
        "{moteur} : le plafond a laissé passer des lignes"
    );

    nettoyer(state);
}

#[tokio::test(flavor = "multi_thread")]
async fn etiquetage_par_filtre_sur_sqlite() {
    let dossier = tune_core::test_scratch::scratch_dir("etiquetage-par-filtre-7128");
    let state = AppState::new(
        &dossier.join("tune.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState SQLite");
    scenario(&state, "SQLite").await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7128_etiquetage_par_filtre() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    scenario(&state, "PostgreSQL").await;
}
