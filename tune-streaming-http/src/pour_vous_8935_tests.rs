//! #8935 — `GET /streaming/{service}/for-you` et
//! `GET /streaming/{service}/playlists/{id}/recommendations` : la route rend
//! ce que le connecteur sert, et 501 pour un service qui n'en a pas.

use super::*;
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, PagePersonnalisee, RangeePersonnalisee, SearchResults, StreamAlbum, StreamArtist,
    StreamPlaylist, StreamTrack, StreamUrl, TitresRecommandes,
};

fn piste(id: &str, titre: &str) -> StreamTrack {
    serde_json::from_value(serde_json::json!({
        "id": id, "title": titre, "artist": "X", "album": null, "album_id": null,
        "duration_ms": 1000, "cover_path": null, "track_number": null,
        "disc_number": null, "explicit": false, "isrc": null, "composer": null,
        "artist_id": null, "quality": null
    }))
    .expect("piste")
}

/// Un connecteur minimal ; `mixes` à `None` garde le comportement par défaut
/// du trait (un service sans mixes).
struct ServicePourVous {
    nom: String,
    /// `None` : le comportement par défaut du trait (501).
    page: Option<PagePersonnalisee>,
    /// Les appels de recommandations reçus : (playlist, limit, offset).
    recos: Arc<std::sync::Mutex<Vec<(String, usize, usize)>>>,
    gere_recos: bool,
}

fn hors_sujet<T>() -> Result<T, TuneError> {
    Err(TuneError::Streaming("hors sujet".into()))
}

#[async_trait::async_trait]
impl StreamingService for ServicePourVous {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        &self.nom
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _e: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        hors_sujet()
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        hors_sujet()
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        hors_sujet()
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        hors_sujet()
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        hors_sujet()
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        hors_sujet()
    }
    async fn get_artist_albums(&self, _a: &str) -> Result<Vec<StreamAlbum>, TuneError> {
        hors_sujet()
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        hors_sujet()
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        hors_sujet()
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        hors_sujet()
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        hors_sujet()
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        hors_sujet()
    }
    async fn get_for_you(&self) -> Result<PagePersonnalisee, TuneError> {
        match &self.page {
            Some(p) => Ok(p.clone()),
            None => Err(TuneError::Unsupported("pas de page".into())),
        }
    }
    async fn get_playlist_recommendations(
        &self,
        playlist_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<TitresRecommandes, TuneError> {
        if !self.gere_recos {
            return Err(TuneError::Unsupported("pas de recommandations".into()));
        }
        self.recos
            .lock()
            .unwrap()
            .push((playlist_id.to_string(), limit, offset));
        Ok(TitresRecommandes {
            tracks: vec![piste("31", "Reco")],
            total: 95,
            offset,
        })
    }
}

fn app(service: ServicePourVous) -> Router {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(service));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    Router::new().nest("/api/v1/streaming", router().with_state(etat))
}

async fn lire(app: &Router, uri: &str) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let reponse = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn la_route_rend_la_page_pour_vous_du_connecteur_8935() {
    let app = app(ServicePourVous {
        nom: "pv-8935".into(),
        page: Some(PagePersonnalisee {
            source: "for_you".into(),
            rows: vec![RangeePersonnalisee {
                id: "m-tracks".into(),
                title: "Tracks for you".into(),
                tracks: vec![piste("900001", "Piste Une")],
                ..Default::default()
            }],
        }),
        recos: Arc::default(),
        gere_recos: false,
    });
    let (statut, corps) = lire(&app, "/api/v1/streaming/pv-8935/for-you").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["source"], "for_you");
    assert_eq!(corps["rows"][0]["title"], "Tracks for you");
    assert_eq!(
        corps["rows"][0]["tracks"][0]["source_id"], "900001",
        "une piste se lit par source_id, comme partout"
    );
    assert_eq!(corps["rows"][0]["albums"], serde_json::json!([]));
}

#[tokio::test]
async fn un_service_sans_page_pour_vous_rend_501_8935() {
    let app = app(ServicePourVous {
        nom: "sans-pv-8935".into(),
        page: None,
        recos: Arc::default(),
        gere_recos: false,
    });
    let (statut, _) = lire(&app, "/api/v1/streaming/sans-pv-8935/for-you").await;
    assert_eq!(statut, StatusCode::NOT_IMPLEMENTED);
    let (statut, _) = lire(
        &app,
        "/api/v1/streaming/sans-pv-8935/playlists/abc/recommendations",
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_IMPLEMENTED);
}

#[tokio::test]
async fn la_route_des_recommandations_transmet_sa_page_8935() {
    let recos: Arc<std::sync::Mutex<Vec<(String, usize, usize)>>> = Arc::default();
    let app = app(ServicePourVous {
        nom: "reco-8935".into(),
        page: None,
        recos: recos.clone(),
        gere_recos: true,
    });
    let (statut, corps) = lire(
        &app,
        "/api/v1/streaming/reco-8935/playlists/1f2e-3d4c/recommendations",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["tracks"][0]["source_id"], "31");
    assert_eq!(corps["total"], 95);
    let (statut, corps) = lire(
        &app,
        "/api/v1/streaming/reco-8935/playlists/1f2e-3d4c/recommendations?limit=5&offset=10",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["offset"], 10);
    assert_eq!(
        recos.lock().unwrap().as_slice(),
        [
            ("1f2e-3d4c".to_string(), 20, 0),
            ("1f2e-3d4c".to_string(), 5, 10)
        ],
        "20 titres par défaut, puis la page demandée"
    );
}
