//! #7729 — ouvrir TIDAL > Favoris gelait Tune : la route attendait le verrou
//! du registre des services, puis celui du service, sans borne et sans une
//! ligne de journal. Un rafraîchissement du jeton qui tient l'un ou l'autre
//! (ou le miroir des favoris, qui laisse une écriture en attente) suffisait à
//! laisser l'écran sur « Chargement… ».
//!
//! Le témoin tient ces verrous comme le ferait la tâche de fond, et exige
//! que la route réponde quand même, par un 503 nommé, dans le délai borné.

use super::*;
use std::time::{Duration, Instant};
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
};

/// Un connecteur actif et connecté, sans aucun favori.
struct ServiceSansFavori {
    nom: String,
}

#[async_trait::async_trait]
impl StreamingService for ServiceSansFavori {
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
        AuthStatus {
            authenticated: true,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_tracks(&self) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
}

/// Chaque essai a son nom de service : les caches de ce module sont des
/// `static` partagés par tout le processus de test.
fn app(nom: &str) -> (Router, Arc<Mutex<ServiceRegistry>>) {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceSansFavori {
        nom: nom.to_string(),
    }));
    let registre = Arc::new(Mutex::new(registre));
    let etat = StreamingHttpState::new(backend, registre.clone(), Arc::new(EventBus::new()));
    (
        Router::new().nest("/api/v1/streaming", router().with_state(etat)),
        registre,
    )
}

/// Le statut de `GET favorites/tracks`, ou `None` si la route n'a pas
/// répondu en 15 s — bien au-delà du délai borné de la route.
async fn favoris(app: &Router, nom: &str) -> (Option<StatusCode>, Duration) {
    use tower::ServiceExt;
    let debut = Instant::now();
    let requete = app.clone().oneshot(
        axum::http::Request::builder()
            .uri(format!("/api/v1/streaming/{nom}/favorites/tracks"))
            .body(axum::body::Body::empty())
            .unwrap(),
    );
    let statut = tokio::time::timeout(Duration::from_secs(15), requete)
        .await
        .ok()
        .map(|r| r.unwrap().status());
    (statut, debut.elapsed())
}

#[tokio::test]
async fn les_favoris_repondent_meme_service_tenu_en_ecriture_7729() {
    let nom = "gel-ecriture-7729";
    let (app, registre) = app(nom);
    let arc = registre.lock().await.get(nom).expect("service enregistré");
    // Le rafraîchisseur de jetons tient le service en écriture pendant un
    // appel réseau qui ne revient pas.
    let _ecriture = arc.write().await;
    let (statut, duree) = favoris(&app, nom).await;
    assert_eq!(
        statut,
        Some(StatusCode::SERVICE_UNAVAILABLE),
        "#7729 : la route des favoris reste suspendue derrière le verrou du service ({duree:?})"
    );
}

#[tokio::test]
async fn les_favoris_repondent_meme_registre_tenu_7729() {
    let nom = "gel-registre-7729";
    let (app, registre) = app(nom);
    // Le registre tenu pendant tout un tour de rafraîchissement.
    let _registre = registre.lock().await;
    let (statut, duree) = favoris(&app, nom).await;
    assert_eq!(
        statut,
        Some(StatusCode::SERVICE_UNAVAILABLE),
        "#7729 : la route des favoris reste suspendue derrière le registre des services ({duree:?})"
    );
}

/// Sans verrou tenu, la route répond tout de suite, comme avant.
#[tokio::test]
async fn les_favoris_libres_repondent_sans_attendre_7729() {
    let nom = "libre-7729";
    let (app, _registre) = app(nom);
    let (statut, duree) = favoris(&app, nom).await;
    assert_eq!(statut, Some(StatusCode::OK));
    assert!(
        duree < Duration::from_secs(2),
        "#7729 : la route libre a attendu {duree:?}"
    );
}
