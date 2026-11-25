//! Fil 3075 (Tune Remote Android) — une zone navigateur reste en pause plus
//! de trente minutes, le ramasse-miettes retire sa session de flux, puis
//! Lecture. Attendu : la MÊME piste repart à la position mémorisée sur un
//! flux neuf, sans seconde ligne d'historique ; une piste disparue se dit
//! avec un code stable ; les autres zones gardent leur comportement.
//!
//! Complète `reprise_navigateur_8967_tests` (piste de service) avec une piste
//! de la bibliothèque, l'historique et la piste supprimée.
use super::PlayRequest;
use super::PlaybackOrchestrator;
use super::reprise_piste_disparue_3075::{
    CODE_PISTE_DISPARUE_A_LA_REPRISE, message_de_piste_disparue,
};
use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::OutputCommandError;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Une piste de la bibliothèque (vrai FLAC sur disque). Rend le répertoire,
/// qui doit vivre aussi longtemps que le test, et le chemin du fichier.
fn piste_de_bibliotheque(orch: &PlaybackOrchestrator) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let piste = dir.path().join("morceau.flac");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
        &piste,
    )
    .unwrap();
    let chemin = piste.to_string_lossy().into_owned();
    orch.db
        .execute("INSERT INTO artists (id, name) VALUES (1, 'Artiste')", &[])
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
    orch.db
        .execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
             duration_ms, sample_rate, bit_depth, channels) \
             VALUES (1, 'Morceau', 1, 1, ?, 'flac', 300000, 44100, 16, 2)",
            &[&chemin as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    (dir, piste)
}

fn lignes_d_historique(orch: &PlaybackOrchestrator) -> i64 {
    orch.db
        .query_one("SELECT COUNT(*) FROM listen_history", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
}

/// Zone navigateur qui joue la piste 1 ; le lecteur a confirmé la lecture
/// puis rapporté 95 s. Rend `(orch, zone, flux, répertoire, fichier)`.
async fn zone_navigateur_a_95_s() -> (
    PlaybackOrchestrator,
    i64,
    String,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let orch = orchestrateur();
    let (dir, fichier) = piste_de_bibliotheque(&orch);
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Téléphone de Fabien", Some("browser"), None)
        .unwrap();
    let resultat = orch
        .play(PlayRequest {
            zone_id,
            track_id: Some(1),
            ..Default::default()
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
    orch.signaler_lecture_navigateur(zone_id, &sid, 1_000).await;
    orch.signaler_lecture_navigateur(zone_id, &sid, 95_000)
        .await;
    assert_eq!(
        orch.playback.get_state(zone_id).await.position_ms,
        95_000,
        "prémisse : la position du lecteur est mémorisée"
    );
    (orch, zone_id, sid, dir, fichier)
}

/// Pause, puis la session disparaît comme après trente minutes sans lecture.
async fn pause_puis_purge(orch: &PlaybackOrchestrator, zone_id: i64, sid: &str) {
    orch.pause(zone_id, None).await.expect("pause");
    orch.streamer.remove_session(sid).await;
    assert!(
        !orch.streamer.session_alive(sid).await,
        "prémisse : session purgée"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reprise_apres_purge_meme_piste_meme_position_nouveau_flux_3075() {
    let (orch, zone_id, sid, _dir, _fichier) = zone_navigateur_a_95_s().await;
    // Le sondeur valide l'écoute (seuil franchi à 95 s) : une ligne.
    orch.valider_l_ecoute(&orch.playback.get_state(zone_id).await);
    let historique_avant = lignes_d_historique(&orch);
    assert_eq!(
        historique_avant, 1,
        "prémisse : la lecture confirmée a écrit sa ligne d'historique"
    );
    pause_puis_purge(&orch, zone_id, &sid).await;

    orch.resume(zone_id, None)
        .await
        .expect("fil 3075 : Lecture après la purge doit rétablir la lecture");

    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(etat.state, PlayState::Playing);
    let np = etat.now_playing.expect("la piste est toujours là");
    assert_eq!(np.track_id, Some(1), "la MÊME piste");
    let nouveau = np.stream_id.expect("un flux neuf");
    assert_ne!(nouveau, sid, "un flux NEUF, l'ancien est mort");
    assert!(orch.streamer.session_alive(&nouveau).await);
    assert_eq!(etat.position_ms, 95_000, "reprise à la position mémorisée");

    // Le lecteur recharge le nouveau flux et le confirme, puis le sondeur
    // repasse : la reprise n'est pas une nouvelle écoute.
    orch.signaler_lecture_navigateur(zone_id, &nouveau, 96_000)
        .await;
    orch.signaler_lecture_navigateur(zone_id, &nouveau, 101_000)
        .await;
    orch.valider_l_ecoute(&orch.playback.get_state(zone_id).await);
    assert_eq!(
        lignes_d_historique(&orch),
        historique_avant,
        "une reprise n'écrit pas de seconde ligne d'historique"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn piste_supprimee_pendant_la_pause_rend_un_code_stable_3075() {
    let (orch, zone_id, sid, _dir, fichier) = zone_navigateur_a_95_s().await;
    pause_puis_purge(&orch, zone_id, &sid).await;
    std::fs::remove_file(&fichier).unwrap();

    let erreur = orch
        .resume(zone_id, None)
        .await
        .expect_err("une piste supprimée ne peut pas reprendre");
    let OutputCommandError::Failed { message, .. } = &erreur else {
        panic!("erreur inattendue : {erreur:?}");
    };
    assert!(
        message.starts_with(CODE_PISTE_DISPARUE_A_LA_REPRISE),
        "le code stable ouvre le message : {message}"
    );
    assert!(
        message_de_piste_disparue(message).is_some_and(|m| !m.is_empty()),
        "la phrase pour l'auditeur se relit sans le code : {message}"
    );
    assert_ne!(
        orch.playback.get_state(zone_id).await.state,
        PlayState::Playing,
        "rien ne prétend jouer"
    );
}

/// Une zone qui n'est PAS lue par un navigateur, sans périphérique joignable,
/// dont la session est morte : comportement d'avant, l'explication « session
/// perdue », sans relecture ni code de piste disparue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zone_non_navigateur_inchangee_3075() {
    let orch = orchestrateur();
    let (_dir, _fichier) = piste_de_bibliotheque(&orch);
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("dlna"), None)
        .unwrap();
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(1),
                title: "Morceau".into(),
                source: "local".into(),
                duration_ms: 300_000,
                stream_id: Some("flux-purge-3075".into()),
                ..Default::default()
            },
        )
        .await;
    orch.playback.pause(zone_id).await;
    let generation = orch.playback.get_state(zone_id).await.track_generation;

    let erreur = orch
        .resume(zone_id, None)
        .await
        .expect_err("une zone DLNA sans périphérique garde l'explication");
    let OutputCommandError::Failed { message, .. } = &erreur else {
        panic!("erreur inattendue : {erreur:?}");
    };
    assert!(
        message_de_piste_disparue(message).is_none(),
        "pas de code de piste disparue hors zone navigateur : {message}"
    );
    let etat = orch.playback.get_state(zone_id).await;
    assert_eq!(etat.track_generation, generation, "aucune relecture tentée");
    assert_eq!(
        etat.now_playing.and_then(|np| np.stream_id).as_deref(),
        Some("flux-purge-3075"),
        "le flux n'a pas été recréé"
    );
}
