use super::*;
use crate::scanner::walker::{EcrituresDuLot, ScanStats, scan_files_batched, scan_files_parallel};
use lofty::probe::Probe;
use symphonia::core::{
    formats::{FormatOptions, probe::Hint},
    io::MediaSourceStream,
    meta::MetadataOptions,
};

fn scratch() -> crate::test_scratch::ScratchDir {
    crate::test_scratch::scratch_dir_in(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target"),
        "ogg-4412",
    )
}

// Xiph Ogg FLAC mapping v1: https://xiph.org/flac/ogg_mapping.html .
// Rewrap real FLAC packets from the existing repository fixture, with empty tags.
// No external encoder, network download or fabricated compressed audio.
fn ogg_flac(vendor_bytes: usize) -> Vec<u8> {
    let native = include_bytes!("../../tests/fixtures/flac/ref_16_44100_mono.flac");
    let mut identification = b"\x7fFLAC\x01\x00\x00\x01fLaC".to_vec();
    identification.extend_from_slice(&native[4..42]);
    let mut comment = Vec::new();
    comment.extend_from_slice(&(vendor_bytes as u32).to_le_bytes());
    comment.resize(4 + vendor_bytes, b'x');
    comment.extend_from_slice(&0u32.to_le_bytes());
    let mut metadata = vec![0x84];
    metadata.extend_from_slice(&(comment.len() as u32).to_be_bytes()[1..]);
    metadata.extend(comment);
    let mut reader = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            MediaSourceStream::new(
                Box::new(std::io::Cursor::new(native.to_vec())),
                Default::default(),
            ),
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let mut packets = vec![(identification, 0u64), (metadata, 0u64)];
    let mut granule = 0u64;
    while let Some(packet) = reader.next_packet().unwrap() {
        granule += packet.dur.get();
        packets.push((packet.data.into_vec(), granule));
    }
    assert!(packets.len() > 2, "real encoded FLAC frames are required");
    let mut output = Vec::new();
    let mut sequence = 0u32;
    let last = packets.len() - 1;
    for (index, (packet, granule)) in packets.into_iter().enumerate() {
        let mut offset = 0;
        loop {
            let count = (packet.len() - offset).min(255 * 254);
            let final_page = offset + count == packet.len();
            let mut lacing = vec![255; count / 255];
            if final_page || count % 255 != 0 {
                lacing.push((count % 255) as u8);
            }
            let flags = if offset > 0 { 1 } else { 0 }
                | if index == 0 { 2 } else { 0 }
                | if index == last && final_page { 4 } else { 0 };
            let mut page = b"OggS\x00".to_vec();
            page.push(flags);
            page.extend_from_slice(&if final_page { granule } else { u64::MAX }.to_le_bytes());
            page.extend_from_slice(&4412u32.to_le_bytes());
            page.extend_from_slice(&sequence.to_le_bytes());
            page.extend_from_slice(&0u32.to_le_bytes());
            page.push(lacing.len() as u8);
            page.extend(lacing);
            page.extend_from_slice(&packet[offset..offset + count]);
            let mut crc = 0u32;
            for byte in &page {
                crc ^= (*byte as u32) << 24;
                for _ in 0..8 {
                    crc = if crc & 0x8000_0000 != 0 {
                        (crc << 1) ^ 0x04c1_1db7
                    } else {
                        crc << 1
                    };
                }
            }
            page[22..26].copy_from_slice(&crc.to_le_bytes());
            output.extend(page);
            sequence += 1;
            offset += count;
            if final_page {
                break;
            }
        }
    }
    output
}

#[test]
fn web_dump_and_magic_only_are_not_filename_tracks() {
    let dir = scratch();
    for ext in ["ogg", "oga", "opus", "OGG"] {
        let path = dir.join(format!("web.{ext}"));
        std::fs::write(&path, b"<!doctype html><html>browser cache dump</html>").unwrap();
        assert!(
            try_read_metadata(&path).is_err(),
            "web dump accepted for {ext}"
        );
    }
    let path = dir.join("magic.ogg");
    std::fs::write(&path, b"OggS this is not an Ogg page or an audio stream").unwrap();
    assert!(
        try_read_metadata(&path).is_err(),
        "magic bytes alone are insufficient"
    );
}

#[test]
fn real_vorbis_opus_and_non_lofty_ogg_flac_remain_readable() {
    let dir = scratch();
    for fixture in ["test_vorbis.ogg", "test.opus"] {
        let path = dir.join(fixture);
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
            &path,
        )
        .unwrap();
        assert_eq!(ogg_fallback_has_audio(&path), Some(true), "{fixture}");
        assert!(try_read_metadata(&path).is_ok(), "{fixture}");
    }
    let path = dir.join("Tagless.oga");
    std::fs::write(&path, ogg_flac(0)).unwrap();
    assert!(
        Probe::open(&path)
            .unwrap()
            .guess_file_type()
            .unwrap()
            .read()
            .is_err(),
        "exercise the failed-Lofty fallback"
    );
    assert_eq!(ogg_fallback_has_audio(&path), Some(true));
    let mut reader = symphonia::default::get_probe()
        .probe(
            &Hint::new(),
            MediaSourceStream::new(
                Box::new(std::fs::File::open(&path).unwrap()),
                Default::default(),
            ),
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .unwrap();
    let track = reader
        .default_track(symphonia::core::formats::TrackType::Audio)
        .unwrap();
    let Some(symphonia::core::codecs::CodecParameters::Audio(params)) = &track.codec_params else {
        panic!("audio parameters");
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &Default::default())
        .unwrap();
    let mut frames = 0;
    while let Some(packet) = reader.next_packet().unwrap() {
        frames += decoder.decode(&packet).unwrap().frames();
    }
    assert!(frames > 0, "Ogg-FLAC fixture really decodes to audio");
    assert_eq!(
        try_read_metadata(&path).unwrap().title.as_deref(),
        Some("Tagless")
    );
}

#[test]
fn probe_budget_is_inconclusive_for_large_valid_headers() {
    let dir = scratch();
    let path = dir.join("Large header.ogg");
    std::fs::write(&path, ogg_flac(OGG_FALLBACK_PROBE_BYTES as usize + 100)).unwrap();
    assert_eq!(ogg_fallback_has_audio(&path), None);
    assert!(
        try_read_metadata(&path).is_ok(),
        "budget exhaustion must not reject valid audio"
    );
}

fn assert_scan(stats: &ScanStats) {
    assert_eq!(stats.total_files, 2);
    assert_eq!(stats.metadata_ok, 1);
    assert_eq!(stats.metadata_failed, 1);
    assert_eq!(stats.metadata_timeout, 0);
    assert_eq!(stats.failed_paths.len(), 1);
    assert!(
        stats.failed_paths[0].contains("web.ogg (Ogg audio probe failed"),
        "{:?}",
        stats.failed_paths
    );
}

#[test]
fn direct_and_batched_scans_report_dump_failure_and_keep_real_audio() {
    let dir = scratch();
    let invalid = dir.join("web.ogg");
    let valid = dir.join("Audio.oga");
    let mut dump = b"<!doctype html><html>saved web page</html>".to_vec();
    dump.resize(5_428_640, b'x');
    std::fs::write(&invalid, dump).unwrap();
    std::fs::write(&valid, ogg_flac(0)).unwrap();
    let paths = vec![invalid, valid];
    let (files, stats) = scan_files_parallel(&paths, false, None);
    assert_scan(&stats);
    assert!(
        files
            .iter()
            .find(|f| f.path.ends_with("web.ogg"))
            .unwrap()
            .metadata
            .is_none()
    );
    assert!(
        files
            .iter()
            .find(|f| f.path.ends_with("Audio.oga"))
            .unwrap()
            .metadata
            .is_some()
    );
    let mut batched = Vec::new();
    let stats = scan_files_batched(&paths, false, 1, |files, _, _| {
        batched.extend(files);
        EcrituresDuLot::SANS_PERTE
    });
    assert_scan(&stats);
    assert_eq!(batched.iter().filter(|f| f.metadata.is_some()).count(), 1);
}
