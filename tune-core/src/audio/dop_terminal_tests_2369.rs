use super::{decode_dsd_to_dop_streaming, decode_dsd_to_dop_streaming_with_timeout};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};

const DSF: &[u8] = include_bytes!("../../tests/fixtures/dsd/ref_dsd64_stereo.dsf");
const DFF: &[u8] = include_bytes!("../../tests/fixtures/dsd/ref_dsd64_stereo.dff");

// Existing fixtures contain 9000 DSD bytes/channel, hence 4500 stereo DoP frames.
const PCM_BYTES: usize = 4500 * 6;
const FORMATS: [(&str, &[u8]); 2] = [("dsf", DSF), ("dff", DFF)];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

fn fixture(ext: &str, bytes: &[u8]) -> (tempfile::TempDir, String) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(format!("source.{ext}"));
    std::fs::write(&path, bytes).unwrap();
    (temp, path.to_str().unwrap().to_owned())
}

#[test]
fn short_first_and_final_block_is_delivered_and_announced_for_dsf_and_dff() {
    let rt = runtime();
    let _guard = rt.enter();
    for (ext, bytes) in FORMATS {
        let (_temp, path) = fixture(ext, bytes);
        let (tx, mut rx) = mpsc::channel(64);
        let ready = Arc::new(Notify::new());
        let mut first = false;
        let format = decode_dsd_to_dop_streaming(
            &path,
            ext,
            tx,
            65536,
            &mut first,
            &Some(ready.clone()),
            rt.handle(),
        )
        .unwrap();
        assert_eq!(format, (24, 176400));
        assert!(
            first,
            "{ext}: the short final block is also the first payload and must be announced"
        );
        assert!(
            rt.block_on(tokio::time::timeout(
                Duration::from_millis(50),
                ready.notified()
            ))
            .is_ok(),
            "{ext}: successful short payload must notify readiness"
        );
        let payload = rx.try_recv().expect("short payload must reach receiver");
        assert_eq!(payload.len(), PCM_BYTES);
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
        for (index, frame) in payload.chunks_exact(6).enumerate() {
            let marker = if index % 2 == 0 { 0x05 } else { 0xfa };
            assert_eq!(
                [frame[2], frame[5]],
                [marker; 2],
                "DoP markers must remain intact"
            );
        }
    }
}

#[test]
fn complete_chunks_and_tail_match_single_block_without_duplicate_notification() {
    let rt = runtime();
    let _guard = rt.enter();
    for (ext, bytes) in FORMATS {
        let (_temp, path) = fixture(ext, bytes);
        let mut outputs = Vec::new();
        for chunk_size in [65536, 4096] {
            let (tx, mut rx) = mpsc::channel(64);
            let mut first = false;
            decode_dsd_to_dop_streaming(&path, ext, tx, chunk_size, &mut first, &None, rt.handle())
                .unwrap();
            let mut payload = Vec::new();
            while let Ok(chunk) = rx.try_recv() {
                assert_eq!(
                    chunk.len() % 6,
                    0,
                    "all delivered chunks must contain complete frames"
                );
                payload.extend(chunk);
            }
            assert_eq!(payload.len(), PCM_BYTES);
            outputs.push(payload);
        }
        assert_eq!(
            outputs[0], outputs[1],
            "{ext}: transport batching must not change payload"
        );
        // Caller may already have announced readiness (e.g. a header).
        let ready = Arc::new(Notify::new());
        let (tx, _rx) = mpsc::channel(64);
        let mut first = true;
        decode_dsd_to_dop_streaming(
            &path,
            ext,
            tx,
            65536,
            &mut first,
            &Some(ready.clone()),
            rt.handle(),
        )
        .unwrap();
        assert!(
            rt.block_on(tokio::time::timeout(
                Duration::from_millis(20),
                ready.notified()
            ))
            .is_err(),
            "already announced streams must not add another readiness notification"
        );
    }
}

#[test]
fn abandoned_consumer_is_not_success_for_full_or_short_block() {
    let rt = runtime();
    let _guard = rt.enter();
    for (ext, bytes) in FORMATS {
        let (_temp, path) = fixture(ext, bytes);
        for chunk_size in [4096, 65536] {
            let (tx, rx) = mpsc::channel(1);
            drop(rx); // also models an explicit Stop which closes the session.
            let mut first = false;
            let ready = Arc::new(Notify::new());
            let result = decode_dsd_to_dop_streaming(
                &path,
                ext,
                tx,
                chunk_size,
                &mut first,
                &Some(ready.clone()),
                rt.handle(),
            );
            let error = result
                .expect_err("an abandoned DoP consumer is an interrupted stream, not completion");
            assert!(error.starts_with("dop_stream_consumer_closed:"), "{error}");
            assert!(!first, "failed send must not mark payload delivered");
            assert!(
                rt.block_on(tokio::time::timeout(
                    Duration::from_millis(20),
                    ready.notified()
                ))
                .is_err(),
                "abandoned consumer must not receive a readiness notification"
            );
        }
    }
}

#[test]
fn stalled_consumer_reports_timeout_for_full_or_short_block() {
    let rt = runtime();
    let _guard = rt.enter();
    for (ext, bytes) in FORMATS {
        let (_temp, path) = fixture(ext, bytes);
        for chunk_size in [4096, 65536] {
            let (tx, mut rx) = mpsc::channel(1);
            tx.try_send(vec![0xde, 0xad]).unwrap(); // no capacity, receiver remains alive.
            let mut first = false;
            let result = decode_dsd_to_dop_streaming_with_timeout(
                &path,
                ext,
                tx,
                chunk_size,
                &mut first,
                &None,
                rt.handle(),
                Duration::from_millis(20),
            );
            let error = result.expect_err("timeout must not be reported as a completed DoP stream");
            assert!(error.starts_with("dop_stream_send_timeout:"), "{error}");
            assert!(!first, "timed-out payload was not delivered");
            assert_eq!(rx.try_recv().unwrap(), [0xde, 0xad]);
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ));
        }
    }
}
