//! #8050 — les favoris Spotify (titres, albums, playlists, artistes suivis)
//! s'arrêtaient à 50 : seule la première page de l'API était lue.
//!
//! Un faux Spotify local sert 120 éléments par liste, par pages de 50 au
//! plus, avec `total` et `next` comme l'API réelle : les 120 doivent
//! remonter, en trois requêtes. Deux faux Spotify déréglés vérifient la
//! borne : une suite toujours annoncée ne doit pas faire boucler la lecture.

use super::*;
use axum::extract::{Query, State};
use axum::routing::get;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const NOMBRE: usize = 120;

#[derive(Clone, Default)]
struct Appels(Arc<HashMap<&'static str, AtomicUsize>>);

impl Appels {
    fn nouveau() -> Self {
        let mut m = HashMap::new();
        for k in ["tracks", "albums", "playlists", "following"] {
            m.insert(k, AtomicUsize::new(0));
        }
        Self(Arc::new(m))
    }
    fn compter(&self, k: &'static str) {
        self.0[k].fetch_add(1, Ordering::SeqCst);
    }
    fn lire(&self, k: &'static str) -> usize {
        self.0[k].load(Ordering::SeqCst)
    }
}

/// Une page `offset` / `limit` façon Spotify, `next` absent à la fin.
fn page(
    base: &str,
    q: &HashMap<String, String>,
    fabrique: impl Fn(usize) -> serde_json::Value,
) -> serde_json::Value {
    let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20);
    let limit = limit.min(50);
    let offset: usize = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let fin = (offset + limit).min(NOMBRE);
    let items: Vec<_> = (offset..fin).map(fabrique).collect();
    let next = if fin < NOMBRE {
        json!(format!("{base}?offset={fin}&limit={limit}"))
    } else {
        serde_json::Value::Null
    };
    json!({"href": base, "items": items, "limit": limit, "offset": offset,
           "total": NOMBRE, "next": next, "previous": null})
}

async fn faux_spotify() -> (SpotifyService, Appels) {
    async fn tracks(
        State(a): State<Appels>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        a.compter("tracks");
        axum::Json(page("/v1/me/tracks", &q, |i| {
            json!({"added_at": "2026-11-01T10:00:00Z",
                   "track": {"id": format!("t{i}"), "name": format!("titre {i}"),
                             "duration_ms": 1000, "artists": [], "album": {}}})
        }))
    }
    async fn albums(
        State(a): State<Appels>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        a.compter("albums");
        axum::Json(page("/v1/me/albums", &q, |i| {
            json!({"added_at": "2026-11-01T10:00:00Z",
                   "album": {"id": format!("a{i}"), "name": format!("album {i}"),
                             "artists": [], "images": []}})
        }))
    }
    async fn playlists(
        State(a): State<Appels>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        a.compter("playlists");
        axum::Json(page("/v1/me/playlists", &q, |i| {
            json!({"id": format!("p{i}"), "name": format!("playlist {i}"),
                   "images": [], "tracks": {"total": 1}, "owner": {}})
        }))
    }
    async fn following(
        State(a): State<Appels>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        a.compter("following");
        // Curseur : `after` = identifiant du dernier artiste de la page.
        let limit: usize = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(20);
        let limit = limit.min(50);
        let debut = q
            .get("after")
            .and_then(|v| v.strip_prefix("r"))
            .and_then(|v| v.parse::<usize>().ok())
            .map_or(0, |n| n + 1);
        let fin = (debut + limit).min(NOMBRE);
        let items: Vec<_> = (debut..fin)
            .map(|i| json!({"id": format!("r{i}"), "name": format!("artiste {i}"), "images": []}))
            .collect();
        let (next, after) = if fin < NOMBRE {
            let dernier = format!("r{}", fin - 1);
            (
                json!(format!(
                    "/v1/me/following?type=artist&after={dernier}&limit={limit}"
                )),
                json!(dernier),
            )
        } else {
            (serde_json::Value::Null, serde_json::Value::Null)
        };
        axum::Json(
            json!({"artists": {"items": items, "next": next, "total": NOMBRE,
                                      "limit": limit, "cursors": {"after": after}}}),
        )
    }
    let appels = Appels::nouveau();
    let app = axum::Router::new()
        .route("/v1/me/tracks", get(tracks))
        .route("/v1/me/albums", get(albums))
        .route("/v1/me/playlists", get(playlists))
        .route("/v1/me/following", get(following))
        .with_state(appels.clone());
    lancer(app, appels).await
}

async fn lancer<E: Clone + Send + Sync + 'static>(
    app: axum::Router,
    etat: E,
) -> (SpotifyService, E) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let base = format!("http://{}", ecoute.local_addr().expect("adresse locale"));
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton".into());
    (svc, etat)
}

#[tokio::test]
async fn les_120_titres_favoris_remontent_8050() {
    let (svc, appels) = faux_spotify().await;
    let titres = svc.get_user_tracks().await.expect("titres favoris");
    assert_eq!(
        titres.len(),
        NOMBRE,
        "#8050 : titres favoris tronqués, seule la première page de /me/tracks est lue"
    );
    assert_eq!(titres[0].id, "t0");
    assert_eq!(titres[NOMBRE - 1].id, format!("t{}", NOMBRE - 1));
    assert_eq!(
        appels.lire("tracks"),
        3,
        "#8050 : 120 titres = 3 pages de 50"
    );
}

#[tokio::test]
async fn les_120_albums_favoris_remontent_8050() {
    let (svc, appels) = faux_spotify().await;
    let albums = svc.get_user_albums().await.expect("albums favoris");
    assert_eq!(
        albums.len(),
        NOMBRE,
        "#8050 : albums favoris tronqués, seule la première page de /me/albums est lue"
    );
    assert_eq!(albums[NOMBRE - 1].id, format!("a{}", NOMBRE - 1));
    assert_eq!(
        appels.lire("albums"),
        3,
        "#8050 : 120 albums = 3 pages de 50"
    );
}

#[tokio::test]
async fn les_120_playlists_remontent_8050() {
    let (svc, appels) = faux_spotify().await;
    let playlists = svc.get_user_playlists().await.expect("playlists");
    assert_eq!(
        playlists.len(),
        NOMBRE,
        "#8050 : playlists tronquées, seule la première page de /me/playlists est lue"
    );
    assert_eq!(appels.lire("playlists"), 3);
}

#[tokio::test]
async fn les_120_artistes_suivis_remontent_8050() {
    let (svc, appels) = faux_spotify().await;
    let artistes = svc.get_user_artists().await.expect("artistes suivis");
    assert_eq!(
        artistes.len(),
        NOMBRE,
        "#8050 : artistes suivis tronqués, seul le premier curseur de /me/following est lu"
    );
    assert_eq!(artistes[NOMBRE - 1].id, format!("r{}", NOMBRE - 1));
    assert_eq!(appels.lire("following"), 3);
}

/// Un Spotify qui ignore `offset` et annonce toujours une suite, sans total :
/// la lecture doit s'arrêter dès la page sans élément neuf (garde de #7729),
/// sans doublons.
#[tokio::test]
async fn une_suite_toujours_annoncee_sur_la_meme_page_ne_boucle_pas_8050() {
    async fn tracks(State(a): State<Arc<AtomicUsize>>) -> axum::Json<serde_json::Value> {
        a.fetch_add(1, Ordering::SeqCst);
        let items: Vec<_> = (0..50)
            .map(|i| json!({"track": {"id": format!("t{i}"), "name": "x", "artists": [], "album": {}}}))
            .collect();
        axum::Json(json!({"items": items, "limit": 50, "offset": 0,
                          "next": "/v1/me/tracks?offset=50&limit=50"}))
    }
    let appels = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/v1/me/tracks", get(tracks))
        .with_state(appels.clone());
    let (svc, appels) = lancer(app, appels).await;
    let titres = tokio::time::timeout(std::time::Duration::from_secs(20), svc.get_user_tracks())
        .await
        .expect("#8050 : la pagination boucle sans fin sur une page qui annonce toujours `next`")
        .expect("titres favoris");
    let requetes = appels.load(Ordering::SeqCst);
    assert!(
        requetes <= 2,
        "#8050 : la pagination ne s'arrête pas sur une page qui stagne ({requetes} requêtes)"
    );
    assert_eq!(
        titres.len(),
        50,
        "#8050 : la même page relue a été empilée en doublons"
    );
}

/// Un Spotify qui invente toujours des éléments neufs et une suite : seule la
/// borne `PAGES_UTILISATEUR_MAX` arrête la lecture.
#[tokio::test]
async fn une_suite_sans_fin_est_bornee_8050() {
    async fn tracks(
        State(a): State<Arc<AtomicUsize>>,
        Query(q): Query<HashMap<String, String>>,
    ) -> axum::Json<serde_json::Value> {
        a.fetch_add(1, Ordering::SeqCst);
        let offset: usize = q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0);
        let items: Vec<_> = (offset..offset + 50)
            .map(|i| json!({"track": {"id": format!("t{i}"), "name": "x", "artists": [], "album": {}}}))
            .collect();
        axum::Json(json!({"items": items, "limit": 50, "offset": offset,
                          "next": "/v1/me/tracks?encore"}))
    }
    let appels = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new()
        .route("/v1/me/tracks", get(tracks))
        .with_state(appels.clone());
    let (svc, appels) = lancer(app, appels).await;
    let titres = tokio::time::timeout(std::time::Duration::from_secs(60), svc.get_user_tracks())
        .await
        .expect("#8050 : la pagination n'est pas bornée")
        .expect("titres favoris");
    let requetes = appels.load(Ordering::SeqCst);
    assert_eq!(
        requetes,
        SpotifyService::PAGES_UTILISATEUR_MAX,
        "#8050 : la borne de pages n'arrête pas la lecture"
    );
    assert_eq!(titres.len(), SpotifyService::PAGES_UTILISATEUR_MAX * 50);
}
