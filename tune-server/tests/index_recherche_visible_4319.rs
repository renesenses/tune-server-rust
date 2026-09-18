//! #4319 — « je le vois dans Répertoires, la recherche ne le trouve pas ».
//!
//! Tades, fil forum 1841 (16/09/2026), 212 372 pistes : « quand je regarde
//! répertoire j'ai bien 2 albums Mahler par Mehta la 2 et la 3 ; quand je fais
//! une recherche je ne trouve que la 2 ».
//!
//! Deux explications très différentes se cachent derrière cette phrase — le mot
//! cherché ne correspond pas au titre indexé, ou **la ligne manque à l'index**.
//! Le rapport de diagnostic, celui que le testeur COLLE sur le forum, donnait le
//! nombre d'albums et jamais le nombre d'albums **indexés** : il ne permettait de
//! trancher ni l'une ni l'autre.
//!
//! Ce fichier cloue ce que le rapport doit dire. Il ne corrige aucun défaut de
//! recherche : il rend le défaut LISIBLE, ce qui est le préalable.
//!
//! ⚠️ `tune-server` porte `autotests = false` — déclaré dans `server_contracts.rs`.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

async fn rapport() -> (Value, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    let resp = app
        .oneshot(
            Request::get("/api/v1/system/bug-report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (serde_json::from_slice(&bytes).unwrap(), state)
}

/// Une bibliothèque vide : l'index est à zéro, et le rapport le DIT — zéro
/// mesuré n'est pas la même chose qu'aucune mesure.
#[tokio::test]
async fn le_rapport_annonce_la_couverture_de_l_index() {
    let (corps, _state) = rapport().await;
    let idx = &corps["library"]["search_index"];
    assert!(
        idx.is_object(),
        "le rapport doit porter la couverture de l'index : {corps}"
    );
    for table in ["albums", "tracks", "artists"] {
        assert_eq!(
            idx[table].as_i64(),
            Some(0),
            "table {table} : sur SQLite, le compte doit être lisible"
        );
    }
}

/// ⭐ Le cœur du ticket : un album AJOUTÉ est indexé, et l'écart se voit.
///
/// Les déclencheurs FTS posés par la migration indexent à l'insertion. Si un
/// jour ils manquent — base ancienne, migration partielle —, ce test tombe, et
/// c'est exactement le cas de Tades qu'il attraperait.
#[tokio::test]
async fn un_album_insere_entre_dans_l_index_et_le_rapport_le_montre() {
    let (_, state) = rapport().await;
    state
        .backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Zubin Mehta');\
             INSERT INTO albums (id, title, artist_id) VALUES \
               (1, 'Mahler: Symphony No. 2', 1), (2, 'Mahler: Symphony No. 3', 1);",
        )
        .unwrap();

    let app = tune_server::routes::router(state.clone());
    let resp = app
        .oneshot(
            Request::get("/api/v1/system/bug-report")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(corps["library"]["albums"].as_i64(), Some(2));
    assert_eq!(
        corps["library"]["search_index"]["albums"].as_i64(),
        Some(2),
        "les deux albums doivent être INDEXÉS : c'est ce qui distingue « le mot \
         ne correspond pas » de « la ligne manque à l'index »"
    );

    // Le markdown — ce que le testeur colle — porte la même chose, lisible.
    let md = corps["markdown"].as_str().unwrap_or_default();
    assert!(
        md.contains("Index de recherche : albums 2/2"),
        "le rapport collé doit montrer la couverture ; il porte : {}",
        md.lines()
            .find(|l| l.contains("Index de recherche"))
            .unwrap_or("<rien>")
    );
}
