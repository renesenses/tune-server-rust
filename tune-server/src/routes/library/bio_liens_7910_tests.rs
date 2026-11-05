//! Témoins de #7910 sur les routes : `bio_segments` à côté de `bio`, sans
//! appel réseau (bio stockée dans la langue demandée).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::metadata::bio_segments::{self, GenreDeLien, LienExterne};

use crate::state::AppState;

const BIO_ARTISTE: &str = "Radiohead a été fondé par Thom Yorke. Leur album OK Computer \
                           paraît en 1997. Read more on Last.fm";
const BIO_ALBUM: &str = "OK Computer est le troisième album de Radiohead, avec Thom Yorke.";

fn etat() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    for (id, nom) in [(1i64, "Radiohead"), (2, "Thom Yorke")] {
        state
            .backend
            .execute(
                "INSERT INTO artists (id, name) VALUES (?, ?)",
                &[&id as &dyn ToSqlValue, &nom as &dyn ToSqlValue],
            )
            .unwrap();
    }
    let (album, titre, artiste) = (10i64, "OK Computer", 1i64);
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[
                &album as &dyn ToSqlValue,
                &titre as &dyn ToSqlValue,
                &artiste as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    ArtistRepo::with_backend(state.backend.clone())
        .update_bio_full(1, BIO_ARTISTE, "lastfm", None, "CC-BY-SA-3.0", "fr")
        .unwrap();
    AlbumRepo::with_backend(state.backend.clone())
        .update_bio_full(10, BIO_ALBUM, "wikipedia", None, "CC-BY-SA-4.0", "fr")
        .unwrap();
    state
}

async fn demander(state: &AppState, chemin: &str) -> (StatusCode, Value) {
    let resp = crate::routes::router(state.clone())
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn la_bio_d_artiste_porte_ses_segments_et_garde_son_texte_7910() {
    let state = etat();
    bio_segments::ranger_liens_externes(
        &state.backend,
        GenreDeLien::Artiste,
        1,
        BIO_ARTISTE,
        &[LienExterne {
            texte: "Read more on Last.fm".into(),
            url: "https://www.last.fm/music/Radiohead".into(),
        }],
    );
    let (status, corps) = demander(&state, "/api/v1/library/artists/1/bio?lang=fr").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["bio"],
        json!(BIO_ARTISTE),
        "le texte brut ne change pas"
    );
    assert_eq!(corps["artist"], json!("Radiohead"));
    let liens: Vec<Value> = corps["bio_segments"]
        .as_array()
        .expect("bio_segments présent")
        .iter()
        .filter(|s| s.get("lien").is_some() || s.get("url").is_some())
        .cloned()
        .collect();
    assert_eq!(
        liens,
        vec![
            json!({"texte": "Thom Yorke", "lien": {"type": "artiste", "id": 2}}),
            json!({"texte": "OK Computer", "lien": {"type": "album", "id": 10}}),
            json!({"texte": "Read more on Last.fm", "url": "https://www.last.fm/music/Radiohead"}),
        ],
        "Radiohead (la page) n'est pas lié ; Thom Yorke et OK Computer le sont ; \
         le lien Last.fm d'origine est gardé"
    );
}

#[tokio::test]
async fn la_bio_d_album_exclut_l_album_et_son_artiste_7910() {
    let state = etat();
    let (status, corps) = demander(&state, "/api/v1/library/albums/10/bio?lang=fr").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["bio"], json!(BIO_ALBUM));
    let liens: Vec<Value> = corps["bio_segments"]
        .as_array()
        .expect("bio_segments présent")
        .iter()
        .filter(|s| s.get("lien").is_some())
        .cloned()
        .collect();
    assert_eq!(
        liens,
        vec![json!({"texte": "Thom Yorke", "lien": {"type": "artiste", "id": 2}})]
    );
}

#[tokio::test]
async fn une_bio_reecrite_ne_reprend_pas_les_liens_de_l_ancienne_7910() {
    let state = etat();
    bio_segments::ranger_liens_externes(
        &state.backend,
        GenreDeLien::Artiste,
        1,
        "une autre bio",
        &[LienExterne {
            texte: "Read more on Last.fm".into(),
            url: "https://www.last.fm/music/Radiohead".into(),
        }],
    );
    let (_, corps) = demander(&state, "/api/v1/library/artists/1/bio?lang=fr").await;
    assert!(
        corps["bio_segments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s.get("url").is_none()),
        "{corps}"
    );
}

#[tokio::test]
async fn sans_bio_la_reponse_ne_change_pas_7910() {
    let state = etat();
    let (status, _) = demander(&state, "/api/v1/library/artists/999/bio").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
