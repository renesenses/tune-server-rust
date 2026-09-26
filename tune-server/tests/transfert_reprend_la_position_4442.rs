//! #4442 — transférer la lecture VERS une zone réseau doit la reprendre à la
//! position de la source, pas au début.
//!
//! FabienM (0.9.154, fil 1839) : vers la zone Salon — un Devialet Phantom,
//! renderer Rygel — « le transfert reprend depuis le début ». Aucun journal
//! n'a été joint : ce que le Phantom a fait du `Seek` n'est pas mesuré.
//!
//! Ce qui est établi, dans le code : `do_transfer` envoyait le `Seek` nu
//! dans la foulée du `Play`, sans laisser le `Play` prendre et sans relire
//! l'état ensuite — les deux précautions que la relecture d'égaliseur prend
//! pour CE renderer (fil 1780 : un `Seek` reçu avant que le `Play` ait pris
//! laisse le Rygel en pause). La sortie factice reproduit ce comportement
//! documenté (`with_seek_qui_laisse_en_pause`).
//!
//! Contre-épreuve : remettre `orchestrator.seek(...)` à la place de
//! `reprendre_la_position_transferee(...)` dans `do_transfer` fait tomber
//! `le_transfert_reprend_a_la_position_et_ne_reste_pas_en_pause`.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` du manifeste.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::TransportState;
use tune_core::outputs::mock::MockOutput;
use tune_server::state::AppState;

const POSITION_SOURCE_MS: i64 = 80_000;
/// Plus longue que la position : `seek` borne la position à la durée RÉELLE
/// du fichier, et la fixture FLAC du dépôt ne dure qu'une seconde.
const DUREE_PISTE_MS: u32 = 100_000;
const TAUX: u32 = 8_000;

/// Un WAV PCM 16 bits mono de silence, écrit à la main (aucun outil).
fn wav_de_silence(duree_ms: u32) -> Vec<u8> {
    let donnees = TAUX * 2 * duree_ms / 1000;
    let mut wav = Vec::with_capacity(44 + donnees as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + donnees).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&TAUX.to_le_bytes());
    wav.extend_from_slice(&(TAUX * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&donnees.to_le_bytes());
    wav.resize(44 + donnees as usize, 0);
    wav
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(chemin)
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
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

/// Ce que la sortie cible a reçu : position, état, nombre de relances.
async fn cible_recue(state: &AppState) -> (u64, TransportState, u64) {
    let registre = state.outputs.lock().await;
    let sortie = registre.get("dlna-salon").expect("sortie enregistree");
    let garde = sortie.lock().await;
    let statut = garde.get_status().await.expect("statut de la cible");
    let mock = garde
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("la sortie factice");
    (statut.position_ms, statut.state, mock.resume_call_count())
}

/// Une zone source qui joue une piste WAV réelle à 80 s, et une zone cible
/// DLNA dont le renderer se comporte comme le Rygel du Devialet.
async fn source_a_80_s() -> (axum::Router, AppState, i64, i64, tempfile::TempDir) {
    // `do_transfer` persiste la file de la cible à côté de `config.db_path`
    // (`queue_state/`) : on la pose dans le dossier jetable, sinon ce dossier
    // atterrirait dans le répertoire de la caisse.
    let dir = tempfile::tempdir().unwrap();
    let config = tune_server::config::TuneConfig {
        db_path: dir.path().join("tune.db").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let state = AppState::new(":memory:", 0, config).unwrap();
    let app = tune_server::routes::router(state.clone());

    let chemin = dir.path().join("piste.wav");
    std::fs::write(&chemin, wav_de_silence(DUREE_PISTE_MS)).unwrap();
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
                                 duration_ms, sample_rate, bit_depth, channels, source) \
             VALUES (1, 'Canopée', 1, 1, '{}', 'wav', {DUREE_PISTE_MS}, {TAUX}, 16, 1, 'local');",
            chemin.to_string_lossy()
        ))
        .unwrap();

    let zones = ZoneRepo::with_backend(state.backend.clone());
    let source = zones
        .create("Parents", Some("dlna"), Some("dlna-parents"))
        .unwrap();
    let cible = zones
        .create("Salon", Some("dlna"), Some("dlna-salon"))
        .unwrap();
    {
        let mut registre = state.outputs.lock().await;
        registre.register(Box::new(
            MockOutput::new("dlna-parents", "Beosound Stage").with_type("dlna"),
        ));
        registre.register(Box::new(
            MockOutput::new("dlna-salon", "Phantom 108db")
                .with_type("dlna")
                .with_seek_qui_laisse_en_pause(),
        ));
    }

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{source}/queue/add"),
        json!({ "track_ids": [1] }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mise en file : {corps}");
    state
        .orchestrator
        .play_from_queue(source, 0)
        .await
        .expect("lecture sur la source");
    state
        .playback
        .update_position(source, POSITION_SOURCE_MS)
        .await;

    (app, state, source, cible, dir)
}

#[tokio::test]
async fn le_transfert_reprend_a_la_position_et_ne_reste_pas_en_pause() {
    let (app, state, source, cible, _dir) = source_a_80_s().await;

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{source}/transfer/{cible}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "transfert : {corps}");

    let (position, etat, relances) = cible_recue(&state).await;
    assert_eq!(
        position, POSITION_SOURCE_MS as u64,
        "le renderer cible doit avoir reçu la position de la source"
    );
    assert_eq!(
        etat,
        TransportState::Playing,
        "un renderer resté en pause après le `Seek` (Rygel du Devialet, fil 1780) \
         doit être relancé — {relances} relance(s)"
    );
}

/// Une source en pause reste en pause sur la cible : la relance d'après le
/// `Seek` ne doit pas faire entendre la piste avant la pause.
#[tokio::test]
async fn une_source_en_pause_reste_en_pause_sans_relance() {
    let (app, state, source, cible, _dir) = source_a_80_s().await;
    state.playback.pause(source).await;

    let (status, corps) = poster(
        &app,
        &format!("/api/v1/zones/{source}/transfer/{cible}"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "transfert : {corps}");

    let (position, etat, relances) = cible_recue(&state).await;
    assert_eq!(position, POSITION_SOURCE_MS as u64);
    assert_eq!(etat, TransportState::Paused);
    assert_eq!(relances, 0, "aucune relance quand la source était en pause");
}
