//! #6380 — Sandro, fil 2404 (rc7, GentooPlayer, Diretta Renderer en DLNA,
//! piste QOBUZ) : après un redémarrage, les pistes locales reprennent à leur
//! position (fil 2403), mais une piste Qobuz repart de 0:00.
//!
//! Journal du fil 2404 : `playback_position_restored zone_id=1
//! position_ms=19000 … source="qobuz"` au démarrage, puis au « Lecture »
//! `proxy_session_created`, `Play`, `file_de_lecture_ecrite
//! branche="titre_seul_deja_en_file"` — et AUCUNE ligne
//! `reprise_a_la_position_restauree`. Le bouton Lecture du client web, zone
//! arrêtée sur une piste de service, envoie `{source, source_id}`
//! (`playback-controls.ts`) : c'est la branche « titre de service seul » de
//! `POST /play`, qui bâtissait son `PlayRequest` avec `seek_ms: None`. Les
//! correctifs #2876 et #6662 ne couvraient que `{track_id}`, le corps vide et
//! `/resume`.
//!
//! Le banc : le VRAI routeur, une zone DLNA, un faux Qobuz dont l'URL de flux
//! ne mène nulle part (aucun appel réel), et une sortie factice `dlna` qui
//! note les `seek` reçus. Le redémarrage est rejoué comme
//! `restore_service_track_positions` le fait.
//!
//! Contre-épreuve : remettre `seek_ms: None` et `state.orchestrator.play`
//! dans la branche « titre de service seul » fait tomber
//! `la_lecture_d_une_piste_qobuz_apres_redemarrage_reprend_a_sa_position`
//! (aucun `Seek` reçu, curseur à 0).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::NowPlaying;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamQuality,
    StreamTrack, StreamUrl, StreamingService,
};

const SORTIE: &str = "dlna:uuid-diretta-renderer-6380-qobuz";
const SOURCE_ID: &str = "354925415";
/// La position du fil 2404.
const POSITION_RESTAUREE: i64 = 19_000;
const DUREE_MS: i64 = 395_000;

struct FauxQobuz;

fn non_servi() -> TuneError {
    TuneError::Streaming("faux Qobuz : rien d'autre n'est servi".into())
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
        Err(non_servi())
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
        Err(non_servi())
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        // Sans album : la route garde la branche « titre seul » (#5372),
        // celle du journal du fil 2404.
        Ok(StreamTrack {
            id: id.into(),
            title: "Redemption".into(),
            artist: "Banc 6380".into(),
            album: None,
            album_id: None,
            duration_ms: DUREE_MS as u64,
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
    async fn get_track_url(&self, id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Ok(StreamUrl {
            url: format!("https://127.0.0.1:9/file/{id}.flac"),
            mime_type: "audio/flac".into(),
            quality: StreamQuality {
                codec: "FLAC".into(),
                sample_rate: 44_100,
                bit_depth: 16,
                bitrate: None,
                channels: 2,
            },
            expires_at: None,
            headers: Vec::new(),
        })
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
    }
    async fn get_artist(&self, _id: &str) -> Result<StreamArtist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist(&self, _id: &str) -> Result<StreamPlaylist, TuneError> {
        Err(non_servi())
    }
    async fn get_playlist_tracks(&self, _id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err(non_servi())
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
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Diretta 6380 Qobuz", Some("dlna"), Some(SORTIE))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(SORTIE, "Diretta 6380 Qobuz").with_type("dlna"),
    ));
    (state, zone)
}

/// Ce que fait `restore_service_track_positions` au démarrage pour cette zone.
async fn redemarrer_a(state: &AppState, zone: i64, position_ms: i64) {
    state
        .playback
        .restore_position(
            zone,
            position_ms,
            NowPlaying {
                title: "Redemption".into(),
                source: "qobuz".into(),
                source_id: Some(SOURCE_ID.into()),
                duration_ms: DUREE_MS,
                ..Default::default()
            },
        )
        .await;
}

/// Le corps qu'envoie le bouton Lecture du client web, zone arrêtée sur une
/// piste de service (`playback-controls.ts`).
async fn appuyer_sur_lecture(state: &AppState, zone: i64) -> (StatusCode, Value) {
    let rep = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/zones/{zone}/play"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "source": "qobuz", "source_id": SOURCE_ID }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = rep.status();
    let octets = to_bytes(rep.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// `(play_media reçus, cibles des seek reçus)`, après la pose du `Seek`
/// détaché (`REPLAY_OUTPUT_SEEK_SETTLE_MS`, 500 ms).
async fn recu_par_la_sortie(state: &AppState) -> (usize, Vec<u64>) {
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    let arc = { state.outputs.lock().await.get(SORTIE) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.play_call_count().await, mock.seek_calls())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_lecture_d_une_piste_qobuz_apres_redemarrage_reprend_a_sa_position() {
    let (state, zone) = banc().await;
    redemarrer_a(&state, zone, POSITION_RESTAUREE).await;
    let (statut, rendu) = appuyer_sur_lecture(&state, zone).await;
    assert!(statut.is_success(), "{statut} {rendu}");
    let (lectures, seeks) = recu_par_la_sortie(&state).await;
    assert_eq!(lectures, 1, "prémisse : la piste Qobuz a été jouée");
    assert_eq!(
        seeks,
        vec![POSITION_RESTAUREE as u64],
        "le renderer doit recevoir UN Seek à la position restaurée, sinon la \
         piste Qobuz repart de 0:00 (#6380, fil 2404)"
    );
    assert_eq!(
        state.playback.pending_resume_ms(zone).await,
        None,
        "la reprise ne vaut qu'une fois"
    );
}

/// Témoin : une autre piste du même service, lancée après le redémarrage, part
/// du début — la reprise ne vaut que pour la piste restaurée.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_autre_piste_qobuz_part_du_debut_sans_seek() {
    let (state, zone) = banc().await;
    state
        .playback
        .restore_position(
            zone,
            POSITION_RESTAUREE,
            NowPlaying {
                title: "Autre".into(),
                source: "qobuz".into(),
                source_id: Some("autre-piste".into()),
                duration_ms: DUREE_MS,
                ..Default::default()
            },
        )
        .await;
    let (statut, rendu) = appuyer_sur_lecture(&state, zone).await;
    assert!(statut.is_success(), "{statut} {rendu}");
    let (lectures, seeks) = recu_par_la_sortie(&state).await;
    assert_eq!(lectures, 1, "prémisse : la piste a été jouée");
    assert!(seeks.is_empty(), "aucun Seek hors reprise : {seeks:?}");
}
