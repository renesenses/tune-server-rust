//! Fil 9316 (suite) — un podcast relayé tel quel vers un renderer réseau
//! affiche la fréquence et la profondeur que la sonde des VU-mètres MESURE,
//! plutôt que « inconnu » (décision du 22/11, rc14).
//!
//! Le banc : la VRAIE session du mandataire (`relayer_un_flux_distant`, sans
//! format connu), la VRAIE sonde (`decode_http_stream_for_levels`) sur un WAV
//! 48 kHz / 24 bits servi en HTTP, et l'observateur que `play_episode`
//! branche (`observateur_du_format_relaye`).
//!
//! Contre-épreuve : vider le corps de `publier_le_format_mesure` (ou ne plus
//! appeler l'observateur dans la sonde) fait tomber
//! `la_sonde_publie_le_format_du_podcast_relaye_9316`.

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;

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

/// Sert UNE fois un WAV `sr` Hz / `bits` bits stéréo d'une seconde.
fn servir_un_wav(sr: u32, bits: u16) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let bloc = 2 * (bits / 8);
            let data = vec![0u8; sr as usize * bloc as usize];
            let mut wav = Vec::with_capacity(44 + data.len());
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&sr.to_le_bytes());
            wav.extend_from_slice(&(sr * u32::from(bloc)).to_le_bytes());
            wav.extend_from_slice(&bloc.to_le_bytes());
            wav.extend_from_slice(&bits.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
            wav.extend_from_slice(&data);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                wav.len()
            );
            let _ = s.write_all(&wav);
        }
    });
    format!("http://{addr}/episode-9316.wav")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_sonde_publie_le_format_du_podcast_relaye_9316() {
    let orch = orchestrateur();
    let url = servir_un_wav(48_000, 24);
    let (_, sid) = orch
        .relayer_un_flux_distant(&url, "audio/wav", Some(1_000), None, None)
        .await;

    // Avant la sonde : inconnu, pour le chemin du signal comme pour le fil.
    let avant = orch.streamer.fil_du_chemin_du_signal(&sid).await.unwrap();
    assert_eq!((avant.sample_rate, avant.bit_depth), (0, 0));

    let au_format = orch
        .observateur_du_format_relaye(&sid)
        .await
        .expect("la session relayée existe");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let reported = Arc::new(std::sync::atomic::AtomicI64::new(600_000));
    tokio::task::spawn_blocking(move || {
        super::decode_http_stream_for_levels(url, "wav".into(), tx, reported, Some(au_format))
            .expect("la sonde décode le WAV servi")
    })
    .await
    .expect("tâche de la sonde");

    let chemin = orch.streamer.fil_du_chemin_du_signal(&sid).await.unwrap();
    assert_eq!(
        (chemin.sample_rate, chemin.bit_depth),
        (48_000, 24),
        "fil 9316 : le chemin du signal voit ce que la sonde a mesuré"
    );
    // L'audio servi et ce que les autres lecteurs du fil annoncent ne
    // changent pas.
    let fil = orch.streamer.stream_output_wire(&sid).await.unwrap();
    assert_eq!((fil.sample_rate, fil.bit_depth), (0, 0));
}

/// Une valeur que l'appelant CONNAÎT n'est jamais remplacée par la mesure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_mesure_ne_remplace_pas_un_format_connu_9316() {
    let orch = orchestrateur();
    let (_, sid) = orch
        .relayer_un_flux_distant(
            "http://127.0.0.1:9/e.m4a",
            "audio/aac",
            None,
            Some(44_100),
            None,
        )
        .await;
    let au_format = orch.observateur_du_format_relaye(&sid).await.unwrap();
    au_format(48_000, Some(24));
    let chemin = orch.streamer.fil_du_chemin_du_signal(&sid).await.unwrap();
    assert_eq!((chemin.sample_rate, chemin.bit_depth), (44_100, 24));
}
