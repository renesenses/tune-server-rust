//! Fil 3009 (FabienM), point 7 — `GET /{service}/artists/{id}/similar`.
//!
//! La page artiste Qobuz doit montrer, comme celle de la bibliothèque, ses
//! « Artistes similaires ». Le service les connaît (`get_similar_artists`) ;
//! la route les rend à la fiche, sans l'artiste lui-même ni doublon, et DIT
//! (501) quand le service n'en sait rien.

use super::*;
use std::sync::Mutex as MutexStd;
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
};

/// Ce que `get_similar_artists` a reçu : (identifiant, borne).
type Demandes = Arc<MutexStd<Vec<(String, usize)>>>;

struct ServiceVoisins {
    nom: String,
    /// Le service déclare-t-il la capacité ?
    sait: bool,
    demandes: Demandes,
}

fn artiste(id: &str, nom: &str) -> StreamArtist {
    StreamArtist {
        id: id.into(),
        name: nom.into(),
        image_path: Some(format!("https://static.example/{id}.jpg")),
        bio: None,
    }
}

const PANNE: &str = "hors sujet";

#[async_trait::async_trait]
impl StreamingService for ServiceVoisins {
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
        Err(PANNE.into())
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        Err(PANNE.into())
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(PANNE.into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err(PANNE.into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(PANNE.into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err(PANNE.into())
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err(PANNE.into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(PANNE.into())
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
    /// Ce que rend un catalogue réel : l'artiste lui-même glissé dans la
    /// liste, un doublon d'identifiant, un nom vide — et trois vrais voisins.
    async fn get_similar_artists(
        &self,
        artist_id: &str,
        limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        self.demandes
            .lock()
            .unwrap()
            .push((artist_id.to_string(), limit));
        if artist_id == "en-panne" {
            return Err(TuneError::Streaming("amont: 500".into()));
        }
        Ok(vec![
            artiste("q-2", "Agnes Obel"),
            artiste(artist_id, "Alela Diane"),
            artiste("q-3", "Laura Marling"),
            artiste("q-2", "Agnes Obel"),
            artiste("q-4", "  "),
            artiste("q-5", "Marissa Nadler"),
        ])
    }
    fn propose_des_artistes_similaires(&self) -> bool {
        self.sait
    }
}

/// Chaque essai a son nom de service : les caches de ce module sont des
/// `static` partagés par tout le processus de test.
fn app(nom: &str, sait: bool) -> (Router, Demandes) {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let demandes = Arc::new(MutexStd::new(Vec::new()));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceVoisins {
        nom: nom.to_string(),
        sait,
        demandes: demandes.clone(),
    }));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    (
        Router::new().nest("/api/v1/streaming", router().with_state(etat)),
        demandes,
    )
}

async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let r = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn la_fiche_recoit_les_voisins_du_service_sans_l_artiste_ni_doublon() {
    let (app, demandes) = app("essai-voisins-3009", true);
    let (statut, corps) = get(
        &app,
        "/api/v1/streaming/essai-voisins-3009/artists/q-1/similar",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    let voisins: Vec<(String, String, String)> = corps
        .as_array()
        .expect("une liste d'artistes")
        .iter()
        .map(|a| {
            (
                a["id"].as_str().unwrap_or_default().to_string(),
                a["name"].as_str().unwrap_or_default().to_string(),
                a["image_path"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    assert_eq!(
        voisins,
        vec![
            (
                "q-2".to_string(),
                "Agnes Obel".to_string(),
                "https://static.example/q-2.jpg".to_string()
            ),
            (
                "q-3".to_string(),
                "Laura Marling".to_string(),
                "https://static.example/q-3.jpg".to_string()
            ),
            (
                "q-5".to_string(),
                "Marissa Nadler".to_string(),
                "https://static.example/q-5.jpg".to_string()
            ),
        ],
        "ni l'artiste lui-même, ni doublon, ni nom vide ; l'ordre du service et le portrait"
    );
    assert_eq!(
        demandes.lock().unwrap().as_slice(),
        &[("q-1".to_string(), SIMILAIRES_ARTISTE_PAR_DEFAUT + 1)],
        "la borne par défaut, plus un pour l'artiste lui-même"
    );
}

#[tokio::test]
async fn la_borne_demandee_est_tenue_et_plafonnee() {
    let (app, demandes) = app("essai-voisins-borne-3009", true);
    let (statut, corps) = get(
        &app,
        "/api/v1/streaming/essai-voisins-borne-3009/artists/q-1/similar?limit=2",
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps.as_array().map(Vec::len), Some(2));
    let (statut, _) = get(
        &app,
        "/api/v1/streaming/essai-voisins-borne-3009/artists/q-1/similar?limit=100000",
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        demandes.lock().unwrap().last().map(|d| d.1),
        Some(SIMILAIRES_ARTISTE_PLAFOND + 1),
        "une borne démesurée est ramenée au plafond"
    );
}

#[tokio::test]
async fn un_service_sans_similarite_le_dit_en_501() {
    let (app, demandes) = app("essai-voisins-muet-3009", false);
    let (statut, _) = get(
        &app,
        "/api/v1/streaming/essai-voisins-muet-3009/artists/q-1/similar",
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_IMPLEMENTED);
    assert!(
        demandes.lock().unwrap().is_empty(),
        "le service n'est pas interrogé"
    );
}

#[tokio::test]
async fn une_panne_du_service_n_est_pas_une_liste_vide() {
    let (app, _) = app("essai-voisins-panne-3009", true);
    let (statut, _) = get(
        &app,
        "/api/v1/streaming/essai-voisins-panne-3009/artists/en-panne/similar",
    )
    .await;
    assert!(
        statut.is_server_error() && statut != StatusCode::NOT_IMPLEMENTED,
        "une panne doit rester une panne, pas un 200 vide ni un 501 : {statut}"
    );
}
