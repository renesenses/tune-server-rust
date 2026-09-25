//! Les favoris LOCAUX (cœurs de piste, d'album, d'artiste) sur PostgreSQL.
//!
//! `ProfileRepo` liait `favorites.profile_id` et `favorites.item_id` en
//! CHAÎNE (`profile_id.to_string()`), avec ce commentaire : « the Postgres
//! mirror stores these columns as TEXT ». C'était vrai avant la migration 012.
//! Depuis, les deux colonnes sont `BIGINT` sur les DEUX naissances d'une base
//! PostgreSQL — scripts numérotés (installation native) comme bascule depuis
//! SQLite (012 les convertit, elles figurent dans son `fk_cols`). Mesuré le
//! 25/09/2026 sur PostgreSQL 16.15 :
//!
//! ```text
//! ERROR:  operator does not exist: bigint = text
//! STATEMENT:  SELECT id, profile_id, item_type, item_id, created_at
//!             FROM favorites WHERE profile_id = $1 ORDER BY created_at DESC
//! ```
//!
//! Conséquence sur toute installation PostgreSQL : mettre un titre, un album
//! ou un artiste en favori rend 500, la liste des favoris revient vide (la
//! route avale l'erreur), l'ordre manuel et le retrait échouent. SQLite, qui
//! compare `'1' = 1` sans broncher, ne voyait rien.
//!
//! Le scénario passe par les VRAIES routes (site d'appel, pas la fonction
//! pure) et tourne sur les deux moteurs : SQLite dans la suite ordinaire,
//! PostgreSQL quand `TUNE_TEST_PG_URL` est posée (étape dédiée de
//! `test-postgres.yml`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_server::state::AppState;

async fn appel(
    state: &AppState,
    methode: &str,
    route: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app = tune_server::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(route);
    let body = match corps {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    let valeur = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, valeur)
}

fn ids(liste: &Value) -> Vec<i64> {
    liste
        .as_array()
        .map(|a| a.iter().filter_map(|f| f["item_id"].as_i64()).collect())
        .unwrap_or_default()
}

/// Le parcours complet d'un cœur : poser, relire (brut, filtré, trié), ranger
/// à la main, retirer. Rend la liste des écarts, vide quand tout va bien.
async fn parcours(state: &AppState) -> Vec<String> {
    state
        .backend
        .execute("DELETE FROM favorites WHERE profile_id = 1", &[])
        .expect("vidage des favoris");
    let mut ecarts = Vec::new();

    for (genre, id) in [("track", 4101_i64), ("track", 4102), ("album", 4103)] {
        let (st, corps) = appel(
            state,
            "POST",
            "/api/v1/profiles/1/favorites/add",
            Some(json!({"item_type": genre, "item_id": id})),
        )
        .await;
        if st != StatusCode::CREATED {
            ecarts.push(format!("ajout {genre} {id} : {st} {corps}"));
        }
    }

    let (_, tous) = appel(state, "GET", "/api/v1/profiles/1/favorites", None).await;
    let mut vus = ids(&tous);
    vus.sort();
    if vus != vec![4101, 4102, 4103] {
        ecarts.push(format!("liste complète : {vus:?} ({tous})"));
    }

    let (_, pistes) = appel(
        state,
        "GET",
        "/api/v1/profiles/1/favorites?item_type=track",
        None,
    )
    .await;
    let mut vues = ids(&pistes);
    vues.sort();
    if vues != vec![4101, 4102] {
        ecarts.push(format!("liste filtrée : {vues:?} ({pistes})"));
    }

    let (st, corps) = appel(
        state,
        "POST",
        "/api/v1/profiles/1/favorites/reorder",
        Some(json!({"item_type": "track", "item_ids": [4102, 4101]})),
    )
    .await;
    if st != StatusCode::OK || corps["ordered"].as_i64() != Some(2) {
        ecarts.push(format!("ordre manuel : {st} {corps}"));
    }
    let (_, ranges) = appel(
        state,
        "GET",
        "/api/v1/profiles/1/favorites?item_type=track&sort=manual",
        None,
    )
    .await;
    if ids(&ranges) != vec![4102, 4101] {
        ecarts.push(format!(
            "relecture de l'ordre manuel : {:?} ({ranges})",
            ids(&ranges)
        ));
    }

    let (st, corps) = appel(
        state,
        "POST",
        "/api/v1/profiles/1/favorites/remove",
        Some(json!({"item_type": "track", "item_id": 4101})),
    )
    .await;
    if st != StatusCode::OK {
        ecarts.push(format!("retrait : {st} {corps}"));
    }
    let (_, apres) = appel(state, "GET", "/api/v1/profiles/1/favorites", None).await;
    let mut restent = ids(&apres);
    restent.sort();
    if restent != vec![4102, 4103] {
        ecarts.push(format!("après retrait : {restent:?} ({apres})"));
    }
    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn favoris_locaux_sur_sqlite() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = parcours(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_favoris_locaux_lies_en_entier() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    // Pas de `ok()?` : une connexion qui échoue doit ROUGIR, jamais sauter.
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    let ecarts = parcours(&state).await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
