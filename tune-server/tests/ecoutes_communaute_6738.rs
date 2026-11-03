//! #6738 — « So What (128) » : les routes de la bibliothèque posent
//! `community_listens` sur les pistes, les albums et les artistes, lus au
//! site par lot (`POST /api/v1/community/listen-counts`). Éprouvé contre un
//! faux site local, sur le vrai routeur.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::{
    album_repo::AlbumRepo,
    artist_repo::ArtistRepo,
    models::{Album, Artist, Track},
    settings_repo::SettingsRepo,
    track_repo::TrackRepo,
};

type Recus = Arc<Mutex<Vec<Value>>>;

/// Le faux site : 128 écoutes pour « So What », 5 400 pour « Kind of Blue »,
/// 43 100 pour « Miles Davis » ; le reste est sous le seuil (absent).
async fn faux_site() -> (String, Recus) {
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};

    async fn compter(State(r): State<Recus>, Json(c): Json<Value>) -> Json<Value> {
        r.lock().unwrap().push(c.clone());
        let items: Vec<Value> = c["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                let n = match (
                    e["type"].as_str()?,
                    e["title"].as_str(),
                    e["artist"].as_str(),
                ) {
                    ("recording", Some("So What"), _) => 128,
                    ("release", Some("Kind of Blue"), _) => 5400,
                    ("artist", _, Some("Miles Davis")) => 43100,
                    _ => return None,
                };
                Some(json!({"id": e["id"], "listens": n}))
            })
            .collect();
        Json(json!({ "items": items }))
    }
    let recus: Recus = Arc::default();
    let app = Router::new()
        .route("/api/v1/community/listen-counts", post(compter))
        .with_state(recus.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), recus)
}

struct Bibliotheque {
    app: axum::Router,
    reglages: SettingsRepo,
    artiste: i64,
    album: i64,
}

fn bibliotheque() -> Bibliotheque {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("bibliotheque isolee");
    let artiste = ArtistRepo::with_backend(etat.backend.clone())
        .create(&Artist::new("Miles Davis".into()))
        .unwrap();
    let mut album = Album::new("Kind of Blue".into());
    album.artist_id = Some(artiste);
    album.source = "local".into();
    let album = AlbumRepo::with_backend(etat.backend.clone())
        .create(&album)
        .unwrap();
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    for (n, titre) in ["So What", "Freddie Freeloader"].into_iter().enumerate() {
        let mut piste = Track::new(titre.into());
        piste.artist_id = Some(artiste);
        piste.album_id = Some(album);
        piste.file_path = Some(format!("/fixture-6738/{n}.flac"));
        piste.track_number = n as i32 + 1;
        piste.duration_ms = 60_000;
        pistes.create(&piste).unwrap();
    }
    Bibliotheque {
        reglages: SettingsRepo::with_backend(etat.backend.clone()),
        app: tune_server::routes::router(etat),
        artiste,
        album,
    }
}

fn relier(b: &Bibliotheque, url: &str, envoi: bool) {
    let s = &b.reglages;
    s.set("mozaik_base_url", url).unwrap();
    s.set("mozaik_access_token", "jeton-de-test").unwrap();
    s.set("server_id", "serveur-6738").unwrap();
    s.set(
        tune_core::cloud::library_sync::CLE_JETON_DE_LIAISON,
        "liaison-de-test",
    )
    .unwrap();
    s.set(
        tune_core::cloud::ecoutes_au_compte::CLE_ACTIVATION,
        if envoi { "true" } else { "false" },
    )
    .unwrap();
}

async fn get(app: &axum::Router, chemin: &str) -> Value {
    let r = app
        .clone()
        .oneshot(
            Request::get(format!("/api/v1{chemin}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{chemin}");
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

fn par_titre<'a>(liste: &'a Value, titre: &str) -> &'a Value {
    liste
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["title"] == json!(titre))
        .unwrap_or_else(|| panic!("{titre} absent de {liste}"))
}

#[tokio::test]
async fn les_routes_posent_les_ecoutes_de_la_communaute() {
    let (url, recus) = faux_site().await;
    let b = bibliotheque();
    relier(&b, &url, true);

    let pistes = get(&b.app, &format!("/library/albums/{}/tracks", b.album)).await;
    assert_eq!(
        par_titre(&pistes, "So What")["community_listens"],
        json!(128)
    );
    assert!(
        par_titre(&pistes, "Freddie Freeloader")
            .get("community_listens")
            .is_none(),
        "sous le seuil : absent, ni null ni 0 — {pistes}"
    );
    let pistes = get(&b.app, &format!("/library/artists/{}/tracks", b.artiste)).await;
    assert_eq!(
        par_titre(&pistes, "So What")["community_listens"],
        json!(128)
    );
    let page = get(&b.app, "/library/tracks").await;
    assert_eq!(
        par_titre(&page["items"], "So What")["community_listens"],
        json!(128)
    );

    let fiche = get(&b.app, &format!("/library/albums/{}", b.album)).await;
    assert_eq!(fiche["community_listens"], json!(5400));
    let grille = get(&b.app, "/library/albums").await;
    assert_eq!(
        par_titre(&grille["items"], "Kind of Blue")["community_listens"],
        json!(5400)
    );
    let albums = get(&b.app, &format!("/library/artists/{}/albums", b.artiste)).await;
    assert_eq!(
        par_titre(&albums, "Kind of Blue")["community_listens"],
        json!(5400)
    );

    let artiste = get(&b.app, &format!("/library/artists/{}", b.artiste)).await;
    assert_eq!(artiste["community_listens"], json!(43100));

    // Le cache : la piste et l'album vus plusieurs fois ne sont demandés
    // qu'une fois (une demande par genre).
    // Une seule prise du verrou : relu dans le message d'échec pendant que la
    // première prise est tenue, il bloquait le banc au lieu de le faire rougir.
    let demandes = recus.lock().unwrap().clone();
    assert_eq!(demandes.len(), 3, "{demandes:?}");
}

/// Envoi des écoutes coupé : aucun appel, aucun champ.
#[tokio::test]
async fn envoi_coupe_aucun_appel_aucun_champ() {
    let (url, recus) = faux_site().await;
    let b = bibliotheque();
    relier(&b, &url, false);

    let pistes = get(&b.app, &format!("/library/albums/{}/tracks", b.album)).await;
    let fiche = get(&b.app, &format!("/library/albums/{}", b.album)).await;
    let artiste = get(&b.app, &format!("/library/artists/{}", b.artiste)).await;
    assert!(
        par_titre(&pistes, "So What")
            .get("community_listens")
            .is_none()
    );
    assert!(fiche.get("community_listens").is_none());
    assert!(artiste.get("community_listens").is_none());
    assert!(recus.lock().unwrap().is_empty());
}
