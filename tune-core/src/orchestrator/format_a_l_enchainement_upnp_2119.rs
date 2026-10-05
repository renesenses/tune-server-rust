//! Fil 2119 — un album lancé depuis le serveur multimédia de Tune LUI-MÊME
//! garde son vrai format à l'enchaînement sans blanc.
//!
//! Rapport envoyé depuis Tune 1.0.0-rc1 : WAV 24 bits / 176,4 kHz lu sur un
//! Devialet en DLNA, source `upnp`, URI
//! `http://<nous>:8888/api/v1/library/tracks/<id>/audio`. La piste 1 démarre
//! par `play_inner`, qui relit le `track_id` dans l'URI (#4323). Les pistes
//! suivantes arrivent par `advance_queue_metadata`, sans flux pré-armé
//! (`stream_id="absent"` : l'URL est servie directement) : le format, la
//! fréquence et la profondeur restaient vides, et le chemin du signal
//! annonçait « FLAC 44kHz/16bit ».

use std::sync::Arc;
use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::models::Track;
use crate::db::play_queue_repo::{PlayQueueRepo, StreamingQueueItem};
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::registry::ServiceRegistry;

use super::PlaybackOrchestrator;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Une piste de la bibliothèque en WAV 24/176,4, comme celles du rapport.
fn piste_wav_hi_res(orch: &PlaybackOrchestrator, titre: &str, n: i64) -> i64 {
    let mut t = Track::new(titre.into());
    t.artist_name = Some("Eugen Jochum".into());
    t.album_title = Some("Carmina Burana".into());
    t.file_path = Some(format!("/music/carmina/{n:02}.wav"));
    t.format = Some("wav".into());
    t.sample_rate = Some(176_400);
    t.bit_depth = Some(24);
    t.channels = 2;
    t.duration_ms = 208_533;
    TrackRepo::with_backend(orch.db.clone()).create(&t).unwrap()
}

/// File `upnp` de deux pistes servies par `base` ; la piste 1 joue.
async fn album_upnp(base: &str) -> (PlaybackOrchestrator, i64, i64) {
    let orch = orchestrateur();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("My Devialet", Some("dlna"), Some("uuid:devialet"))
        .unwrap();
    let ids = [
        piste_wav_hi_res(&orch, "01 - O Fortuna", 1),
        piste_wav_hi_res(&orch, "02 - Fortune plango vulnera", 2),
    ];
    let file: Vec<StreamingQueueItem> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            (
                crate::upnp_server::track_audio_url(base, *id),
                // Le point de contrôle n'a rien nommé : titre vide.
                String::new(),
                String::new(),
                None,
                None,
                0i64,
                Some("upnp".to_string()),
                Some(i as i64 + 1),
                None,
            )
        })
        .collect();
    PlayQueueRepo::with_backend(orch.db.clone())
        .set_streaming_queue(zone_id, &file)
        .unwrap();
    orch.playback
        .play(
            zone_id,
            NowPlaying {
                track_id: Some(ids[0]),
                title: "01 - O Fortuna".into(),
                source: "upnp".into(),
                source_id: Some(crate::upnp_server::track_audio_url(base, ids[0])),
                format: Some("wav".into()),
                sample_rate: Some(176_400),
                bit_depth: Some(24),
                ..Default::default()
            },
        )
        .await;
    (orch, zone_id, ids[1])
}

async fn piste_en_cours(orch: &PlaybackOrchestrator, zone_id: i64) -> NowPlaying {
    orch.playback
        .get_state(zone_id)
        .await
        .now_playing
        .expect("la zone joue")
}

/// Le témoin : la piste 2, enchaînée sans flux pré-armé, publie wav/176400/24.
#[tokio::test]
async fn la_piste_2_enchainee_garde_son_format_wav_hi_res_2119() {
    let base = "http://127.0.0.1:8888";
    let (orch, zone_id, piste_2) = album_upnp(base).await;

    orch.advance_queue_metadata(zone_id, 1)
        .await
        .expect("l'avance gapless doit aboutir");

    let np = piste_en_cours(&orch, zone_id).await;
    assert_eq!(
        np.stream_id, None,
        "aucun flux pré-armé : URL servie telle quelle"
    );
    assert_eq!(
        (np.format.as_deref(), np.sample_rate, np.bit_depth),
        (Some("wav"), Some(176_400), Some(24)),
        "la piste 2 doit annoncer le format de sa fiche de bibliothèque, et \
         non rien (affiché « FLAC 44kHz/16bit » par le chemin du signal)"
    );
    assert_eq!(
        np.track_id,
        Some(piste_2),
        "même règle qu'au démarrage (#4323)"
    );
    assert_eq!(np.title, "02 - Fortune plango vulnera");
    assert_eq!(np.source, "upnp", "la source de la file ne bouge pas");
    assert_eq!(
        np.source_id.as_deref(),
        Some(crate::upnp_server::track_audio_url(base, piste_2).as_str()),
        "l'URI d'origine reste la clé de session du renderer"
    );
}

/// Négatif : l'URI vient d'un AUTRE Tune du réseau. Son identifiant ne
/// désigne pas nos pistes : rien n'est repris, aucun format n'est inventé.
#[tokio::test]
async fn l_uri_d_un_autre_tune_ne_reprend_rien_2119() {
    let (orch, zone_id, _) = album_upnp("http://203.0.113.9:8888").await;

    orch.advance_queue_metadata(zone_id, 1).await.unwrap();

    let np = piste_en_cours(&orch, zone_id).await;
    assert_eq!(np.track_id, None);
    assert_eq!(
        (np.format.as_deref(), np.sample_rate, np.bit_depth),
        (None, None, None),
        "un hôte étranger ne doit rien affirmer"
    );
    assert_eq!(np.source, "upnp");
}
