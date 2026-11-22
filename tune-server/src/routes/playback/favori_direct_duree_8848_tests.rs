//! #8848 — un favori ajouté directement en file, sans playlist ni sortie audio.
use axum::body::{Body, to_bytes};
use axum::http::Request;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tower::ServiceExt;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::playlist_repo::ServiceEntry;
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
const CAIPIRINHA: &str = "90601421";
/// Le titre de départ de la lecture, durée connue, servi sans attente.
const DEPART: &str = "138664";

/// Le faux Qobuz : il compte ses lectures de fiche et peut être LENT sur
/// « Caipirinha » (un Qobuz qui ne répond pas vite).
struct FauxQobuz {
    appels: Arc<AtomicUsize>,
    nom: &'static str,
    porte: Option<Arc<tokio::sync::Notify>>,
}

fn non_prevu(quoi: &str) -> TuneError {
    TuneError::Streaming(format!("le faux service ne sert pas : {quoi}"))
}

fn fiche(id: &str, titre: &str, duree: u64, album: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: titre.into(),
        artist: "DjeuhDjoah & Lieutenant Nicholson".into(),
        album: Some(titre.into()),
        album_id: Some(album.into()),
        duration_ms: duree,
        cover_path: None,
        track_number: Some(1),
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: Some("artiste-temoin".into()),
    }
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
        self.nom
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
            CAIPIRINHA => {
                self.appels.fetch_add(1, Ordering::SeqCst);
                if let Some(porte) = &self.porte {
                    porte.notified().await;
                }
                Ok(fiche(id, "Caipirinha", DUREE, ALBUM))
            }
            DEPART => Ok(fiche(id, "El niño", 241_000, "aimez")),
            _ => Err(TuneError::NotFound(id.to_string())),
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
    zid: i64,
    nom: &'static str,
    appels: Arc<AtomicUsize>,
    porte: Option<Arc<tokio::sync::Notify>>,
}
async fn banc(nom: &'static str, lent: bool) -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let appels = Arc::new(AtomicUsize::new(0));
    let porte = lent.then(|| Arc::new(tokio::sync::Notify::new()));
    state.services.lock().await.register(Box::new(FauxQobuz {
        nom,
        appels: appels.clone(),
        porte: porte.clone(),
    }));
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("File factice", Some("browser"), None)
        .unwrap();
    Banc {
        state,
        zid,
        nom,
        appels,
        porte,
    }
}
fn entree(b: &Banc) -> ServiceEntry {
    ServiceEntry {
        source: b.nom.into(),
        source_id: CAIPIRINHA.into(),
        title: "Caipirinha".into(),
        artist: None,
        album: None,
        album_source_id: None,
        duration_ms: Some(0),
        cover_url: None,
    }
}
async fn ajouter(b: &Banc, duree: u64, references: bool) {
    let mut corps = serde_json::json!({ "source":b.nom, "source_id":CAIPIRINHA, "title":"Caipirinha", "duration_ms":duree });
    if references {
        corps["album_id_service"] = ALBUM.into();
        corps["artist_id_service"] = "artiste-temoin".into();
    }
    let reponse = tokio::time::timeout(
        Duration::from_secs(1),
        super::router().with_state(b.state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/{}/queue/add", b.zid))
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        ),
    )
    .await
    .expect("#8848 : la complétion de durée retient la réponse HTTP")
    .unwrap();
    assert!(reponse.status().is_success(), "{:?}", reponse.status());
    let _ = to_bytes(reponse.into_body(), 1_000_000).await.unwrap();
}
fn duree(b: &Banc) -> Option<i64> {
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .get_ordered(b.zid)
        .unwrap()
        .first()
        .and_then(|e| e.duration_ms)
}
async fn attendre(b: &Banc) {
    let attente = tokio::time::timeout(Duration::from_secs(2), async {
        while duree(b) != Some(DUREE as i64) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        attente.is_ok(),
        "#8848 : le favori reste à 0:00 après la fiche, durée={:?}",
        duree(b)
    );
}
#[tokio::test]
async fn un_service_lent_ne_retient_pas_l_ajout_et_previent_apres_completion_8848() {
    for nom in ["qobuz", "tidal"] {
        let b = banc(nom, true).await;
        let mut events = b.state.event_bus.subscribe();
        ajouter(&b, 0, true).await;
        assert_eq!(duree(&b), Some(0));
        b.porte.as_ref().unwrap().notify_one();
        attendre(&b).await;
        let mut prevenu = false;
        while let Ok(e) = events.try_recv() {
            if e.event_type == "playback.queue.updated"
                && e.data["durations_completed"].as_u64().unwrap_or(0) > 0
            {
                prevenu = true;
            }
        }
        assert!(
            prevenu,
            "#8848 : durée corrigée sans événement de rafraîchissement"
        );
        assert_eq!(b.appels.load(Ordering::SeqCst), 1);
    }
}
#[tokio::test]
async fn le_cache_complete_un_favori_sans_relire_le_service_8848() {
    let b = banc("qobuz", false).await;
    let mut e = entree(&b);
    crate::routes::playlists::metadonnees_7738::completer_durees(
        &b.state,
        vec![&mut e],
        Duration::from_secs(1),
    )
    .await;
    assert_eq!(e.duration_ms, Some(DUREE as i64));
    ajouter(&b, 0, true).await;
    attendre(&b).await;
    assert_eq!(b.appels.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn la_fiche_deja_lue_pour_les_references_donne_aussi_sa_duree_8848() {
    let b = banc("tidal", false).await;
    ajouter(&b, 0, false).await;
    attendre(&b).await;
    assert_eq!(
        b.appels.load(Ordering::SeqCst),
        1,
        "#8848 : deuxième lecture inutile de la fiche"
    );
    let lignes = PlayQueueRepo::with_backend(b.state.backend.clone())
        .get_ordered(b.zid)
        .unwrap();
    assert_eq!(lignes[0].album_ref.as_deref(), Some(ALBUM));
    assert_eq!(lignes[0].artist_ref.as_deref(), Some("artiste-temoin"));
}
#[tokio::test]
async fn une_duree_connue_n_est_pas_remplacee_8848() {
    let b = banc("qobuz", false).await;
    ajouter(&b, 123_000, true).await;
    tokio::task::yield_now().await;
    assert_eq!(duree(&b), Some(123_000));
    assert_eq!(b.appels.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn une_piste_retiree_pendant_la_fiche_n_est_pas_recreee_8848() {
    let b = banc("qobuz", true).await;
    ajouter(&b, 0, true).await;
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .clear(b.zid)
        .unwrap();
    let mut events = b.state.event_bus.subscribe();
    b.porte.as_ref().unwrap().notify_one();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        PlayQueueRepo::with_backend(b.state.backend.clone())
            .get_ordered(b.zid)
            .unwrap()
            .is_empty()
    );
    while let Ok(e) = events.try_recv() {
        assert!(e.data["durations_completed"].as_u64().unwrap_or(0) == 0);
    }
}

#[tokio::test]
async fn le_delai_global_couvre_aussi_l_attente_du_registre_8848() {
    let b = banc("qobuz", true).await;
    let _registre_occupe = b.state.services.lock().await;
    let resultat = tokio::time::timeout(
        Duration::from_secs(1),
        super::duree_favori_8848::effectuer(
            &b.state,
            b.zid,
            vec![entree(&b)],
            Duration::from_millis(30),
        ),
    )
    .await;
    assert!(
        resultat.is_ok(),
        "#8848 : le délai de complétion ne borne pas les verrous"
    );
    assert_eq!(b.appels.load(Ordering::SeqCst), 0);
}
