//! #8921 — Calum Loudon, fil 2381 (réponse 11409, rc12, WiiM Ultra en DLNA,
//! TIDAL) : une lecture lancée PENDANT qu'une piste joue, sans Stop, attendait
//! la fin du téléchargement DASH du préchargement de la piste suivante
//! (7,7 s, 29,3 s et 11,9 s avant la requête TIDAL de la nouvelle piste).
//!
//! Le préchargement Tidal tient le verrou en ÉCRITURE du service pendant tout
//! le téléchargement. Le crochet d'abandon de #8249 est dans l'orchestrateur,
//! mais la route `play` lit le service AVANT d'y arriver : fiche du titre
//! (`{source, source_id}` sans titre, le corps du testeur), pistes de l'album
//! (`streaming_album_id`, le geste du client web). Les tests de #8249
//! appelaient le crochet directement et ne voyaient pas ce chemin.
//!
//! Le banc : le VRAI routeur, une zone DLNA factice qui joue `A`, la file
//! `[A, LENTE, C]`, et un faux Tidal dont le « téléchargement DASH » de
//! `LENTE` ne finit jamais (aucun appel réel, aucun son). Le préchargement
//! de `LENTE` est en vol quand la lecture arrive.
//!
//! Contre-épreuve : retirer l'appel à
//! `abandonner_le_prechargement_pour_la_demande` dans
//! `play_sans_conteneur_de_file` fait tomber les trois tests « part sans
//! attendre » (la nouvelle piste n'est jamais demandée au service).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::error::TuneError;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::NowPlaying;
use tune_core::streaming::traits::{
    AuthStatus, SearchResults, StreamAlbum, StreamArtist, StreamPlaylist, StreamTrack, StreamUrl,
    StreamingService,
};

const SORTIE: &str = "dlna:uuid-wiim-ultra-8921";
/// La piste suivante, dont le téléchargement DASH ne finit jamais.
const LENTE: &str = "LENTE";

type Demandes = Arc<std::sync::Mutex<Vec<String>>>;

struct FauxTidal {
    demandes: Demandes,
}

fn non_servi() -> TuneError {
    TuneError::Streaming("faux Tidal : rien n'est servi".into())
}

fn piste(id: &str, album_id: Option<&str>) -> StreamTrack {
    StreamTrack {
        id: id.into(),
        title: format!("Titre {id}"),
        artist: "Banc 8921".into(),
        album: None,
        album_id: album_id.map(str::to_string),
        duration_ms: 200_000,
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
impl StreamingService for FauxTidal {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn name(&self) -> &str {
        "tidal"
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
        // Sans album : le titre seul garde la file d'un titre (#5372).
        Ok(piste(id, None))
    }
    async fn get_track_url(&self, id: &str, _q: Option<&str>) -> Result<StreamUrl, TuneError> {
        self.demandes.lock().unwrap().push(id.to_string());
        if id == LENTE {
            // Un téléchargement DASH qui n'en finit pas, verrou tenu.
            std::future::pending::<()>().await;
        }
        Err(non_servi())
    }
    async fn get_album(&self, _id: &str) -> Result<StreamAlbum, TuneError> {
        Err(non_servi())
    }
    async fn get_album_tracks(&self, id: &str) -> Result<Vec<StreamTrack>, TuneError> {
        Ok(vec![piste("ALB-1", Some(id)), piste("ALB-2", Some(id))])
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

fn titre_tidal(source_id: &str) -> QueueInput {
    QueueInput::Streaming {
        source: "tidal".into(),
        source_id: source_id.into(),
        title: format!("Titre {source_id}"),
        artist: "Banc 8921".into(),
        album: Some("Un album".into()),
        cover_url: None,
        duration_ms: 200_000,
        media_format: None,
        sample_rate: None,
        bit_depth: None,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

struct Banc {
    state: AppState,
    zone: i64,
    demandes: Demandes,
    prechargement: tokio::task::JoinHandle<()>,
}

/// La zone joue `A`, file `[A, LENTE, C]`, préchargement de `LENTE` en vol.
async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let demandes: Demandes = Arc::new(std::sync::Mutex::new(Vec::new()));
    state.services.lock().await.register(Box::new(FauxTidal {
        demandes: demandes.clone(),
    }));
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("WiiM Ultra 8921", Some("dlna"), Some(SORTIE))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(SORTIE, "WiiM Ultra 8921").with_type("dlna"),
    ));
    PlayQueueRepo::with_backend(state.backend.clone())
        .append(
            zone,
            &[titre_tidal("A"), titre_tidal(LENTE), titre_tidal("C")],
        )
        .unwrap();
    state
        .playback
        .play(
            zone,
            NowPlaying {
                title: "Titre A".into(),
                source: "tidal".into(),
                source_id: Some("A".into()),
                duration_ms: 200_000,
                ..Default::default()
            },
        )
        .await;
    state.playback.update_queue_info(zone, 0, 3).await;

    let prechargement = {
        let moteur = state.orchestrator.prefetch.clone();
        let (db, services, playback) = (
            state.backend.clone(),
            state.services.clone(),
            state.playback.clone(),
        );
        tokio::spawn(async move { moteur.prefetch_next(db, services, playback, zone).await })
    };
    assert!(
        demande_dans(&demandes, LENTE, Duration::from_secs(2)).await,
        "prémisse : le préchargement de la piste suivante est chez le service"
    );
    assert!(
        !service_libre(&state).await,
        "prémisse : le préchargement tient le verrou du service"
    );
    Banc {
        state,
        zone,
        demandes,
        prechargement,
    }
}

async fn demande_dans(demandes: &Demandes, id: &str, delai: Duration) -> bool {
    let fin = tokio::time::Instant::now() + delai;
    while tokio::time::Instant::now() < fin {
        if demandes.lock().unwrap().iter().any(|d| d == id) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Le verrou du service est-il repris dans la seconde ?
async fn service_libre(state: &AppState) -> bool {
    tokio::time::timeout(Duration::from_secs(1), async {
        let svc = state
            .services
            .lock()
            .await
            .get("tidal")
            .expect("faux Tidal");
        drop(svc.write().await);
    })
    .await
    .is_ok()
}

/// `POST /api/v1/zones/{zone}/play` en tâche de fond, comme la sonde de Calum.
fn lancer_la_lecture(banc: &Banc, corps: Value) -> tokio::task::JoinHandle<()> {
    let routeur = crate::routes::router(banc.state.clone());
    let uri = format!("/api/v1/zones/{}/play", banc.zone);
    tokio::spawn(async move {
        let _ = routeur
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(corps.to_string()))
                    .unwrap(),
            )
            .await;
    })
}

/// La nouvelle piste est demandée au service sans attendre la fin du
/// préchargement, et le préchargement abandonné se termine.
async fn part_sans_attendre(banc: Banc, corps: Value, attendue: &str) {
    let lecture = lancer_la_lecture(&banc, corps);
    let partie = demande_dans(&banc.demandes, attendue, Duration::from_secs(3)).await;
    lecture.abort();
    assert!(
        partie,
        "la lecture de `{attendue}` attend la fin du préchargement de `{LENTE}` \
         (demandes au service : {:?}) — #8921",
        banc.demandes.lock().unwrap()
    );
    tokio::time::timeout(Duration::from_secs(2), banc.prechargement)
        .await
        .expect("le préchargement abandonné se termine")
        .unwrap();
}

/// Le corps de la sonde de Calum : `{source, source_id}` sans titre, pour un
/// titre hors de la file.
#[tokio::test]
async fn un_titre_hors_file_part_sans_attendre_le_prechargement_8921() {
    let banc = banc().await;
    part_sans_attendre(
        banc,
        json!({ "source": "tidal", "source_id": "NOUVELLE" }),
        "NOUVELLE",
    )
    .await;
}

/// Même album, autre piste déjà dans la file (`relance_dans_la_file_contexte_
/// garde_6465` du journal de Calum).
#[tokio::test]
async fn un_titre_deja_dans_la_file_part_sans_attendre_le_prechargement_8921() {
    let banc = banc().await;
    part_sans_attendre(banc, json!({ "source": "tidal", "source_id": "C" }), "C").await;
}

/// Un autre album depuis le client web (`streaming_album_id`).
#[tokio::test]
async fn un_autre_album_part_sans_attendre_le_prechargement_8921() {
    let banc = banc().await;
    part_sans_attendre(
        banc,
        json!({ "source": "tidal", "streaming_album_id": "AUTRE-ALBUM" }),
        "ALB-1",
    )
    .await;
}

/// La piste préchargée elle-même garde son préchargement (la lecture reprend
/// son téléchargement, #7247) ; une lecture sur une AUTRE zone ne coupe pas
/// celui de cette zone.
#[tokio::test]
async fn la_piste_prechargee_et_les_autres_zones_gardent_le_prechargement_8921() {
    let banc = banc().await;
    let meme_piste = super::PlayRequest {
        source: Some("tidal".into()),
        source_id: Some(LENTE.into()),
        ..Default::default()
    };
    assert!(
        !super::abandonner_le_prechargement_pour_la_demande(&banc.state, banc.zone, &meme_piste),
        "la lecture de la piste préchargée a abandonné son propre préchargement"
    );
    let autre_album = super::PlayRequest {
        source: Some("tidal".into()),
        streaming_album_id: Some("AUTRE-ALBUM".into()),
        ..Default::default()
    };
    assert!(
        !super::abandonner_le_prechargement_pour_la_demande(
            &banc.state,
            banc.zone + 1,
            &autre_album
        ),
        "une lecture sur une autre zone a coupé le préchargement de celle-ci"
    );
    assert!(
        !service_libre(&banc.state).await,
        "le préchargement tient toujours le service"
    );
    banc.prechargement.abort();
}
