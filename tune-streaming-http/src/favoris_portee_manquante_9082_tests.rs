//! #9082 — un compte Spotify relié sans la portée `user-follow-read` :
//! la lecture des artistes suivis rend un refus au code stable
//! (`spotify_reconnexion_artistes_suivis`). La route des favoris en faisait
//! un 502, que l'écran Streaming affichait comme une panne du serveur à
//! chaque lancement. Elle rend désormais une liste vide qualifiée, en 200.
use super::*;
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::spotify::{CODE_RECONNEXION_ARTISTES_SUIVIS, PORTEE_ARTISTES_SUIVIS};
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
};

/// Un connecteur connecté dont la lecture des artistes suivis échoue avec
/// l'erreur donnée.
struct ServiceArtistesRefuses {
    nom: String,
    refus_de_portee: bool,
}

#[async_trait::async_trait]
impl StreamingService for ServiceArtistesRefuses {
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
        if self.refus_de_portee {
            Err(TuneError::Refused {
                code: CODE_RECONNEXION_ARTISTES_SUIVIS,
                message: "spotify: la liaison du compte ne porte pas la portee user-follow-read"
                    .into(),
            })
        } else {
            Err(TuneError::Streaming(
                "spotify GET /me/following: HTTP 500 — Server error".into(),
            ))
        }
    }
}

/// Chaque essai a son nom de service : les caches de ce module sont des
/// `static` partagés par tout le processus de test.
async fn artistes_suivis(nom: &str, refus_de_portee: bool) -> (StatusCode, Option<Value>) {
    use tower::ServiceExt;
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceArtistesRefuses {
        nom: nom.to_string(),
        refus_de_portee,
    }));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    let app = Router::new().nest("/api/v1/streaming", router().with_state(etat));
    let reponse = app
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("/api/v1/streaming/{nom}/favorites/artists"))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 16)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&octets).ok())
}

#[tokio::test]
async fn une_portee_manquante_rend_une_liste_vide_qualifiee_9082() {
    let (statut, corps) = artistes_suivis("portee-manquante-9082", true).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "#9082 : une portée manquante n'est pas une panne du serveur"
    );
    let corps = corps.expect("corps JSON");
    assert_eq!(corps["artists"], json!([]));
    assert_eq!(corps["portee_manquante"], PORTEE_ARTISTES_SUIVIS);
    assert_eq!(corps["code"], CODE_RECONNEXION_ARTISTES_SUIVIS);
}

/// Contre-épreuve : une vraie panne de Spotify reste une erreur.
#[tokio::test]
async fn une_panne_reste_une_erreur_9082() {
    let (statut, _) = artistes_suivis("panne-9082", false).await;
    assert_ne!(
        statut,
        StatusCode::OK,
        "une panne ne doit pas passer pour une liste vide"
    );
}
