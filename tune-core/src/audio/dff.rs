//! DFF (DSDIFF) file parser.
//!
//! DSDIFF format specification (IFF-based, big-endian):
//! - FRM8 chunk: "FRM8" magic, chunk size (u64 BE), "DSD " form type
//! - Property chunk "PROP": contains sub-chunks:
//!   - "FS  ": sample rate (u32 BE)
//!   - "CHNL": channel count (u16 BE) + channel IDs
//!   - "CMPR": compression type (4 bytes: "DSD " or "DST ")
//! - DSD Sound Data chunk "DSD ": raw DSD data (interleaved by sample)
//! - DST Sound Data chunk "DST ": DSD compressé sans perte (SACD), contenant
//!   - "FRTE": nombre de trames (u32 BE) + cadence en trames/s (u16 BE)
//!   - "DSTF": une trame compressée, répétée — et parfois "DSTC" (CRC)
//!
//! Le contenu des trames DSTF n'est pas décodé ici : c'est un codec entropique
//! complet (ISO/IEC 14496-3 sous-partie 10). Ce module en lit l'enveloppe, ce
//! qui suffit à annoncer la bonne durée et à refuser la lecture par un message
//! juste — au lieu de prétendre que le fichier n'a pas de données audio.
//!
//! All multi-byte values are big-endian.
//! DSD bit ordering: MSB first within each byte.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Parsed DFF (DSDIFF) file header information.
#[derive(Debug, Clone)]
pub struct DffInfo {
    pub channels: u32,
    pub sample_rate: u32,
    pub compression: String,
    pub data_offset: u64,
    pub data_size: u64,
    /// Nombre de trames DST, lu dans FRTE. `None` pour un fichier non compressé.
    pub dst_frames: Option<u32>,
    /// Cadence des trames DST en trames/s (75 sur un SACD), lue dans FRTE.
    pub dst_frame_rate: Option<u16>,
}

impl DffInfo {
    /// Vrai si les données audio sont compressées en DST.
    pub fn is_dst(&self) -> bool {
        self.compression.trim_end() == "DST"
    }

    /// Le seul garde qui autorise `data_offset`/`data_size` à partir vers le
    /// convertisseur DSD→PCM ou DSD→DoP. Liste BLANCHE : tout ce qui n'est pas
    /// du DSD non compressé est refusé, y compris une compression inconnue.
    ///
    /// Il vivait en double — un message soigné dans `read_dff_data`, qui n'a
    /// aucun appelant dans le dépôt, et un message générique dans
    /// `DffStreamReader::open`, seul chemin réellement emprunté. L'utilisateur
    /// ne recevait donc jamais l'explication utile. Une seule source ici.
    pub fn ensure_raw_dsd(&self) -> Result<(), String> {
        if self.is_dst() {
            // Message distinct du cas « compression inconnue » : ici le fichier
            // est parfaitement lisible et sa durée est connue, seul le décodage
            // manque. Le mot « decode » n'est pas décoratif : `playback.rs` s'en
            // sert pour rendre un 502 (contenu illisible) au lieu d'un 500
            // (panne de Tune).
            return Err(format!(
                "DFF: cannot decode DST compressed audio ({} frames at {} fps) — \
                 convert to uncompressed DSD in the meantime",
                self.dst_frames.unwrap_or(0),
                self.dst_frame_rate.unwrap_or(0)
            ));
        }
        if self.compression != "DSD " {
            return Err(format!(
                "DFF: cannot decode compression '{}' (only uncompressed DSD supported)",
                self.compression
            ));
        }
        Ok(())
    }

    /// Track duration in milliseconds from the DSDIFF header. DSD is 1 bit per
    /// sample, so samples-per-channel = data_size*8/channels, and
    /// duration = samples-per-channel / sample_rate. `None` if the header can't
    /// yield a positive value. Single source of truth for the scan-time
    /// (metadata) and play-time (orchestrator) duration recovery.
    pub fn duration_ms(&self) -> Option<u64> {
        // En DST, `data_size` est la taille COMPRESSÉE : la formule ci-dessous
        // sous-estimerait la durée d'autant que le fichier compresse bien.
        // FRTE donne l'information exacte, indépendante du taux de compression.
        if let (Some(frames), Some(rate)) = (self.dst_frames, self.dst_frame_rate)
            && rate > 0
        {
            return Some(frames as u64 * 1000 / rate as u64);
        }

        let denom = self.channels as u64 * self.sample_rate as u64;
        (denom > 0).then(|| self.data_size.saturating_mul(8).saturating_mul(1000) / denom)
    }
}

/// Read a big-endian u16 from a byte slice at the given offset.
fn read_u16_be(buf: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes([buf[offset], buf[offset + 1]])
}

/// Read a big-endian u32 from a byte slice at the given offset.
fn read_u32_be(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

/// Read a big-endian u64 from a byte slice at the given offset.
fn read_u64_be(buf: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

/// Parse a DFF (DSDIFF) file and return metadata needed for decoding.
pub fn parse_dff(path: &str) -> Result<DffInfo, String> {
    let mut file = File::open(path).map_err(|e| format!("dff open: {e}"))?;

    // --- FRM8 header (12 bytes): "FRM8" + size(u64) + "DSD " ---
    let mut frm8 = [0u8; 12];
    file.read_exact(&mut frm8)
        .map_err(|e| format!("dff read FRM8: {e}"))?;

    if &frm8[0..4] != b"FRM8" {
        return Err("not a DFF file: missing 'FRM8' magic".into());
    }

    // FRM8 chunk size (not including the 12-byte header)
    let _frm8_size = read_u64_be(&frm8, 4);

    // Read form type
    let mut form_type = [0u8; 4];
    file.read_exact(&mut form_type)
        .map_err(|e| format!("dff read form type: {e}"))?;

    if &form_type != b"DSD " {
        return Err(format!(
            "not a DFF/DSD file: form type is '{}'",
            String::from_utf8_lossy(&form_type)
        ));
    }

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u32> = None;
    let mut compression: Option<String> = None;
    let mut dst_frames: Option<u32> = None;
    let mut dst_frame_rate: Option<u16> = None;
    let mut data_offset: Option<u64> = None;
    let mut data_size: Option<u64> = None;

    // Parse chunks until we have all the info we need
    loop {
        let pos = file
            .stream_position()
            .map_err(|e| format!("dff stream_position: {e}"))?;

        // Read chunk header: 4 bytes ID + 8 bytes size
        let mut chunk_header = [0u8; 12];
        if file.read_exact(&mut chunk_header).is_err() {
            break; // EOF
        }

        let chunk_id = &chunk_header[0..4];
        let chunk_size = read_u64_be(&chunk_header, 4);

        match chunk_id {
            b"PROP" => {
                // Property chunk: read the "SND " sub-type, then parse sub-chunks
                let mut prop_type = [0u8; 4];
                file.read_exact(&mut prop_type)
                    .map_err(|e| format!("dff read PROP type: {e}"))?;

                if &prop_type != b"SND " {
                    // Skip non-SND property chunks
                    let skip = chunk_size.saturating_sub(4);
                    file.seek(SeekFrom::Current(skip as i64))
                        .map_err(|e| format!("dff skip PROP: {e}"))?;
                    continue;
                }

                // Parse sub-chunks within PROP/SND
                let prop_end = pos + 12 + chunk_size;
                while file
                    .stream_position()
                    .map_err(|e| format!("dff pos: {e}"))?
                    < prop_end
                {
                    let mut sub_header = [0u8; 12];
                    if file.read_exact(&mut sub_header).is_err() {
                        break;
                    }

                    let sub_id = [sub_header[0], sub_header[1], sub_header[2], sub_header[3]];
                    let sub_size = read_u64_be(&sub_header, 4);

                    match &sub_id {
                        b"FS  " => {
                            let mut fs_buf = [0u8; 4];
                            file.read_exact(&mut fs_buf)
                                .map_err(|e| format!("dff read FS: {e}"))?;
                            sample_rate = Some(read_u32_be(&fs_buf, 0));
                            // Skip any remaining bytes in this sub-chunk
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                file.seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip FS extra: {e}"))?;
                            }
                        }
                        b"CHNL" => {
                            let mut chnl_buf = [0u8; 2];
                            file.read_exact(&mut chnl_buf)
                                .map_err(|e| format!("dff read CHNL: {e}"))?;
                            channels = Some(read_u16_be(&chnl_buf, 0) as u32);
                            // Skip channel ID bytes
                            let skip = sub_size.saturating_sub(2);
                            if skip > 0 {
                                file.seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip CHNL ids: {e}"))?;
                            }
                        }
                        b"CMPR" => {
                            let mut cmpr_buf = [0u8; 4];
                            file.read_exact(&mut cmpr_buf)
                                .map_err(|e| format!("dff read CMPR: {e}"))?;
                            compression = Some(String::from_utf8_lossy(&cmpr_buf).to_string());
                            // Skip any remaining bytes (e.g. compression name string)
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                file.seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip CMPR extra: {e}"))?;
                            }
                        }
                        _ => {
                            // Skip unknown sub-chunk
                            // Pad to even boundary (IFF rule)
                            let padded = (sub_size + 1) & !1;
                            file.seek(SeekFrom::Current(padded as i64))
                                .map_err(|e| format!("dff skip sub-chunk: {e}"))?;
                        }
                    }
                }
            }
            b"DSD " => {
                // DSD Sound Data chunk — the actual audio samples
                data_offset = Some(pos + 12); // data starts right after the chunk header
                data_size = Some(chunk_size);
                // Don't need to read past this for header parsing
                break;
            }
            b"DST " => {
                // Enveloppe DST. On ne lit que FRTE : il donne la durée exacte,
                // que la taille compressée ne permet pas de déduire. Les trames
                // DSTF restent où elles sont, `data_offset` pointant sur le
                // début de l'enveloppe pour qui saura les décoder un jour.
                //
                // L'ID du chunk sonore fait FOI sur la nature des octets ; CMPR
                // n'en est qu'une DÉCLARATION, et elle peut manquer ou mentir.
                // Sans cette ligne, un DSDIFF dont le CMPR est absent retombait
                // sur le défaut `"DSD "` (plus bas), `is_dst()` répondait faux,
                // le garde de `DffStreamReader::open` laissait passer, et
                // `data_offset` — qui pointe ici sur l'ENVELOPPE — envoyait des
                // en-têtes ASCII et des trames à codage arithmétique dans
                // `DsdToPcmStreamer`/`DsdToDoP` : du bruit blanc pleine échelle
                // vers l'ampli. En cas de désaccord entre le chunk et CMPR, on
                // tranche donc toujours du côté qui REFUSE de lire.
                compression = Some("DST ".to_string());
                let dst_end = pos + 12 + chunk_size;
                while file
                    .stream_position()
                    .map_err(|e| format!("dff pos: {e}"))?
                    < dst_end
                {
                    let mut sub_header = [0u8; 12];
                    if file.read_exact(&mut sub_header).is_err() {
                        break;
                    }
                    let sub_id = [sub_header[0], sub_header[1], sub_header[2], sub_header[3]];
                    let sub_size = read_u64_be(&sub_header, 4);

                    if &sub_id == b"FRTE" {
                        let mut frte = [0u8; 6];
                        file.read_exact(&mut frte)
                            .map_err(|e| format!("dff read FRTE: {e}"))?;
                        dst_frames = Some(read_u32_be(&frte, 0));
                        dst_frame_rate = Some(read_u16_be(&frte, 4));
                        break; // seul FRTE nous intéresse dans l'en-tête
                    }

                    let padded = (sub_size + 1) & !1;
                    file.seek(SeekFrom::Current(padded as i64))
                        .map_err(|e| format!("dff skip DST sub-chunk: {e}"))?;
                }
                data_offset = Some(pos + 12);
                data_size = Some(chunk_size);
                break;
            }
            _ => {
                // Skip unknown chunk (pad to even boundary per IFF spec)
                let padded = (chunk_size + 1) & !1;
                file.seek(SeekFrom::Current(padded as i64))
                    .map_err(|e| format!("dff skip chunk: {e}"))?;
            }
        }
    }

    let sample_rate = sample_rate.ok_or("DFF: missing FS (sample rate) sub-chunk")?;
    let channels = channels.ok_or("DFF: missing CHNL (channels) sub-chunk")?;
    let compression = compression.unwrap_or_else(|| "DSD ".into());
    let data_offset = data_offset.ok_or("DFF: missing DSD/DST sound data chunk")?;
    let data_size = data_size.ok_or("DFF: missing DSD/DST sound data chunk size")?;

    if channels == 0 || channels > 8 {
        return Err(format!("invalid channel count: {channels}"));
    }
    if sample_rate < 2_000_000 || sample_rate > 50_000_000 {
        return Err(format!("unexpected DSD sample rate: {sample_rate}"));
    }

    Ok(DffInfo {
        channels,
        sample_rate,
        compression,
        data_offset,
        data_size,
        dst_frames,
        dst_frame_rate,
    })
}

/// Read all DSD sample data from a DFF file.
///
/// DFF stores data interleaved by sample (MSB first per byte):
/// byte layout is already ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...
/// so no de-interleaving is needed — just read the raw bytes.
pub fn read_dff_data(path: &str, info: &DffInfo) -> Result<Vec<u8>, String> {
    info.ensure_raw_dsd()?;

    let mut file = File::open(path).map_err(|e| format!("dff open: {e}"))?;
    file.seek(SeekFrom::Start(info.data_offset))
        .map_err(|e| format!("dff seek: {e}"))?;

    let mut data = vec![0u8; info.data_size as usize];
    file.read_exact(&mut data)
        .map_err(|e| format!("dff read data: {e}"))?;

    Ok(data)
}

/// Streaming DFF reader that yields DSD data in fixed-size chunks.
///
/// DFF data is already byte-interleaved, so no de-interleaving is needed.
/// This reader simply reads the data section in fixed-size chunks to avoid
/// loading the entire file into memory.
///
/// Memory usage: O(chunk_size), typically 32 KB per call.
pub struct DffStreamReader {
    file: File,
    remaining: usize,
    chunk_buf: Vec<u8>,
    data_offset: u64,
    data_size: usize,
}

impl DffStreamReader {
    /// Open a DFF file for streaming reading.
    ///
    /// `read_chunk_size`: how many bytes to read per `next_chunk()` call.
    /// Must be a multiple of `channels` to maintain byte alignment.
    pub fn open(path: &str, info: &DffInfo, read_chunk_size: usize) -> Result<Self, String> {
        // Seul point de passage vers `DsdToPcmStreamer` et `DsdToDoP` pour un
        // DFF (decode.rs:1971, 2103, 2252) : c'est ici que le refus doit être
        // à la fois SÛR et EXPLICABLE.
        info.ensure_raw_dsd()?;

        let mut file = File::open(path).map_err(|e| format!("dff open: {e}"))?;
        file.seek(SeekFrom::Start(info.data_offset))
            .map_err(|e| format!("dff seek: {e}"))?;

        Ok(DffStreamReader {
            file,
            remaining: info.data_size as usize,
            chunk_buf: vec![0u8; read_chunk_size],
            data_offset: info.data_offset,
            data_size: info.data_size as usize,
        })
    }

    /// Seek to a `channels`-aligned interleaved byte offset. DFF stores DSD as
    /// interleaved bytes (LR LR …), so aligning to a channel boundary keeps the
    /// bit stream phased. Returns the byte offset actually reached. Enables DSD
    /// seek in the streaming path (previously seek restarted at 0 — Xavier).
    pub fn seek_to_interleaved_byte(
        &mut self,
        target: usize,
        channels: usize,
    ) -> Result<usize, String> {
        let aligned = (target / channels * channels).min(self.data_size);
        self.file
            .seek(SeekFrom::Start(self.data_offset + aligned as u64))
            .map_err(|e| format!("dff seek: {e}"))?;
        self.remaining = self.data_size - aligned;
        Ok(aligned)
    }

    /// Read the next chunk of byte-interleaved DSD data.
    ///
    /// Returns `Ok(Some(chunk))` or `Ok(None)` at EOF.
    pub fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        if self.remaining == 0 {
            return Ok(None);
        }

        let to_read = self.chunk_buf.len().min(self.remaining);
        let buf = &mut self.chunk_buf[..to_read];
        self.file
            .read_exact(buf)
            .map_err(|e| format!("dff read chunk: {e}"))?;
        self.remaining -= to_read;

        Ok(Some(buf.to_vec()))
    }
}

/// Parse DFF header from an in-memory buffer (for testing).
pub fn parse_dff_from_bytes(data: &[u8]) -> Result<DffInfo, String> {
    use std::io::Cursor;

    if data.len() < 16 {
        return Err("buffer too small for DFF header".into());
    }

    // Use a cursor to simulate file I/O
    let mut cursor = Cursor::new(data);

    // FRM8 header
    let mut frm8 = [0u8; 12];
    cursor
        .read_exact(&mut frm8)
        .map_err(|e| format!("dff read FRM8: {e}"))?;

    if &frm8[0..4] != b"FRM8" {
        return Err("not a DFF file: missing 'FRM8' magic".into());
    }

    let mut form_type = [0u8; 4];
    cursor
        .read_exact(&mut form_type)
        .map_err(|e| format!("dff read form type: {e}"))?;

    if &form_type != b"DSD " {
        return Err("not a DFF/DSD file".into());
    }

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u32> = None;
    let mut compression: Option<String> = None;
    let mut dst_frames: Option<u32> = None;
    let mut dst_frame_rate: Option<u16> = None;
    let mut data_offset: Option<u64> = None;
    let mut data_size: Option<u64> = None;

    loop {
        let pos = cursor
            .stream_position()
            .map_err(|e| format!("dff pos: {e}"))?;

        let mut chunk_header = [0u8; 12];
        if cursor.read_exact(&mut chunk_header).is_err() {
            break;
        }

        let chunk_id = [
            chunk_header[0],
            chunk_header[1],
            chunk_header[2],
            chunk_header[3],
        ];
        let chunk_size = read_u64_be(&chunk_header, 4);

        match &chunk_id {
            b"PROP" => {
                let mut prop_type = [0u8; 4];
                cursor
                    .read_exact(&mut prop_type)
                    .map_err(|e| format!("dff read PROP type: {e}"))?;

                if &prop_type != b"SND " {
                    let skip = chunk_size.saturating_sub(4);
                    cursor
                        .seek(SeekFrom::Current(skip as i64))
                        .map_err(|e| format!("dff skip: {e}"))?;
                    continue;
                }

                let prop_end = pos + 12 + chunk_size;
                while cursor
                    .stream_position()
                    .map_err(|e| format!("dff pos: {e}"))?
                    < prop_end
                {
                    let mut sub_header = [0u8; 12];
                    if cursor.read_exact(&mut sub_header).is_err() {
                        break;
                    }

                    let sub_id = [sub_header[0], sub_header[1], sub_header[2], sub_header[3]];
                    let sub_size = read_u64_be(&sub_header, 4);

                    match &sub_id {
                        b"FS  " => {
                            let mut fs_buf = [0u8; 4];
                            cursor
                                .read_exact(&mut fs_buf)
                                .map_err(|e| format!("dff read FS: {e}"))?;
                            sample_rate = Some(read_u32_be(&fs_buf, 0));
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                cursor
                                    .seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip: {e}"))?;
                            }
                        }
                        b"CHNL" => {
                            let mut chnl_buf = [0u8; 2];
                            cursor
                                .read_exact(&mut chnl_buf)
                                .map_err(|e| format!("dff read CHNL: {e}"))?;
                            channels = Some(read_u16_be(&chnl_buf, 0) as u32);
                            let skip = sub_size.saturating_sub(2);
                            if skip > 0 {
                                cursor
                                    .seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip: {e}"))?;
                            }
                        }
                        b"CMPR" => {
                            let mut cmpr_buf = [0u8; 4];
                            cursor
                                .read_exact(&mut cmpr_buf)
                                .map_err(|e| format!("dff read CMPR: {e}"))?;
                            compression = Some(String::from_utf8_lossy(&cmpr_buf).to_string());
                            let skip = sub_size.saturating_sub(4);
                            if skip > 0 {
                                cursor
                                    .seek(SeekFrom::Current(skip as i64))
                                    .map_err(|e| format!("dff skip: {e}"))?;
                            }
                        }
                        _ => {
                            let padded = (sub_size + 1) & !1;
                            cursor
                                .seek(SeekFrom::Current(padded as i64))
                                .map_err(|e| format!("dff skip: {e}"))?;
                        }
                    }
                }
            }
            b"DSD " => {
                data_offset = Some(pos + 12);
                data_size = Some(chunk_size);
                break;
            }
            b"DST " => {
                // Même lecture que dans `parse_dff` : seul FRTE est requis ici,
                // et l'ID du chunk prime sur CMPR pour la même raison de
                // sécurité (cf. le commentaire de `parse_dff`).
                compression = Some("DST ".to_string());
                let dst_end = pos + 12 + chunk_size;
                while cursor
                    .stream_position()
                    .map_err(|e| format!("dff pos: {e}"))?
                    < dst_end
                {
                    let mut sub_header = [0u8; 12];
                    if cursor.read_exact(&mut sub_header).is_err() {
                        break;
                    }
                    let sub_id = [sub_header[0], sub_header[1], sub_header[2], sub_header[3]];
                    let sub_size = read_u64_be(&sub_header, 4);

                    if &sub_id == b"FRTE" {
                        let mut frte = [0u8; 6];
                        cursor
                            .read_exact(&mut frte)
                            .map_err(|e| format!("dff read FRTE: {e}"))?;
                        dst_frames = Some(read_u32_be(&frte, 0));
                        dst_frame_rate = Some(read_u16_be(&frte, 4));
                        break;
                    }

                    let padded = (sub_size + 1) & !1;
                    cursor
                        .seek(SeekFrom::Current(padded as i64))
                        .map_err(|e| format!("dff skip: {e}"))?;
                }
                data_offset = Some(pos + 12);
                data_size = Some(chunk_size);
                break;
            }
            _ => {
                let padded = (chunk_size + 1) & !1;
                cursor
                    .seek(SeekFrom::Current(padded as i64))
                    .map_err(|e| format!("dff skip: {e}"))?;
            }
        }
    }

    let sample_rate = sample_rate.ok_or("DFF: missing FS sub-chunk")?;
    let channels = channels.ok_or("DFF: missing CHNL sub-chunk")?;
    let compression = compression.unwrap_or_else(|| "DSD ".into());
    let data_offset = data_offset.ok_or("DFF: missing DSD/DST data chunk")?;
    let data_size = data_size.ok_or("DFF: missing DSD/DST data chunk")?;

    Ok(DffInfo {
        channels,
        sample_rate,
        compression,
        data_offset,
        data_size,
        dst_frames,
        dst_frame_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid DFF header in memory.
    /// Fichier DSDIFF compressé DST : même en-tête, mais CMPR = "DST " et un
    /// chunk "DST " contenant FRTE puis des trames DSTF.
    fn build_dff_dst(channels: u16, sample_rate: u32, frames: u32, frame_rate: u16) -> Vec<u8> {
        build_dff_dst_opt_cmpr(channels, sample_rate, frames, frame_rate, true)
    }

    /// Même fichier, mais le sous-chunk CMPR peut être OMIS. Les octets audio
    /// restent exactement les mêmes — seule la DÉCLARATION disparaît. C'est le
    /// cas qui décide si Tune se fie au chunk sonore ou à une étiquette.
    fn build_dff_dst_opt_cmpr(
        channels: u16,
        sample_rate: u32,
        frames: u32,
        frame_rate: u16,
        with_cmpr: bool,
    ) -> Vec<u8> {
        let mut prop = Vec::new();
        prop.extend_from_slice(b"SND ");
        prop.extend_from_slice(b"FS  ");
        prop.extend_from_slice(&4u64.to_be_bytes());
        prop.extend_from_slice(&sample_rate.to_be_bytes());
        prop.extend_from_slice(b"CHNL");
        prop.extend_from_slice(&2u64.to_be_bytes());
        prop.extend_from_slice(&channels.to_be_bytes());
        if with_cmpr {
            prop.extend_from_slice(b"CMPR");
            prop.extend_from_slice(&4u64.to_be_bytes());
            prop.extend_from_slice(b"DST ");
        }

        // Contenu du chunk DST : FRTE, puis deux trames factices. Leur contenu
        // n'a pas à être décodable : on vérifie l'enveloppe, pas le codec.
        let mut dst = Vec::new();
        dst.extend_from_slice(b"FRTE");
        dst.extend_from_slice(&6u64.to_be_bytes());
        dst.extend_from_slice(&frames.to_be_bytes());
        dst.extend_from_slice(&frame_rate.to_be_bytes());
        for _ in 0..2 {
            dst.extend_from_slice(b"DSTF");
            dst.extend_from_slice(&8u64.to_be_bytes());
            dst.extend_from_slice(&[0xAAu8; 8]);
        }

        let frm8_content = 4 + 12 + prop.len() as u64 + 12 + dst.len() as u64;

        let mut buf = Vec::new();
        buf.extend_from_slice(b"FRM8");
        buf.extend_from_slice(&frm8_content.to_be_bytes());
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(b"PROP");
        buf.extend_from_slice(&(prop.len() as u64).to_be_bytes());
        buf.extend_from_slice(&prop);
        buf.extend_from_slice(b"DST ");
        buf.extend_from_slice(&(dst.len() as u64).to_be_bytes());
        buf.extend_from_slice(&dst);
        buf
    }

    /// Écrit un buffer dans un vrai fichier temporaire : `DffStreamReader::open`
    /// et `read_dff_data` prennent un CHEMIN, pas un buffer, et ce sont eux le
    /// chemin réellement emprunté à la lecture. `tempfile` donne un nom unique
    /// par exécution et le supprime — deux agents ne peuvent pas se marcher
    /// dessus, contrairement à un chemin fixe sous /tmp.
    fn write_tmp_dff(bytes: &[u8]) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut tmp = tempfile::Builder::new()
            .prefix("i1387-dff-")
            .suffix(".dff")
            .tempfile()
            .unwrap();
        tmp.write_all(bytes).unwrap();
        tmp.flush().unwrap();
        tmp
    }

    /// ⚠️ LE CAS QUI ENVOYAIT DU BRUIT À L'AMPLI.
    ///
    /// Un DSDIFF dont les données vivent dans un chunk `DST ` mais dont le
    /// sous-chunk CMPR est absent. Avant le correctif, `compression` retombait
    /// sur le défaut `"DSD "`, `is_dst()` répondait faux, le garde laissait
    /// passer, et `data_offset` — qui pointe sur l'ENVELOPPE DST — envoyait
    /// « FRTE », « DSTF » et des trames à codage arithmétique dans le
    /// convertisseur DSD→PCM/DoP. Contre-épreuve : retirer la ligne
    /// `compression = Some("DST ")` de la branche `b"DST "` rend ce test ROUGE.
    #[test]
    fn dst_without_cmpr_is_still_recognised_and_refused() {
        let bytes = build_dff_dst_opt_cmpr(2, 2_822_400, 4500, 75, false);
        let info = parse_dff_from_bytes(&bytes).unwrap();

        assert!(
            info.is_dst(),
            "le chunk sonore est 'DST ' : l'absence de CMPR ne doit pas le faire passer pour du DSD brut"
        );
        assert!(
            info.ensure_raw_dsd().is_err(),
            "des trames DST ne doivent jamais atteindre le convertisseur"
        );

        // Et le refus doit tenir sur le chemin RÉEL, pas seulement en mémoire.
        let tmp = write_tmp_dff(&bytes);
        let path = tmp.path().to_str().unwrap();
        let info = parse_dff(path).unwrap();
        assert!(info.is_dst());
        assert!(DffStreamReader::open(path, &info, 4096).is_err());
    }

    /// Le refus doit ÊTRE COMPRÉHENSIBLE. Il remonte tel quel à l'utilisateur
    /// via `zone.playback_error` (« Impossible de décoder la piste : … »).
    /// Contre-épreuve : sans l'appel à `ensure_raw_dsd` dans
    /// `DffStreamReader::open`, le message redevient « unsupported compression
    /// 'DST ' » — sans le mot DST en clair, sans la durée, sans remède — et ce
    /// test passe au ROUGE.
    #[test]
    fn dst_refusal_names_the_format_and_the_remedy() {
        let bytes = build_dff_dst(2, 2_822_400, 4500, 75);
        let tmp = write_tmp_dff(&bytes);
        let path = tmp.path().to_str().unwrap();
        let info = parse_dff(path).unwrap();

        // `.err().unwrap()` et non `unwrap_err()` : `DffStreamReader` n'est pas
        // `Debug`, et le rendre `Debug` pour un test serait la queue qui remue
        // le chien.
        let err = DffStreamReader::open(path, &info, 4096)
            .err()
            .expect("un DFF/DST doit être refusé");
        assert!(err.contains("DST"), "le format doit être nommé : {err}");
        assert!(
            err.contains("4500") && err.contains("75"),
            "le fichier est lisible et sa durée connue, il faut le dire : {err}"
        );
        assert!(
            err.contains("convert"),
            "un refus sans remède n'aide personne : {err}"
        );
        // `playback.rs` rend un 502 (contenu illisible) au lieu d'un 500
        // (panne de Tune) sur les erreurs contenant « decode ».
        assert!(
            err.contains("decode"),
            "doit être classé 502, pas 500 : {err}"
        );
    }

    /// GARDE ANTI-RÉGRESSION — doit rester VERT avant comme après.
    /// Un refus trop large qui bloquerait les DSD non compressés serait pire
    /// que le défaut d'origine : c'est ce que Marco Polo écoute aujourd'hui.
    #[test]
    fn uncompressed_dsd_still_opens_and_reads() {
        let dsd = vec![0x69u8; 4096];
        let bytes = build_dff_header(2, 2_822_400, &dsd);
        let tmp = write_tmp_dff(&bytes);
        let path = tmp.path().to_str().unwrap();

        let info = parse_dff(path).unwrap();
        assert!(!info.is_dst());
        assert!(info.ensure_raw_dsd().is_ok());

        let mut reader = DffStreamReader::open(path, &info, 1024)
            .expect("un DFF non compressé doit continuer à s'ouvrir");
        let mut total = Vec::new();
        while let Some(chunk) = reader.next_chunk().unwrap() {
            total.extend_from_slice(&chunk);
        }
        assert_eq!(total, dsd, "les octets DSD doivent sortir intacts");

        assert_eq!(read_dff_data(path, &info).unwrap(), dsd);
    }

    #[test]
    fn parse_dst_reads_frame_table() {
        // 75 trames/s est la cadence d'un SACD ; 4500 trames = 60 s.
        let info = parse_dff_from_bytes(&build_dff_dst(2, 2_822_400, 4500, 75)).unwrap();
        assert_eq!(info.compression, "DST ");
        assert!(info.is_dst());
        assert_eq!(info.dst_frames, Some(4500));
        assert_eq!(info.dst_frame_rate, Some(75));
    }

    #[test]
    fn dst_duration_comes_from_frames_not_compressed_size() {
        // Le piège : `data_size` est ici la taille COMPRESSÉE (quelques dizaines
        // d'octets). La formule bit-à-bit rendrait une durée quasi nulle ; FRTE
        // donne la vraie, 60 s.
        let info = parse_dff_from_bytes(&build_dff_dst(2, 2_822_400, 4500, 75)).unwrap();
        assert_eq!(info.duration_ms(), Some(60_000));
        assert!(
            info.data_size < 1000,
            "l'enveloppe de test est bien plus petite qu'une minute de DSD"
        );
    }

    #[test]
    fn dsd_duration_unaffected_by_dst_path() {
        // Non-régression : un fichier non compressé garde le calcul d'origine.
        let info =
            parse_dff_from_bytes(&build_dff_header(2, 2_822_400, &vec![0u8; 705_600])).unwrap();
        assert_eq!(info.dst_frames, None);
        assert_eq!(info.duration_ms(), Some(1000));
    }

    fn build_dff_header(channels: u16, sample_rate: u32, dsd_data: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();

        // --- Build PROP chunk content first to know its size ---
        let mut prop_content = Vec::new();
        prop_content.extend_from_slice(b"SND "); // 4 bytes

        // FS sub-chunk: "FS  " + size(8) + sample_rate(4)
        prop_content.extend_from_slice(b"FS  ");
        prop_content.extend_from_slice(&4u64.to_be_bytes());
        prop_content.extend_from_slice(&sample_rate.to_be_bytes());

        // CHNL sub-chunk: "CHNL" + size(8) + channel_count(2)
        prop_content.extend_from_slice(b"CHNL");
        let chnl_data_size = 2u64; // just the count, no channel IDs for simplicity
        prop_content.extend_from_slice(&chnl_data_size.to_be_bytes());
        prop_content.extend_from_slice(&channels.to_be_bytes());

        // CMPR sub-chunk: "CMPR" + size(8) + "DSD "(4)
        prop_content.extend_from_slice(b"CMPR");
        prop_content.extend_from_slice(&4u64.to_be_bytes());
        prop_content.extend_from_slice(b"DSD ");

        let prop_chunk_size = prop_content.len() as u64;

        // DSD data chunk size
        let dsd_chunk_size = dsd_data.len() as u64;

        // Total FRM8 content size: 4 (form type "DSD ") + 12 (PROP header) + prop_content
        //                          + 12 (DSD header) + dsd_data
        let frm8_content_size = 4 + 12 + prop_chunk_size + 12 + dsd_chunk_size;

        // --- FRM8 header ---
        buf.extend_from_slice(b"FRM8");
        buf.extend_from_slice(&frm8_content_size.to_be_bytes());
        buf.extend_from_slice(b"DSD "); // form type

        // --- PROP chunk ---
        buf.extend_from_slice(b"PROP");
        buf.extend_from_slice(&prop_chunk_size.to_be_bytes());
        buf.extend_from_slice(&prop_content);

        // --- DSD Sound Data chunk ---
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&dsd_chunk_size.to_be_bytes());
        buf.extend_from_slice(dsd_data);

        buf
    }

    #[test]
    fn parse_valid_dff_header() {
        let dsd_data = vec![0u8; 4096];
        let buf = build_dff_header(2, 2_822_400, &dsd_data);

        let info = parse_dff_from_bytes(&buf).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 2_822_400);
        assert_eq!(info.compression, "DSD ");
        assert_eq!(info.data_size, 4096);
    }

    #[test]
    fn dff_info_duration_ms_from_header() {
        // Exactly 1 s of stereo DSD64: data_size = channels * (samples/ch) / 8
        //                            = 2 * 2_822_400 / 8 = 705_600 bytes.
        // Without this the scan left DFF tracks at duration_ms = 0.
        let dsd_data = vec![0u8; 705_600];
        let info = parse_dff_from_bytes(&build_dff_header(2, 2_822_400, &dsd_data)).unwrap();
        assert_eq!(info.duration_ms(), Some(1000));
    }

    #[test]
    fn parse_dff_dsd128() {
        let dsd_data = vec![0u8; 8192];
        let buf = build_dff_header(2, 5_644_800, &dsd_data);

        let info = parse_dff_from_bytes(&buf).unwrap();
        assert_eq!(info.sample_rate, 5_644_800);
        assert_eq!(info.channels, 2);
    }

    #[test]
    fn parse_dff_mono() {
        let dsd_data = vec![0u8; 2048];
        let buf = build_dff_header(1, 2_822_400, &dsd_data);

        let info = parse_dff_from_bytes(&buf).unwrap();
        assert_eq!(info.channels, 1);
    }

    #[test]
    fn parse_dff_bad_magic() {
        let mut buf = build_dff_header(2, 2_822_400, &[0u8; 1024]);
        buf[0] = b'X'; // corrupt "FRM8"
        let result = parse_dff_from_bytes(&buf);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("FRM8"));
    }

    #[test]
    fn parse_dff_bad_form_type() {
        let mut buf = build_dff_header(2, 2_822_400, &[0u8; 1024]);
        buf[12] = b'X'; // corrupt "DSD " form type
        let result = parse_dff_from_bytes(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn parse_dff_too_short() {
        let result = parse_dff_from_bytes(&[0u8; 10]);
        assert!(result.is_err());
    }
}
