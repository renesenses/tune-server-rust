//! #8892 (forum, fil 2727) — un titre Qobuz importé par une playlist M3U puis
//! retiré du catalogue n'affichait plus que son titre : la ligne ne gardait
//! que ce que le fichier portait, et l'écran ne complétait la fiche (#7738)
//! que si Qobuz la rendait encore.
//!
//! Attendu : chaque résolution réussie MÉMORISE la fiche (artiste, album,
//! pochette, durée) sur la ligne ; un titre que Qobuz ne connaît plus (404,
//! `TuneError::NotFound`) est rendu `disponible: false` — grisé, non jouable
//! côté client — avec cette fiche mémorisée. Une panne de Qobuz n'est PAS un
//! retrait : la ligne n'est pas grisée.
//!
//! Contre-épreuve : sans `memoriser`, la seconde lecture rend la ligne sans
//! artiste ni album ni pochette ; sans `retires`, elle n'est pas grisée.
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tower::ServiceExt;
use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo, ServiceEntry};
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

use crate::state::AppState;

const KISS: &str = "52345678";
const POCHETTE: &str = "https://static.qobuz.com/images/covers/kiss_600.jpg";

/// L'état du faux Qobuz pour « Kiss ».
const DISPONIBLE: u8 = 0;
const RETIRE: u8 = 1;
const EN_PANNE: u8 = 2;

struct FauxQobuz {
    etat: Arc<AtomicU8>,
}

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le faux service ne sert pas : {quoi}"))
}

#[async_trait::async_trait]
impl StreamingService for FauxQobuz {
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
        match self.etat.load(Ordering::SeqCst) {
            DISPONIBLE if id == KISS => Ok(StreamTrack {
                id: id.into(),
                title: "Kiss".into(),
                artist: "Prince".into(),
                album: Some("Parade".into()),
                album_id: Some("0075992539524".into()),
                duration_ms: 226_000,
                cover_path: Some(POCHETTE.into()),
                track_number: Some(10),
                disc_number: Some(1),
                explicit: false,
                disponible: Some(true),
                quality: None,
                isrc: None,
                composer: None,
                artist_id: None,
            }),
            EN_PANNE => Err(TuneError::Streaming("qobuz /track/get: 503".into())),
            _ => Err(TuneError::NotFound(format!(
                "qobuz /track/get: 404 No result matching given argument ({id})"
            ))),
        }
    }
    async fn get_track_url(&self, _id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err(non_prevu("get_track_url"))
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_prevu("get_album"))
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_prevu("get_album_tracks"))
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

struct Banc {
    state: AppState,
    pid: i64,
    etat: Arc<AtomicU8>,
}

/// La playlist telle que l'import M3U d'Audirvana l'écrit (#6998) : le
/// titre et la durée de l'EXTINF, rien d'autre.
async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let etat = Arc::new(AtomicU8::new(DISPONIBLE));
    state
        .services
        .lock()
        .await
        .register(Box::new(FauxQobuz { etat: etat.clone() }));
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let pid = repo.create("Régis — import M3U", None, 1).unwrap();
    repo.add_entries_deduped(
        pid,
        &[EntryContent::Service(ServiceEntry {
            source: "qobuz".into(),
            source_id: KISS.into(),
            title: "Kiss".into(),
            artist: None,
            album: None,
            album_source_id: None,
            duration_ms: Some(226_000),
            cover_url: None,
        })],
        None,
    )
    .unwrap();
    Banc { state, pid, etat }
}

async fn lignes(b: &Banc) -> Vec<Value> {
    // Chaque affichage relit Qobuz : le cache des fiches est vidé.
    b.state.metadonnees_playlists.vider().await;
    let rep = super::router()
        .with_state(b.state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/{}/tracks", b.pid))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(rep.status().is_success(), "GET tracks : {}", rep.status());
    let corps = to_bytes(rep.into_body(), 4 * 1024 * 1024).await.unwrap();
    serde_json::from_slice(&corps).unwrap()
}

fn ligne_enregistree(b: &Banc) -> ServiceEntry {
    PlaylistRepo::with_backend(b.state.backend.clone())
        .get_entries(b.pid)
        .unwrap()
        .into_iter()
        .find_map(|e| match e.content {
            EntryContent::Service(s) => Some(s),
            _ => None,
        })
        .expect("la ligne Qobuz")
}

#[tokio::test]
async fn la_fiche_resolue_est_memorisee_sur_la_ligne_8892() {
    let b = banc().await;
    let _ = lignes(&b).await;
    let s = ligne_enregistree(&b);
    assert_eq!(s.artist.as_deref(), Some("Prince"), "artiste mémorisé");
    assert_eq!(s.album.as_deref(), Some("Parade"), "album mémorisé");
    assert_eq!(s.album_source_id.as_deref(), Some("0075992539524"));
    assert_eq!(s.cover_url.as_deref(), Some(POCHETTE), "pochette mémorisée");
    assert_eq!(s.duration_ms, Some(226_000));
    assert_eq!(s.title, "Kiss", "le titre du fichier n'est jamais réécrit");
}

#[tokio::test]
async fn un_titre_retire_de_qobuz_garde_sa_fiche_et_est_grise_8892() {
    let b = banc().await;
    let _ = lignes(&b).await;
    b.etat.store(RETIRE, Ordering::SeqCst);
    let v = lignes(&b).await;
    let l = &v[0];
    assert_eq!(l["title"], "Kiss");
    assert_eq!(
        l["disponible"], false,
        "titre retiré : grisé, non jouable — {l}"
    );
    assert_eq!(l["artist_name"], "Prince", "artiste gardé — {l}");
    assert_eq!(l["album_title"], "Parade", "album gardé — {l}");
    assert_eq!(l["cover_path"], POCHETTE, "pochette gardée — {l}");
    assert_eq!(l["duration_ms"], 226_000, "durée gardée — {l}");
}

#[tokio::test]
async fn une_panne_de_qobuz_n_est_pas_un_retrait_8892() {
    let b = banc().await;
    let _ = lignes(&b).await;
    b.etat.store(EN_PANNE, Ordering::SeqCst);
    let v = lignes(&b).await;
    let l = &v[0];
    assert!(
        l.get("disponible").is_none(),
        "une panne ne grise pas la ligne — {l}"
    );
    assert_eq!(
        l["artist_name"], "Prince",
        "la fiche mémorisée reste affichée — {l}"
    );
}
