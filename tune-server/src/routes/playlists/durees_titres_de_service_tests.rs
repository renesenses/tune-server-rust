//! Recette du 14/11 (Bertrand, « Lecture en cours ») — dans « À suivre », des
//! titres Qobuz s'affichaient « 0:00 ». Ils venaient de la playlist
//! « Ecoutes récentes Bertrand », où ils avaient été posés depuis les
//! Favoris : la table des favoris de service ne garde pas la durée, le client
//! envoyait `duration_ms: 0`, la ligne l'enregistrait, et la lecture de la
//! playlist la recopiait dans la file.
//!
//! Relecture du 14/11 : la LECTURE n'attend jamais le service. Avant de
//! démarrer, elle ne lit que le cache des fiches ; le reste se complète en
//! tâche de fond, qui réécrit la file ET la playlist puis prévient l'écran.
//!
//! Contre-épreuve : sans `completer_durees`, l'ajout garde 0 et la file reste
//! à 0 — les tests rougissent sur la durée.
use axum::body::{Body, to_bytes};
use axum::http::Request;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
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
const CAIPIRINHA: &str = "90601421";
/// Le titre de départ de la lecture, durée connue, servi sans attente.
const DEPART: &str = "138664";

/// Le faux Qobuz : il compte ses lectures de fiche et peut être LENT sur
/// « Caipirinha » (un Qobuz qui ne répond pas vite).
struct FauxQobuz {
    appels: Arc<AtomicUsize>,
    lenteur: Duration,
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
        artist_id: None,
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
            CAIPIRINHA => {
                self.appels.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.lenteur).await;
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
    pid: i64,
    zid: i64,
    appels: Arc<AtomicUsize>,
}

async fn banc_avec(service: Option<Duration>) -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let appels = Arc::new(AtomicUsize::new(0));
    if let Some(lenteur) = service {
        state.services.lock().await.register(Box::new(FauxQobuz {
            appels: appels.clone(),
            lenteur,
        }));
    }
    let pid = PlaylistRepo::with_backend(state.backend.clone())
        .create("Ecoutes récentes Bertrand", None, 1)
        .unwrap();
    let zid = ZoneRepo::with_backend(state.backend.clone())
        .create("Navigateur", Some("browser"), None)
        .unwrap();
    Banc {
        state,
        pid,
        zid,
        appels,
    }
}

async fn banc() -> Banc {
    banc_avec(Some(Duration::ZERO)).await
}

/// La ligne telle que l'écrivait l'ajout depuis les Favoris.
fn caipirinha_sans_duree() -> ServiceEntry {
    ServiceEntry {
        source: "qobuz".into(),
        source_id: CAIPIRINHA.into(),
        title: "Caipirinha".into(),
        artist: Some("DjeuhDjoah & Lieutenant Nicholson".into()),
        album: Some("Caipirinha".into()),
        album_source_id: None,
        duration_ms: Some(0),
        cover_url: None,
    }
}

/// La playlist de Bertrand, déjà enregistrée : un départ à durée connue, puis
/// « Caipirinha » à 0 — c'est elle qu'« À suivre » montre.
fn ranger_la_playlist(b: &Banc) {
    let depart = ServiceEntry {
        source_id: DEPART.into(),
        title: "El niño".into(),
        duration_ms: Some(241_000),
        ..caipirinha_sans_duree()
    };
    PlaylistRepo::with_backend(b.state.backend.clone())
        .add_entries_deduped(
            b.pid,
            &[
                EntryContent::Service(depart),
                EntryContent::Service(caipirinha_sans_duree()),
            ],
            None,
        )
        .unwrap();
}

async fn requete(
    routeur: axum::Router<AppState>,
    state: &AppState,
    methode: &str,
    uri: String,
    corps: serde_json::Value,
) {
    let rep = routeur
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = to_bytes(rep.into_body(), 4 * 1024 * 1024).await;
}

async fn lire_la_playlist(b: &Banc) {
    requete(
        crate::routes::playback::router(),
        &b.state,
        "POST",
        format!("/{}/play", b.zid),
        serde_json::json!({ "playlist_id": b.pid, "start_index": 0 }),
    )
    .await;
}

fn duree_en_file(b: &Banc) -> Option<i64> {
    PlayQueueRepo::with_backend(b.state.backend.clone())
        .get_ordered(b.zid)
        .unwrap()
        .into_iter()
        .find(|e| e.source_id.as_deref() == Some(CAIPIRINHA))
        .unwrap_or_else(|| panic!("« Caipirinha » absent de la file"))
        .duration_ms
}

fn duree_en_playlist(b: &Banc) -> Option<i64> {
    PlaylistRepo::with_backend(b.state.backend.clone())
        .get_entries(b.pid)
        .unwrap()
        .into_iter()
        .find_map(|e| match e.content {
            EntryContent::Service(s) if s.source_id == CAIPIRINHA => Some(s.duration_ms),
            _ => None,
        })
        .flatten()
}

/// Attend (borné) que la tâche de fond ait écrit la durée dans la file.
async fn attendre_la_duree_en_file(b: &Banc) -> Option<i64> {
    for _ in 0..100 {
        if duree_en_file(b).unwrap_or(0) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    duree_en_file(b)
}

#[tokio::test]
async fn l_ajout_d_un_titre_sans_duree_enregistre_celle_de_sa_fiche() {
    let b = banc().await;
    // Le corps qu'envoie « Ajouter à une playlist » sur un favori Qobuz.
    requete(
        super::router(),
        &b.state,
        "POST",
        format!("/{}/tracks", b.pid),
        serde_json::json!({
            "track_ids": [],
            "streaming_tracks": [{
                "source": "qobuz",
                "source_id": CAIPIRINHA,
                "title": "Caipirinha",
                "artist_name": "DjeuhDjoah & Lieutenant Nicholson",
                "album_title": "Caipirinha",
                "duration_ms": 0,
            }],
        }),
    )
    .await;

    let lignes = PlaylistRepo::with_backend(b.state.backend.clone())
        .get_entries(b.pid)
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

/// La ligne de Bertrand, déjà à 0 en base : après la lecture, la file reçoit
/// sa durée (tâche de fond), et la PLAYLIST est réécrite — une seule fois.
#[tokio::test]
async fn la_lecture_d_une_playlist_complete_la_file_puis_la_playlist() {
    let b = banc().await;
    ranger_la_playlist(&b);
    lire_la_playlist(&b).await;

    assert_eq!(
        attendre_la_duree_en_file(&b).await,
        Some(DUREE as i64),
        "la file reste à 0 : « À suivre » affiche « 0:00 »"
    );
    assert_eq!(
        duree_en_playlist(&b),
        Some(DUREE as i64),
        "la playlist n'est pas réécrite : chaque lecture rappellerait Qobuz"
    );
    let appels = b.appels.load(Ordering::SeqCst);
    lire_la_playlist(&b).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        b.appels.load(Ordering::SeqCst),
        appels,
        "une playlist corrigée ne rappelle plus le service"
    );
    assert_eq!(duree_en_file(&b), Some(DUREE as i64));
}

/// Relecture du 14/11 : un Qobuz LENT (3 s par fiche) ne retarde pas la
/// lecture. Elle part tout de suite, la ligne à 0, puis la ligne de la file
/// reçoit sa durée.
#[tokio::test]
async fn un_service_lent_ne_retarde_pas_la_lecture() {
    let b = banc_avec(Some(Duration::from_secs(3))).await;
    ranger_la_playlist(&b);
    let debut = std::time::Instant::now();
    lire_la_playlist(&b).await;
    let attente = debut.elapsed();
    assert!(
        attente < Duration::from_secs(2),
        "la lecture a attendu le service : {attente:?}"
    );
    assert_eq!(
        duree_en_file(&b).unwrap_or(0),
        0,
        "sans fiche en cache, la file part à 0"
    );
    for _ in 0..100 {
        if duree_en_file(&b).unwrap_or(0) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        duree_en_file(&b),
        Some(DUREE as i64),
        "la tâche de fond n'a pas complété la file"
    );
}

/// Sans Qobuz (absent du registre) : la lecture part, la ligne reste à 0,
/// rien ne casse.
#[tokio::test]
async fn sans_service_la_lecture_part_a_zero() {
    let b = banc_avec(None).await;
    ranger_la_playlist(&b);
    let debut = std::time::Instant::now();
    lire_la_playlist(&b).await;
    assert!(debut.elapsed() < Duration::from_secs(2));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(duree_en_file(&b).unwrap_or(0), 0);
    assert_eq!(duree_en_playlist(&b).unwrap_or(0), 0);
}

/// Témoin de #8756 : la playlist vient d'être AFFICHÉE (`GET …/tracks`
/// remplit le cache des fiches) ; la lecture reprend ce cache sans rappeler
/// Qobuz, et la file porte la durée dès l'écriture.
#[tokio::test]
async fn la_lecture_reprend_le_cache_de_la_fiche_sans_rappeler_le_service() {
    let b = banc().await;
    ranger_la_playlist(&b);
    requete(
        super::router(),
        &b.state,
        "GET",
        format!("/{}/tracks", b.pid),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(b.appels.load(Ordering::SeqCst), 1, "l'écran a lu la fiche");
    lire_la_playlist(&b).await;
    assert_eq!(
        duree_en_file(&b),
        Some(DUREE as i64),
        "la fiche en cache n'est pas reprise à l'écriture de la file"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        b.appels.load(Ordering::SeqCst),
        1,
        "la lecture a rappelé Qobuz alors que la fiche était en cache"
    );
}

/// Une durée déjà connue n'est ni remplacée ni redemandée, même pour un
/// titre que le service connaît (233 000 chez lui).
#[tokio::test]
async fn une_duree_connue_n_est_pas_redemandee() {
    let b = banc().await;
    let mut titre = caipirinha_sans_duree();
    titre.duration_ms = Some(111_000);
    super::metadonnees_7738::completer_durees(
        &b.state,
        vec![&mut titre],
        super::metadonnees_7738::DELAI_A_L_AJOUT,
    )
    .await;
    assert_eq!(titre.duration_ms, Some(111_000), "une durée connue reste");
    assert_eq!(b.appels.load(Ordering::SeqCst), 0, "aucune fiche demandée");
}
