//! #5078 (GgB, fil 1945, v0.9.165) — « les crêtemètre, vumètre et spectro
//! sont muets » pendant la lecture d'un CD.
//!
//! ## La cause, lue dans le code
//!
//! `resolve_source_pcm` ouvrait sa session WAV et y pompait le PCM du
//! greffon sans jamais attacher de forwarder de niveaux, alors que tous les
//! chemins qui tiennent le PCM en main en attachent un (transcodes locaux et
//! streaming, pré-transcodes, radio décodée). Aucune trame
//! `playback.audio_levels` ne naissait donc pour une source `cd`.
//!
//! ## Les témoins
//!
//! Un faux `FournisseurPcm` rend un signal CONNU : 1 kHz à −6 dBFS sur la
//! seule voie gauche, voie droite muette, 48 kHz (et non 44,1 : la
//! fréquence publiée doit venir du flux, pas d'une constante « CD »).
//!
//! 1. `la_source_pcm_publie_ses_niveaux` : lecture explicite. Des trames
//!    sortent, avec la crête gauche à −6 dBFS, la droite au plancher et la
//!    fréquence du flux. Sur la base : aucune trame.
//! 2. `les_niveaux_d_une_source_pcm_pre_armee_attendent_l_avance` : le
//!    pré-armement gapless n'émet RIEN (ses fenêtres seraient datées de la
//!    piste qui joue encore), puis l'avance qui adopte le flux les publie,
//!    depuis la position 0 de la nouvelle piste.

use std::io::Cursor;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::event_bus::{EventBus, TuneEvent};
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};
use crate::streaming::registry::ServiceRegistry;

use super::{PlayRequest, PlaybackOrchestrator};

const FORMAT: FormatPcm = FormatPcm {
    frequence: 48_000,
    canaux: 2,
    bits: 16,
};

/// Deux secondes : 1 kHz à −6 dBFS à gauche, silence à droite.
fn sinus_gauche() -> Vec<u8> {
    let mut pcm = Vec::new();
    for n in 0..2 * FORMAT.frequence {
        let v = (0.5
            * (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / FORMAT.frequence as f64).sin()
            * i16::MAX as f64)
            .round() as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&0i16.to_le_bytes());
    }
    pcm
}

struct FauxDisque;

impl FournisseurPcm for FauxDisque {
    fn ouvrir(&self, _source_id: &str, _depuis_ms: u64) -> Result<FluxPcm, String> {
        let pcm = sinus_gauche();
        Ok(FluxPcm {
            format: FORMAT,
            octets: pcm.len() as u64,
            duree_ms: 2_000,
            lecteur: Box::new(Cursor::new(pcm)),
        })
    }
}

fn orchestrateur() -> (PlaybackOrchestrator, Arc<EventBus>) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let mut orch = PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let bus = Arc::new(EventBus::new());
    orch.event_bus = Some(bus.clone());
    orch.sources_pcm().inscrire("faux-cd", Arc::new(FauxDisque));
    (orch, bus)
}

fn demande(zone_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        source: Some("faux-cd".into()),
        source_id: Some("disque/1".into()),
        title: Some("Piste 1".into()),
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
            Ok(Ok(_)) => {}
            Ok(Err(e)) => panic!("bus : {e:?}"),
            Err(_) => return vues,
        }
    }
}

fn verifier_le_signal(vues: &[serde_json::Value]) {
    for t in vues {
        assert_eq!(t["sample_rate"].as_u64(), Some(48_000), "{t}");
        let gauche = t["peak_left_db"].as_f64().unwrap();
        let droite = t["peak_right_db"].as_f64().unwrap();
        assert!(
            (gauche + 6.02).abs() < 0.2,
            "crête gauche {gauche} dBFS, attendu −6 dBFS"
        );
        assert!(droite <= -90.0, "voie droite muette, lue à {droite} dBFS");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_source_pcm_publie_ses_niveaux() {
    let zone_id = 985_078;
    let (orch, bus) = orchestrateur();
    let mut rx = bus.subscribe();
    orch.playback.play(zone_id, NowPlaying::default()).await;

    let resolu = orch.resolve_stream(&demande(zone_id)).await.unwrap();
    assert_eq!(resolu.sample_rate, Some(48_000));

    let vues = trames(&mut rx, zone_id, std::time::Duration::from_millis(800)).await;
    assert!(
        vues.len() >= 5,
        "une source PCM qui joue doit publier ses niveaux ; {} trame(s) vue(s) \
         — les instruments muets du fil 1945",
        vues.len()
    );
    verifier_le_signal(&vues);
    assert_eq!(vues[0]["position_ms"].as_i64(), Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn les_niveaux_d_une_source_pcm_pre_armee_attendent_l_avance() {
    let zone_id = 985_079;
    let (orch, bus) = orchestrateur();
    let mut rx = bus.subscribe();
    orch.playback.play(zone_id, NowPlaying::default()).await;

    // Le pré-armement gapless, pendant que la piste précédente joue.
    let resolu = {
        let _prewarm = orch.begin_levels_prewarm(zone_id);
        orch.resolve_stream(&demande(zone_id)).await.unwrap()
    };
    let flux = resolu.stream_id.expect("session");
    let vues = trames(&mut rx, zone_id, std::time::Duration::from_millis(500)).await;
    assert!(
        vues.is_empty(),
        "le pré-armement ne publie rien avant l'avance ; vu {} trame(s)",
        vues.len()
    );

    // L'avance gapless adopte ce flux (`advance_queue_metadata`).
    orch.playback.bump_levels_gen(zone_id);
    let play_seq = orch.playback.current_play_seq(zone_id).await;
    super::source_pcm::adopter_les_niveaux_pre_armes(&flux, play_seq);

    let vues = trames(&mut rx, zone_id, std::time::Duration::from_millis(800)).await;
    assert!(
        vues.len() >= 5,
        "adoptée, la piste pré-armée publie ses niveaux ; vu {} trame(s)",
        vues.len()
    );
    verifier_le_signal(&vues);
    assert_eq!(
        vues[0]["position_ms"].as_i64(),
        Some(0),
        "les niveaux partent du début de la piste adoptée"
    );
}
