//! #5395 — `POST /api/v1/zones/{id}/radio/artist`, la radio artiste à la
//! demande. Ce témoin attaque la ROUTE MONTÉE par `tune_server::routes::router`
//! avec un service simulé : ce qui se prouve ici est le câblage — la file de la
//! zone reçoit le premier lot (environ 20 % de l'artiste de départ), le contexte
//! qui fera recharger l'auto-lecture est écrit, et les refus sont dits.
//! La composition elle-même est prouvée dans `tune-core`
//! (`playback/radio_artiste_tests.rs`).
//!
//! L'API d'enrichissement pointe sur un port fermé : rien ne sort sur Internet.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::TuneError;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

fn piste(id: &str, artiste: &str) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: artiste.into(),
        album: None,
        album_id: None,
        duration_ms: 200_000,
        cover_path: None,
        track_number: None,
        disc_number: None,
        explicit: false,
        disponible: None,
        quality: None,
        isrc: None,
        composer: None,
        artist_id: None,
    }
}

fn artiste(id: &str, nom: &str) -> StreamArtist {
    StreamArtist {
        id: id.into(),
        name: nom.into(),
        image_path: None,
        bio: None,
    }
}

/// « Graine » (id g) et douze voisins v0…v11, six titres chacun (dix pour la
/// graine). Connecté : la radio l'interroge.
struct ServiceSimule;

fn nom_de(id: &str) -> Option<String> {
    if id == "g" {
        return Some("Graine".into());
    }
    let n: usize = id.strip_prefix('v')?.parse().ok()?;
    (n < 12).then(|| format!("Voisin {n}"))
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
        "qobuz-simule"
    }
    fn enabled(&self) -> bool {
        true
    }
    fn set_enabled(&mut self, _enabled: bool) {}
    async fn authenticate(&mut self, _c: &Value) -> Result<AuthStatus, TuneError> {
        Ok(AuthStatus::default())
    }
    async fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authenticated: true,
            ..Default::default()
        }
    }
    async fn logout(&mut self) -> Result<(), TuneError> {
        Ok(())
    }
    async fn search(&self, q: &str, _l: usize) -> Result<SearchResults, TuneError> {
        Ok(SearchResults {
            tracks: Vec::new(),
            albums: Vec::new(),
            artists: if q == "Graine" {
                vec![artiste("g", "Graine")]
            } else {
                Vec::new()
            },
            playlists: Vec::new(),
        })
    }
    async fn get_track(&self, id: &str) -> Result<StreamTrack, TuneError> {
        Err(TuneError::NotFound(format!("piste {id}")))
    }
    async fn get_track_url(&self, _t: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album(&self, _a: &str) -> Result<StreamAlbum, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_album_tracks(&self, _a: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist(&self, _a: &str) -> Result<StreamArtist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_artist_top_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        let Some(nom) = nom_de(id) else {
            return Ok(Vec::new());
        };
        let n = if id == "g" { 10 } else { 6 };
        Ok((0..n).map(|i| piste(&format!("{id}-{i}"), &nom)).collect())
    }
    async fn get_similar_artists(
        &self,
        id: &str,
        _limit: usize,
    ) -> Result<Vec<StreamArtist>, TuneError> {
        Ok(if id == "g" {
            (0..12)
                .map(|n| artiste(&format!("v{n}"), &format!("Voisin {n}")))
                .collect()
        } else {
            Vec::new()
        })
    }
    async fn get_playlist(&self, _p: &str) -> Result<StreamPlaylist, TuneError> {
        Err("hors sujet".into())
    }
    async fn get_playlist_tracks(&self, _p: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Err("hors sujet".into())
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
}

async fn banc() -> (tune_server::state::AppState, axum::Router) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    state
        .backend
        .execute_batch("INSERT INTO zones (id, name, output_type) VALUES (1, 'Onglet', 'browser');")
        .unwrap();
    state
        .orchestrator
        .services
        .lock()
        .await
        .register(Box::new(ServiceSimule));
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

async fn radio(app: &axum::Router, zone: i64, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/zones/{zone}/radio/artist"))
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

fn file(state: &tune_server::state::AppState) -> Vec<tune_core::db::play_queue_repo::QueueEntry> {
    tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(state.backend.clone())
        .get_ordered(1)
        .unwrap()
}

#[tokio::test]
async fn la_radio_remplit_la_file_et_ecrit_son_contexte() {
    let (state, app) = banc().await;
    let (status, corps) = radio(
        &app,
        1,
        json!({"artist": "Graine", "service": "qobuz-simule", "artist_id": "g"}),
    )
    .await;
    // La lecture d'un titre simulé échoue (aucune URL) : seul le câblage est
    // jugé ici, pas la lecture.
    assert_ne!(status, StatusCode::NOT_FOUND, "{corps}");
    assert_ne!(status, StatusCode::BAD_REQUEST, "{corps}");
    let lignes = file(&state);
    assert_eq!(lignes.len(), 50, "un lot de 50 titres en file");
    let de_la_graine = lignes
        .iter()
        .filter(|l| l.artist_name.as_deref() == Some("Graine"))
        .count();
    assert_eq!(de_la_graine, 10, "20 % de l'artiste de départ");
    assert_eq!(lignes[0].artist_name.as_deref(), Some("Graine"));
    assert!(
        lignes
            .iter()
            .all(|l| l.source.as_deref() == Some("qobuz-simule"))
    );
    for paire in lignes.windows(2) {
        assert_ne!(
            paire[0].artist_name, paire[1].artist_name,
            "même artiste d'affilée"
        );
    }
    let ctx = tune_core::playback::radio_artiste::lire_contexte(&state.backend, 1)
        .expect("le contexte de la radio est écrit dans les réglages de la zone");
    assert_eq!(ctx.artiste, "Graine");
    assert_eq!(ctx.service.as_deref(), Some("qobuz-simule"));
    assert_eq!(ctx.dernier_lot.len(), 50);
    let derniere = lignes.last().unwrap();
    assert!(ctx.continue_sur(&format!(
        "qobuz-simule:{}",
        derniere.source_id.as_deref().unwrap()
    )));
}

#[tokio::test]
async fn les_refus_sont_dits_et_ne_touchent_pas_la_file() {
    let (state, app) = banc().await;
    let (status, _) = radio(&app, 1, json!({"artist": "  "})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, corps) = radio(&app, 77, json!({"artist": "Graine"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(corps["error"], "zone_not_found");
    // Un artiste que personne ne connaît : aucun titre, la file reste vide et
    // aucun contexte ne détourne l'auto-lecture.
    let (status, corps) = radio(&app, 1, json!({"artist": "Inconnu au bataillon"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(corps["error"], "radio_artiste_vide");
    assert!(file(&state).is_empty());
    assert!(tune_core::playback::radio_artiste::lire_contexte(&state.backend, 1).is_none());
}
