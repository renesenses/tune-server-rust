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

/// Recursively walk a moov (or child container) payload and add `delta` (which
/// may be negative) to every `stco`/`co64` chunk offset. `buf` is the container's
/// *children* bytes. Returns `false` if any offset would underflow 0 or overflow
/// u32 (`stco`) after the shift.
///
/// A positive delta is used when `moov` is relocated in FRONT of the media
/// (faststart) or grows; a NEGATIVE delta when the media shifts UP because bytes
/// before it were removed — e.g. stripping cover art from a moov that already
/// sits before `mdat` (the media then starts `shrink` bytes earlier).
fn patch_offsets(buf: &mut [u8], delta: i64) -> bool {
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
                    let new = old as i64 + delta;
                    if new < 0 || new > u32::MAX as i64 {
                        // Underflow, or would need a co64 promotion — bail.
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
                    let new = old as i64 + delta;
                    if new < 0 {
                        return false;
                    }
                    buf[e..e + 8].copy_from_slice(&(new as u64).to_be_bytes());
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

/// Locate the first direct child atom of `kind` within `buf` (a container's
/// children region). Returns `(offset_in_buf, total_size_incl_header)`.
fn find_child_atom(buf: &[u8], kind: &[u8; 4]) -> Option<(usize, usize)> {
    let mut pos = 0usize;
    while pos + 8 <= buf.len() {
        let size = be_u32(buf, pos)? as usize;
        if size < 8 || pos + size > buf.len() {
            return None;
        }
        if &buf[pos + 4..pos + 8] == kind {
            return Some((pos, size));
        }
        pos += size;
    }
    None
}

/// Strip every `covr` (cover-art) atom from an in-memory `moov`, returning the
/// rewritten (smaller) moov and the number of bytes removed. Returns `None` when
/// there is no cover art (or the structure isn't the expected
/// `moov > udta > meta > ilst > covr`, or a 64-bit `moov` we don't rewrite).
///
/// Embedded artwork lives in the trailing metadata, never in `mdat`, so removing
/// it is a lossless container edit — the audio bitstream is untouched. A large
/// `covr` (>100 KB is common) makes some DLNA renderers emit a click ("ploc") at
/// the very start of ALAC passthrough playback (Yves, LHC-56): serving the file
/// with the cover removed eliminates the ploc while keeping the artwork in the
/// on-disk file (and thus in the library UI). Because `moov` sits after `mdat` in
/// these files, dropping `covr` shifts no media chunk, so the faststart
/// chunk-offset patch (`+moov.len()`) stays correct with the now-smaller `moov`.
pub fn strip_covr_from_moov(moov: &[u8]) -> Option<(Vec<u8>, u64)> {
    if moov.len() < 8 || &moov[4..8] != b"moov" {
        return None;
    }
    // A 64-bit `moov` (size32 == 1) is not something we rewrite — bail safely.
    if be_u32(moov, 0)? == 1 {
        return None;
    }

    // Descend moov → udta → meta → ilst.
    let moov_children = 8usize;
    let (udta_rel, udta_size) = find_child_atom(&moov[moov_children..], b"udta")?;
    let udta_start = moov_children + udta_rel;
    let udta_children = udta_start + 8;
    let udta_end = udta_start + udta_size;

    let (meta_rel, meta_size) = find_child_atom(&moov[udta_children..udta_end], b"meta")?;
    let meta_start = udta_children + meta_rel;
    let meta_end = meta_start + meta_size;

    // `meta` is normally an ISO FullBox: 8-byte header + 4-byte version/flags,
    // THEN child atoms. A few old QuickTime files omit the version/flags — try
    // the FullBox layout first, then the bare-container layout.
    let (ilst_start, ilst_size) = [meta_start + 12, meta_start + 8]
        .into_iter()
        .filter(|&cs| cs < meta_end)
        .find_map(|cs| find_child_atom(&moov[cs..meta_end], b"ilst").map(|(r, s)| (cs + r, s)))?;
    let ilst_children = ilst_start + 8;
    let ilst_end = ilst_start + ilst_size;

    // Collect every `covr` child within `ilst`.
    let mut covr_ranges: Vec<(usize, usize)> = Vec::new();
    let mut pos = ilst_children;
    while pos + 8 <= ilst_end {
        let size = be_u32(moov, pos)? as usize;
        if size < 8 || pos + size > ilst_end {
            break;
        }
        if &moov[pos + 4..pos + 8] == b"covr" {
            covr_ranges.push((pos, size));
        }
        pos += size;
    }
    if covr_ranges.is_empty() {
        return None;
    }
    let shrink: u64 = covr_ranges.iter().map(|&(_, s)| s as u64).sum();
    if shrink > u32::MAX as u64 {
        return None;
    }
    let shrink32 = shrink as u32;

    // Decrement the size field of every ancestor container by the total covr
    // bytes removed. Each ancestor's size field precedes the covr bytes, so
    // patching before splicing keeps offsets valid.
    let mut out = moov.to_vec();
    for &start in &[0usize, udta_start, meta_start, ilst_start] {
        let old = be_u32(&out, start)?;
        let new = old.checked_sub(shrink32)?;
        out[start..start + 4].copy_from_slice(&new.to_be_bytes());
    }
    // Splice the covr atoms out, tail-first so earlier ranges stay valid.
    for &(cstart, csize) in covr_ranges.iter().rev() {
        out.drain(cstart..cstart + csize);
    }
    Some((out, shrink))
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
    let moov_size = moov.size as i64;

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
    let delta = moov.len() as i64;
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

    // Strip embedded cover art (`covr`) from the moov: it lives in the trailing
    // metadata, never in `mdat`, so this is a lossless container edit. A large
    // `covr` triggers a start-of-track "ploc" on some renderers in ALAC
    // passthrough (Yves, LHC-56). The moov gets smaller; `mdat` (served untouched
    // from disk) doesn't move, so the faststart chunk-offset patch below —
    // computed from the now-smaller moov length — stays exact.
    if let Some((stripped, _shrink)) = strip_covr_from_moov(&moov_bytes) {
        moov_bytes = stripped;
    }

    let header = faststart_header(&ftyp_bytes, &moov_bytes)?;
    // NOTE: `total` intentionally may be smaller than the on-disk size — the
    // cover-strip path (`strip_covr_from_moov`) removes `covr` art from `moov`
    // and re-patches the chunk offsets, which is exactly what kills the "ploc"
    // on 24-bit ALAC passthrough (Yves, LHC). An earlier size-invariant guard
    // here wrongly rejected that legitimate shrink; the strip path validates
    // its own byte accounting, so no cross-check is needed.
    let total = header.len() as u64 + body_len;

    Some(FaststartMap {
        header,
        body_src_start,
        body_len,
        total,
    })
}

/// Strip cover art from an ALREADY-faststart M4A (`ftyp | moov | mdat…`, moov
/// BEFORE mdat) so ALAC passthrough doesn't "ploc" on renderers like the LHC-56.
///
/// [`prepare_faststart`] only handles the moov-after-mdat layout (it relocates
/// moov and strips the cover on the way). Files that are already faststart —
/// common with modern encoders / `ffmpeg -movflags +faststart` — skipped that
/// path entirely, so their cover survived and they still ploc'd (Yves: Aurora
/// fixed, but "Do What U Will" / "ABOVE AND BEYOND" still clicked; no
/// `m4a_faststart_applied` was logged for them).
///
/// Here moov stays at the front; removing `covr` only shrinks it, so `mdat`
/// (served untouched from disk as the body) starts `shrink` bytes earlier — we
/// patch the chunk offsets by `-shrink`. Requires the simple `ftyp` immediately
/// followed by `moov`, then the rest; returns `None` (serve raw) otherwise or
/// when there's no cover to remove.
pub fn prepare_cover_strip_faststart(path: &std::path::Path) -> Option<FaststartMap> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let file_len = f.metadata().ok()?.len();

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

    // ftyp first (at 0), moov immediately after ftyp, and moov BEFORE mdat
    // (already faststart). Anything else → let prepare_faststart / raw handle it.
    if ftyp.1 != 0 || &atoms[0].0 != b"ftyp" {
        return None;
    }
    let ftyp_end = ftyp.1 + ftyp.2;
    if moov.1 != ftyp_end {
        return None; // atoms between ftyp and moov — not the simple faststart shape
    }
    if moov.1 >= mdat.1 {
        return None; // moov after mdat → prepare_faststart's job
    }

    let moov_end = moov.1 + moov.2;

    let mut ftyp_bytes = vec![0u8; ftyp.2 as usize];
    f.seek(SeekFrom::Start(ftyp.1)).ok()?;
    f.read_exact(&mut ftyp_bytes).ok()?;
    let mut moov_bytes = vec![0u8; moov.2 as usize];
    f.seek(SeekFrom::Start(moov.1)).ok()?;
    f.read_exact(&mut moov_bytes).ok()?;

    // No cover → nothing to do (an already-faststart file without art doesn't
    // ploc and needs no rewrite).
    let (stripped_moov, shrink) = strip_covr_from_moov(&moov_bytes)?;

    // mdat now starts `shrink` bytes earlier → shift every chunk offset down.
    let mut patched = stripped_moov;
    if patched.len() < 8 || !patch_offsets(&mut patched[8..], -(shrink as i64)) {
        return None;
    }

    let mut header = Vec::with_capacity(ftyp_bytes.len() + patched.len());
    header.extend_from_slice(&ftyp_bytes);
    header.extend_from_slice(&patched);

    // Body = everything after the original moov (mdat + any trailing atoms),
    // served untouched from disk.
    let body_src_start = moov_end;
    let body_len = file_len - moov_end;
    let total = header.len() as u64 + body_len;
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

    /// A `udta > meta(FullBox) > { hdlr, ilst > { [covr], ©nam } }` metadata
    /// subtree. When `cover` is `Some`, a `covr` atom carries it.
    fn udta_with_metadata(cover: Option<&[u8]>) -> Vec<u8> {
        let nam = atom(b"\xa9nam", b"Title");
        let mut ilst_body = Vec::new();
        if let Some(c) = cover {
            ilst_body.extend_from_slice(&atom(b"covr", c));
        }
        ilst_body.extend_from_slice(&nam);
        let ilst = atom(b"ilst", &ilst_body);
        let hdlr = atom(b"hdlr", &[0u8; 20]);
        let mut meta_body = vec![0u8; 4]; // FullBox version+flags
        meta_body.extend_from_slice(&hdlr);
        meta_body.extend_from_slice(&ilst);
        let meta = atom(b"meta", &meta_body);
        atom(b"udta", &meta)
    }

    /// A moov with an stbl→stco (audio side) AND a udta metadata subtree that
    /// optionally embeds cover art — mirrors a real iTunes/ffmpeg ALAC file.
    fn moov_with_stco_and_meta(offsets: &[u32], cover: Option<&[u8]>) -> Vec<u8> {
        let mut stco_body = vec![0u8; 4];
        stco_body.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
        for o in offsets {
            stco_body.extend_from_slice(&o.to_be_bytes());
        }
        let stco = atom(b"stco", &stco_body);
        let stbl = atom(b"stbl", &stco);
        let minf = atom(b"minf", &stbl);
        let mdia = atom(b"mdia", &minf);
        let trak = atom(b"trak", &mdia);
        let mut moov_body = trak;
        moov_body.extend_from_slice(&udta_with_metadata(cover));
        atom(b"moov", &moov_body)
    }

    #[test]
    fn strip_covr_removes_cover_and_fixes_sizes() {
        let cover = vec![0xEEu8; 500];
        let moov = moov_with_stco_and_meta(&[100], Some(&cover));
        let covr_total = 8 + cover.len(); // covr atom = header + payload

        let (stripped, shrink) = strip_covr_from_moov(&moov).expect("has cover");
        assert_eq!(shrink as usize, covr_total);
        assert_eq!(stripped.len(), moov.len() - covr_total);
        // No covr bytes remain, but the other metadata survives.
        assert!(!stripped.windows(4).any(|w| w == b"covr"), "covr gone");
        assert!(stripped.windows(4).any(|w| w == b"\xa9nam"), "©nam kept");
        // The moov's own size field now equals its real length.
        assert_eq!(be_u32(&stripped, 0).unwrap() as usize, stripped.len());
        // Re-parsing the whole moov must succeed (all ancestor sizes consistent).
        let parsed = parse_top_level(&stripped).expect("stripped moov parses");
        assert_eq!(parsed.len(), 1);
        assert_eq!(&parsed[0].kind, b"moov");
        assert_eq!(parsed[0].size, stripped.len());
        // The chunk offset must be UNTOUCHED (strip never shifts media chunks).
        let sidx = stripped.windows(4).position(|w| w == b"stco").unwrap();
        assert_eq!(be_u32(&stripped, sidx + 4 + 8).unwrap(), 100);
    }

    #[test]
    fn strip_covr_none_without_metadata() {
        // No udta at all → nothing to strip.
        assert!(strip_covr_from_moov(&moov_with_stco(&[100])).is_none());
    }

    #[test]
    fn strip_covr_none_when_ilst_has_no_cover() {
        let moov = moov_with_stco_and_meta(&[100], None);
        assert!(strip_covr_from_moov(&moov).is_none());
    }

    #[test]
    fn prepare_faststart_strips_cover_and_shrinks_total() {
        // Non-faststart file: ftyp | mdat | moov(with cover). prepare_faststart
        // must relocate moov AND drop the cover, so `total` is the disk size
        // minus the covr atom and the served header carries no covr bytes.
        let ftyp = atom(b"ftyp", b"M4A isom");
        let mdat = atom(b"mdat", &vec![0x11u8; 300]);
        let cover = vec![0x77u8; 1000];
        let chunk_off = (ftyp.len() + 8) as u32; // first sample byte inside mdat
        let moov = moov_with_stco_and_meta(&[chunk_off], Some(&cover));
        let covr_total = 8 + cover.len();

        let mut original = ftyp.clone();
        original.extend_from_slice(&mdat);
        original.extend_from_slice(&moov);

        let path = std::env::temp_dir().join("tune-faststart-cover-strip-test.m4a");
        std::fs::write(&path, &original).unwrap();
        let map = prepare_faststart(&path).expect("prepare");
        std::fs::remove_file(&path).ok();

        // Total served = disk size minus the removed cover.
        assert_eq!(map.total, original.len() as u64 - covr_total as u64);
        // Header = ftyp + stripped moov, no cover.
        assert!(
            !map.header.windows(4).any(|w| w == b"covr"),
            "no covr served"
        );
        assert_eq!(&map.header[4..8], b"ftyp");
        // Body = the untouched mdat region.
        assert_eq!(map.body_src_start, ftyp.len() as u64);
        assert_eq!(map.body_len, mdat.len() as u64);
        // stco patched by +stripped_moov_len (not the original moov len).
        let stripped_moov_len = moov.len() - covr_total;
        let sidx = map.header.windows(4).position(|w| w == b"stco").unwrap();
        let patched = be_u32(&map.header, sidx + 4 + 8).unwrap();
        assert_eq!(patched as usize, chunk_off as usize + stripped_moov_len);
        // header + body == total (self-consistent virtual file).
        assert_eq!(map.header.len() as u64 + map.body_len, map.total);
    }

    #[test]
    fn prepare_cover_strip_faststart_strips_and_shifts_down() {
        // Already-faststart file: ftyp | moov(with cover) | mdat. prepare_faststart
        // declines it; prepare_cover_strip_faststart must strip the cover in place
        // and shift the chunk offsets DOWN by the removed bytes (mdat moves up).
        let ftyp = atom(b"ftyp", b"M4A isom");
        let cover = vec![0x99u8; 800];
        let covr_total = 8 + cover.len();
        // moov length is independent of the stco offset value, so size it first.
        let moov_len = moov_with_stco_and_meta(&[0], Some(&cover)).len();
        // In the original faststart file the first sample sits at ftyp|moov|mdat-header.
        let chunk_off = (ftyp.len() + moov_len + 8) as u32;
        let moov = moov_with_stco_and_meta(&[chunk_off], Some(&cover));
        let mdat = atom(b"mdat", &vec![0x22u8; 400]);

        let mut original = ftyp.clone();
        original.extend_from_slice(&moov);
        original.extend_from_slice(&mdat);

        let path = std::env::temp_dir().join("tune-faststart-cover-inplace-test.m4a");
        std::fs::write(&path, &original).unwrap();
        assert!(
            prepare_faststart(&path).is_none(),
            "already faststart → declined"
        );
        let map = prepare_cover_strip_faststart(&path).expect("cover strip in place");
        std::fs::remove_file(&path).ok();

        // Served size = disk minus the removed cover.
        assert_eq!(map.total, original.len() as u64 - covr_total as u64);
        assert!(
            !map.header.windows(4).any(|w| w == b"covr"),
            "no covr served"
        );
        assert_eq!(&map.header[4..8], b"ftyp");
        // Body = the mdat region, from the ORIGINAL moov_end to EOF.
        let moov_end = ftyp.len() + moov.len();
        assert_eq!(map.body_src_start, moov_end as u64);
        assert_eq!(map.body_len, mdat.len() as u64);
        // stco shifted DOWN by the cover size (mdat starts earlier now).
        let sidx = map.header.windows(4).position(|w| w == b"stco").unwrap();
        let patched = be_u32(&map.header, sidx + 4 + 8).unwrap();
        assert_eq!(patched as usize, chunk_off as usize - covr_total);
        // Self-consistent virtual file.
        assert_eq!(map.header.len() as u64 + map.body_len, map.total);
    }

    #[test]
    fn prepare_cover_strip_faststart_none_without_cover() {
        // Already-faststart file with no cover art → nothing to do.
        let ftyp = atom(b"ftyp", b"M4A isom");
        let moov = moov_with_stco_and_meta(&[(ftyp.len() + 200 + 8) as u32], None);
        let mdat = atom(b"mdat", &vec![0u8; 100]);
        let mut original = ftyp;
        original.extend_from_slice(&moov);
        original.extend_from_slice(&mdat);
        let path = std::env::temp_dir().join("tune-faststart-nocover-inplace-test.m4a");
        std::fs::write(&path, &original).unwrap();
        let r = prepare_cover_strip_faststart(&path);
        std::fs::remove_file(&path).ok();
        assert!(r.is_none(), "no cover → serve raw");
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
