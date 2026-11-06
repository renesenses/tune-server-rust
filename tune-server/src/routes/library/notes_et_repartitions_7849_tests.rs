//! #7849 — les routes des notes personnelles et la clé `distributions` de
//! `GET /library/stats`, de bout en bout par le routeur (SQLite). Les mêmes
//! propriétés sur PostgreSQL vivent dans tune-core
//! (`db::notes_et_repartitions_7849_tests::pg_7849_notes_et_repartitions`).
//!
//! * `notes_par_les_routes_7849` : création, lecture, mise à jour, liste,
//!   recherche `q`, suppression, type inconnu refusé ;
//! * `notes_par_les_routes_separees_par_profil_7849` : le témoin de la
//!   contre-épreuve — un profil ne lit, ne liste, ne trouve, ne remplace ni ne
//!   supprime la note d'un autre ;
//! * `les_notes_d_album_existantes_se_lisent_par_la_route_7849` : une base
//!   d'avant la migration 138 rouverte rend ses notes d'album par la route ;
//! * `les_stats_gagnent_les_repartitions_sans_changer_le_reste_7849`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::profile_repo::ProfileRepo;

async fn appeler(
    state: &AppState,
    profil: Option<i64>,
    methode: &str,
    url: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app = crate::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(url);
    if let Some(p) = profil {
        req = req.header("X-Profile-Id", p.to_string());
    }
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn profil(state: &AppState, nom: &str) -> i64 {
    ProfileRepo::with_backend(state.backend.clone())
        .create(nom, None, None)
        .unwrap()
}

fn ids(liste: &Value) -> Vec<(String, i64)> {
    let mut v: Vec<(String, i64)> = liste["notes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| {
            (
                n["item_type"].as_str().unwrap().to_string(),
                n["item_id"].as_i64().unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

const NOTES: &str = "/api/v1/library/notes";

#[tokio::test]
async fn notes_par_les_routes_7849() {
    let s = etat();
    let p = profil(&s, "p-7849");
    let une = format!("{NOTES}/track/12");

    let (st, _) = appeler(&s, Some(p), "GET", &une, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, v) = appeler(
        &s,
        Some(p),
        "PUT",
        &une,
        Some(json!({"texte_md": "## Solo\n<script>x</script> à 2:10"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["item_type"], "track");
    assert_eq!(v["item_id"], 12);
    assert!(v["maj_le"].as_str().is_some_and(|d| d.ends_with('Z')));

    let (st, v) = appeler(&s, Some(p), "GET", &une, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        v["texte_md"], "## Solo\n<script>x</script> à 2:10",
        "texte gardé brut : l'assainissement est au rendu"
    );

    appeler(
        &s,
        Some(p),
        "PUT",
        &une,
        Some(json!({"texte_md": "Solo de piano"})),
    )
    .await;
    appeler(
        &s,
        Some(p),
        "PUT",
        &format!("{NOTES}/artist/3"),
        Some(json!({"texte_md": "Vu au Duc des Lombards"})),
    )
    .await;
    appeler(
        &s,
        Some(p),
        "PUT",
        &format!("{NOTES}/album/5"),
        Some(json!({"texte_md": "PIANO un peu en retrait"})),
    )
    .await;
    let (_, v) = appeler(&s, Some(p), "GET", &une, None).await;
    assert_eq!(v["texte_md"], "Solo de piano");

    let (st, v) = appeler(&s, Some(p), "GET", NOTES, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["count"], 3);
    let (_, v) = appeler(&s, Some(p), "GET", &format!("{NOTES}?q=piano"), None).await;
    assert_eq!(
        ids(&v),
        vec![("album".to_string(), 5), ("track".to_string(), 12)]
    );
    let (_, v) = appeler(
        &s,
        Some(p),
        "GET",
        &format!("{NOTES}?q=piano&item_type=album"),
        None,
    )
    .await;
    assert_eq!(ids(&v), vec![("album".to_string(), 5)]);

    let (st, v) = appeler(&s, Some(p), "DELETE", &une, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["deleted"], true);
    let (st, _) = appeler(&s, Some(p), "GET", &une, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Un texte vide supprime.
    let (st, v) = appeler(
        &s,
        Some(p),
        "PUT",
        &format!("{NOTES}/artist/3"),
        Some(json!({"texte_md": "  "})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["deleted"], true);

    let (st, _) = appeler(&s, Some(p), "GET", &format!("{NOTES}/playlist/1"), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = appeler(
        &s,
        Some(p),
        "GET",
        &format!("{NOTES}?item_type=genre"),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = appeler(
        &s,
        Some(p),
        "PUT",
        &format!("{NOTES}/album/5"),
        Some(json!({"texte_md": "x".repeat(64 * 1024 + 1)})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn notes_par_les_routes_separees_par_profil_7849() {
    let s = etat();
    let alice = profil(&s, "alice-7849");
    let bruno = profil(&s, "bruno-7849");
    let album = format!("{NOTES}/album/40");

    appeler(
        &s,
        Some(alice),
        "PUT",
        &album,
        Some(json!({"texte_md": "Note d'Alice"})),
    )
    .await;

    let (st, _) = appeler(&s, Some(bruno), "GET", &album, None).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "#7849 : Bruno lit la note d'Alice"
    );
    let (_, v) = appeler(&s, Some(bruno), "GET", NOTES, None).await;
    assert_eq!(
        v["count"], 0,
        "#7849 : la liste de Bruno montre la note d'Alice : {v}"
    );
    let (_, v) = appeler(&s, Some(bruno), "GET", &format!("{NOTES}?q=alice"), None).await;
    assert_eq!(
        v["count"], 0,
        "#7849 : la recherche de Bruno trouve la note d'Alice : {v}"
    );

    let (_, v) = appeler(&s, Some(bruno), "DELETE", &album, None).await;
    assert_eq!(
        v["deleted"], false,
        "#7849 : Bruno a supprimé une note qui n'est pas la sienne"
    );
    appeler(
        &s,
        Some(bruno),
        "PUT",
        &album,
        Some(json!({"texte_md": "Note de Bruno"})),
    )
    .await;

    let (st, v) = appeler(&s, Some(alice), "GET", &album, None).await;
    assert_eq!(st, StatusCode::OK, "#7849 : la note d'Alice a disparu");
    assert_eq!(
        v["texte_md"], "Note d'Alice",
        "#7849 : l'écriture de Bruno a remplacé la note d'Alice"
    );
    let (_, v) = appeler(&s, Some(bruno), "GET", &album, None).await;
    assert_eq!(v["texte_md"], "Note de Bruno");
}

#[tokio::test]
async fn les_notes_d_album_existantes_se_lisent_par_la_route_7849() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("notes-7849.db");
    let chemin = chemin.to_str().unwrap();
    let p2 = {
        let s = AppState::new(chemin, 0, Default::default()).unwrap();
        let p2 = profil(&s, "p2-7849");
        let b = s.backend.clone();
        b.execute(
            "INSERT INTO albums (id, title) VALUES (1, 'Kind of Blue')",
            &[],
        )
        .unwrap();
        // Base d'avant la 138 : ni table, ni marque.
        b.execute("DROP TABLE notes_personnelles", &[]).unwrap();
        b.execute("DELETE FROM _migrations WHERE version >= 138", &[])
            .unwrap();
        b.execute(
            "INSERT INTO album_ratings (album_id, profile_id, rating, note) VALUES \
             (1, 1, 5, 'Écouté en boucle'), (1, ?, 3, NULL)",
            &[&p2 as &dyn ToSqlValue],
        )
        .unwrap();
        p2
    };
    // Redémarrage : les migrations rejouent la 138.
    let s = AppState::new(chemin, 0, Default::default()).unwrap();
    let (st, v) = appeler(&s, None, "GET", &format!("{NOTES}/album/1"), None).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "#7849 : la note d'album n'a pas été reprise"
    );
    assert_eq!(v["texte_md"], "Écouté en boucle");
    let (st, _) = appeler(&s, Some(p2), "GET", &format!("{NOTES}/album/1"), None).await;
    assert_eq!(
        st,
        StatusCode::NOT_FOUND,
        "une note sur cinq sans texte ne fait pas de note"
    );
    // L'ancienne colonne est toujours lue par l'export, et suit la nouvelle route.
    appeler(
        &s,
        None,
        "PUT",
        &format!("{NOTES}/album/1"),
        Some(json!({"texte_md": "Écouté en boucle, encore"})),
    )
    .await;
    let (_, v) = appeler(&s, None, "GET", "/api/v1/library/ratings/export", None).await;
    assert_eq!(v["ratings"][0]["note"], "Écouté en boucle, encore");
}

#[tokio::test]
async fn les_stats_gagnent_les_repartitions_sans_changer_le_reste_7849() {
    let s = etat();
    let b = s.backend.clone();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (10, 'Miles Davis')",
        "INSERT INTO albums (id, title, year) VALUES (100, 'A', 1959)",
        "INSERT INTO tracks (id, album_id, title, file_path, format, sample_rate, year) VALUES \
            (1, 100, 'a', '/m/a', 'flac', 44100, NULL), \
            (2, 100, 'b', '/m/b', 'FLAC', 96000, 1961), \
            (3, NULL, 'c', '/m/c', 'mp3', 44100, NULL)",
        "INSERT INTO track_credits (track_id, artist_id, artist_name, role) VALUES \
            (1, 10, 'Miles Davis', 'performer'), (2, 10, 'Miles Davis', 'performer'), \
            (2, NULL, 'Bill Evans', 'performer'), (1, NULL, 'Miles Davis', 'composer')",
    ] {
        b.execute(sql, &[]).unwrap();
    }
    let (st, v) = appeler(&s, None, "GET", "/api/v1/library/stats", None).await;
    assert_eq!(st, StatusCode::OK);
    for cle in [
        "artists",
        "albums",
        "tracks",
        "listens",
        "zones",
        "total_duration_ms",
        "total_size_bytes",
        "tracks_by_source",
        "upnp_import",
    ] {
        assert!(v.get(cle).is_some(), "clé existante perdue : {cle}");
    }
    assert_eq!(v["tracks"], 3);
    assert_eq!(
        v["distributions"],
        json!({
            "by_codec": [
                {"value": "flac", "tracks": 2},
                {"value": "mp3", "tracks": 1},
            ],
            "by_sample_rate": [
                {"value": 44100, "tracks": 2},
                {"value": 96000, "tracks": 1},
            ],
            "by_decade": [
                {"value": 1950, "tracks": 1},
                {"value": 1960, "tracks": 1},
                {"value": null, "tracks": 1},
            ],
            "by_credit_role": [
                {"value": "performer", "tracks": 2, "people": 2},
                {"value": "composer", "tracks": 1, "people": 1},
            ],
        })
    );
}
