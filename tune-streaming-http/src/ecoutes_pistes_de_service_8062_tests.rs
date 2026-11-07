//! #8062 — `GET /streaming/{service}/playlists/{id}/tracks` et
//! `GET /streaming/{service}/albums/{id}/tracks` : les colonnes « # Plays »,
//! « Last played », « Year » et « Genre » du tableau restaient vides pour
//! toutes les pistes d'une playlist Qobuz.
//!
//! Contrat (celui de la bibliothèque, `colonnesPistes.ts` côté web) :
//! `play_count` = 0 et `last_played_at` = null pour une piste jamais jouée ;
//! les écoutes sont comptées tous profils confondus, comme dans la
//! bibliothèque ; `year` et `genre` ne sont écrits que si le service les
//! publie.
//!
//! Contre-épreuve : rendre les pistes sans `pistes_avec_ecoutes` fait tomber
//! les deux témoins (clé `play_count` absente).

use super::*;
use tune_core::TuneError;
use tune_core::db::history_repo::ListenRecord;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::traits::{
    AnneeEtGenre, AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist,
    StreamTrack, StreamUrl,
};

const SERVICE: &str = "qobuz-8062";

struct ServiceAPistes;

fn hors_sujet<T>() -> Result<T, TuneError> {
    Err(TuneError::Streaming("hors sujet".into()))
}

fn piste(id: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: "Interprète 8062".into(),
        album: Some("Album 8062".into()),
        album_id: Some("album-8062".into()),
        duration_ms: 200_000,
        cover_path: None,
        track_number: Some(1),
        disc_number: Some(1),
        explicit: false,
        quality: None,
        isrc: None,
        disponible: None,
        composer: None,
        artist_id: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for ServiceAPistes {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        SERVICE
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
        Ok(vec![piste("8062001"), piste("8062002")])
    }
    /// L'album publie son année et son genre ; la playlist, non (repli du
    /// trait) : les deux cas du contrat.
    async fn get_album_tracks_avec_annee_et_genre(
        &self,
        a: &str,
    ) -> Result<Vec<(StreamTrack, AnneeEtGenre)>, TuneError> {
        let extras = AnneeEtGenre {
            year: Some(1999),
            genre: Some("Jazz".into()),
        };
        Ok(self
            .get_album_tracks(a)
            .await?
            .into_iter()
            .map(|t| (t, extras.clone()))
            .collect())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        hors_sujet()
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        hors_sujet()
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste("8062001"), piste("8062002")])
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
}

/// Une base où la piste `8062001` a été écoutée deux fois, par deux profils.
fn app() -> Router {
    let base = SqliteDb::open_in_memory().expect("sqlite en memoire");
    base.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&base).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(base);
    let historique = HistoryRepo::with_backend(backend.clone());
    for profil in [1, 2] {
        historique
            .record(&ListenRecord {
                title: "Titre 8062001".into(),
                artist_name: Some("Interprète 8062".into()),
                source: SERVICE.into(),
                source_id: Some("8062001".into()),
                duration_ms: 200_000,
                profile_id: Some(profil),
                ..Default::default()
            })
            .unwrap();
    }
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(ServiceAPistes));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    Router::new().nest("/api/v1/streaming", router().with_state(etat))
}

async fn lire(app: &Router, uri: &str, profil: i64) -> Vec<Value> {
    use tower::ServiceExt;
    let reponse = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .uri(uri)
                .header("X-Profile-Id", profil.to_string())
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice::<Vec<Value>>(&octets).expect("une liste de pistes")
}

fn par_id<'a>(pistes: &'a [Value], id: &str) -> &'a Value {
    pistes
        .iter()
        .find(|p| p["source_id"] == id)
        .unwrap_or_else(|| panic!("piste {id} absente : {pistes:?}"))
}

fn verifier_ecoutes(pistes: &[Value]) {
    let ecoutee = par_id(pistes, "8062001");
    let jamais = par_id(pistes, "8062002");
    assert_eq!(
        ecoutee.get("play_count"),
        Some(&json!(2)),
        "#8062 : une piste Qobuz écoutée deux fois doit porter play_count = 2, tous profils \
         confondus : {ecoutee}"
    );
    assert!(
        ecoutee["last_played_at"].is_string(),
        "#8062 : last_played_at doit être renseigné : {ecoutee}"
    );
    assert_eq!(
        jamais.get("play_count"),
        Some(&json!(0)),
        "#8062 : une piste jamais jouée porte play_count = 0, pas une clé absente : {jamais}"
    );
    assert_eq!(
        jamais.get("last_played_at"),
        Some(&Value::Null),
        "#8062 : une piste jamais jouée porte last_played_at = null : {jamais}"
    );
}

#[tokio::test]
async fn la_playlist_de_service_porte_les_ecoutes_8062() {
    let app = app();
    let uri = format!("/api/v1/streaming/{SERVICE}/playlists/pl-8062/tracks");
    for profil in [1, 2] {
        let pistes = lire(&app, &uri, profil).await;
        verifier_ecoutes(&pistes);
        // Le service ne publie ni année ni genre pour cette playlist : la
        // clé reste absente, jamais inventée.
        let p = par_id(&pistes, "8062001");
        assert!(p.get("year").is_none() && p.get("genre").is_none(), "{p}");
    }
}

#[tokio::test]
async fn l_album_de_service_porte_les_ecoutes_l_annee_et_le_genre_8062() {
    let app = app();
    let uri = format!("/api/v1/streaming/{SERVICE}/albums/album-8062/tracks");
    for profil in [1, 2] {
        let pistes = lire(&app, &uri, profil).await;
        verifier_ecoutes(&pistes);
        for p in &pistes {
            assert_eq!(p.get("year"), Some(&json!(1999)), "#8062 : année : {p}");
            assert_eq!(p.get("genre"), Some(&json!("Jazz")), "#8062 : genre : {p}");
        }
    }
}
