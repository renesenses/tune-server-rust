//! Minimal single-track M4A (MP4 audio) muxer, shared by the native ALAC
//! (#1526) and AAC (#1527) encoders.
//!
//! Layout: `ftyp` + `mdat` + `moov`, every packet in one chunk, `stco`
//! pointing at the start of the mdat payload. The codec-specific part is
//! the sample entry: its four-char kind (`alac` / `mp4a`) and the codec
//! configuration box nested inside it (`alac` cookie box / `esds`).

/// One encoded audio track ready to be wrapped in an M4A container.
pub struct M4aTrack<'a> {
    /// Sample entry kind: `b"alac"` or `b"mp4a"`.
    pub sample_entry_kind: [u8; 4],
    /// Complete codec configuration box (already boxed): the `alac` cookie
    /// box, or the `esds` descriptor box for AAC.
    pub codec_box: Vec<u8>,
    pub packets: &'a [Vec<u8>],
    /// PCM frames per packet (4096 for ALAC, 1024 for AAC).
    pub frames_per_packet: u32,
    /// Total PCM frames described by the track — the last packet may be
    /// partial (ALAC) or padded (AAC priming/trailing).
    pub total_frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
}

pub fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + payload.len());
    b.extend_from_slice(&((payload.len() as u32 + 8).to_be_bytes()));
    b.extend_from_slice(kind);
    b.extend_from_slice(payload);
    b
}

pub fn full_box(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + payload.len());
    p.push(version);
    p.extend_from_slice(&flags.to_be_bytes()[1..]);
    p.extend_from_slice(payload);
    boxed(kind, &p)
}

/// Assemble the complete file.
pub fn mux(track: &M4aTrack) -> Vec<u8> {
    let M4aTrack {
        sample_entry_kind,
        codec_box,
        packets,
        frames_per_packet,
        total_frames,
        sample_rate,
        channels,
        bit_depth,
    } = track;
    let (frames_per_packet, total_frames, sample_rate, channels, bit_depth) = (
        *frames_per_packet,
        *total_frames,
        *sample_rate,
        *channels,
        *bit_depth,
    );

    let ftyp = boxed(b"ftyp", &{
        let mut p = Vec::new();
        p.extend_from_slice(b"M4A "); // major brand
        p.extend_from_slice(&0u32.to_be_bytes()); // minor version
        p.extend_from_slice(b"M4A isomiso2"); // compatible brands
        p
    });

    let mdat_payload_len: usize = packets.iter().map(|p| p.len()).sum();
    // Chunk offset = ftyp + mdat header (8 bytes).
    let chunk_offset = (ftyp.len() + 8) as u32;

    // --- stsd: AudioSampleEntry with the nested codec config box ---
    let mut entry = Vec::new();
    entry.extend_from_slice(&[0u8; 6]); // reserved
    entry.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    entry.extend_from_slice(&[0u8; 8]); // reserved
    entry.extend_from_slice(&channels.to_be_bytes());
    entry.extend_from_slice(&bit_depth.to_be_bytes());
    entry.extend_from_slice(&[0u8; 4]); // pre_defined + reserved
    entry.extend_from_slice(&(sample_rate << 16).to_be_bytes()); // 16.16
    entry.extend_from_slice(codec_box);
    let sample_entry = boxed(sample_entry_kind, &entry);
    let stsd = full_box(b"stsd", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&sample_entry);
        p
    });

    // --- stts: N-1 full packets + the final (possibly short) one ---
    let full_packets = (packets.len() as u64).saturating_sub(1);
    let last_frames = total_frames - full_packets * frames_per_packet as u64;
    let mut stts_entries: Vec<(u32, u32)> = Vec::new();
    if full_packets > 0 {
        stts_entries.push((full_packets as u32, frames_per_packet));
    }
    stts_entries.push((1, last_frames as u32));
    let stts = full_box(b"stts", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&(stts_entries.len() as u32).to_be_bytes());
        for (count, delta) in &stts_entries {
            p.extend_from_slice(&count.to_be_bytes());
            p.extend_from_slice(&delta.to_be_bytes());
        }
        p
    });

    let stsc = full_box(b"stsc", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes()); // one entry
        p.extend_from_slice(&1u32.to_be_bytes()); // first_chunk
        p.extend_from_slice(&(packets.len() as u32).to_be_bytes()); // samples_per_chunk
        p.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index
        p
    });

    let stsz = full_box(b"stsz", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 → per-sample table
        p.extend_from_slice(&(packets.len() as u32).to_be_bytes());
        for pk in packets.iter() {
            p.extend_from_slice(&(pk.len() as u32).to_be_bytes());
        }
        p
    });

    let stco = full_box(b"stco", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&chunk_offset.to_be_bytes());
        p
    });

    let stbl = boxed(b"stbl", &[stsd, stts, stsc, stsz, stco].concat());
    let smhd = full_box(b"smhd", 0, 0, &[0u8; 4]);
    let dref = full_box(b"dref", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_be_bytes());
        p.extend_from_slice(&full_box(b"url ", 0, 1, &[])); // self-contained
        p
    });
    let dinf = boxed(b"dinf", &dref);
    let minf = boxed(b"minf", &[smhd, dinf, stbl].concat());

    let hdlr = full_box(b"hdlr", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&[0u8; 4]); // pre_defined
        p.extend_from_slice(b"soun");
        p.extend_from_slice(&[0u8; 12]); // reserved
        p.extend_from_slice(b"SoundHandler\0");
        p
    });

    // mdhd: timescale = sample rate, duration = frames (32-bit is plenty:
    // > 24 h of 192 kHz before overflow).
    let mdhd = full_box(b"mdhd", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&[0u8; 8]); // creation + modification
        p.extend_from_slice(&sample_rate.to_be_bytes());
        p.extend_from_slice(&(total_frames as u32).to_be_bytes());
        p.extend_from_slice(&0x55C4u16.to_be_bytes()); // language: und
        p.extend_from_slice(&0u16.to_be_bytes());
        p
    });

    let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());

    // tkhd/mvhd durations in a 1000 Hz movie timescale.
    let dur_ms = (total_frames * 1000 / sample_rate as u64) as u32;
    let tkhd = full_box(b"tkhd", 0, 7, &{
        let mut p = Vec::new();
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(&1u32.to_be_bytes()); // track id
        p.extend_from_slice(&[0u8; 4]);
        p.extend_from_slice(&dur_ms.to_be_bytes());
        p.extend_from_slice(&[0u8; 8]); // reserved
        p.extend_from_slice(&[0u8; 8]); // layer/group/volume/reserved
        // identity matrix
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.extend_from_slice(&[0u8; 8]); // width/height
        p
    });
    let trak = boxed(b"trak", &[tkhd, mdia].concat());

    let mvhd = full_box(b"mvhd", 0, 0, &{
        let mut p = Vec::new();
        p.extend_from_slice(&[0u8; 8]);
        p.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        p.extend_from_slice(&dur_ms.to_be_bytes());
        p.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        p.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        p.extend_from_slice(&[0u8; 10]); // reserved
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.extend_from_slice(&[0u8; 24]); // pre_defined
        p.extend_from_slice(&2u32.to_be_bytes()); // next track id
        p
    });
    let moov = boxed(b"moov", &[mvhd, trak].concat());

    let mut out = Vec::with_capacity(ftyp.len() + 8 + mdat_payload_len + moov.len());
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&((mdat_payload_len as u32 + 8).to_be_bytes()));
    out.extend_from_slice(b"mdat");
    for pk in packets.iter() {
        out.extend_from_slice(pk);
    }
    out.extend_from_slice(&moov);
    out
}
