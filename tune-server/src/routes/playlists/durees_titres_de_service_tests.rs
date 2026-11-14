//! Recette du 14/11 (Bertrand, « Lecture en cours ») — dans « À suivre », des
//! titres Qobuz s'affichaient « 0:00 ». Ils venaient de la playlist
//! « Ecoutes récentes Bertrand », où ils avaient été posés depuis les
//! Favoris : la table des favoris de service ne garde pas la durée, le client
//! envoyait `duration_ms: 0`, la ligne l'enregistrait, et la lecture de la
//! playlist la recopiait dans la file.
//!
//! Contre-épreuve : sans `completer_durees`, l'ajout garde 0 et la file reçoit
//! 0 — les deux tests rougissent sur la durée.
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo, ServiceEntry};
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

use crate::state::AppState;

/// La durée que la fiche Qobuz donne à « Caipirinha » (3:53).
const DUREE: u64 = 233_000;
const ALBUM: &str = "obxtaattzzowa";

struct FauxQobuz;

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
        if id != "90601421" {
            return Err(TuneError::NotFound(id.to_string()));
        }
        Ok(StreamTrack {
            id: id.into(),
            title: "Caipirinha".into(),
            artist: "DjeuhDjoah & Lieutenant Nicholson".into(),
            album: Some("Caipirinha".into()),
            album_id: Some(ALBUM.into()),
            duration_ms: DUREE,
            cover_path: None,
            track_number: Some(1),
            disc_number: None,
            explicit: false,
            disponible: None,
            quality: None,
            isrc: None,
            composer: None,
            artist_id: None,
        })
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

async fn banc() -> (AppState, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.services.lock().await.register(Box::new(FauxQobuz));
    let pid = PlaylistRepo::with_backend(state.backend.clone())
        .create("Ecoutes récentes Bertrand", None, 1)
        .unwrap();
    (state, pid)
}

/// La ligne telle que l'écrivait l'ajout depuis les Favoris.
fn caipirinha_sans_duree() -> ServiceEntry {
    ServiceEntry {
        source: "qobuz".into(),
        source_id: "90601421".into(),
        title: "Caipirinha".into(),
        artist: Some("DjeuhDjoah & Lieutenant Nicholson".into()),
        album: Some("Caipirinha".into()),
        album_source_id: None,
        duration_ms: Some(0),
        cover_url: None,
    }
}

async fn poster(
    routeur: axum::Router<AppState>,
    state: &AppState,
    uri: String,
    corps: serde_json::Value,
) {
    let rep = routeur
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = to_bytes(rep.into_body(), 1024 * 1024).await;
}

#[tokio::test]
async fn l_ajout_d_un_titre_sans_duree_enregistre_celle_de_sa_fiche() {
    let (state, pid) = banc().await;
    // Le corps qu'envoie « Ajouter à une playlist » sur un favori Qobuz.
    poster(
        super::router(),
        &state,
        format!("/{pid}/tracks"),
        serde_json::json!({
            "track_ids": [],
            "streaming_tracks": [{
                "source": "qobuz",
                "source_id": "90601421",
                "title": "Caipirinha",
                "artist_name": "DjeuhDjoah & Lieutenant Nicholson",
                "album_title": "Caipirinha",
                "duration_ms": 0,
            }],
        }),
    )
    .await;

    let lignes = PlaylistRepo::with_backend(state.backend.clone())
        .get_entries(pid)
        .unwrap();
    let [ligne] = lignes.as_slice() else {
        panic!("une ligne attendue : {lignes:?}")
    };
    let EntryContent::Service(s) = &ligne.content else {
        panic!("un titre de service attendu : {ligne:?}")
    };
    assert_eq!(
        s.duration_ms,
        Some(DUREE as i64),
        "la ligne garde 0 : « À suivre » affichera « 0:00 »"
    );
    assert_eq!(s.album_source_id.as_deref(), Some(ALBUM));
}

#[tokio::test]
async fn la_lecture_d_une_playlist_complete_la_duree_de_la_file() {
    let (state, pid) = banc().await;
    // Une ligne DÉJÀ enregistrée sans durée (la playlist de Bertrand).
    PlaylistRepo::with_backend(state.backend.clone())
        .add_entries_deduped(pid, &[EntryContent::Service(caipirinha_sans_duree())], None)
        .unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Navigateur", Some("browser"), None)
        .unwrap();

    poster(
        crate::routes::playback::router(),
        &state,
        format!("/{zid}/play"),
        serde_json::json!({ "playlist_id": pid }),
    )
    .await;

    // La file est écrite AVANT la lecture : quoi qu'il advienne de celle-ci
    // sur ce banc sans sortie, elle doit porter la durée.
    let file = PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(zid)
        .unwrap();
    let [ligne] = file.as_slice() else {
        panic!("un titre en file attendu : {} lignes", file.len())
    };
    assert_eq!(
        ligne.duration_ms,
        Some(DUREE as i64),
        "la file reçoit 0 : « À suivre » affiche « 0:00 »"
    );
}

#[tokio::test]
async fn une_duree_connue_n_est_pas_redemandee() {
    let (state, _) = banc().await;
    let mut titre = caipirinha_sans_duree();
    titre.duration_ms = Some(111_000);
    titre.source_id = "inconnu".into();
    super::metadonnees_7738::completer_durees(
        &state,
        vec![&mut titre],
        super::metadonnees_7738::DELAI_AVANT_ECRITURE,
    )
    .await;
    assert_eq!(titre.duration_ms, Some(111_000), "une durée connue reste");
}
