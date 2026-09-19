//! #4436 — « poser le drapeau ne RÉUNIT pas les vignettes » : la route de
//! fusion réunissait bien les pistes, mais laissait l'album réuni sous
//! l'artiste d'UNE des pistes (« Raz Olsher » sur le .18), et jetait les
//! marqueurs des albums absorbés. Le geste du client (onglet
//! « Compilations » : marquer, puis réunir) est rejoué ici de bout en bout.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_metadata_repo::AlbumMetadataRepo;
use tune_core::db::backend::ToSqlValue;

type Etat = crate::state::AppState;

fn serveur() -> (axum::Router, Etat) {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method("POST")
        .uri(chemin)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn inserer(state: &Etat, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    state.backend.execute(sql, params).unwrap();
    state.backend.last_insert_rowid()
}

fn artiste(state: &Etat, nom: &str) -> i64 {
    inserer(
        state,
        "INSERT INTO artists (name) VALUES (?)",
        &[&nom as &dyn ToSqlValue],
    )
}

fn album(state: &Etat, titre: &str, artist_id: i64) -> i64 {
    inserer(
        state,
        "INSERT INTO albums (title, artist_id, source, track_count) VALUES (?, ?, 'local', 0)",
        &[&titre as &dyn ToSqlValue, &artist_id],
    )
}

fn piste(state: &Etat, album_id: i64, artist_id: i64, numero: i64) {
    inserer(
        state,
        "INSERT INTO tracks (title, album_id, artist_id, track_number, file_path) VALUES (?, ?, ?, ?, ?)",
        &[
            &format!("Piste {numero}") as &dyn ToSqlValue,
            &album_id,
            &artist_id,
            &numero,
            &format!("/musique/coco/{numero:02}.flac"),
        ],
    );
}

fn compte(state: &Etat, sql: &str, id: i64) -> i64 {
    state
        .backend
        .query_one(sql, &[&id as &dyn ToSqlValue])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

fn artiste_de(state: &Etat, album_id: i64) -> String {
    state
        .backend
        .query_one(
            "SELECT ar.name FROM albums al JOIN artists ar ON ar.id = al.artist_id WHERE al.id = ?",
            &[&album_id as &dyn ToSqlValue],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .unwrap_or_default()
}

fn album_existe(state: &Etat, id: i64) -> bool {
    compte(state, "SELECT COUNT(*) FROM albums WHERE id = ?", id) > 0
}

/// « Coco María Presents Club Coco ¡AHORA! », éclaté en trois vignettes, une
/// par artiste de piste — deux pistes sous Raz Olsher, une sous chacun des
/// deux autres.
fn la_compilation_eclatee(state: &Etat) -> (Vec<i64>, i64) {
    const TITRE: &str = "Coco María Presents Club Coco ¡AHORA!";
    let raz = artiste(state, "Raz Olsher");
    let barth = artiste(state, "Barth");
    let poi = artiste(state, "Giorgio Poi");
    let a_raz = album(state, TITRE, raz);
    piste(state, a_raz, raz, 1);
    piste(state, a_raz, raz, 2);
    let a_barth = album(state, TITRE, barth);
    piste(state, a_barth, barth, 3);
    let a_poi = album(state, TITRE, poi);
    piste(state, a_poi, poi, 4);
    (vec![a_barth, a_raz, a_poi], a_raz)
}

/// Le geste de l'onglet « Compilations » : marquer, puis réunir. Une seule
/// ligne album à l'arrivée, sous l'artiste générique, drapeau tenu.
#[tokio::test]
async fn reunir_une_compilation_donne_un_seul_disque_sous_l_artiste_generique() {
    let (app, state) = serveur();
    let (ids, cible) = la_compilation_eclatee(&state);

    // 1. Marquer — le client le fait AVANT de réunir (#1183).
    let (statut, corps) = poster(
        &app,
        "/api/v1/library/albums/batch-update",
        json!({ "album_ids": ids, "is_compilation": true }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    // 2. Réunir.
    let (statut, corps) = poster(
        &app,
        "/api/v1/metadata/albums/merge",
        json!({ "album_ids": ids }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["master_id"].as_i64(),
        Some(cible),
        "la cible est l'album le plus fourni, pas le premier coché"
    );
    assert_eq!(corps["tracks_moved"].as_i64(), Some(2), "{corps}");
    assert_eq!(corps["total_tracks"].as_i64(), Some(4), "{corps}");
    for &id in &ids {
        assert_eq!(album_existe(&state, id), id == cible, "album {id}");
    }
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?",
            cible
        ),
        4
    );

    // 🔴 Le point du ticket : un seul disque, artiste d'album GÉNÉRIQUE —
    // pas « Raz Olsher » parce qu'il avait deux pistes.
    assert_eq!(artiste_de(&state, cible), "Various Artists");
    assert_eq!(corps["album_artist"], "Various Artists");

    // Le drapeau tient, et l'artiste comme le drapeau sont tenus à la main :
    // ni la passe de réparation ni le scan ne reviendront dessus (C3, #4427).
    assert_eq!(
        compte(
            &state,
            "SELECT COALESCE(is_compilation, 0) FROM albums WHERE id = ?",
            cible
        ),
        1
    );
    let tenus = AlbumMetadataRepo::with_backend(state.backend.clone())
        .champs_edites_a_la_main(cible)
        .unwrap();
    for champ in ["artist", "is_compilation"] {
        assert!(
            tenus.iter().any(|c| c == champ),
            "{champ} devrait être tenu à la main : {tenus:?}"
        );
    }
}

/// Contre-épreuve du périmètre : réunir deux éclats d'un disque ORDINAIRE
/// (onglet « Doublons », sans drapeau) ne touche pas à l'artiste et ne crée
/// aucun « Various Artists » pour rien.
#[tokio::test]
async fn reunir_sans_drapeau_garde_l_artiste_du_disque() {
    let (app, state) = serveur();
    let floyd = artiste(&state, "Pink Floyd");
    let gilmour = artiste(&state, "David Gilmour");
    let cible = album(&state, "The Wall", floyd);
    piste(&state, cible, floyd, 1);
    piste(&state, cible, floyd, 2);
    let eclat = album(&state, "The Wall", gilmour);
    piste(&state, eclat, gilmour, 3);

    let (statut, corps) = poster(
        &app,
        "/api/v1/metadata/albums/merge",
        json!({ "album_ids": [eclat, cible] }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["master_id"].as_i64(), Some(cible));
    assert!(!album_existe(&state, eclat));
    assert_eq!(artiste_de(&state, cible), "Pink Floyd");
    assert!(corps["album_artist"].is_null(), "{corps}");
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM artists WHERE name = 'Various Artists' AND ? = 1",
            1
        ),
        0
    );
}

/// Ce que la fusion à la main jetait : un favori posé sur un éclat suit
/// désormais la cible (c'est `absorber`, pas un `DELETE` nu).
#[tokio::test]
async fn le_favori_d_un_eclat_suit_la_cible() {
    let (app, state) = serveur();
    let (ids, cible) = la_compilation_eclatee(&state);
    let eclat = ids[0];
    inserer(
        &state,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', ?)",
        &[&eclat as &dyn ToSqlValue],
    );
    let (statut, corps) = poster(
        &app,
        "/api/v1/metadata/albums/merge",
        json!({ "album_ids": ids }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        compte(
            &state,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'album' AND item_id = ?",
            cible
        ),
        1,
        "le favori de l'éclat devait suivre la cible"
    );
}
