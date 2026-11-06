//! #7822 — `GET /streaming/{service}/mixes` : les mixes personnels du compte.
//!
//! La route rend ce que le connecteur sert (TIDAL : My Daily Discovery, My
//! Mix 1 à 8…), sous la forme d'une playlist de service, et une liste vide —
//! jamais une erreur — pour un service qui n'a pas de mixes.

use super::*;
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
};

/// Un connecteur minimal ; `mixes` à `None` garde le comportement par défaut
/// du trait (un service sans mixes).
struct ServiceAMixes {
    nom: String,
    mixes: Option<Vec<StreamPlaylist>>,
}

fn hors_sujet<T>() -> Result<T, TuneError> {
    Err(TuneError::Streaming("hors sujet".into()))
}

#[async_trait::async_trait]
impl StreamingService for ServiceAMixes {
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
    async fn get_user_mixes(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        match &self.mixes {
            Some(m) => Ok(m.clone()),
            None => Ok(vec![]),
        }
    }
}

fn mix(id: &str, nom: &str) -> StreamPlaylist {
    StreamPlaylist {
        id: format!("mix:{id}"),
        name: nom.into(),
        description: None,
        cover_path: None,
        track_count: 0,
        owner: Some("TIDAL".into()),
        editable: Some(false),
        covers: Vec::new(),
    }
}

fn app(service: ServiceAMixes) -> Router {
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
async fn la_route_rend_les_mixes_du_compte_7822() {
    let app = app(ServiceAMixes {
        nom: "mixes-7822".into(),
        mixes: Some(vec![
            mix("0011aa", "My Daily Discovery"),
            mix("0022bb", "My Mix 1"),
        ]),
    });
    let (statut, corps) = lire(&app, "/api/v1/streaming/mixes-7822/mixes").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let rendus: Vec<(&str, &str)> = corps
        .as_array()
        .expect("une liste")
        .iter()
        .map(|m| {
            (
                m["source_id"].as_str().unwrap_or(""),
                m["name"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        rendus,
        [
            ("mix:0011aa", "My Daily Discovery"),
            ("mix:0022bb", "My Mix 1")
        ],
        "la route doit servir les mixes du connecteur, lisibles par leur source_id"
    );
}

#[tokio::test]
async fn un_service_sans_mix_rend_une_liste_vide_7822() {
    let app = app(ServiceAMixes {
        nom: "sans-mix-7822".into(),
        mixes: None,
    });
    let (statut, corps) = lire(&app, "/api/v1/streaming/sans-mix-7822/mixes").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps, serde_json::json!([]));
}
