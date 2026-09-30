//! #5478 — la date du DÉPÔT dans une étiquette, rendue par les trois routes
//! que lit l'écran « Écouter plus tard » (web#1802).
//!
//! Avant : `item_tags` n'avait pas de date, et `streaming_item_tags` en avait
//! une qu'aucune route ne rendait. L'écran ne pouvait pas trier par « ajouté
//! récemment ».
//!
//! Tout passe par la ROUTE MONTÉE (`crate::routes::router`), sur une base
//! SQLite de FICHIER — jamais `:memory:` (#5043).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::PlaylistRepo;

fn etat(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, crate::state::AppState) {
    let dossier = tune_core::test_scratch::scratch_dir(&format!("etiquette-5478-{epreuve}"));
    let etat = crate::state::AppState::new(
        &dossier.join("tune.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .unwrap();
    (dossier, etat)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, path: &str) -> Value {
    let (st, v) = appel(app, Request::get(path).body(Body::empty()).unwrap()).await;
    assert_eq!(st, StatusCode::OK, "{path} : {v}");
    v
}

async fn post(app: &axum::Router, path: &str, body: Value) -> StatusCode {
    let req = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    appel(app, req).await.0
}

/// `AAAA-MM-JJTHH:MM:SSZ` — la forme que pose `now_iso8601`.
fn est_une_date_iso(v: &Value) -> bool {
    let Some(s) = v.as_str() else { return false };
    s.len() == 20 && s.as_bytes()[10] == b'T' && s.ends_with('Z') && s.starts_with("20")
}

fn ligne<'a>(v: &'a Value, famille: &str, pred: impl Fn(&Value) -> bool) -> &'a Value {
    v[famille]
        .as_array()
        .unwrap_or_else(|| panic!("{famille} absent : {v}"))
        .iter()
        .find(|l| pred(l))
        .unwrap_or_else(|| panic!("ligne introuvable dans {famille} : {v}"))
}

#[tokio::test]
async fn les_trois_routes_rendent_la_date_du_depot() {
    let (_dossier, etat) = etat("routes");
    let app = crate::routes::router(etat.clone());

    let (st, v) = appel(
        &app,
        Request::post("/api/v1/tags")
            .header("Content-Type", "application/json")
            .body(Body::from(json!({"name": "Écouter plus tard"}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    let tag = v["id"].as_i64().unwrap();

    // Deux albums LOCAUX : l'un déposé par la route, l'autre posé comme avant
    // la migration 113 — sans date.
    let artiste = ArtistRepo::with_backend(etat.backend.clone())
        .get_or_create("Emile Parisien", None, None)
        .unwrap();
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let neuf = albums
        .get_or_create("Floating", artiste.id.unwrap(), None)
        .unwrap()
        .id
        .unwrap();
    let ancien = albums
        .get_or_create("Sfumato", artiste.id.unwrap(), None)
        .unwrap()
        .id
        .unwrap();
    assert_eq!(
        post(
            &app,
            &format!("/api/v1/tags/{tag}/items"),
            json!({"item_type": "album", "item_id": neuf})
        )
        .await,
        StatusCode::CREATED
    );
    let p: [&dyn ToSqlValue; 2] = [&tag, &ancien];
    etat.backend
        .execute(
            "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (?, 'album', ?)",
            &p,
        )
        .unwrap();

    // Une playlist LOCALE (profil par défaut).
    let liste = PlaylistRepo::with_backend(etat.backend.clone())
        .create("Pour dimanche", None, 1)
        .unwrap();
    assert_eq!(
        post(
            &app,
            &format!("/api/v1/tags/{tag}/items"),
            json!({"item_type": "playlist", "item_id": liste})
        )
        .await,
        StatusCode::CREATED
    );

    // Le streaming : un album, un titre, une playlist avec son image.
    for (item_type, source_id) in [("album", "a1"), ("track", "t1"), ("playlist", "p1")] {
        assert_eq!(
            post(
                &app,
                &format!("/api/v1/tags/{tag}/streaming-items"),
                json!({
                    "item_type": item_type, "source": "qobuz", "source_id": source_id,
                    "title": format!("Qobuz {source_id}"),
                    "cover_url": format!("https://static.qobuz.com/{source_id}.jpg"),
                }),
            )
            .await,
            StatusCode::CREATED,
            "{item_type}"
        );
    }

    let v = get(&app, &format!("/api/v1/tags/{tag}/albums")).await;
    let l = ligne(&v, "albums", |l| l["id"].as_i64() == Some(neuf));
    assert!(
        est_une_date_iso(&l["tagged_at"]),
        "album local déposé : tagged_at = {}",
        l["tagged_at"]
    );
    let l = ligne(&v, "albums", |l| l["id"].as_i64() == Some(ancien));
    assert!(
        l.get("tagged_at").is_some_and(Value::is_null),
        "une pose d'avant la colonne rend tagged_at NULL, pas une date inventée : {l}"
    );
    let l = ligne(&v, "albums", |l| l["source_id"] == "a1");
    assert!(
        est_une_date_iso(&l["tagged_at"]),
        "album de streaming : {l}"
    );

    let v = get(&app, &format!("/api/v1/tags/{tag}/tracks")).await;
    let l = ligne(&v, "tracks", |l| l["source_id"] == "t1");
    assert!(
        est_une_date_iso(&l["tagged_at"]),
        "titre de streaming : {l}"
    );

    let v = get(&app, &format!("/api/v1/tags/{tag}/playlists")).await;
    let l = ligne(&v, "playlists", |l| l["id"].as_i64() == Some(liste));
    assert!(est_une_date_iso(&l["tagged_at"]), "playlist locale : {l}");
    let l = ligne(&v, "playlists", |l| l["source_id"] == "p1");
    assert!(
        est_une_date_iso(&l["tagged_at"]),
        "playlist de streaming : {l}"
    );
    assert_eq!(
        l["cover_path"], "https://static.qobuz.com/p1.jpg",
        "l'image posée à l'étiquetage doit être rendue : {l}"
    );
}

/// Un second dépôt du même objet ne RAJEUNIT pas le premier.
#[tokio::test]
async fn un_second_depot_ne_reecrit_pas_la_date() {
    let (_dossier, etat) = etat("second");
    let repo = tune_core::db::tag_repo::TagRepo::with_backend(etat.backend.clone());
    let tag = repo.create("Sas", None).unwrap();
    let artiste = ArtistRepo::with_backend(etat.backend.clone())
        .get_or_create("A", None, None)
        .unwrap();
    let album = AlbumRepo::with_backend(etat.backend.clone())
        .get_or_create("B", artiste.id.unwrap(), None)
        .unwrap()
        .id
        .unwrap();
    repo.tag_item(tag, "album", album).unwrap();
    let p: [&dyn ToSqlValue; 1] = [&tag];
    etat.backend
        .execute(
            "UPDATE item_tags SET created_at = '2020-01-01T00:00:00Z' WHERE tag_id = ?",
            &p,
        )
        .unwrap();
    repo.tag_item(tag, "album", album).unwrap();
    assert_eq!(
        repo.items_by_tag_dated(tag, "album").unwrap(),
        vec![(album, Some("2020-01-01T00:00:00Z".to_string()))]
    );
}
