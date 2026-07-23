//! MP4/M4A "faststart" — relocate the `moov` atom to the front of the file.
//!
//! Many ALAC/M4A files (CD rips from iTunes/others) store the `moov` atom
//! (all the track metadata + the `stco`/`co64` chunk-offset tables) at the END
//! of the file, after `mdat`. When such a file is served directly (passthrough)
//! to a DLNA renderer, the renderer must seek to the end of the file to read
//! `moov` before it can decode a single sample — a large end-seek, especially
//! painful when the source is on a network mount (SMB/NAS). The result is a slow
//! start and a storm of Range requests (Yves, LHC-56, 192/24 ALAC on a NAS).
//!
//! Faststart rewrites the file as `ftyp | moov | …rest…` so the renderer reads
//! the metadata immediately and starts playing at once. Because moving `moov`
//! before `mdat` shifts every media chunk down by `moov`'s size, we patch each
//! `stco` (32-bit) / `co64` (64-bit) chunk-offset table by `+moov_size`.
//!
//! Pure byte manipulation — no decode/encode, so it's cheap (I/O-bound). If the
//! file is already faststart, unusual, or would overflow a 32-bit `stco`, we
//! return `None` and the caller serves the original file unchanged.

/// Read a big-endian u32 at `off`.
fn be_u32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// A top-level atom: (type, absolute start offset, total size incl. header).
struct Atom {
    kind: [u8; 4],
    start: usize,
    size: usize,
}

/// Parse the top-level atom list. Returns `None` on any malformed size.
fn parse_top_level(data: &[u8]) -> Option<Vec<Atom>> {
    let mut atoms = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size32 = be_u32(data, pos)? as usize;
        let kind: [u8; 4] = data.get(pos + 4..pos + 8)?.try_into().ok()?;
        let size = if size32 == 1 {
            // 64-bit largesize follows the type. Combine in u64 so the shift
            // is valid on 32-bit targets (usize is 32-bit on armv7), then fit
            // back into usize — a >4 GB atom on a 32-bit host yields None.
            let hi = be_u32(data, pos + 8)? as u64;
            let lo = be_u32(data, pos + 12)? as u64;
            usize::try_from((hi << 32) | lo).ok()?
        } else if size32 == 0 {
            // Extends to EOF.
            data.len() - pos
        } else {
            size32
        };
        if size < 8 || pos + size > data.len() {
            return None;
        }
        atoms.push(Atom {
            kind,
            start: pos,
            size,
        });
        pos += size;
    }
    Some(atoms)
}

/// Container atoms whose payload is itself a sequence of child atoms (no leading
/// fields), so we can recurse into them to find `stco`/`co64`.
fn is_container(kind: &[u8; 4]) -> bool {
    matches!(
        kind,
        b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"edts" | b"udta"
    )
}

/// Recursively walk a moov (or child container) payload and add `delta` to every
/// `stco`/`co64` chunk offset. `buf` is the container's *children* bytes.
/// Returns `false` if an `stco` entry would overflow u32 after the shift.
fn patch_offsets(buf: &mut [u8], delta: u64) -> bool {
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let Some(size32) = be_u32(buf, pos) else {
            return true;
        };
        let size = size32 as usize;
        if size < 8 || pos + size > buf.len() {
            // Malformed / 64-bit sized child we don't expect inside moov — stop
            // walking this level rather than corrupt anything.
            break;
        }
        let kind: [u8; 4] = match buf[pos + 4..pos + 8].try_into() {
            Ok(k) => k,
            Err(_) => break,
        };
        let body = pos + 8;
        match &kind {
            b"stco" => {
                // [version+flags:4][entry_count:4][offset:4 × count]
                let Some(count) = be_u32(buf, body + 4) else {
                    return true;
                };
                let mut e = body + 8;
                for _ in 0..count {
                    if e + 4 > buf.len() {
                        break;
                    }
                    let Some(old) = be_u32(buf, e) else {
                        return true;
                    };
                    let new = old as u64 + delta;
                    if new > u32::MAX as u64 {
                        // Would need a co64 promotion — bail to be safe.
                        return false;
                    }
                    buf[e..e + 4].copy_from_slice(&(new as u32).to_be_bytes());
                    e += 4;
                }
            }
            b"co64" => {
                let Some(count) = be_u32(buf, body + 4) else {
                    return true;
                };
                let mut e = body + 8;
                for _ in 0..count {
                    if e + 8 > buf.len() {
                        break;
                    }
                    let old = u64::from_be_bytes(match buf[e..e + 8].try_into() {
                        Ok(a) => a,
                        Err(_) => return true,
                    });
                    let new = old + delta;
                    buf[e..e + 8].copy_from_slice(&new.to_be_bytes());
                    e += 8;
                }
            }
            k if is_container(k) => {
                let end = pos + size;
                if !patch_offsets(&mut buf[body..end], delta) {
                    return false;
                }
            }
            _ => {}
        }
        pos += size;
    }
    true
}

/// Produce a faststart copy of an MP4/M4A byte buffer, or `None` when it's
/// already faststart / not applicable / unsafe to rewrite.
pub fn faststart_m4a(data: &[u8]) -> Option<Vec<u8>> {
    let atoms = parse_top_level(data)?;

    let moov_idx = atoms.iter().position(|a| &a.kind == b"moov")?;
    let mdat_idx = atoms.iter().position(|a| &a.kind == b"mdat")?;

    // Already faststart: moov before mdat → nothing to do.
    if moov_idx < mdat_idx {
        return None;
    }

    let moov = &atoms[moov_idx];
    let moov_size = moov.size as u64;

    // Copy moov and patch its chunk-offset tables (+moov_size, the amount every
    // media chunk shifts once moov is inserted before mdat).
    let mut moov_bytes = data.get(moov.start..moov.start + moov.size)?.to_vec();
    // moov payload = children after the 8-byte header (moov is never 64-bit here
    // in practice; parse_top_level already handled the top-level size).
    if !patch_offsets(&mut moov_bytes[8..], moov_size) {
        return None; // 32-bit overflow → leave original alone.
    }

    // Rebuild: ftyp (if any, kept first) → moov → every other atom in order.
    let mut out = Vec::with_capacity(data.len());
    if let Some(ftyp) = atoms.iter().find(|a| &a.kind == b"ftyp") {
        out.extend_from_slice(&data[ftyp.start..ftyp.start + ftyp.size]);
    }
    out.extend_from_slice(&moov_bytes);
    for a in &atoms {
        if &a.kind == b"ftyp" || &a.kind == b"moov" {
            continue;
        }
        out.extend_from_slice(&data[a.start..a.start + a.size]);
    }
    Some(out)
}

/// Build the faststart header = `ftyp` ++ patched `moov` (chunk offsets shifted
/// by `+moov.len()`). Returns `None` on 32-bit `stco` overflow.
pub fn faststart_header(ftyp: &[u8], moov: &[u8]) -> Option<Vec<u8>> {
    let delta = moov.len() as u64;
    let mut moov_owned = moov.to_vec();
    if moov_owned.len() < 8 || !patch_offsets(&mut moov_owned[8..], delta) {
        return None;
    }
    let mut out = Vec::with_capacity(ftyp.len() + moov_owned.len());
    out.extend_from_slice(ftyp);
    out.extend_from_slice(&moov_owned);
    Some(out)
}

/// A virtual faststart layout that serves without copying `mdat`: the
/// `header` (ftyp + patched moov, in memory) is served first, then the original
/// file's body bytes `[body_src_start .. body_src_start+body_len]` (the mdat).
#[derive(Clone)]
pub struct FaststartMap {
    pub header: Vec<u8>,
    pub body_src_start: u64,
    pub body_len: u64,
    pub total: u64,
}

/// Read only the atom table + `ftyp`/`moov` of an M4A file (never `mdat`) and
/// build a [`FaststartMap`] for on-the-fly faststart serving. Returns `None`
/// when the file is already faststart, `ftyp` isn't first, `moov` isn't last, or
/// anything is malformed — the caller then serves the original file unchanged.
pub fn prepare_faststart(path: &std::path::Path) -> Option<FaststartMap> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();

    // Walk the top-level atom table by seeking over headers only.
    let mut atoms: Vec<([u8; 4], u64, u64)> = Vec::new();
    let mut pos = 0u64;
    let mut hdr = [0u8; 16];
    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos)).ok()?;
        f.read_exact(&mut hdr[..8]).ok()?;
        let size32 = u32::from_be_bytes(hdr[0..4].try_into().ok()?) as u64;
        let kind = [hdr[4], hdr[5], hdr[6], hdr[7]];
        let size = if size32 == 1 {
            f.read_exact(&mut hdr[8..16]).ok()?;
            u64::from_be_bytes(hdr[8..16].try_into().ok()?)
        } else if size32 == 0 {
            file_len - pos
        } else {
            size32
        };
        if size < 8 || pos + size > file_len {
            return None;
        }
        atoms.push((kind, pos, size));
        pos += size;
    }
    if atoms.is_empty() {
        return None;
    }

    let ftyp = *atoms.iter().find(|a| &a.0 == b"ftyp")?;
    let moov = *atoms.iter().find(|a| &a.0 == b"moov")?;
    let mdat = *atoms.iter().find(|a| &a.0 == b"mdat")?;

    // Require the simple, dominant shape: ftyp at offset 0, moov last, after
    // mdat, and everything between ftyp and moov contiguous (the body).
    if ftyp.1 != 0 || &atoms[0].0 != b"ftyp" {
        return None;
    }
    if moov.1 != atoms.last()?.1 {
        return None;
    }
    if moov.1 <= mdat.1 {
        return None; // already faststart (moov before mdat) or unusual
    }

    let ftyp_end = ftyp.1 + ftyp.2;
    if moov.1 < ftyp_end {
        return None;
    }
    let body_src_start = ftyp_end;
    let body_len = moov.1 - ftyp_end;

    let mut ftyp_bytes = vec![0u8; ftyp.2 as usize];
    f.seek(SeekFrom::Start(ftyp.1)).ok()?;
    f.read_exact(&mut ftyp_bytes).ok()?;
    let mut moov_bytes = vec![0u8; moov.2 as usize];
    f.seek(SeekFrom::Start(moov.1)).ok()?;
    f.read_exact(&mut moov_bytes).ok()?;

    let header = faststart_header(&ftyp_bytes, &moov_bytes)?;
    let total = header.len() as u64 + body_len;

    // NOTE: no size-invariant cross-check here. `total` may legitimately be
    // smaller than the on-disk size once the cover-strip faststart path lands
    // (it removes `covr` art from `moov`, the real fix for the 24-bit ALAC
    // "ploc"). An earlier guard rejecting `total != file_len` wrongly blocked
    // that shrink; the strip path validates its own byte accounting.
    Some(FaststartMap {
        header,
        body_src_start,
        body_len,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let size = (8 + body.len()) as u32;
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }

    /// Build a minimal moov containing one stbl→stco with the given offsets.
    fn moov_with_stco(offsets: &[u32]) -> Vec<u8> {
        let mut stco_body = vec![0u8; 4]; // version+flags
        stco_body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            stco_body.extend_from_slice(&o.to_be_bytes());
        }
        let stco = atom(b"stco", &stco_body);
        let stbl = atom(b"stbl", &stco);
        let minf = atom(b"minf", &stbl);
        let mdia = atom(b"mdia", &minf);
        let trak = atom(b"trak", &mdia);
        atom(b"moov", &trak)
    }

    #[test]
    fn relocates_moov_and_patches_stco() {
        let ftyp = atom(b"ftyp", b"M4A isom");
        let mdat = atom(b"mdat", &vec![0xAAu8; 100]);
        let moov = moov_with_stco(&[/* chunk in mdat at */ ftyp.len() as u32 + 8]);
        let moov_size = moov.len() as u32;

        // Original: ftyp | mdat | moov  (moov at end)
        let mut original = ftyp.clone();
        original.extend_from_slice(&mdat);
        original.extend_from_slice(&moov);

        let fs = faststart_m4a(&original).expect("should relocate");

        // New order: ftyp | moov | mdat
        assert_eq!(&fs[4..8], b"ftyp");
        let after_ftyp = ftyp.len();
        assert_eq!(&fs[after_ftyp + 4..after_ftyp + 8], b"moov");
        let after_moov = after_ftyp + moov.len();
        assert_eq!(&fs[after_moov + 4..after_moov + 8], b"mdat");
        assert_eq!(fs.len(), original.len());

        // The stco offset must have shifted by +moov_size.
        let re = faststart_m4a(&fs);
        assert!(re.is_none(), "already faststart → no second rewrite");

        // Verify the patched offset value directly.
        let parsed = parse_top_level(&fs).unwrap();
        let m = parsed.iter().find(|a| &a.kind == b"moov").unwrap();
        // Find the stco offset inside the relocated moov.
        let moov_slice = &fs[m.start..m.start + m.size];
        // locate 'stco'
        let sidx = moov_slice
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco present");
        let first_off = be_u32(moov_slice, sidx + 4 + 8).unwrap(); // +ver/flags +count
        let expected = (after_ftyp as u32 + 8) + moov_size;
        assert_eq!(first_off, expected, "chunk offset shifted by moov size");
    }

    #[test]
    fn already_faststart_returns_none() {
        let ftyp = atom(b"ftyp", b"M4A isom");
        let moov = moov_with_stco(&[999]);
        let mdat = atom(b"mdat", &vec![0u8; 10]);
        // ftyp | moov | mdat  (already faststart)
        let mut data = ftyp;
        data.extend_from_slice(&moov);
        data.extend_from_slice(&mdat);
        assert!(faststart_m4a(&data).is_none());
    }

    #[test]
    fn co64_is_patched() {
        // moov with a co64 (64-bit offsets).
        let mut co64_body = vec![0u8; 4];
        co64_body.extend_from_slice(&1u32.to_be_bytes()); // 1 entry
        co64_body.extend_from_slice(&50u64.to_be_bytes());
        let co64 = atom(b"co64", &co64_body);
        let stbl = atom(b"stbl", &co64);
        let minf = atom(b"minf", &stbl);
        let mdia = atom(b"mdia", &minf);
        let trak = atom(b"trak", &mdia);
        let moov = atom(b"moov", &trak);
        let moov_size = moov.len() as u64;
        let ftyp = atom(b"ftyp", b"isom");
        let mdat = atom(b"mdat", &vec![0u8; 60]);
        let mut original = ftyp.clone();
        original.extend_from_slice(&mdat);
        original.extend_from_slice(&moov);

        let fs = faststart_m4a(&original).unwrap();
        let cidx = fs.windows(4).position(|w| w == b"co64").unwrap();
        let val = u64::from_be_bytes(fs[cidx + 4 + 8..cidx + 4 + 8 + 8].try_into().unwrap());
        assert_eq!(val, 50 + moov_size);
    }

    #[test]
    fn garbage_returns_none() {
        assert!(faststart_m4a(b"not an mp4 file at all").is_none());
        assert!(faststart_m4a(&[]).is_none());
    }

    #[test]
    fn prepare_map_reads_only_header_and_maps_body() {
        let ftyp = atom(b"ftyp", b"M4A isom");
        let mdat = atom(b"mdat", &vec![0x42u8; 200]);
        let moov = moov_with_stco(&[ftyp.len() as u32 + 8]);
        let mut original = ftyp.clone();
        original.extend_from_slice(&mdat);
        original.extend_from_slice(&moov);

        // Write to a temp file (unique name; no Date/rand needed).
        let path = std::env::temp_dir().join("tune-faststart-unit-test-fixture.m4a");
        std::fs::write(&path, &original).unwrap();
        let map = prepare_faststart(&path).expect("prepare");
        std::fs::remove_file(&path).ok();

        // header = ftyp + patched moov; body = the mdat region of the original.
        assert_eq!(map.header.len(), ftyp.len() + moov.len());
        assert_eq!(&map.header[4..8], b"ftyp");
        assert_eq!(&map.header[ftyp.len() + 4..ftyp.len() + 8], b"moov");
        assert_eq!(map.body_src_start, ftyp.len() as u64);
        assert_eq!(map.body_len, mdat.len() as u64);
        assert_eq!(map.total, original.len() as u64);

        // The patched stco offset shifted by +moov_size.
        let sidx = map.header.windows(4).position(|w| w == b"stco").unwrap();
        let off = be_u32(&map.header, sidx + 4 + 8).unwrap();
        assert_eq!(off, (ftyp.len() as u32 + 8) + moov.len() as u32);
    }
}
