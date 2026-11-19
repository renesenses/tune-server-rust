//! #7330 — le VRAI sondeur face au vrai `OaatOutput` pendant l'attente de fin
//! jouée.
//!
//! Relecture du 14/11 : `chain_exhausted` levé AVANT l'attente faisait voir au
//! sondeur une sortie « en lecture », figée à la durée, sans gapless interne :
//! la condition de `reached_end_exclusive`. Au bout de trois sondes, il
//! avançait la file, la tâche qui attendait était interrompue, la fin restait
//! coupée et le `stop` n'était jamais envoyé. Le cas : un album, avance de
//! départ de 3 à 4 s.
//!
//! Le banc : une piste de 4 s (au-delà de `END_MARGIN_MS`), une suivante
//! annoncée en file, une avance de départ de plus de 3 s (RTT d'horloge de
//! 1,6 s), et `tick` appelé toutes les 250 ms. La file ne doit bouger qu'une
//! fois, et seulement après le `stop` reçu par l'endpoint.

use super::*;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::OutputRegistry;
use crate::outputs::oaat::banc_fin_7330::{self, ENDPOINT_ID};
use crate::outputs::traits::PlayMedia;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::streaming::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

const PISTE_MS: u32 = 4_000;

#[tokio::test]
async fn le_sondeur_n_avance_la_file_qu_apres_le_stop_de_fin_jouee_7330() {
    let banc = banc_fin_7330::monter(PISTE_MS, Duration::from_millis(800)).await;

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("OAAT 7330", Some("oaat"), Some(ENDPOINT_ID))
        .unwrap();
    let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
    outputs.lock().await.register(Box::new(banc.sortie()));
    let playback = Arc::new(PlaybackManager::new());
    let orchestrator = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        outputs.clone(),
        None,
    ));
    let poller = PositionPoller::new(
        orchestrator,
        playback.clone(),
        outputs.clone(),
        db,
        Arc::new(Mutex::new(HashMap::new())),
    );

    playback
        .play(
            zone,
            NowPlaying {
                track_id: Some(1),
                title: "Plage 1".into(),
                source: "local".into(),
                duration_ms: PISTE_MS as i64,
                ..Default::default()
            },
        )
        .await;
    // Une suivante en file : la plage 2 de l'album.
    playback.update_queue_info(zone, 0, 2).await;
    let depart = playback.get_state(zone).await;
    let mut polls = HashMap::from([(zone, ZonePollState::new(depart.track_generation))]);
    let mut idle = HashMap::new();
    {
        let arc = outputs.lock().await.get(ENDPOINT_ID).unwrap();
        let sortie = arc.lock().await;
        sortie
            .play_media(&PlayMedia {
                url: &banc.url,
                mime_type: "audio/wav",
                title: Some("Plage 1"),
                duration_ms: Some(PISTE_MS as u64),
                ..Default::default()
            })
            .await
            .expect("play_media");
    }
    if let Some(ps) = polls.get_mut(&zone) {
        ps.track_started_at = Some(Instant::now());
        ps.track_loaded_at = Instant::now();
    }

    // Premier instant (horloge oaat) où la zone quitte la plage 1, et nombre
    // de changements de génération observés.
    let mut premier_mouvement_ns: Option<u64> = None;
    let mut changements = 0u32;
    let mut generation = depart.track_generation;
    let mut gapless_menteur = false;
    let limite = std::time::Instant::now() + Duration::from_secs(30);
    let mut apres_stop: Option<std::time::Instant> = None;
    while std::time::Instant::now() < limite {
        poller.tick(&mut polls, &mut idle, &Instant::now()).await;

        let o = banc.photo();
        // Pendant l'attente (LAST_PACKET reçu, pas encore de stop), la sortie
        // qui se dit en lecture doit encore annoncer son gapless interne :
        // sinon `reached_end_exclusive` la juge finie.
        if o.last_packet_recu_ns.is_some() && o.stop_recu_ns.is_none() {
            let arc = outputs.lock().await.get(ENDPOINT_ID).unwrap();
            let sortie = arc.lock().await;
            let joue = sortie
                .get_status()
                .await
                .map(|s| s.state == TransportState::Playing)
                .unwrap_or(false);
            if joue && !sortie.supports_internal_gapless() {
                gapless_menteur = true;
            }
        }

        let etat = playback.get_state(zone).await;
        if etat.track_generation != generation {
            changements += 1;
            generation = etat.track_generation;
        }
        let a_bouge = etat.track_generation != depart.track_generation
            || etat.state != crate::playback::PlayState::Playing
            || etat.queue_position != depart.queue_position;
        if a_bouge && premier_mouvement_ns.is_none() {
            premier_mouvement_ns = Some(oaat_core::time::now_ns());
        }
        if o.stop_recu_ns.is_some() || o.ferme_ns.is_some() {
            let t = *apres_stop.get_or_insert_with(std::time::Instant::now);
            if t.elapsed() > Duration::from_secs(3) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let o = banc.photo();
    let (premier_pts, premier_recu) = o.premier.expect("aucun paquet audio : rien de prouvé");
    assert!(
        premier_pts.saturating_sub(premier_recu) >= 3_000_000_000,
        "le banc veut une avance de départ d'au moins 3 s ({} ms)",
        premier_pts.saturating_sub(premier_recu) / 1_000_000
    );
    assert!(
        o.last_packet_recu_ns.is_some(),
        "fin de piste jamais atteinte : {o:?}"
    );
    assert!(
        !gapless_menteur,
        "pendant l'attente de fin jouée, la sortie se disait en lecture SANS \
         gapless interne : le sondeur la juge finie (reached_end_exclusive)"
    );
    let stop = o.stop_recu_ns.unwrap_or_else(|| {
        panic!(
            "aucun stop reçu : l'attente a été interrompue (connexion fermée \
             sans stop) — le sondeur a avancé la file pendant la fin jouée : {o:?}"
        )
    });
    assert!(
        stop >= o.dernier_pts_audio.unwrap(),
        "stop reçu avant l'échéance du dernier paquet"
    );
    if let Some(mouvement) = premier_mouvement_ns {
        assert!(
            mouvement >= stop,
            "la file a bougé {} ms AVANT le stop de fin : la fin de la piste est coupée",
            (stop - mouvement) / 1_000_000
        );
    }
    assert!(
        changements <= 1,
        "la file a avancé {changements} fois pour une seule fin de piste"
    );
}
