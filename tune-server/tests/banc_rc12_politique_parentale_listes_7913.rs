//! Banc rc12 (#7913, PR #8606) — « un artiste bloqué n'apparaît plus nulle
//! part » pour un profil supervisé, sur les listes que les notes de version
//! citent et que `politique_parentale_7913.rs` ne joue pas : l'accueil agrégé
//! (`/home`), les albums d'un GENRE, les albums d'une COLLECTION, la
//! recherche de la bibliothèque et la liste des pistes.
//!
//! Chaque route est d'abord ouverte par l'administrateur (témoin : l'artiste
//! bloqué y est bien), puis par le profil supervisé (il n'y est plus, l'artiste
//! libre y reste). Base en mémoire, aucun réseau.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;

fn app() -> (axum::Router, tune_server::state::AppState) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let db = &state.backend;
    let sql = |s: &str, p: &[&dyn ToSqlValue]| {
        db.execute(s, p).unwrap();
    };
    sql(
        "INSERT INTO profiles (id, username, display_name, is_admin) VALUES (2, 'enfant', 'Enfant', 0)",
        &[],
    );
    for (id, nom) in [(7i64, "Groupe Interdit"), (8, "Groupe Libre")] {
        sql("INSERT INTO artists (id, name) VALUES (?, ?)", &[&id, &nom]);
        sql(
            "INSERT INTO albums (id, title, artist_id, genre) VALUES (?, ?, ?, 'Banc Rock')",
            &[&(id * 10), &format!("Album de {nom}"), &id],
        );
        sql(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, genre) VALUES (?, ?, ?, ?, ?, 'flac', 'Banc Rock')",
            &[
                &(id * 100),
                &format!("Titre de {nom}"),
                &id,
                &(id * 10),
                &format!("/musique/{id}.flac"),
            ],
        );
    }
    // L'accueil « Ajouts récents » lit la date du fichier.
    sql(
        "UPDATE tracks SET file_mtime = CAST(strftime('%s', 'now') AS REAL)",
        &[],
    );
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: i64,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("X-Profile-Id", profil.to_string())
        .header("content-type", "application/json")
        .body(
            corps
                .map(|c| Body::from(c.to_string()))
                .unwrap_or_else(Body::empty),
        )
        .unwrap();
    let reponse = app.clone().oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn superviser(app: &axum::Router) {
    let (statut, corps) = appel(
        app,
        "PUT",
        "/api/v1/profiles/2/politique-parentale",
        ADMIN,
        Some(json!({"revision": 0, "politique": {
            "artistes_bloques": [{"id": 7, "nom": "Groupe Interdit"}],
        }})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
}

fn contient(corps: &Value, aiguille: &str) -> bool {
    corps.to_string().contains(aiguille)
}

/// Une collection (dossier) qui range les deux albums, créée par l'API.
async fn collection_des_deux_albums(app: &axum::Router) -> i64 {
    let (statut, corps) = appel(
        app,
        "POST",
        "/api/v1/library/collections",
        ADMIN,
        Some(json!({"name": "Banc parental"})),
    )
    .await;
    assert!(statut.is_success(), "{statut} {corps}");
    let id = corps["id"].as_i64().expect("identifiant de collection");
    for album in [70, 80] {
        let (statut, corps) = appel(
            app,
            "POST",
            &format!("/api/v1/library/collections/{id}/albums/{album}"),
            ADMIN,
            None,
        )
        .await;
        assert!(statut.is_success(), "ajout de {album} : {statut} {corps}");
    }
    id
}

#[tokio::test]
async fn banc_rc12_7913_genres_collections_recherche_accueil_et_pistes() {
    let (app, _state) = app();
    let collection = collection_des_deux_albums(&app).await;
    superviser(&app).await;
    let chemins = [
        "/api/v1/home".to_string(),
        "/api/v1/library/genres/Banc%20Rock/albums".to_string(),
        format!("/api/v1/library/collections/{collection}/albums"),
        "/api/v1/library/search?q=Groupe".to_string(),
        "/api/v1/library/tracks".to_string(),
    ];
    let mut fuites = Vec::new();
    for chemin in &chemins {
        let (statut, admin) = appel(&app, "GET", chemin, ADMIN, None).await;
        assert!(statut.is_success(), "{chemin} (admin) : {statut} {admin}");
        assert!(
            contient(&admin, "Groupe Interdit") && contient(&admin, "Groupe Libre"),
            "témoin : l'administrateur doit voir les deux artistes par {chemin} : {admin}"
        );
        let (statut, enfant) = appel(&app, "GET", chemin, ENFANT, None).await;
        assert!(statut.is_success(), "{chemin} (enfant) : {statut} {enfant}");
        assert!(
            contient(&enfant, "Groupe Libre"),
            "l'artiste libre doit rester visible par {chemin} : {enfant}"
        );
        if contient(&enfant, "Groupe Interdit") {
            fuites.push(format!("{chemin} : {enfant}"));
        }
    }
    assert!(
        fuites.is_empty(),
        "#7913 : artiste bloqué servi au profil supervisé par :\n{}",
        fuites.join("\n")
    );
}

/// Le total d'une liste paginée de PISTES compte seulement ce que le profil
/// voit (le banc existant le prouve pour les albums et les artistes).
#[tokio::test]
async fn banc_rc12_7913_le_total_des_pistes_est_exact() {
    let (app, _state) = app();
    superviser(&app).await;
    let chemin = "/api/v1/library/tracks?limit=1&offset=0";
    let (_, admin) = appel(&app, "GET", chemin, ADMIN, None).await;
    assert_eq!(admin["total"], 2, "témoin : deux pistes : {admin}");
    let (_, enfant) = appel(&app, "GET", chemin, ENFANT, None).await;
    assert_eq!(enfant["total"], 1, "#7913 : total des pistes : {enfant}");
}
