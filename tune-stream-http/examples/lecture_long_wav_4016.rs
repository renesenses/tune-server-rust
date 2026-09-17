//! Mesure explicite du transport et du décodage WAV de #4016, sans matériel ni compte musical.
//! cargo run -p tune-stream-http --example lecture_long_wav_4016
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
        tokio::time::timeout(std::time::Duration::from_secs(600), measure(included))
            .await
            .expect("le transport de 6,36 Go doit finir en 600 s sur le banc");
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
    let (reader_tx, reader_rx) = tokio::sync::mpsc::channel(4);
    let reader = tokio::task::spawn_blocking(move || decode_http(reader_rx));
    let mut body = response.bytes_stream();
    let mut count = 0u64;
    let mut header = Vec::new();
    let mut tail = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.expect("corps HTTP complet");
        let header_missing = 44 - header.len();
        header.extend_from_slice(&chunk[..header_missing.min(chunk.len())]);
        reader_tx
            .send(chunk.clone())
            .await
            .expect("le lecteur WAV doit rester ouvert jusqu'à la vraie fin (#4016)");
        count += chunk.len() as u64;
        tail.extend_from_slice(&chunk);
        if tail.len() > MARKER.len() {
            tail.drain(..tail.len() - MARKER.len());
        }
    }
    drop(reader_tx);
    let decoded_frames = reader.await.expect("lecteur WAV indépendant");
    assert_eq!(
        decoded_frames,
        PCM_LEN / 6,
        "les 46 minutes doivent être décodées"
    );
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
    assert_eq!(wav_data, u32::MAX, "pas de fausse taille finie");
    println!(
        "{{\"producer_header\":{included},\"http_bytes\":{count},\"wav_data_bytes\":{wav_data},\"pcm_frames\":{},\"decoded_frames\":{},\"elapsed_ms\":{}}}",
        PCM_LEN / 6,
        decoded_frames,
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

// Bounded bridge: the independent reader consumes the actual HTTP response,
// including its header, with no seek, sparse-file substitution or PCM cache.
struct HttpSource {
    rx: std::sync::Mutex<tokio::sync::mpsc::Receiver<bytes::Bytes>>,
    block: bytes::Bytes,
}
impl std::io::Read for HttpSource {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        while self.block.is_empty() {
            match self.rx.get_mut().unwrap().blocking_recv() {
                Some(block) => self.block = block,
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.block.len());
        out[..n].copy_from_slice(&self.block.split_to(n));
        Ok(n)
    }
}
impl std::io::Seek for HttpSource {
    fn seek(&mut self, _: std::io::SeekFrom) -> std::io::Result<u64> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}
impl symphonia::core::io::MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        Some(PCM_LEN + 44)
    }
}
fn decode_http(rx: tokio::sync::mpsc::Receiver<bytes::Bytes>) -> u64 {
    use symphonia::core::{
        formats::{TrackType, probe::Hint},
        io::MediaSourceStream,
    };
    let source = MediaSourceStream::new(
        Box::new(HttpSource {
            rx: std::sync::Mutex::new(rx),
            block: bytes::Bytes::new(),
        }),
        Default::default(),
    );
    let mut reader = symphonia::default::get_probe()
        .probe(&Hint::new(), source, Default::default(), Default::default())
        .unwrap();
    let track = reader.default_track(TrackType::Audio).unwrap();
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(
            track.codec_params.as_ref().unwrap().audio().unwrap(),
            &Default::default(),
        )
        .unwrap();
    let mut pcm_bytes = 0u64;
    let mut frames = 0u64;
    let mut tail = Vec::new();
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(symphonia::core::errors::Error::IoError(e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => panic!("WAV illisible : {e}"),
        };
        pcm_bytes += packet.data.len() as u64;
        tail.extend_from_slice(&packet.data);
        if tail.len() > MARKER.len() {
            tail.drain(..tail.len() - MARKER.len());
        }
        let decoded = decoder.decode(&packet).expect("PCM décodable");
        frames += decoded.frames() as u64;
        if pcm_bytes == PCM_LEN {
            let mut samples = Vec::<i32>::new();
            decoded.copy_to_vec_interleaved(&mut samples);
            assert_eq!(
                &samples[samples.len() - 2..],
                &[0x30344e00, 0x21363100],
                "la dernière trame doit réellement être décodée"
            );
        }
    }
    assert_eq!(
        pcm_bytes, PCM_LEN,
        "le lecteur WAV ne doit pas s'arrêter à 2 ou 4 Gio (#4016)"
    );
    assert_eq!(tail, MARKER, "le conteneur doit livrer le dernier marqueur");
    frames
}
