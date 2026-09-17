//! Mesure explicite du transport de #4016, sans matériel ni compte musical.
//! cargo run -p tune-stream-http --example mesure_long_wav_4016
//! Le PCM synthétique traverse une vraie connexion HTTP. Il n'est pas conservé
//! en mémoire ou sur disque. Cette mesure ne simule pas un DAC.
use futures_util::StreamExt;
use std::sync::{Arc, atomic::Ordering::SeqCst};
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

const PCM_LEN: u64 = 6_359_040_000; // 46 min, 384 kHz, 24 bits stéréo.
const MARKER: &[u8; 8] = b"FIN4016!";

#[tokio::main]
async fn main() {
    for included in [false, true] {
        tokio::time::timeout(std::time::Duration::from_secs(180), measure(included))
            .await
            .expect("le transport de 6,36 Go doit finir en 180 s sur le banc");
    }
}

async fn measure(included: bool) {
    let started = std::time::Instant::now();
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 384_000,
        bit_depth: 24,
        channels: 2,
        duration_ms: Some(46 * 60 * 1000),
        ..Default::default()
    };
    let session = Arc::new(StreamSession::new("long4016".into(), info, false, 4));
    session.wav_header_included.store(included, SeqCst);
    let tx = session.tx.lock().await.clone().expect("producteur");
    session.close_sender().await;
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [("long4016".to_string(), session)].into_iter().collect(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, tune_stream_http::router(sessions))
            .await
            .unwrap();
    });
    let producer = tokio::spawn(async move {
        if included {
            // Le décodeur progressif émet aujourd'hui cet en-tête sans durée.
            tx.send(tune_core::audio::wav::build_wav_header(2, 384_000, 24).to_vec())
                .await
                .unwrap();
        }
        let mut sent = 0;
        while sent < PCM_LEN {
            let size = (PCM_LEN - sent).min(64 * 1024) as usize;
            let mut block = vec![0x55; size];
            if sent + size as u64 == PCM_LEN {
                block[size - MARKER.len()..].copy_from_slice(MARKER);
            }
            tx.send(block)
                .await
                .expect("le lecteur reste jusqu'au dernier bloc");
            sent += size as u64;
        }
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(format!("http://{addr}/stream/long4016.wav"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let announced = response.content_length().expect("longueur connue");
    assert_eq!(announced, PCM_LEN + 44, "HTTP doit annoncer toute la piste");
    let mut body = response.bytes_stream();
    let mut count = 0u64;
    let mut header = Vec::new();
    let mut tail = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.expect("corps HTTP complet");
        let header_missing = 44 - header.len();
        header.extend_from_slice(&chunk[..header_missing.min(chunk.len())]);
        count += chunk.len() as u64;
        tail.extend_from_slice(&chunk);
        if tail.len() > MARKER.len() {
            tail.drain(..tail.len() - MARKER.len());
        }
    }
    producer.await.unwrap();
    server.abort();
    let _ = server.await;
    assert_eq!(
        count,
        PCM_LEN + 44,
        "le transport ne doit pas couper à 2 ou 4 Gio"
    );
    assert_eq!(tail, MARKER, "la fin de la piste doit réellement arriver");
    assert_eq!(&header[..4], b"RIFF");
    let wav_data = u32::from_le_bytes(header[40..44].try_into().unwrap());
    println!(
        "{{\"producer_header\":{included},\"http_bytes\":{count},\"wav_data_bytes\":{wav_data},\"pcm_frames\":{},\"wav_frames\":{},\"elapsed_ms\":{}}}",
        PCM_LEN / 6,
        u64::from(wav_data) / 6,
        started.elapsed().as_millis()
    );
    if let Some(dir) = std::env::var_os("TUNE_MEASURE_DIR") {
        std::fs::write(
            std::path::Path::new(&dir).join(format!("header-{included}.wav")),
            header,
        )
        .unwrap();
    }
}
