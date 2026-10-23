//! #6456 (Levente, fil 2305, FiiO K13 R2R) — « in this case the 'vu' meters
//! are not working » : un `.dsf` servi en DoP sur une sortie locale laissait
//! les VU-mètres vides.
//!
//! ## La cause, lue dans le code
//!
//! La branche DoP (`anticiper_le_dop`) ouvre sa session et y pousse le
//! porteur DoP, sans rien d'autre. Ce porteur est du DSD emballé dans du PCM
//! 24 bits : aucun niveau ne s'y lit, et aucun autre chemin ne décodait la
//! piste — le décodage-pour-niveaux du passthrough saute le DSD, et la sortie
//! locale ne mesure rien elle-même. Aucune trame `playback.audio_levels` ne
//! naissait, avec ou sans déplacement. Seule l'avance gapless mesurait un DSD
//! (`queue.rs`, décodage du fichier).
//!
//! ## Les témoins
//!
//! 1. `le_dop_publie_les_niveaux_de_la_piste` : lecture explicite d'un DSF
//!    en DoP sur une zone locale. Des trames sortent, au-dessus du plancher,
//!    datées depuis 0.
//! 2. `apres_un_deplacement_les_niveaux_partent_de_la_position` : même
//!    lecture relancée à 2 s : les trames sont datées depuis 2 s.
//! 3. `le_frein_compte_l_avance_depuis_le_depart` : le décodage-pour-niveaux
//!    d'une lecture relancée à une position freine contre cette position, et
//!    non contre 0.

use std::sync::Arc;

use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::{EventBus, TuneEvent};
use crate::playback::{NowPlaying, PlaybackManager};

use super::{PlayRequest, PlaybackOrchestrator};

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

/// Une piste DSF de 3 s en DSD64 et une zone locale réglée en DoP.
fn piste_et_zone(orch: &PlaybackOrchestrator, chemin: &str) -> (i64, i64) {
    orch.db
        .execute(
            "INSERT INTO tracks (id, title, file_path, format, duration_ms, sample_rate, \
             bit_depth, channels) VALUES (1, 'Money for Nothing', ?, 'dsf', 3000, 2822400, 1, 2)",
            &[&chemin.to_string() as &dyn crate::db::backend::ToSqlValue],
        )
        .unwrap();
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let zone_id = zones
        .create("K13 R2R", Some("local"), Some("local:dac-6456"))
        .unwrap();
    zones.update_dsd_mode(zone_id, "dop").unwrap();
    (zone_id, 1)
}

fn requete(zone_id: i64, track_id: i64, seek_ms: Option<u64>) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some("local:dac-6456".into()),
        track_id: Some(track_id),
        source: Some("local".into()),
        seek_ms,
        ..Default::default()
    }
}

/// Les trames `playback.audio_levels` de la zone publiées pendant `duree`.
async fn trames(
    rx: &mut tokio::sync::broadcast::Receiver<TuneEvent>,
    zone_id: i64,
    duree: std::time::Duration,
) -> Vec<serde_json::Value> {
    let fin = tokio::time::Instant::now() + duree;
    let mut vues = Vec::new();
    loop {
        let reste = fin.saturating_duration_since(tokio::time::Instant::now());
        if reste.is_zero() {
            return vues;
        }
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Ok(ev))
                if ev.event_type == "playback.audio_levels"
                    && ev.data["zone_id"].as_i64() == Some(zone_id) =>
            {
                vues.push(ev.data.clone());
            }
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(e)) => panic!("bus : {e:?}"),
            Err(_) => return vues,
        }
    }
}

async fn lire_en_dop(seek_ms: Option<u64>) -> (Vec<serde_json::Value>, String) {
    let dossier = tempfile::tempdir().unwrap();
    let dsf = dossier.path().join("11 - Money for Nothing.dsf");
    crate::audio::decode::dop_deplacement_tests_6456::ecrire_dsf(
        &dsf,
        3 * crate::audio::decode::dop_deplacement_tests_6456::OCTETS_PAR_SECONDE,
    );
    let (orch, bus) = orchestrateur();
    let (zone_id, track_id) = piste_et_zone(&orch, dsf.to_str().unwrap());
    let mut rx = bus.subscribe();
    orch.playback.play(zone_id, NowPlaying::default()).await;
    if let Some(seek) = seek_ms {
        orch.playback.seek(zone_id, seek as i64).await;
    }

    let resolu = orch
        .resolve_local_track(&requete(zone_id, track_id, seek_ms))
        .await
        .unwrap_or_else(|e| panic!("le DSF en DoP doit se résoudre : {e}"));
    let vues = trames(&mut rx, zone_id, std::time::Duration::from_millis(900)).await;
    drop(dossier);
    (vues, resolu.mime_type)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_dop_publie_les_niveaux_de_la_piste() {
    let (vues, mime) = lire_en_dop(None).await;
    // Le décor tient : c'est bien la branche DoP qui a servi.
    assert_eq!(mime, "audio/wav");
    assert!(
        vues.len() >= 5,
        "#6456 — un DSF lu en DoP doit publier ses niveaux ; {} trame(s) vue(s) : \
         les VU-mètres vides du fil 2305",
        vues.len()
    );
    assert_eq!(vues[0]["position_ms"].as_i64(), Some(0));
    let crete = vues
        .iter()
        .filter_map(|t| t["peak_left_db"].as_f64())
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        crete > -90.0,
        "les niveaux décrivent la musique, pas le plancher : crête {crete} dBFS"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn apres_un_deplacement_les_niveaux_partent_de_la_position() {
    let (vues, _) = lire_en_dop(Some(2_000)).await;
    assert!(
        !vues.is_empty(),
        "#6456 — après un déplacement, le DoP doit publier ses niveaux"
    );
    let premiere = vues[0]["position_ms"].as_i64().unwrap();
    assert!(
        (2_000..2_100).contains(&premiere),
        "les niveaux d'une lecture relancée à 2 s sont datés depuis 2 s, pas depuis \
         {premiere} ms"
    );
}

/// Le frein d'un décodage relancé à 60 s : la zone est à 60 s, le décodeur a
/// fourni 31 s de fenêtres. Compté depuis le départ, il est à 91 s, au-delà
/// du plafond (60 + 30 s) : le puits doit retenir le décodeur. Compté depuis
/// 0, il croyait n'être qu'à 31 s et continuait — une avance de 61 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_frein_compte_l_avance_depuis_le_depart() {
    let zone_id = 986_456;
    let playback = Arc::new(PlaybackManager::new());
    playback.play(zone_id, NowPlaying::default()).await;
    playback.seek(zone_id, 60_000).await;
    let (levels_tx, mut levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let consomme = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let (sink_tx, relais_tx) = super::spawn_braked_levels_sink_depuis(
        playback.clone(),
        zone_id,
        levels_tx,
        consomme,
        60_000,
    );
    for _ in 0..31 {
        relais_tx
            .send(crate::audio::tap::RawWindow {
                pcm: Vec::new(),
                bit_depth: 24,
                channels: 2,
                sample_rate: 88_200,
                window: std::time::Duration::from_secs(1),
            })
            .unwrap();
    }
    // Le relais a tout compté (il remet chaque fenêtre au forwarder).
    for _ in 0..31 {
        tokio::time::timeout(std::time::Duration::from_secs(2), levels_rx.recv())
            .await
            .expect("fenêtre relayée")
            .expect("relais ouvert");
    }
    // Le puits (4 places) ne doit pas tout avaler : il freine.
    let mut acceptes = 0;
    for _ in 0..8 {
        match tokio::time::timeout(
            std::time::Duration::from_millis(300),
            sink_tx.send(vec![0u8; 16]),
        )
        .await
        {
            Ok(Ok(())) => acceptes += 1,
            _ => break,
        }
    }
    assert!(
        acceptes < 8,
        "le décodage-pour-niveaux relancé à 60 s, 31 s d'avance fournies, n'est pas \
         freiné : l'avance est comptée depuis 0 et non depuis le départ"
    );
    drop(relais_tx);
}
