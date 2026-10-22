//! #6418 (Cyrille Moutia, fil 2296) — un album dont on change le genre dans
//! « Éditer l'album » doit apparaître sous son NOUVEAU genre dans
//! Bibliothèque › Genres, sans attendre un rechargement.
//!
//! La modale reporte la réponse de `PUT /library/albums/{id}` dans le magasin
//! des albums (`mettreAJourAlbum`), et l'onglet Genres groupe d'abord sur le
//! tableau `genres`, puis seulement sur la colonne `genre` (comme
//! `list_genres`). La réponse doit donc porter le tableau `genres` RÉÉCRIT par
//! la recopie de #5314, et non celui que l'album avait avant l'édition : sans
//! quoi l'album restait rangé sous son ancien genre (« The River » dans la
//! capture du testeur), alors que la modale affichait « Pop/Rock ».

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

type Etat = crate::state::AppState;

fn banc() -> Etat {
    let state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let b = &state.backend;
    b.execute(
        "INSERT INTO artists (id, name) VALUES (1, 'Bruce Springsteen')",
        &[],
    )
    .unwrap();
    b.execute(
        "INSERT INTO albums (id, title, artist_id, genre, genres) \
         VALUES (1, 'The River', 1, 'The River', '[\"The River\"]')",
        &[],
    )
    .unwrap();
    b.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, genre, genres) \
         VALUES (1, 'The River', 1, 1, '/m/the-river.m4a', 'The River', '[\"The River\"]')",
        &[],
    )
    .unwrap();
    state
}

async fn appel(state: &Etat, methode: &str, uri: &str, corps: Value) -> (StatusCode, Value) {
    let app = axum::Router::new()
        .nest("/library", crate::routes::library::router())
        .with_state(state.clone());
    let rep = app
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .header("content-type", "application/json")
                .body(if corps.is_null() {
                    Body::empty()
                } else {
                    Body::from(corps.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let octets = to_bytes(rep.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn genres_de(v: &Value) -> Vec<String> {
    v["genres"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default()
}

#[tokio::test]
async fn la_reponse_de_l_edition_porte_les_genres_reecrits() {
    let state = banc();

    let (s, v) = appel(
        &state,
        "PUT",
        "/library/albums/1",
        json!({ "genre": "Pop/Rock" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["genre"], "Pop/Rock", "{v}");
    assert_eq!(
        genres_de(&v),
        ["Pop", "Rock"],
        "#6418 : la réponse portait encore les genres d'AVANT l'édition ({v}) ; \
         le client range l'album d'après ce tableau"
    );

    // Et c'est bien ce que la base tient, et ce que relit une fiche d'album.
    let (s, relu) = appel(&state, "GET", "/library/albums/1", Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{relu}");
    assert_eq!(genres_de(&relu), genres_de(&v), "{relu}");
}

#[tokio::test]
async fn sans_changement_de_genre_la_reponse_garde_les_genres_de_l_album() {
    let state = banc();

    let (s, v) = appel(
        &state,
        "PUT",
        "/library/albums/1",
        json!({ "title": "The River (2015)" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(genres_de(&v), ["The River"], "{v}");
}
