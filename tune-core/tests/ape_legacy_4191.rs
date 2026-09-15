//! #4191: real 3.97 encoder fixtures, checked against their original PCM.
//! Provenance and regeneration: fixtures/ape/legacy3970/README.md.
//! No encoder or external decoder runs during tests.
use sha2::{Digest, Sha256};
use std::io::Cursor;
use tune_core::audio::decode::{decode_to_pcm, decode_to_pcm_streaming};

fn fixture(name: &str) -> String {
    format!(
        "{}/tests/fixtures/ape/legacy3970/{name}.ape",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn verify(name: &str) {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/ape/legacy3970/manifest.json")).unwrap();
    let c = manifest
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap();
    let encoded = std::fs::read(fixture(name)).unwrap();
    assert_eq!(sha256(&encoded), c["ape_sha256"]);
    let mut decoder = ape_decoder::ApeDecoder::new(Cursor::new(encoded)).unwrap();
    assert_eq!(decoder.info().version, 3970);
    assert_eq!(
        u64::from(decoder.info().compression_level),
        c["compression"].as_u64().unwrap()
    );
    assert_eq!(
        u64::from(decoder.info().total_frames),
        c["frames"].as_u64().unwrap()
    );
    assert_eq!(
        sha256(&decoder.decode_all().expect("3.97 entropy must decode")),
        c["pcm_sha256"]
    );
    // Exercise Tune's actual production entry point, not only the dependency.
    let decoded = decode_to_pcm(&fixture(name), None, None, 0.0, 0.0)
        .expect("Tune must decode APE 3.97, including compression 4000");
    assert_eq!(decoded.sample_rate, 44100);
    assert_eq!(u64::from(decoded.channels), c["channels"].as_u64().unwrap());
    assert_eq!(
        u64::from(decoded.bit_depth),
        c["bits"].as_u64().unwrap().max(16)
    );
    assert_eq!(
        decoded.samples_i32.len() as u64,
        c["blocks"].as_u64().unwrap() * c["channels"].as_u64().unwrap()
    );
    assert_eq!(sha256(&decoded.pcm_bytes()), c["tune_pcm_sha256"]);
}

macro_rules! cases {
    ($($name:ident),+ $(,)?) => {$(#[test] fn $name() { verify(stringify!($name)); })+};
}
cases!(
    stereo16_c4000,
    stereo16_c2000,
    mono16_c4000,
    stereo24_c4000,
    stereo8_c4000,
    pseudo16_c4000,
    silence16_c4000,
    multiframe16_c4000
);

fn periodic_pcm(start: usize, end: usize) -> Vec<u8> {
    (start..end)
        .flat_map(|i| {
            (0..2).flat_map(move |ch| {
                let sample = (((i * (ch + 1)) % 1001) as i16 - 500) * 16;
                sample.to_le_bytes()
            })
        })
        .collect()
}

#[test]
fn legacy_seek_crosses_real_frame_boundary_exactly() {
    let name = fixture("multiframe16_c4000");
    let mut decoder = ape_decoder::ApeDecoder::new(std::fs::File::open(&name).unwrap()).unwrap();
    let start = 294_900;
    let end = 294_930;
    assert_eq!(
        decoder.decode_range(start, end).unwrap(),
        periodic_pcm(start as usize, end as usize)
    );
    // An exact integer-second seek avoids any floating-point rounding ambiguity.
    let decoded = decode_to_pcm(&name, None, None, 6.0, 0.0).unwrap();
    assert_eq!(decoded.pcm_bytes(), periodic_pcm(6 * 44100, 299008));
}

#[tokio::test]
async fn legacy_streaming_emits_exact_pcm() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let consumer = tokio::spawn(async move {
        let mut pcm = Vec::new();
        while let Some(chunk) = rx.recv().await {
            pcm.extend(chunk);
        }
        pcm
    });
    let result = tokio::task::spawn_blocking(move || {
        decode_to_pcm_streaming(&fixture("multiframe16_c4000"), None, None, tx, 32768)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result, (16, 44100));
    assert_eq!(consumer.await.unwrap(), periodic_pcm(0, 299008));
}

#[test]
fn legacy_crc_corruption_is_still_rejected() {
    let mut bytes = std::fs::read(fixture("stereo16_c4000")).unwrap();
    // First frame starts at 36; its first word is the stored CRC.
    bytes[36] ^= 1;
    let mut decoder = ape_decoder::ApeDecoder::new(Cursor::new(&bytes)).unwrap();
    assert!(matches!(
        decoder.decode_all(),
        Err(ape_decoder::ApeError::InvalidChecksum)
    ));
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("corrupt.ape");
    std::fs::write(&path, bytes).unwrap();
    assert!(decode_to_pcm(path.to_str().unwrap(), None, None, 0.0, 0.0).is_err());
}

#[test]
fn modern_entropy_still_matches_existing_reference_wav() {
    let root = format!("{}/tests/fixtures/ape", env!("CARGO_MANIFEST_DIR"));
    let wav = std::fs::read(format!("{root}/sine_16s_c3000.wav")).unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    let mut offset = 12;
    let expected = loop {
        let len = u32::from_le_bytes(wav[offset + 4..offset + 8].try_into().unwrap()) as usize;
        if &wav[offset..offset + 4] == b"data" {
            break &wav[offset + 8..offset + 8 + len];
        }
        offset += 8 + len + len % 2;
    };
    for level in [3000, 4000] {
        let path = format!("{root}/sine_16s_c{level}.ape");
        let ape = std::fs::read(&path).unwrap();
        let mut decoder = ape_decoder::ApeDecoder::new(Cursor::new(ape)).unwrap();
        assert_eq!(decoder.info().version, 3990);
        assert_eq!(decoder.info().compression_level, level);
        assert_eq!(decoder.decode_all().unwrap(), expected);
        let decoded = decode_to_pcm(&path, None, None, 0.0, 0.0).unwrap();
        assert_eq!(decoded.pcm_bytes(), expected);
    }
}
