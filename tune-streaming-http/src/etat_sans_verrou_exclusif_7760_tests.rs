//! #7760 — lire l'état d'un service ne retient pas ses lectures de favoris.
//!
//! `GET /streaming/{service}/status` prenait le verrou d'ÉCRITURE du service.
//! Pendant une lecture lente (favoris, playlists), cette écriture se mettait
//! en file, et le verrou de tokio fait passer une écriture en attente avant
//! les lectures arrivées après elle : les widgets « Favourite albums » et
//! « My playlists » attendaient la fin de la lecture lente, puis le délai du
//! widget tombait.
//!
//! Ici la lecture lente est simulée en tenant le verrou de lecture du service
//! pendant `LECTURE_LENTE` ; aucun appel ne sort de la machine.

use super::*;
use tune_core::TuneError;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AuthStatus, PlaylistTag, PlaylistTagGroup, SearchResults, StreamAlbum, StreamArtist,
    StreamPlaylist, StreamTrack, StreamUrl,
};

/// Durée de la lecture lente qui tient le verrou de lecture.
const LECTURE_LENTE: Duration = Duration::from_secs(3);
/// Ce qu'une réponse qui n'attend pas la lecture lente peut prendre au plus.
const REPONSE_RAPIDE: Duration = Duration::from_millis(1500);

/// Un compte sans favoris, connecté ou non.
struct CompteVide {
    nom: String,
    connecte: bool,
}

fn absent<T>() -> Result<T, TuneError> {
    Err(TuneError::Streaming("hors du banc".into()))
}

#[async_trait::async_trait]
impl StreamingService for CompteVide {
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
        Err(TuneError::Streaming("username required".into()))
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: self.connecte,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn get_user_favorites_dated(&self, _t: &str) -> Result<Option<Vec<Value>>, TuneError> {
        Ok(Some(Vec::new()))
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        absent()
    }
    async fn get_track(&self, _t: &str) -> Result<StreamTrack, TuneError> {
        absent()
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        absent()
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        absent()
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        absent()
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        absent()
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        absent()
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        absent()
    }
    async fn get_user_playlists(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_albums(&self) -> Result<Vec<StreamAlbum>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_user_artists(&self) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_featured(&self) -> Result<Vec<StreamPlaylist>, TuneError> {
        absent()
    }
    async fn get_playlist_tags(&self) -> Result<Vec<PlaylistTag>, TuneError> {
        absent()
    }
    async fn get_featured_playlists_by_tag(
        &self,
        _genre: Option<&str>,
    ) -> Result<Vec<PlaylistTagGroup>, TuneError> {
        absent()
    }
}

fn etat(nom: &str, connecte: bool) -> StreamingHttpState {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(CompteVide {
        nom: nom.to_string(),
        connecte,
    }));
    StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    )
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
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
    let octets = axum::body::to_bytes(reponse.into_body(), 64 * 1024)
        .await
        .expect("corps lisible");
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Tient le verrou de lecture du service pendant `LECTURE_LENTE`, comme une
/// lecture de favoris qui attend Qobuz.
async fn lecture_lente_en_cours(etat: &StreamingHttpState, nom: &str) {
    let arc = get_svc(etat, nom).await.expect("service enregistré");
    let (pris, verrou_pris) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _garde = arc.read().await;
        let _ = pris.send(());
        tokio::time::sleep(LECTURE_LENTE).await;
    });
    verrou_pris.await.expect("verrou de lecture pris");
}

/// Compte connecté : l'état, puis les favoris albums, pendant une lecture
/// lente. Ni l'un ni l'autre n'attend la fin de cette lecture.
#[tokio::test]
async fn l_etat_d_un_compte_connecte_ne_retient_pas_les_favoris_7760() {
    let nom = "faux-qobuz-connecte-7760";
    let etat = etat(nom, true);
    let app = Router::new().nest("/api/v1/streaming", router().with_state(etat.clone()));
    lecture_lente_en_cours(&etat, nom).await;

    let debut = Instant::now();
    let app_etat = app.clone();
    let route_etat = format!("/api/v1/streaming/{nom}/status");
    let etat_demande = tokio::spawn(async move { get_json(&app_etat, &route_etat).await });
    // L'état est demandé AVANT les favoris, comme les Réglages ouverts
    // pendant que l'accueil charge ses widgets.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (statut, favoris) =
        get_json(&app, &format!("/api/v1/streaming/{nom}/favorites/albums")).await;
    let favoris_en = debut.elapsed();
    let (statut_etat, corps_etat) = etat_demande.await.expect("tâche de l'état");

    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        favoris["albums"],
        json!([]),
        "un compte vide rend une liste vide"
    );
    assert_eq!(statut_etat, StatusCode::OK);
    assert_eq!(corps_etat["authenticated"], true);
    assert!(
        favoris_en < REPONSE_RAPIDE,
        "#7760 : les favoris d'un compte vide ont attendu {favoris_en:?} derrière la \
         lecture de l'état, qui attendait elle-même une lecture lente — le widget \
         tomberait en « délai »"
    );
}

/// Qobuz non connecté : l'interrogation ne le connecterait pas, l'état se lit
/// sans attendre la lecture lente.
#[tokio::test]
async fn l_etat_de_qobuz_non_connecte_n_attend_pas_une_lecture_lente_7760() {
    let etat = etat("qobuz", false);
    let app = Router::new().nest("/api/v1/streaming", router().with_state(etat.clone()));
    lecture_lente_en_cours(&etat, "qobuz").await;

    let debut = Instant::now();
    let (statut, corps) = get_json(&app, "/api/v1/streaming/qobuz/status").await;
    let ecoule = debut.elapsed();

    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["authenticated"], false);
    assert!(
        ecoule < REPONSE_RAPIDE,
        "#7760 : l'état de Qobuz a attendu {ecoule:?} une lecture lente pour un verrou \
         d'écriture dont il n'a pas l'usage"
    );
}

/// Les services qui se connectent par interrogation gardent ce chemin.
#[test]
fn seul_qobuz_renonce_a_l_interrogation_7760() {
    assert!(!connexion_par_interrogation("qobuz"));
    for service in ["tidal", "spotify", "deezer", "youtube"] {
        assert!(connexion_par_interrogation(service), "{service}");
    }
}
