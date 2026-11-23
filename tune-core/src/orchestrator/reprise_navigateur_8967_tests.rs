//! #8967 (FabienM, fil 2864, rc12) — zone navigateur « PC Fab », piste en
//! pause depuis la veille : Lecture rendait un 502 « sa session de flux n'est
//! plus disponible… Relancez la piste » (`resume_stream_session_lost
//! position_ms=None`). Le ramasse-miettes avait retiré la session après
//! trente minutes sans octet servi, et `resume` ne savait rétablir qu'une zone
//! à périphérique.
//!
//! Attendu : la piste repart, à la dernière position rapportée par le lecteur
//! quand elle est connue, sinon au début, par un flux neuf.
//!
//! Contre-épreuve : retirer le bras « zone navigateur » de `reprendre`
//! (`transport.rs`) fait retomber ces épreuves sur l'erreur.
use super::PlayRequest;
use super::PlaybackOrchestrator;
use super::reprise_dlna_position_2095::QobuzDeBanc;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const SOURCE_ID: &str = "8967001";

/// Une zone navigateur (aucun périphérique) qui joue une piste Qobuz.
/// Rend `(orchestrateur, zone, stream_id du premier flux)`.
async fn zone_navigateur_en_lecture() -> (PlaybackOrchestrator, i64, String) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let mut services = ServiceRegistry::new();
    services.register(Box::new(QobuzDeBanc));
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(services)),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("PC Fab", Some("browser"), None)
        .unwrap();
    let resultat = orch
        .play(PlayRequest {
            zone_id,
            output_device_id: None,
            track_id: None,
            source: Some("qobuz".into()),
            source_id: Some(SOURCE_ID.into()),
            title: Some("Castle In The Snow".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(240_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
            album_ref: None,
        })
        .await
        .expect("prémisse : la zone navigateur joue la piste");
    assert!(
        resultat.stream_url.is_some(),
        "prémisse : un flux est servi"
    );
    let sid = orch
        .playback
        .get_state(zone_id)
        .await
        .now_playing
        .and_then(|np| np.stream_id)
        .expect("prémisse : la piste porte un flux");
    (orch, zone_id, sid)
}

/// Pause, puis la session disparaît comme après trente minutes sans lecture.
async fn pause_puis_session_expiree(orch: &PlaybackOrchestrator, zone_id: i64, sid: &str) {
    orch.pause(zone_id, None).await.expect("pause");
    orch.streamer.remove_session(sid).await;
    assert!(
        !orch.streamer.session_alive(sid).await,
        "prémisse : la session est morte"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lecture_apres_session_expiree_relance_a_la_position_du_lecteur_8967() {
    let (orch, zone_id, sid) = zone_navigateur_en_lecture().await;
    // Le lecteur web confirme, puis rapporte sa position (toutes les 5 s).
    orch.signaler_lecture_navigateur(zone_id, &sid, 1_000).await;
    orch.signaler_lecture_navigateur(zone_id, &sid, 95_000)
        .await;
    assert_eq!(
        orch.playback.get_state(zone_id).await.position_ms,
        95_000,
        "prémisse : la position du lecteur est mémorisée"
    );
    pause_puis_session_expiree(&orch, zone_id, &sid).await;

    orch.resume(zone_id, None)
        .await
        .expect("#8967 : Lecture après expiration doit relancer la piste, pas échouer");

    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(etat.state, PlayState::Playing);
    let np = etat.now_playing.expect("la piste est toujours là");
    assert_eq!(np.source_id.as_deref(), Some(SOURCE_ID), "la MÊME piste");
    let nouveau = np.stream_id.expect("un flux neuf");
    assert_ne!(nouveau, sid, "un flux NEUF, l'ancien est mort");
    assert!(orch.streamer.session_alive(&nouveau).await);
    assert_eq!(etat.position_ms, 95_000, "reprise au point rapporté");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_position_rapportee_la_piste_repart_au_debut_8967() {
    let (orch, zone_id, sid) = zone_navigateur_en_lecture().await;
    pause_puis_session_expiree(&orch, zone_id, &sid).await;

    orch.resume(zone_id, None)
        .await
        .expect("#8967 : sans position connue, la piste repart du début");

    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(etat.state, PlayState::Playing);
    assert_eq!(etat.position_ms, 0);
    let nouveau = etat.now_playing.and_then(|np| np.stream_id).unwrap();
    assert_ne!(nouveau, sid);
}
