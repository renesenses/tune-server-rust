//! #6990 — TIDAL AAC 320 → WAV vers un renderer (Wattson Emerson Digital) :
//! « crépitements / signal déformé », quand TIDAL FLAC → WAV joue juste.
//!
//! Le FLAC converti part en FICHIER (`Content-Length`, `Range` honoré à
//! l'octet). L'AAC part par le canal progressif (`produire_le_canal_aac`) :
//! WAV chunké, sans durée, donc sans longueur. Question posée ici : une
//! reprise `bytes=N-` d'un renderer qui a lu N octets puis rouvert sa
//! connexion reçoit-elle la suite EXACTE du flux, comme d'un fichier ?
use super::*;
use futures_util::StreamExt;
use std::sync::{Arc, atomic::Ordering::SeqCst};

/// Octet `i` du flux : un saut ou un décalage se voit.
fn octet(i: usize) -> u8 {
    ((i % 251) as u8) ^ ((i / 251) as u8)
}

fn flux(pcm: usize) -> Vec<u8> {
    let mut v = tune_core::audio::wav::build_wav_header(2, 44_100, 16).to_vec();
    v.extend((44..44 + pcm).map(octet));
    v
}

/// La session de `pretranscoder_en_flac` (canal AAC) : WAV 16 bits stéréo,
/// `duration_ms` comme demandé, en-tête émis par le producteur, blocs du
/// décodeur de 32 768 octets.
async fn session_aac(
    id: &str,
    duration_ms: Option<u64>,
    source: &[u8],
) -> (
    tune_core::http::streamer::SharedSessions,
    Arc<StreamSession>,
) {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        channels: 2,
        bit_depth: 16,
        duration_ms,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 256));
    session.wav_header_included.store(true, SeqCst);
    let tx = session.tx.lock().await.clone().expect("tx");
    session.close_sender().await;
    let mut blocs = vec![source[..44].to_vec()];
    blocs.extend(source[44..].chunks(32_768).map(<[u8]>::to_vec));
    tokio::spawn(async move {
        for b in blocs {
            if tx.send(b).await.is_err() {
                return;
            }
        }
    });
    let sessions: tune_core::http::streamer::SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session.clone())].into_iter().collect(),
    ));
    (sessions, session)
}

async fn ouvrir(
    sessions: &tune_core::http::streamer::SharedSessions,
    id: &str,
    debut: u64,
) -> (StatusCode, HeaderMap, axum::body::BodyDataStream) {
    let mut req = HeaderMap::new();
    req.insert("Range", format!("bytes={debut}-").parse().unwrap());
    req.insert("User-Agent", "Wattson".parse().unwrap());
    let r = handle_stream(
        axum::extract::Path(format!("{id}.wav")),
        axum::extract::State(sessions.clone()),
        req,
    )
    .await;
    let (parts, body) = r.into_parts();
    (parts.status, parts.headers, body.into_data_stream())
}

async fn lire(corps: &mut axum::body::BodyDataStream, n: Option<usize>, recu: &mut Vec<u8>) {
    while n.is_none_or(|n| recu.len() < n) {
        match tokio::time::timeout(std::time::Duration::from_secs(10), corps.next()).await {
            Ok(Some(Ok(b))) => recu.extend_from_slice(&b),
            Ok(_) => return,
            Err(_) => panic!("corps muet 10 s ({} octets reçus)", recu.len()),
        }
    }
}

/// Un renderer lit le début, ferme, puis reprend à l'octet où il s'était
/// arrêté. Ce qu'il reçoit doit être, à l'octet, la suite du flux.
async fn reprise(id: &str, duration_ms: Option<u64>) -> Result<(), String> {
    let source = flux(4_000_000);
    let (sessions, _s) = session_aac(id, duration_ms, &source).await;
    let (_, _, mut a) = ouvrir(&sessions, id, 0).await;
    let mut recu_a = Vec::new();
    lire(&mut a, Some(1_000_000), &mut recu_a).await;
    // Le renderer n'a consommé que 1 000 000 octets ; le reste de ce bloc
    // HTTP était encore dans ses tampons quand il a fermé.
    let n = 1_000_000usize;
    drop(a);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (statut, entetes, mut b) = ouvrir(&sessions, id, n as u64).await;
    let mut recu_b = Vec::new();
    lire(&mut b, Some(200_000), &mut recu_b).await;
    let attendu = &source[n..n + recu_b.len().min(source.len() - n)];
    if let Some(i) = recu_b.iter().zip(attendu).position(|(x, y)| x != y) {
        return Err(format!(
            "reprise `bytes={n}-` ({statut}, Content-Range {:?}) : l'octet {} du flux diffère \
             — la suite servie n'est pas celle du flux, un saut dans le signal à chaque reprise",
            entetes.get("Content-Range"),
            n + i
        ));
    }
    Ok(())
}

/// Témoin : la même session AVEC durée (longueur connue) sert la reprise à
/// l'octet depuis la retenue (#5426).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aac_6990_reprise_avec_longueur_exacte() {
    reprise("i6990-avec", Some(22_675)).await.unwrap();
}

/// Le canal AAC tel qu'il est créé : sans durée, chunké.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn aac_6990_reprise_chunkee_exacte() {
    reprise("i6990-chunk", None).await.unwrap();
}
