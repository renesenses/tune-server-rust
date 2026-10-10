//! #5372 — un titre de service lancé SEUL (recherche, tuile d'accueil,
//! historique) joue son ALBUM à partir de lui, au lieu d'une file d'un titre
//! qui s'arrête à sa fin.
//!
//! Contre-épreuve : sans la résolution de l'album, la route garde la branche
//! « titre seul », qui n'écrit la file qu'après une lecture réussie et n'y met
//! qu'un titre ; `le_titre_seul_joue_son_album_a_partir_de_lui` rougit sur la
//! longueur de la file.

use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const ALBUM: &str = "album-5372";

/// Un faux service : ses titres appartiennent à `ALBUM` (sauf « orphelin »),
/// qui en compte trois. Les URL de flux ne servent pas : rien ne dépend du
/// réseau ici.
struct FauxService {
    lectures_d_album: Arc<AtomicUsize>,
}

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le faux service ne sert pas : {quoi}"))
}

fn piste(id: &str, n: u32, album_id: Option<&str>) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {n}"),
        artist: "Artiste".into(),
        album: Some("Album".into()),
        album_id: album_id.map(str::to_string),
        duration_ms: 200_000,
        cover_path: None,
        track_number: Some(n),
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

#[async_trait::async_trait]
impl StreamingService for FauxService {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "qobuz"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &serde_json::Value) -> Result<AuthStatus, TuneError> {
        Err(non_prevu("authenticate"))
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..AuthStatus::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, _q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Err(non_prevu("search"))
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        match id {
            "t1" => Ok(piste("t1", 1, Some(ALBUM))),
            "t2" => Ok(piste("t2", 2, Some(ALBUM))),
            "t3" => Ok(piste("t3", 3, Some(ALBUM))),
            "orphelin" => Ok(piste("orphelin", 1, None)),
            _ => Err(TuneError::NotFound(id.to_string())),
        }
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_prevu("get_track_url"))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    async fn get_album_tracks(&self, album_id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        self.lectures_d_album.fetch_add(1, Ordering::SeqCst);
        if album_id != ALBUM {
            return Err(TuneError::NotFound(album_id.to_string()));
        }
        Ok(vec![
            piste("t1", 1, Some(ALBUM)),
            piste("t2", 2, Some(ALBUM)),
            piste("t3", 3, Some(ALBUM)),
        ])
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_prevu("get_artist"))
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_prevu("get_playlist"))
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_playlist_tracks"))
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
}

async fn banc() -> (AppState, i64, Arc<AtomicUsize>) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let compteur = Arc::new(AtomicUsize::new(0));
    state.services.lock().await.register(Box::new(FauxService {
        lectures_d_album: compteur.clone(),
    }));
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Navigateur", Some("browser"), None)
        .unwrap();
    (state, zid, compteur)
}

/// Le corps qu'envoient la recherche Qobuz, les tuiles d'accueil et
/// l'historique : `source` + `source_id`, ni `track_ids` ni album.
async fn lancer_le_titre(state: &AppState, zid: i64, source_id: &str) {
    let corps = serde_json::json!({
        "source": "qobuz",
        "source_id": source_id,
        "title": "Titre",
        "artist_name": "Artiste",
    });
    let rep = router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{zid}/play"))
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = to_bytes(rep.into_body(), 1024 * 1024).await;
}

#[tokio::test]
async fn le_titre_seul_joue_son_album_a_partir_de_lui() {
    let (state, zid, compteur) = banc().await;
    lancer_le_titre(&state, zid, "t2").await;

    // La file de l'album est écrite AVANT la lecture : quoi qu'il advienne de
    // celle-ci sur ce banc sans sortie, elle doit porter l'album entier.
    let file = PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap();
    let ids: Vec<_> = file.iter().map(|e| e.source_id.clone()).collect();
    assert_eq!(
        ids,
        vec![Some("t1".into()), Some("t2".into()), Some("t3".into())],
        "l'album entier en file, pas le seul titre"
    );
    assert_eq!(
        compteur.load(Ordering::SeqCst),
        1,
        "l'album lu pour le titre ne se relit pas"
    );
}

#[tokio::test]
async fn un_titre_sans_album_ne_charge_pas_d_album() {
    let (state, zid, compteur) = banc().await;
    lancer_le_titre(&state, zid, "orphelin").await;

    assert_eq!(
        compteur.load(Ordering::SeqCst),
        0,
        "sans album connu, aucune lecture d'album"
    );
    let file = PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap();
    assert!(
        file.len() <= 1,
        "le comportement d'avant : au plus le seul titre ({} en file)",
        file.len()
    );
}

#[tokio::test]
async fn un_titre_deja_en_file_ne_relit_pas_son_album() {
    let (state, zid, compteur) = banc().await;
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(
            zid,
            &[QueueInput::Streaming {
                source: "qobuz".into(),
                source_id: "t3".into(),
                title: "Titre 3".into(),
                artist: "Artiste".into(),
                album: None,
                cover_url: None,
                duration_ms: 200_000,
                track_number: None,
                disc_number: None,
                album_ref: None,
                artist_ref: None,
            }],
        )
        .unwrap();
    lancer_le_titre(&state, zid, "t3").await;

    assert_eq!(
        compteur.load(Ordering::SeqCst),
        0,
        "Stop puis Play sur un titre en file garde la file (Pierre M.)"
    );
    let file = PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap();
    assert_eq!(file.len(), 1, "la file existante est intacte");
}

#[test]
fn la_place_du_titre_dans_l_album() {
    let pistes = vec![piste("t1", 1, Some(ALBUM)), piste("t2", 2, Some(ALBUM))];
    assert_eq!(position_du_titre_dans_l_album(&pistes, "t2"), Some(1));
    assert_eq!(position_du_titre_dans_l_album(&pistes, "t9"), None);
}
