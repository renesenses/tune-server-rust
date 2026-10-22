//! #6380 — Sandro, fils 2351 et 2352 (GentooPlayer, Diretta Renderer en DLNA,
//! piste de la BIBLIOTHÈQUE en FLAC passthrough) : après un redémarrage,
//! « Lecture » repart de 0:00 au lieu de la position restaurée.
//!
//! Journal du fil 2351 : `playback_position_restored position_ms=7000`, puis
//! `reprise_a_la_position_restauree position_ms=7000`, `file_session_created`,
//! `SetAVTransportURI`, `Play`, et le renderer ouvre le flux en `bytes=0-` :
//! **aucun `Seek`**. La position atteignait bien le `PlayRequest` (#2876),
//! mais une session fichier sert depuis l'octet 0 et ne lit pas `seek_ms`
//! (#2893).
//!
//! Le banc : le VRAI routeur, une zone DLNA en FLAC natif, un vrai fichier
//! FLAC, et une sortie factice de type `dlna` qui note les `seek` reçus. Le
//! redémarrage est rejoué comme `restore_playback_positions` le fait.
//!
//! Contre-épreuve : faire appeler `state.orchestrator.play` au lieu de
//! `play_a_la_position_restauree` dans `lancer_la_lecture` fait tomber les
//! trois tests de reprise (aucun `Seek` reçu).

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::NowPlaying;

const SORTIE: &str = "dlna:uuid-diretta-renderer-6380";
const PISTE: i64 = 3520;
/// La position du fil 2351 : une Pause à 7 s, puis le redémarrage.
const POSITION_RESTAUREE: i64 = 7_000;

struct Banc {
    state: AppState,
    zone: i64,
    _dossier: tempfile::TempDir,
}

async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures/test.flac");
    let chemin = dossier.path().join("twelve-spaceships.flac");
    std::fs::copy(&source, &chemin).unwrap();
    let chemin = chemin.to_string_lossy().into_owned();
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Banc 6380')",
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Banc 6380', 1)",
    ] {
        state.backend.execute(sql, &[]).unwrap();
    }
    let p: [&dyn ToSqlValue; 2] = [&PISTE, &chemin];
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, sample_rate, \
             bit_depth, file_path, format, channels, source, track_number, disc_number) \
             VALUES (?, 'Twelve Spaceships', 1, 1, 440320, 44100, 16, ?, 'flac', 2, 'local', 1, 1)",
            &p,
        )
        .unwrap();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone = zones
        .create("Diretta 6380", Some("dlna"), Some(SORTIE))
        .unwrap();
    // FLAC natif : la piste part en passthrough, dans une session FICHIER.
    zones.update_dlna_native_flac(zone, true).unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(SORTIE, "Diretta 6380").with_type("dlna"),
    ));
    Banc {
        state,
        zone,
        _dossier: dossier,
    }
}

/// Ce que fait `restore_playback_positions` au démarrage pour cette zone.
async fn redemarrer_a(b: &Banc, position_ms: i64) {
    let piste = TrackRepo::with_backend(b.state.backend.clone())
        .get(PISTE)
        .unwrap()
        .expect("la piste du banc");
    b.state
        .playback
        .restore_position(
            b.zone,
            position_ms,
            NowPlaying {
                source: "local".into(),
                ..NowPlaying::from_track(&piste)
            },
        )
        .await;
}

async fn appeler(b: &Banc, url: &str, corps: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method("POST").uri(url);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = crate::routes::router(b.state.clone())
        .oneshot(req.body(body).unwrap())
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
async fn recu_par_la_sortie(b: &Banc) -> (usize, Vec<u64>) {
    tokio::time::sleep(std::time::Duration::from_millis(1_200)).await;
    let arc = { b.state.outputs.lock().await.get(SORTIE) }.expect("sortie enregistrée");
    let guard = arc.lock().await;
    let mock = guard.as_any().downcast_ref::<MockOutput>().expect("mock");
    (mock.play_call_count().await, mock.seek_calls())
}

async fn verifier_la_reprise(b: &Banc, chemin: &str) {
    let np = b
        .state
        .playback
        .get_state(b.zone)
        .await
        .now_playing
        .unwrap();
    let sid = np.stream_id.expect("la lecture a posé un flux");
    assert!(
        b.state.streamer.is_seekable_session(&sid).await,
        "prémisse : la piste passthrough part dans une session fichier, servie depuis 0"
    );
    let (lectures, seeks) = recu_par_la_sortie(b).await;
    assert_eq!(lectures, 1, "{chemin} : prémisse, la piste a été jouée");
    assert_eq!(
        seeks,
        vec![POSITION_RESTAUREE as u64],
        "{chemin} : le renderer doit recevoir UN Seek à la position restaurée, \
         sinon il repart de 0:00 (#6380)"
    );
}

/// Le bouton Lecture du client web à l'arrêt : `{"track_id": N}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_lecture_apres_redemarrage_envoie_le_seek_a_la_position_restauree() {
    let b = banc().await;
    redemarrer_a(&b, POSITION_RESTAUREE).await;
    let (statut, rendu) = appeler(
        &b,
        &format!("/api/v1/zones/{}/play", b.zone),
        Some(json!({ "track_id": PISTE })),
    )
    .await;
    assert!(statut.is_success(), "{statut} {rendu}");
    verifier_la_reprise(&b, "play {track_id}").await;
}

/// `POST /play` sans corps — Play après Stop des autres clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_lecture_sans_corps_apres_redemarrage_envoie_le_seek() {
    let b = banc().await;
    redemarrer_a(&b, POSITION_RESTAUREE).await;
    let (statut, rendu) = appeler(&b, &format!("/api/v1/zones/{}/play", b.zone), None).await;
    assert!(statut.is_success(), "{statut} {rendu}");
    verifier_la_reprise(&b, "play sans corps").await;
}

/// `POST /resume` sur la zone arrêtée par le redémarrage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn la_reprise_apres_redemarrage_envoie_le_seek() {
    let b = banc().await;
    redemarrer_a(&b, POSITION_RESTAUREE).await;
    let (statut, rendu) = appeler(&b, &format!("/api/v1/zones/{}/resume", b.zone), None).await;
    assert!(statut.is_success(), "{statut} {rendu}");
    verifier_la_reprise(&b, "resume").await;
}

/// Témoin : une position restaurée à 0 (ou aucune) laisse la lecture partir du
/// début, sans `Seek`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn une_lecture_depuis_le_debut_n_envoie_aucun_seek() {
    let b = banc().await;
    redemarrer_a(&b, 0).await;
    let (statut, rendu) = appeler(
        &b,
        &format!("/api/v1/zones/{}/play", b.zone),
        Some(json!({ "track_id": PISTE })),
    )
    .await;
    assert!(statut.is_success(), "{statut} {rendu}");
    let (lectures, seeks) = recu_par_la_sortie(&b).await;
    assert_eq!(lectures, 1, "prémisse : la piste a été jouée");
    assert!(seeks.is_empty(), "aucun Seek sans reprise : {seeks:?}");
}

/// Le « curseur à 0 avant Lecture » (fil 2284, réponse 8717) : côté serveur,
/// la zone arrêtée par le redémarrage rend bien la position restaurée, sur la
/// fiche de zone comme sur la liste.
#[tokio::test]
async fn la_zone_arretee_apres_redemarrage_rend_la_position_restauree() {
    let b = banc().await;
    redemarrer_a(&b, POSITION_RESTAUREE).await;
    for url in [
        format!("/api/v1/zones/{}", b.zone),
        "/api/v1/zones".to_string(),
    ] {
        let rep = crate::routes::router(b.state.clone())
            .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::OK, "{url}");
        let octets = to_bytes(rep.into_body(), usize::MAX).await.unwrap();
        let corps: Value = serde_json::from_slice(&octets).unwrap();
        let zone = if corps.is_array() {
            corps
                .as_array()
                .unwrap()
                .iter()
                .find(|z| z["id"] == b.zone)
                .cloned()
                .expect("la zone dans la liste")
        } else {
            corps
        };
        assert_eq!(zone["state"], "stopped", "{url}");
        assert_eq!(zone["position_ms"], POSITION_RESTAUREE, "{url} : {zone}");
    }
}
