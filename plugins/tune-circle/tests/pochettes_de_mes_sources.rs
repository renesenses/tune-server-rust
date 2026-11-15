//! web#3001 — la pochette d'un album de contact, prise dans MES sources :
//! ma bibliothèque d'abord, puis Qobuz et TIDAL connectés. Services simulés,
//! base en mémoire : aucun réseau.

mod commun;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use tune_circle::pochettes_de_mes_sources::{
    ALBUMS_PAR_DEMANDE_MAX, PochettesDeMesSources, router,
};
use tune_core::TuneError;
use tune_core::db::backend::DbBackend;
use tune_core::streaming::ServiceRegistry;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

use commun::*;

struct FauxService {
    nom: &'static str,
    connecte: bool,
    albums: Vec<StreamAlbum>,
    recherches: Arc<AtomicUsize>,
}

fn album(id: &str, titre: &str, artiste: &str, pochette: Option<&str>) -> StreamAlbum {
    serde_json::from_value(json!({
        "id": id, "title": titre, "artist": artiste, "artist_id": null,
        "cover_path": pochette, "year": null, "track_count": 10, "quality": null,
    }))
    .expect("StreamAlbum de banc")
}

fn non() -> TuneError {
    TuneError::Streaming("non".into())
}

#[async_trait]
impl StreamingService for FauxService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Err(non())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Err(non())
    }
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        self.recherches.fetch_add(1, Ordering::SeqCst);
        Ok(SearchResults {
            tracks: vec![],
            albums: self.albums.clone(),
            artists: vec![],
            playlists: vec![],
        })
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(non())
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(vec![])
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(vec![])
    }
    async fn create_playlist(&self, _n: &str, _d: Option<&str>) -> Result<String, TuneError> {
        Err(non())
    }
    async fn add_tracks_to_playlist(&self, _p: &str, _t: &[String]) -> Result<usize, TuneError> {
        Err(non())
    }
    async fn delete_playlist(&self, _p: &str) -> Result<(), TuneError> {
        Err(non())
    }
    async fn remove_tracks_from_playlist(
        &self,
        _p: &str,
        _t: &[String],
    ) -> Result<usize, TuneError> {
        Err(non())
    }
    async fn add_favorite(&mut self, _t: &str, _i: &str) -> Result<(), TuneError> {
        Err(non())
    }
    async fn remove_favorite(&mut self, _t: &str, _i: &str) -> Result<(), TuneError> {
        Err(non())
    }
}

struct Banc {
    app: axum::Router,
    qobuz: Arc<AtomicUsize>,
    tidal: Arc<AtomicUsize>,
}

/// `services` : (nom, connecté, albums rendus par sa recherche).
fn banc(
    backend: Arc<dyn DbBackend>,
    services: Vec<(&'static str, bool, Vec<StreamAlbum>)>,
) -> Banc {
    let qobuz = Arc::new(AtomicUsize::new(0));
    let tidal = Arc::new(AtomicUsize::new(0));
    let mut registre = ServiceRegistry::new();
    for (nom, connecte, albums) in services {
        let recherches = if nom == "qobuz" {
            qobuz.clone()
        } else {
            tidal.clone()
        };
        registre.register(Box::new(FauxService {
            nom,
            connecte,
            albums,
            recherches,
        }));
    }
    let etat = PochettesDeMesSources::new(backend, Arc::new(tokio::sync::Mutex::new(registre)));
    Banc {
        app: router(Arc::new(etat)),
        qobuz,
        tidal,
    }
}

fn base_vide() -> Arc<dyn DbBackend> {
    // Aucun cloud n'est joint par cette route : une adresse jamais appelée.
    base("http://127.0.0.1:9", None)
}

async fn demander(app: &axum::Router, albums: Value) -> Vec<Value> {
    let r = appel(
        app,
        "POST",
        "/my-sources/album-covers",
        Some(json!({ "albums": albums })),
    )
    .await;
    assert_eq!(r.statut, axum::http::StatusCode::OK);
    r.json()["covers"].as_array().cloned().expect("covers")
}

#[tokio::test]
async fn ma_bibliotheque_d_abord_sans_chemin_et_sans_appeler_les_services() {
    let backend = base_vide();
    backend
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (20, 'Joël Grare');\
             INSERT INTO albums (id, title, artist_id, cover_path) \
             VALUES (10, 'Follow', 20, '/Users/secret-3001/Follow/cover.jpg');",
        )
        .unwrap();
    let b = banc(
        backend,
        vec![(
            "qobuz",
            true,
            vec![album("q1", "Follow", "Joël Grare", Some("https://q/1.jpg"))],
        )],
    );
    let covers = demander(
        &b.app,
        json!([{ "title": "Follow", "artist_name": "Joel Grare" }]),
    )
    .await;
    assert_eq!(covers[0]["source"], "local");
    assert_eq!(covers[0]["album_id"], 10);
    let adresse = covers[0]["cover_path"].as_str().unwrap();
    assert!(adresse.starts_with("/api/v1/library/artwork/"), "{adresse}");
    assert!(
        !adresse.contains("secret"),
        "un chemin de fichier est sorti : {adresse}"
    );
    assert_eq!(
        b.qobuz.load(Ordering::SeqCst),
        0,
        "la bibliothèque suffisait"
    );
}

#[tokio::test]
async fn puis_qobuz_puis_tidal_meme_titre_et_meme_artiste_seulement() {
    let b = banc(
        base_vide(),
        vec![
            (
                "qobuz",
                true,
                vec![
                    album(
                        "q0",
                        "Abbey Road",
                        "The Beatles Tribute Band",
                        Some("https://q/0.jpg"),
                    ),
                    album(
                        "q1",
                        "Abbey Road (Remastered 2019)",
                        "The Beatles",
                        Some("https://q/1.jpg"),
                    ),
                ],
            ),
            (
                "tidal",
                true,
                vec![album(
                    "t1",
                    "Rumours",
                    "Fleetwood Mac",
                    Some("https://t/1.jpg"),
                )],
            ),
        ],
    );
    let covers = demander(
        &b.app,
        json!([
            { "title": "Abbey Road", "artist_name": "The Beatles" },
            { "title": "Rumours", "artist_name": "Fleetwood Mac" },
            { "title": "Inconnu", "artist_name": "Personne" },
            { "title": "Sans artiste", "artist_name": "" },
        ]),
    )
    .await;
    assert_eq!(covers.len(), 4);
    assert_eq!(
        covers[0],
        json!({ "source": "qobuz", "cover_path": "https://q/1.jpg" })
    );
    assert_eq!(
        covers[1],
        json!({ "source": "tidal", "cover_path": "https://t/1.jpg" })
    );
    assert_eq!(covers[2], Value::Null);
    assert_eq!(covers[3], Value::Null);
    // « Sans artiste » n'est cherché nulle part.
    assert_eq!(b.qobuz.load(Ordering::SeqCst), 3);
    assert_eq!(b.tidal.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn un_service_deconnecte_n_est_pas_interroge() {
    let b = banc(
        base_vide(),
        vec![(
            "qobuz",
            false,
            vec![album("q1", "Follow", "Joël Grare", Some("https://q/1.jpg"))],
        )],
    );
    let covers = demander(
        &b.app,
        json!([{ "title": "Follow", "artist_name": "Joël Grare" }]),
    )
    .await;
    assert_eq!(covers[0], Value::Null);
    assert_eq!(b.qobuz.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn la_page_rouverte_ne_recherche_plus_rien() {
    let b = banc(
        base_vide(),
        vec![(
            "qobuz",
            true,
            vec![album("q1", "Follow", "Joël Grare", Some("https://q/1.jpg"))],
        )],
    );
    let page = json!([
        { "title": "Follow", "artist_name": "Joël Grare" },
        { "title": "Inconnu", "artist_name": "Personne" },
    ]);
    let premier = demander(&b.app, page.clone()).await;
    assert_eq!(b.qobuz.load(Ordering::SeqCst), 2);
    let second = demander(&b.app, page).await;
    assert_eq!(
        b.qobuz.load(Ordering::SeqCst),
        2,
        "pas de cache : la page a été recherchée deux fois"
    );
    assert_eq!(premier, second);
}

#[tokio::test]
async fn une_demande_est_bornee_a_une_page() {
    let b = banc(base_vide(), vec![("qobuz", true, vec![])]);
    let albums: Vec<Value> = (0..ALBUMS_PAR_DEMANDE_MAX + 25)
        .map(|n| json!({ "title": format!("Album {n}"), "artist_name": "X" }))
        .collect();
    let covers = demander(&b.app, Value::Array(albums)).await;
    assert_eq!(covers.len(), ALBUMS_PAR_DEMANDE_MAX);
    assert_eq!(b.qobuz.load(Ordering::SeqCst), ALBUMS_PAR_DEMANDE_MAX);
}
