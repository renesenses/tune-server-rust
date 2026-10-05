//! Un service désactivé n'est plus interrogé par les routes `/{service}/…`.
//!
//! Retour de terrain : Qobuz « disabled, not authenticated »
//! dans le rapport, et pourtant quatre fois dans le même journal
//! `qobuz /playlist/getTags: 500` suivi d'un 502 de Tune sur
//! `featured-playlists/by-tag`. La route prenait le service nommé par l'URL
//! sans regarder la case « Actif ».

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, PlaylistTag, PlaylistTagGroup, SearchResults, StreamAlbum, StreamArtist,
    StreamPlaylist, StreamTrack, StreamUrl,
};

/// Un connecteur qui compte TOUT appel amont.
struct ServiceEteint {
    nom: String,
    actif: bool,
    appels: Arc<AtomicUsize>,
}

impl ServiceEteint {
    fn appel<T>(&self) -> Result<T, TuneError> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        Err(TuneError::Streaming("amont: 500".into()))
    }
}

#[async_trait::async_trait]
impl StreamingService for ServiceEteint {
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
        self.actif
    }
    fn set_enabled(&mut self, e: bool) {
        self.actif = e;
    }
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
        self.appel()
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        self.appel()
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        self.appel()
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        self.appel()
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.appel()
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        self.appel()
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        self.appel()
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.appel()
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.appel()
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        self.appel()
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        self.appel()
    }
    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        self.appel()
    }
    async fn get_playlist_tags(&self) -> Result<Vec<PlaylistTag>, TuneError> {
        self.appel()
    }
    async fn get_featured_playlists_by_tag(
        &self,
        _genre: Option<&str>,
    ) -> Result<Vec<PlaylistTagGroup>, TuneError> {
        self.appel()
    }
}

/// Chaque essai a son nom de service : les caches de ce module sont des
/// `static` partagés par tout le processus de test.
fn app(nom: &str, actif: bool) -> (Router, Arc<AtomicUsize>) {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let appels = Arc::new(AtomicUsize::new(0));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceEteint {
        nom: nom.to_string(),
        actif,
        appels: appels.clone(),
    }));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    (
        Router::new().nest("/api/v1/streaming", router().with_state(etat)),
        appels,
    )
}

async fn appeler(app: &Router, methode: &str, uri: &str) -> StatusCode {
    use tower::ServiceExt;
    app.clone()
        .oneshot(
            axum::http::Request::builder()
                .method(methode)
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// Les routes que l'accueil, la page découverte, les favoris et la reprise
/// d'une piste appellent : aucune n'atteint le service éteint.
const ROUTES_DE_DONNEES: &[&str] = &[
    "featured-playlists/by-tag",
    "playlist-tags",
    "featured",
    "search?q=x",
    "tracks/1/url",
    "artists/1",
    "albums/1",
];

#[tokio::test]
async fn un_service_desactive_n_est_jamais_appele() {
    let (app, appels) = app("eteint-hs", false);
    for route in ROUTES_DE_DONNEES {
        let statut = appeler(&app, "GET", &format!("/api/v1/streaming/eteint-hs/{route}")).await;
        assert_eq!(
            statut,
            StatusCode::CONFLICT,
            "{route} : un service désactivé doit être refusé (409), pas relayé"
        );
    }
    // Les favoris d'un service éteint : une liste vide (200), comme pour une
    // source sans favoris — le client lit le corps en JSON.
    assert_eq!(
        appeler(&app, "GET", "/api/v1/streaming/eteint-hs/favorites/albums").await,
        StatusCode::OK
    );
    assert_eq!(
        appels.load(Ordering::SeqCst),
        0,
        "un service désactivé a été interrogé"
    );
    // Son état, lui, reste lisible : l'écran des Réglages en a besoin pour
    // afficher la case à cocher.
    assert_eq!(
        appeler(&app, "GET", "/api/v1/streaming/eteint-hs/status").await,
        StatusCode::OK
    );
}

/// Contre-témoin : le même service, activé, est bien interrogé — le refus
/// tient à la case « Actif », pas à la route.
#[tokio::test]
async fn reactive_il_est_de_nouveau_interroge() {
    let (app, appels) = app("rallume-hs", false);
    assert_eq!(
        appeler(&app, "POST", "/api/v1/streaming/rallume-hs/enable").await,
        StatusCode::OK
    );
    let statut = appeler(
        &app,
        "GET",
        "/api/v1/streaming/rallume-hs/featured-playlists/by-tag",
    )
    .await;
    assert_ne!(statut, StatusCode::CONFLICT);
    assert_eq!(appels.load(Ordering::SeqCst), 1);
}
