//! Fusion de #5078 (niveaux d'une source PCM, lot `cd-niveaux`) et de #5051
//! (mode « en direct » de `source_pcm`, lot `entree-audio`).
//!
//! Les deux lots attachaient chacun un relais de niveaux à la session d'une
//! source PCM : #5078 par `niveaux_de_la_source_pcm` (le forwarder de
//! `levels_forwarder_if_allowed`, ou l'attente de l'adoption en gapless),
//! #5051 par un `spawn_paced_levels_forwarder` à lui dans le mode direct.
//! La règle de la fusion : UN SEUL relais par session, le premier, dans les
//! deux modes.
//!
//! Le témoin COMPTE les trames `playback.audio_levels` publiées pendant une
//! fenêtre de temps. Un forwarder cadencé publie une fenêtre de
//! [`WINDOW_MS`] par tranche de [`WINDOW_MS`] : le compte d'un relais est
//! donc connu (≈ fenêtre / 40 ms). Deux relais sur la même zone doubleraient
//! le compte, et publieraient deux fois chaque `position_ms`. Les niveaux
//! restent justes : 1 kHz à −6 dBFS sur la voie gauche, droite muette.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Mutex;

use crate::audio::tap::WINDOW_MS;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::event_bus::{EventBus, TuneEvent};
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::{NowPlaying, PlaybackManager};
use crate::source_pcm::{Consommation, FluxDirect, FluxPcm, FormatPcm, FournisseurPcm};
use crate::streaming::registry::ServiceRegistry;

use super::{PlayRequest, PlaybackOrchestrator};

const FORMAT: FormatPcm = FormatPcm {
    frequence: 48_000,
    canaux: 2,
    bits: 16,
};

/// La fenêtre de comptage.
const FENETRE: std::time::Duration = std::time::Duration::from_millis(1_000);

/// L'échantillon `n` du signal témoin : 1 kHz à −6 dBFS à gauche, silence à
/// droite (une trame stéréo 16 bits, 4 octets).
fn trame(n: u64) -> [u8; 4] {
    let v = (0.5
        * (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / FORMAT.frequence as f64).sin()
        * i16::MAX as f64)
        .round() as i16;
    let g = v.to_le_bytes();
    [g[0], g[1], 0, 0]
}

/// Mode « longueur connue » : trois secondes de signal, durée annoncée.
struct FauxDisque;

impl FournisseurPcm for FauxDisque {
    fn ouvrir(&self, _: &str, _: u64) -> Result<FluxPcm, String> {
        let pcm: Vec<u8> = (0..3 * FORMAT.frequence as u64).flat_map(trame).collect();
        Ok(FluxPcm {
            format: FORMAT,
            octets: pcm.len() as u64,
            duree_ms: 3_000,
            lecteur: Box::new(std::io::Cursor::new(pcm)),
        })
    }
}

/// Mode « en direct » : un flux SANS FIN, rendu plus vite que le temps réel
/// (40 ms de signal, UNE fenêtre de [`WINDOW_MS`], toutes les 20 ms) pour que
/// le forwarder ne manque jamais de fenêtres, jusqu'à ce que le témoin
/// l'arrête.
struct FluxSansFin {
    n: u64,
    arret: Arc<AtomicBool>,
}

impl Read for FluxSansFin {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.arret.load(Ordering::SeqCst) {
            return Ok(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        // Un tronçon = une fenêtre pleine : le forwarder cadence sur la durée
        // AUDIO des fenêtres, un tronçon de 20 ms en ferait deux fois plus.
        let trames = (buf.len() / 4).min(FORMAT.frequence as usize * WINDOW_MS as usize / 1000);
        for i in 0..trames {
            buf[i * 4..i * 4 + 4].copy_from_slice(&trame(self.n));
            self.n += 1;
        }
        Ok(trames * 4)
    }
}

struct FausseEntree {
    arret: Arc<AtomicBool>,
}

impl FournisseurPcm for FausseEntree {
    fn ouvrir(&self, _: &str, _: u64) -> Result<FluxPcm, String> {
        Err("une entrée en direct ne s'ouvre pas en longueur connue".into())
    }
    fn en_direct(&self) -> bool {
        true
    }
    fn ouvrir_direct(&self, _: &str, _: Consommation) -> Result<FluxDirect, String> {
        Ok(FluxDirect {
            format: FORMAT,
            lecteur: Box::new(FluxSansFin {
                n: 0,
                arret: self.arret.clone(),
            }),
            etat: None,
        })
    }
}

fn orchestrateur(arret: Arc<AtomicBool>) -> (PlaybackOrchestrator, Arc<EventBus>) {
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
    orch.sources_pcm()
        .inscrire("fausse-entree", Arc::new(FausseEntree { arret }));
    (orch, bus)
}

fn demande(zone_id: i64, source: &str) -> PlayRequest {
    PlayRequest {
        zone_id,
        source: Some(source.into()),
        source_id: Some("témoin".into()),
        title: Some("Témoin".into()),
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

/// UN relais : le compte suit la cadence d'un forwarder (± marge), jamais le
/// double ; chaque `position_ms` n'est publiée qu'une fois ; crête −6 dBFS.
fn un_seul_relais(mode: &str, vues: &[serde_json::Value]) {
    let attendu = (FENETRE.as_millis() as u64 / WINDOW_MS) as usize;
    let n = vues.len();
    eprintln!("[{mode}] {n} trame(s) de niveaux en {FENETRE:?} ; un relais ≈ {attendu}");
    assert!(
        n * 10 >= attendu * 6,
        "[{mode}] {n} trame(s) en {FENETRE:?} : moins qu'UN relais (≈ {attendu})"
    );
    assert!(
        n * 10 <= attendu * 14,
        "[{mode}] {n} trame(s) en {FENETRE:?} pour ≈ {attendu} attendues : \
         plus d'un relais de niveaux publie pour cette session"
    );
    let mut positions: Vec<i64> = vues
        .iter()
        .map(|t| t["position_ms"].as_i64().unwrap())
        .collect();
    positions.sort_unstable();
    positions.dedup();
    assert_eq!(
        positions.len(),
        n,
        "[{mode}] des positions publiées deux fois : deux relais sur la même session"
    );
    for t in vues {
        assert_eq!(t["sample_rate"].as_u64(), Some(48_000), "{t}");
        let gauche = t["peak_left_db"].as_f64().unwrap();
        let droite = t["peak_right_db"].as_f64().unwrap();
        assert!(
            (gauche + 6.02).abs() < 0.2,
            "[{mode}] crête gauche {gauche} dBFS, attendu −6 dBFS"
        );
        assert!(
            droite <= -90.0,
            "[{mode}] voie droite muette, lue à {droite} dBFS"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_cd_un_seul_relais_de_niveaux() {
    let zone_id = 985_111;
    let (orch, bus) = orchestrateur(Arc::default());
    let mut rx = bus.subscribe();
    orch.playback.play(zone_id, NowPlaying::default()).await;

    let resolu = orch
        .resolve_stream(&demande(zone_id, "faux-cd"))
        .await
        .unwrap();
    assert_eq!(resolu.duration_ms, Some(3_000), "durée connue");

    let vues = trames(&mut rx, zone_id, FENETRE).await;
    un_seul_relais("cd", &vues);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mode_direct_un_seul_relais_de_niveaux() {
    let zone_id = 985_100;
    let arret = Arc::new(AtomicBool::new(false));
    let (orch, bus) = orchestrateur(arret.clone());
    let mut rx = bus.subscribe();
    orch.playback.play(zone_id, NowPlaying::default()).await;

    let resolu = orch
        .resolve_stream(&demande(zone_id, "fausse-entree"))
        .await
        .unwrap();
    assert_eq!(resolu.duration_ms, None, "un direct n'a pas de durée");
    // Le consommateur de la session : sans lui, le petit canal du direct
    // bloquerait la pompe au bout de quelques tronçons.
    let sid = resolu.stream_id.expect("session");
    let session = orch.streamer.sessions_state().lock().await[&sid].clone();
    let vidange = tokio::spawn(async move { while session.recv_chunk().await.is_some() {} });

    let vues = trames(&mut rx, zone_id, FENETRE).await;
    arret.store(true, Ordering::SeqCst);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), vidange).await;
    un_seul_relais("direct", &vues);
}
