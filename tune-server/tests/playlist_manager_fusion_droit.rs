//! `POST /playlist-manager/merge` : la fusion qui apparie d'un catalogue à
//! l'autre relève du même droit que `POST /playlist-manager/transfer`.
//!
//! Le transfert entre services, et d'un service vers la bibliothèque, est
//! Premium (décision du 07/10/2026, `refus_premium_du_transfert`). Une fusion
//! dont une source vit ailleurs que sur la cible fait exactement ce travail :
//! elle lit chez l'un, cherche chez l'autre, crée et remplit. Elle reçoit donc
//! le même refus `402 premium_required` (`code: playlist_transfer`), et rien
//! n'est écrit.
//!
//! Restent gratuites, comme avant : la fusion « au même endroit » (des
//! playlists d'un service vers ce même service, des playlists de la
//! bibliothèque vers la bibliothèque).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};
use tune_server::state::AppState;

type Journal = Arc<Mutex<Vec<String>>>;

/// Un service simulé : des playlists à lire, une recherche fixe, et le
/// journal de toutes ses écritures.
struct ServiceSimule {
    nom: &'static str,
    playlists: HashMap<String, Vec<StreamTrack>>,
    recherche: Vec<StreamTrack>,
    ecritures: Journal,
}

#[async_trait::async_trait]
impl StreamingService for ServiceSimule {
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
    async fn authenticate(&mut self, _credentials: &Value) -> Result<AuthStatus, TuneError> {
        Ok(self.auth_status().await)
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            username: Some("simule".into()),
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _query: &str, _limit: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: self.recherche.clone(),
            albums: Vec::new(),
            artists: Vec::new(),
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, _id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_track_url(
        &self,
        _id: &str,
        _quality: Option<&str>,
    ) -> Result<StreamUrl, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(Vec::new())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(TuneError::NotFound("simule".into()))
    }
    async fn get_playlist_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.ecritures.lock().unwrap().push(format!("lecture:{id}"));
        self.playlists
            .get(id)
            .cloned()
            .ok_or_else(|| TuneError::NotFound(id.into()))
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
    async fn create_playlist(
        &self,
        name: &str,
        _description: Option<&str>,
    ) -> Result<String, TuneError> {
        self.ecritures
            .lock()
            .unwrap()
            .push(format!("creation:{name}"));
        Ok("pl-creee".to_string())
    }
    async fn add_tracks_to_playlist(
        &self,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<usize, TuneError> {
        self.ecritures
            .lock()
            .unwrap()
            .push(format!("ajout:{playlist_id}:{}", track_ids.join(",")));
        Ok(track_ids.len())
    }
    fn supports_write(&self) -> bool {
        true
    }
}

fn piste(id: &str, titre: &str, artiste: &str, duree_ms: u64) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: duree_ms,
        cover_path: None,
        track_number: Some(1),
        disc_number: Some(1),
        explicit: false,
        disponible: None,
        isrc: None,
        composer: None,
        artist_id: None,
        quality: None,
    }
}

/// Les écritures seules (création, ajout), sans les lectures.
fn ecritures(j: &Journal) -> Vec<String> {
    j.lock()
        .unwrap()
        .iter()
        .filter(|e| !e.starts_with("lecture:"))
        .cloned()
        .collect()
}

struct Banc {
    _dossier: tempfile::TempDir,
    state: AppState,
    app: axum::Router,
    journal_source: Journal,
    journal_cible: Journal,
    /// Une playlist de la bibliothèque (profil 1) qui porte la piste 1.
    locale: i64,
}

/// La bibliothèque porte « La Bohème » (piste 1). Le service « source » porte
/// `pl-1` (la même chanson) ; le service « cible » porte `pl-c` et trouve
/// « La Bohème » dans son catalogue.
async fn banc(premium: bool) -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let db = dossier.path().join("tune.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Charles Aznavour')",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) \
             VALUES (?, ?, 1, 1, ?)",
            &[&1i64 as &dyn ToSqlValue, &"La Bohème", &210_000i64],
        )
        .unwrap();
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let locale = repo.create("Ma locale", None, 1).unwrap();
    repo.add_tracks(locale, &[1], None).unwrap();

    let journal_source: Journal = Arc::default();
    let journal_cible: Journal = Arc::default();
    {
        let mut services = state.services.lock().await;
        services.register(Box::new(ServiceSimule {
            nom: "source",
            playlists: HashMap::from([(
                "pl-1".to_string(),
                vec![piste("s-1", "La Bohème", "Charles Aznavour", 210_000)],
            )]),
            recherche: Vec::new(),
            ecritures: journal_source.clone(),
        }));
        services.register(Box::new(ServiceSimule {
            nom: "cible",
            playlists: HashMap::from([(
                "pl-c".to_string(),
                vec![piste("c-1", "La Bohème", "Charles Aznavour", 210_000)],
            )]),
            recherche: vec![piste("c-1", "La Bohème", "Charles Aznavour", 210_000)],
            ecritures: journal_cible.clone(),
        }));
    }
    state.license.set_account_premium(premium, None).await;
    let app = tune_server::routes::router(state.clone());
    Banc {
        _dossier: dossier,
        state,
        app,
        journal_source,
        journal_cible,
        locale,
    }
}

async fn fusion(app: &axum::Router, corps: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri("/api/v1/playlist-manager/merge")
        .header("content-type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.expect("route absente");
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn noms_locaux(state: &AppState) -> Vec<String> {
    PlaylistRepo::with_backend(state.backend.clone())
        .list(1, 100, 0)
        .unwrap()
        .into_iter()
        .map(|p| p.name)
        .collect()
}

fn assert_refus(statut: StatusCode, rendu: &Value, cas: &str) {
    assert_eq!(statut, StatusCode::PAYMENT_REQUIRED, "{cas} : {rendu}");
    assert_eq!(rendu["error"], "premium_required", "{cas} : {rendu}");
    assert_eq!(rendu["code"], "playlist_transfer", "{cas} : {rendu}");
}

/// 🔴 Un compte gratuit : une fusion d'un service vers un AUTRE service est
/// refusée, et rien n'est écrit chez la cible — ni lu chez la source.
#[tokio::test]
async fn fusion_entre_deux_services_refusee_sans_le_droit() {
    let b = banc(false).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [{ "service": "source", "playlist_id": "pl-1" }],
            "target_name": "Fusion croisée",
            "target_service": "cible",
        }),
    )
    .await;
    assert_refus(st, &rendu, "source → cible");
    assert!(
        ecritures(&b.journal_cible).is_empty(),
        "écrit chez la cible"
    );
    assert!(
        b.journal_source.lock().unwrap().is_empty(),
        "la source a été lue avant le refus"
    );
}

/// 🔴 Un compte gratuit : une playlist de la bibliothèque fusionnée vers un
/// service est refusée, comme `transfer` local → service.
#[tokio::test]
async fn fusion_de_la_bibliotheque_vers_un_service_refusee_sans_le_droit() {
    let b = banc(false).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [
                { "service": "local", "playlist_id": b.locale.to_string() },
                { "service": "cible", "playlist_id": "pl-c" }
            ],
            "target_name": "Vers la cible",
            "target_service": "cible",
        }),
    )
    .await;
    assert_refus(st, &rendu, "local → cible");
    assert!(
        ecritures(&b.journal_cible).is_empty(),
        "écrit chez la cible"
    );
}

/// 🔴 Un compte gratuit : une playlist de service fusionnée vers la
/// bibliothèque est refusée, comme `transfer` service → local.
#[tokio::test]
async fn fusion_d_un_service_vers_la_bibliotheque_refusee_sans_le_droit() {
    let b = banc(false).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [{ "service": "source", "playlist_id": "pl-1" }],
            "target_name": "Importée",
        }),
    )
    .await;
    assert_refus(st, &rendu, "source → local");
    assert!(
        !noms_locaux(&b.state).contains(&"Importée".to_string()),
        "une playlist locale a été créée"
    );
}

/// Témoin : le même geste, avec le droit, fusionne chez la cible.
#[tokio::test]
async fn fusion_entre_deux_services_acceptee_avec_le_droit() {
    let b = banc(true).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [{ "service": "source", "playlist_id": "pl-1" }],
            "target_name": "Fusion croisée",
            "target_service": "cible",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    assert_eq!(rendu["service"], "cible", "{rendu}");
    assert_eq!(
        ecritures(&b.journal_cible),
        vec![
            "creation:Fusion croisée".to_string(),
            "ajout:pl-creee:c-1".to_string()
        ]
    );
}

/// Témoin : service → bibliothèque, avec le droit, crée la playlist locale.
#[tokio::test]
async fn fusion_d_un_service_vers_la_bibliotheque_acceptee_avec_le_droit() {
    let b = banc(true).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [{ "service": "source", "playlist_id": "pl-1" }],
            "target_name": "Importée",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    assert!(noms_locaux(&b.state).contains(&"Importée".to_string()));
}

/// Inchangé : fusionner des playlists d'un service VERS CE MÊME service reste
/// gratuit (aucun appariement, les identifiants restent ceux du service).
#[tokio::test]
async fn fusion_au_meme_endroit_chez_un_service_reste_gratuite() {
    let b = banc(false).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [{ "service": "cible", "playlist_id": "pl-c" }],
            "target_name": "Même endroit",
            "target_service": "cible",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    assert_eq!(
        ecritures(&b.journal_cible),
        vec![
            "creation:Même endroit".to_string(),
            "ajout:pl-creee:c-1".to_string()
        ]
    );
}

/// Inchangé : fusionner des playlists de la bibliothèque dans la
/// bibliothèque reste gratuit.
#[tokio::test]
async fn fusion_dans_la_bibliotheque_reste_gratuite() {
    let b = banc(false).await;
    let (st, rendu) = fusion(
        &b.app,
        json!({
            "playlists": [{ "service": "local", "playlist_id": b.locale.to_string() }],
            "target_name": "Locale fusionnée",
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{rendu}");
    assert!(noms_locaux(&b.state).contains(&"Locale fusionnée".to_string()));
}
