//! #7775 : le vrai service DSD natif publie aussi les niveaux de la source.
//! Aucun pilote ni périphérique : seule la session et le bus sont observés.
use super::*;
use crate::event_bus::{EventBus, TuneEvent};
use crate::playback::{NowPlaying, PlaybackManager};
use std::sync::Arc;

fn orchestrateur() -> (PlaybackOrchestrator, Arc<EventBus>) {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let mut orch = PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let bus = Arc::new(EventBus::new());
    orch.event_bus = Some(bus.clone());
    (orch, bus)
}

async fn premiere_trame(
    rx: &mut tokio::sync::broadcast::Receiver<TuneEvent>,
    zone: i64,
    attente: std::time::Duration,
) -> Option<serde_json::Value> {
    tokio::time::timeout(attente, async {
        loop {
            match rx.recv().await {
                Ok(ev)
                    if ev.event_type == "playback.audio_levels" && ev.data["zone_id"] == zone =>
                {
                    return ev.data;
                }
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("bus fermé : {e}"),
            }
        }
    })
    .await
    .ok()
}

async fn observer(
    zone: i64,
    debut_fichier: u64,
    seek: u64,
    prewarm: bool,
) -> Option<serde_json::Value> {
    let dossier = tempfile::tempdir().unwrap();
    let dsf = dossier.path().join("source.dsf");
    crate::audio::decode::dop_deplacement_tests_6456::ecrire_dsf(
        &dsf,
        4 * crate::audio::decode::dop_deplacement_tests_6456::OCTETS_PAR_SECONDE,
    );
    let (orch, bus) = orchestrateur();
    let mut rx = bus.subscribe();
    orch.playback.play(zone, NowPlaying::default()).await;
    orch.playback.seek(zone, seek as i64).await;
    let requete = PlayRequest {
        zone_id: zone,
        seek_ms: Some(seek),
        ..Default::default()
    };
    let mut piste = crate::db::models::Track::new("DSD témoin".into());
    piste.duration_ms = 4_000;
    let _prewarm = prewarm.then(|| orch.begin_levels_prewarm(zone));
    let resolu = orch
        .servir_le_dsd_natif(
            &requete,
            &piste,
            dsf.to_str().unwrap().to_string(),
            2_822_400,
            2,
            crate::audio::dsd_brut::Fenetre {
                debut_ms: debut_fichier,
                fin_ms: Some(4_000),
            },
        )
        .await
        .unwrap()
        .unwrap();
    // Le service reste natif, pas remplacé par la sonde PCM.
    assert_eq!(resolu.mime_type, crate::audio::dsd_brut::MIME);
    assert_eq!(resolu.bit_depth, Some(1));
    assert_eq!(resolu.sample_rate, Some(2_822_400));
    let session = resolu.stream_id.as_ref().unwrap();
    assert!(orch.streamer.session_alive(session).await);
    let vue = premiere_trame(&mut rx, zone, std::time::Duration::from_secs(2)).await;
    orch.playback.stop(zone).await;
    orch.streamer.remove_session(session).await;
    vue
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsd_natif_7775_publie_des_niveaux_reels() {
    let vue = observer(987_775, 0, 0, false).await.expect(
        "#7775 : le service DSD natif ne publie aucun niveau, VU et spectre restent à plat",
    );
    assert_eq!(vue["position_ms"], 0);
    assert!(
        vue["peak_left_db"].as_f64().unwrap() > -90.0,
        "la sonde doit mesurer la source DSD"
    );
    assert!(
        vue["spectrum"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_f64().unwrap() > 0.0)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsd_natif_7775_seek_et_cue_gardent_l_horloge_de_la_piste() {
    // Le fichier part à 2 s (tranche à 1 s + seek de 1 s), la piste à 1 s.
    let vue = observer(987_776, 2_000, 1_000, false)
        .await
        .expect("#7775 : aucun niveau après déplacement en DSD natif");
    assert_eq!(vue["position_ms"], 1_000);
    assert!(vue["peak_left_db"].as_f64().unwrap() > -90.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dsd_natif_7775_la_preparation_ne_publie_pas_la_piste_suivante() {
    assert!(
        observer(987_777, 0, 0, true).await.is_none(),
        "une session seulement préparée ne doit pas remplacer les niveaux de la piste en cours"
    );
}
