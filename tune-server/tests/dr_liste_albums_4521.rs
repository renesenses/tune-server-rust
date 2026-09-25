//! #4521 — `GET /library/albums` porte le Dynamic Range de CHAQUE album.
//!
//! Mesuré sur le .18 le 19/09/2026 : la liste triait (`sort=dr`) et filtrait
//! (`dr_min`/`dr_max`) par DR sans jamais le rendre. Le client v2
//! (`LibraryV2.svelte`, `drNombre`/`hasDr`) lit `dynamic_range` sur chaque
//! album de la liste et cache son tri tant qu'aucun n'en porte : le tri DR
//! n'était jamais proposé.
//!
//! Épreuves contre le VRAI routeur et une vraie base SQLite en mémoire :
//!
//! - la clé est présente sur CHAQUE album, `null` sans DR (jamais `0`) ;
//! - la valeur est celle de la fiche `GET /library/albums/{id}`, album par
//!   album — tag d'album prioritaire, sinon moyenne des pistes, quel que soit
//!   leur producteur (tag, analyse : #3924) ;
//! - le coût, sur une base synthétique de 20 000 albums (`#[ignore]`, lancé à
//!   la main : `--ignored --nocapture`).
//!
//! Cible `[[test]]` propre (`autotests = false`), hors de `server_contracts`.

use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

/// Cinq albums, DR connus d'avance.
///
/// | album   | pistes → métadonnées                          | DR attendu |
/// |---------|-----------------------------------------------|------------|
/// | Alpha   | `dr_album=9`, `dr_track=7`                    | `"9"`      |
/// | Bravo   | `dr_track=12` (tag), `dr_track=14` (analyse)  | `"13"`     |
/// | Charlie | rien                                          | `null`     |
/// | Delta   | `dr_album=0`                                  | `"0"`      |
/// | Echo    | `dr_album=DR12.5`                             | `null`     |
fn bibliotheque() -> axum::Router {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // (id de piste, id d'album, titre, étiquettes)
    type Piste = (
        i64,
        i64,
        &'static str,
        &'static [(&'static str, &'static str)],
    );
    let pistes: [Piste; 6] = [
        (1, 1, "Alpha", &[("dr_album", "9"), ("dr_track", "7")]),
        (2, 2, "Bravo", &[("dr_track", "12"), ("dr_source", "tag")]),
        (
            3,
            2,
            "Bravo",
            &[("dr_track", "14"), ("dr_source", "analysis")],
        ),
        (4, 3, "Charlie", &[]),
        (5, 4, "Delta", &[("dr_album", "0")]),
        (6, 5, "Echo", &[("dr_album", "DR12.5")]),
    ];
    for (piste, album, titre, metas) in pistes {
        exec(
            &state,
            &format!(
                "INSERT INTO albums (id, title, source) SELECT {album}, '{titre}', 'local' \
                 WHERE NOT EXISTS (SELECT 1 FROM albums WHERE id = {album})"
            ),
        );
        exec(
            &state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, file_path, duration_ms, format) \
                 VALUES ({piste}, '{titre} {piste}', {album}, '/m/{piste}.flac', 200000, 'flac')"
            ),
        );
        for (k, v) in metas {
            exec(
                &state,
                &format!(
                    "INSERT INTO track_metadata (track_id, key, value) VALUES ({piste}, '{k}', '{v}')"
                ),
            );
        }
    }
    tune_server::routes::router(state)
}

#[tokio::test]
async fn chaque_album_de_la_liste_porte_son_dr_4521() {
    let app = bibliotheque();
    let (status, body) = get(&app, "/api/v1/library/albums?limit=50&sort=title").await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 5);
    let mut vu = Vec::new();
    for a in items {
        let titre = a["title"].as_str().unwrap().to_string();
        let dr = a
            .get("dynamic_range")
            .cloned()
            .unwrap_or_else(|| panic!("« {titre} » : clé `dynamic_range` ABSENTE de la liste"));
        vu.push((titre, dr));
    }
    assert_eq!(
        vu,
        vec![
            ("Alpha".to_string(), Value::from("9")),
            ("Bravo".to_string(), Value::from("13")),
            ("Charlie".to_string(), Value::Null),
            ("Delta".to_string(), Value::from("0")),
            ("Echo".to_string(), Value::Null),
        ],
        "tag d'album prioritaire ; moyenne tag+analyse ; null sans DR ; DR0 est une mesure"
    );
}

/// La liste et la fiche disent la MÊME chose, album par album : une valeur
/// identique, ou une absence des deux côtés (la fiche omet la clé, la liste
/// rend `null`).
#[tokio::test]
async fn la_liste_dit_le_dr_de_la_fiche_4521() {
    let app = bibliotheque();
    let (_, body) = get(&app, "/api/v1/library/albums?limit=50").await;
    for a in body["items"].as_array().expect("items") {
        let id = a["id"].as_i64().unwrap();
        let (status, fiche) = get(&app, &format!("/api/v1/library/albums/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        let de_la_fiche = fiche.get("dynamic_range").cloned().unwrap_or(Value::Null);
        assert_eq!(
            a["dynamic_range"], de_la_fiche,
            "album {id} ({}) : la liste et la fiche divergent",
            a["title"]
        );
    }
}

/// Le tri `sort=dr` et la valeur rendue concordent : décroissant, les
/// porteurs d'abord, les `null` en queue.
#[tokio::test]
async fn le_tri_serveur_suit_la_valeur_rendue_4521() {
    let app = bibliotheque();
    let (_, body) = get(&app, "/api/v1/library/albums?limit=50&sort=dr&order=desc").await;
    let drs: Vec<Value> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|a| a["dynamic_range"].clone())
        .collect();
    assert_eq!(
        drs,
        vec![
            Value::from("13"),
            Value::from("9"),
            Value::from("0"),
            Value::Null,
            Value::Null
        ]
    );
}

/// Coût, sur une base synthétique : 20 000 albums × 10 pistes, un album sur
/// deux mesuré (10 DR de piste), un sur dix tagué d'album. Page de 2 000
/// (celle des clients iOS/macOS), puis la liste entière.
#[tokio::test]
#[ignore = "mesure de coût, à lancer à la main"]
async fn cout_du_dr_sur_une_grosse_base_4521() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let n_albums = 20_000;
    exec(
        &state,
        &format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {n_albums}) \
             INSERT INTO albums (id, title, source) SELECT x, 'Album ' || x, 'local' FROM c"
        ),
    );
    exec(
        &state,
        &format!(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < {}) \
             INSERT INTO tracks (id, title, album_id, file_path, duration_ms, format) \
             SELECT x, 'T' || x, (x - 1) / 10 + 1, '/m/' || x || '.flac', 200000, 'flac' FROM c",
            n_albums * 10
        ),
    );
    exec(
        &state,
        "INSERT INTO track_metadata (track_id, key, value) \
         SELECT id, 'dr_track', CAST(5 + id % 11 AS TEXT) FROM tracks WHERE album_id % 2 = 0",
    );
    exec(
        &state,
        "INSERT INTO track_metadata (track_id, key, value) \
         SELECT id, 'dr_source', 'analysis' FROM tracks WHERE album_id % 2 = 0",
    );
    exec(
        &state,
        "INSERT INTO track_metadata (track_id, key, value) \
         SELECT id, 'dr_album', '12' FROM tracks WHERE album_id % 10 = 5",
    );
    // Du bruit dans le magasin ouvert : d'autres clés, sur toutes les pistes.
    exec(
        &state,
        "INSERT INTO track_metadata (track_id, key, value) \
         SELECT id, 'composer', 'X' FROM tracks",
    );
    let repo = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone());
    let app = tune_server::routes::router(state);

    for limit in [50_i64, 2000, n_albums] {
        let page = repo
            .list_filtered_seeded(
                limit, 0, "title", "asc", None, None, None, false, None, None,
            )
            .unwrap();
        let ids: Vec<i64> = page.iter().filter_map(|a| a.id).collect();
        let mut meilleure_dr = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let m = repo.dynamic_range_by_ids(&ids).unwrap();
            meilleure_dr = meilleure_dr.min(t.elapsed().as_secs_f64() * 1e3);
            assert!(!m.is_empty());
        }
        let mut meilleure_route = f64::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            let (status, body) = get(
                &app,
                &format!("/api/v1/library/albums?limit={limit}&sort=title"),
            )
            .await;
            meilleure_route = meilleure_route.min(t.elapsed().as_secs_f64() * 1e3);
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["items"].as_array().unwrap().len() as i64, limit);
        }
        println!(
            "MESURE #4521 — {n_albums} albums / {} pistes, page de {limit} : \
             DR seul {meilleure_dr:.1} ms, route entière {meilleure_route:.1} ms",
            n_albums * 10
        );
    }
}
