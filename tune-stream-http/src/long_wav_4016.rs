//! Software regression for #4016. Tiny HTTP bodies exercise header/reconnect
//! handling; a seekable virtual PCM source checks the independent WAV reader
//! beyond 2/4 GiB without allocating or transferring gigabytes in CI.
use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, atomic::Ordering::SeqCst};
use symphonia::core::{
    formats::{SeekMode, SeekTo, TrackType, probe::Hint},
    io::{MediaSource, MediaSourceStream},
    units::Timestamp,
};

const PCM_LEN: u64 = 6_359_040_000;
const LAST_FRAME: [u8; 6] = [1, 2, 3, 4, 5, 6];

async fn session(included: bool, duration: u64) -> Arc<StreamSession> {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 384_000,
        bit_depth: 24,
        channels: 2,
        duration_ms: Some(duration),
        ..Default::default()
    };
    let s = Arc::new(StreamSession::new("long4016".into(), info, false, 4));
    s.wav_header_included.store(included, SeqCst);
    let tx = s.tx.lock().await.clone().unwrap();
    if included {
        // Match the real progressive decoder: its duration is unknown.
        let mut block = tune_core::audio::wav::build_wav_header(2, 384_000, 24).to_vec();
        block.extend_from_slice(&LAST_FRAME); // PCM sharing the header chunk.
        tx.send(block).await.unwrap();
    } else {
        tx.send(LAST_FRAME.to_vec()).await.unwrap();
    }
    drop(tx);
    s.close_sender().await;
    s
}

async fn response(s: Arc<StreamSession>, range: Option<u64>) -> axum::response::Response {
    let sessions = Arc::new(tokio::sync::Mutex::new(
        [("long4016".to_string(), s)].into_iter().collect(),
    ));
    let mut headers = HeaderMap::new();
    if let Some(start) = range {
        headers.insert("Range", format!("bytes={start}-").parse().unwrap());
    }
    handle_stream(Path("long4016.wav".into()), State(sessions), headers).await
}

async fn body(r: axum::response::Response) -> Vec<u8> {
    axum::body::to_bytes(r.into_body(), 1024)
        .await
        .unwrap()
        .to_vec()
}

struct VirtualPcm {
    header: Vec<u8>,
    pos: u64,
}
impl Read for VirtualPcm {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let n = out
            .len()
            .min((PCM_LEN + 44).saturating_sub(self.pos) as usize);
        for (i, b) in out[..n].iter_mut().enumerate() {
            let p = self.pos + i as u64;
            *b = if p < 44 {
                self.header[p as usize]
            } else if p >= PCM_LEN + 44 - 6 {
                LAST_FRAME[(p - (PCM_LEN + 44 - 6)) as usize]
            } else {
                0x55
            };
        }
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for VirtualPcm {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let p = match from {
            SeekFrom::Start(p) => i128::from(p),
            SeekFrom::Current(n) => i128::from(self.pos) + i128::from(n),
            SeekFrom::End(n) => i128::from(PCM_LEN + 44) + i128::from(n),
        };
        self.pos = u64::try_from(p).map_err(|_| std::io::ErrorKind::InvalidInput)?;
        Ok(self.pos)
    }
}
impl MediaSource for VirtualPcm {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(PCM_LEN + 44)
    }
}

fn read_beyond_limits(header: Vec<u8>) {
    let source =
        MediaSourceStream::new(Box::new(VirtualPcm { header, pos: 0 }), Default::default());
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
    for frame in [
        (1u64 << 31) / 6 + 4096,
        (1u64 << 32) / 6 + 4096,
        PCM_LEN / 6 - 1,
    ] {
        reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: Timestamp::from(frame as i64),
                    track_id: 0,
                },
            )
            .expect("le WAV ne doit pas annoncer une fin fictive avant 46 minutes (#4016)");
        let packet = reader.next_packet().unwrap().expect("PCM après la limite");
        let decoded = decoder
            .decode(&packet)
            .expect("PCM décodable après la limite");
        assert!(decoded.frames() > 0);
        if frame == PCM_LEN / 6 - 1 {
            assert!(
                packet.data.ends_with(&LAST_FRAME),
                "dernière trame conservée"
            );
            let mut samples = Vec::<i32>::new();
            decoded.copy_to_vec_interleaved(&mut samples);
            assert_eq!(&samples[samples.len() - 2..], &[0x03020100, 0x06050400]);
            assert!(
                matches!(
                    reader.next_packet(),
                    Err(symphonia::core::errors::Error::IoError(e))
                        if e.kind() == std::io::ErrorKind::UnexpectedEof
                ),
                "fin réelle, sans octets ajoutés"
            );
        } else {
            assert!(packet.data.iter().all(|b| *b == 0x55));
        }
    }
}

#[tokio::test]
async fn long_wav_4016_http_and_producer_headers_allow_the_last_frame() {
    for included in [false, true] {
        let s = session(included, 2_760_000).await;
        let r = response(s, None).await;
        assert_eq!(r.headers()["content-length"], (PCM_LEN + 44).to_string());
        let bytes = body(r).await;
        assert_eq!(&bytes[44..], LAST_FRAME, "un seul en-tête, PCM intact");
        read_beyond_limits(bytes[..44].to_vec());
    }
}

#[tokio::test]
async fn long_wav_4016_reconnect_replays_the_corrected_header() {
    let s = session(true, 2_760_000).await;
    let first = body(response(s.clone(), None).await).await;
    let replay = body(response(s.clone(), Some(0)).await).await;
    assert_eq!(replay, first[..44], "réserve identique à l'en-tête envoyé");
    read_beyond_limits(replay);
    assert_eq!(
        s.octets_du_canal.load(SeqCst),
        50,
        "aucun ajout dans le canal"
    );
}

#[tokio::test]
async fn long_wav_4016_ranges_keep_u64_lengths_and_do_not_reinsert_header() {
    for start in [44, (1u64 << 31), (1u64 << 32) + 4] {
        // All offsets keep the six-byte frame phase relative to byte 44.
        assert_eq!((start - 44) % 6, 0);
        let s = session(true, 2_760_000).await;
        let r = response(s.clone(), Some(start)).await;
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            r.headers()["content-length"],
            (PCM_LEN + 44 - start).to_string()
        );
        assert_eq!(
            r.headers()["content-range"],
            format!("bytes {start}-{}/{}", PCM_LEN + 43, PCM_LEN + 44)
        );
        assert_eq!(body(r).await, LAST_FRAME);
        read_beyond_limits(s.wav_header_stash.get().unwrap().clone());
    }
}

#[tokio::test]
async fn long_wav_4016_short_headers_keep_their_existing_contract() {
    for included in [false, true] {
        let bytes = body(response(session(included, 1_000).await, None).await).await;
        let expected = if included {
            tune_core::audio::wav::build_wav_header(2, 384_000, 24)
        } else {
            build_wav_header(2, 384_000, 24, Some(1_000))
        };
        assert_eq!(&bytes[..44], expected);
        assert_eq!(&bytes[44..], LAST_FRAME);
    }
}

#[test]
fn long_wav_4016_switches_only_above_the_signed_ceiling() {
    // 1 kHz mono 8-bit makes one millisecond exactly one byte.
    for size in [0, 1, i32::MAX as u64 - 36, i32::MAX as u64 - 35, u64::MAX] {
        let h = build_wav_header(1, 1000, 8, Some(size));
        let data = u32::from_le_bytes(h[40..44].try_into().unwrap());
        if size <= i32::MAX as u64 - 36 {
            assert_eq!(u64::from(data), size);
        } else {
            assert_eq!(data, u32::MAX, "aucune fausse fin au plafond signé");
            assert_eq!(&h[4..8], &u32::MAX.to_le_bytes());
        }
    }
}
