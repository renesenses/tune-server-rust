//! Streaming decode of a still-downloading DASH temp file (#1146 Plan C step 2).
//!
//! The Tidal HI-RES DASH path assembles all fMP4 segments into one temp file,
//! THEN pre-transcodes it to FLAC — the download and the decode run one after
//! the other. fMP4 is forward-decodable (each `moof`+`mdat` fragment is
//! self-contained after the `moov` init segment), so the decode can start on the
//! init segment and consume fragments as they land, overlapping the ~10s download
//! under the ~20-30s transcode.
//!
//! This module provides the two halves that make that safe:
//! - [`DashGrowth`]: shared writer→reader progress (bytes available, done/failed)
//!   with a condvar so the reader blocks instead of hitting a premature EOF.
//! - [`GrowingFileSource`]: a symphonia [`MediaSource`] over the temp file whose
//!   `read` returns available bytes and *blocks* at the write frontier until more
//!   arrives (or the download finishes), rather than returning EOF early — which
//!   would silently truncate the track (`decode_symphonia` breaks at EOF).
//!
//! A process-global registry hands the growth handle from the downloader
//! (`tidal.rs`) to the decoder (`decode.rs`) keyed by temp path, WITHOUT changing
//! the `get_track_url`/`StreamUrl` contract. The orchestrator renames the temp to
//! `{path}.decoding` before decoding, so [`take_for`] strips that suffix. All of
//! this only engages behind `TUNE_DASH_STREAM_DECODE`; otherwise the registry is
//! empty and the decoder opens a plain `File` (byte-identical).

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use symphonia::core::io::MediaSource;

struct GrowthState {
    /// Bytes durably written to the temp file and safe to read.
    available: u64,
    /// The downloader finished (successfully or not); no more bytes will come.
    done: bool,
    /// The download failed — the reader should surface an error, not silent EOF.
    failed: bool,
}

/// Shared progress between the DASH downloader (writer) and the decoder (reader).
pub struct DashGrowth {
    inner: Mutex<GrowthState>,
    cv: Condvar,
}

impl DashGrowth {
    /// Create a handle with `initial` bytes already available (the init segment).
    pub fn new(initial: u64) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(GrowthState {
                available: initial,
                done: false,
                failed: false,
            }),
            cv: Condvar::new(),
        })
    }

    /// Publish that the temp file now holds at least `available` bytes.
    pub fn advance(&self, available: u64) {
        let mut g = self.inner.lock().unwrap();
        if available > g.available {
            g.available = available;
        }
        self.cv.notify_all();
    }

    /// The download completed — the current `available` is the final size.
    pub fn finish(&self) {
        let mut g = self.inner.lock().unwrap();
        g.done = true;
        self.cv.notify_all();
    }

    /// The download failed mid-flight — readers get an error at the frontier.
    pub fn fail(&self) {
        let mut g = self.inner.lock().unwrap();
        g.failed = true;
        g.done = true;
        self.cv.notify_all();
    }
}

/// A blocking [`MediaSource`] over a file another task is still appending to.
/// `read`/`seek` block at the write frontier until more bytes arrive or the
/// download finishes, so the forward-only fMP4 decode never sees a premature EOF.
pub struct GrowingFileSource {
    file: std::fs::File,
    pos: u64,
    growth: Arc<DashGrowth>,
}

impl GrowingFileSource {
    pub fn open(path: &str, growth: Arc<DashGrowth>) -> io::Result<Self> {
        Ok(Self {
            file: std::fs::File::open(path)?,
            pos: 0,
            growth,
        })
    }

    /// Block until `target` bytes are available or the download ends. Returns the
    /// current `available` (which may be < target only when `done`), or an error
    /// if the download failed before reaching `target`.
    fn wait_until(&self, target: u64) -> io::Result<u64> {
        let mut g = self.growth.inner.lock().unwrap();
        loop {
            if target <= g.available {
                return Ok(g.available);
            }
            if g.failed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "dash streaming download failed",
                ));
            }
            if g.done {
                return Ok(g.available); // final size reached, no more bytes
            }
            g = self.growth.cv.wait(g).unwrap();
        }
    }
}

impl Read for GrowingFileSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Block until at least one unread byte is available (or real EOF).
        let available = self.wait_until(self.pos + 1)?;
        if self.pos >= available {
            return Ok(0); // done and fully consumed
        }
        let to_read = ((available - self.pos) as usize).min(buf.len());
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(&mut buf[..to_read])?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for GrowingFileSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n,
            SeekFrom::Current(d) => (self.pos as i64 + d).max(0) as u64,
            // byte_len() is None and is_seekable() is false, so symphonia must not
            // seek from the end; refuse rather than guess.
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "no end-relative seek on a growing dash file",
                ));
            }
        };
        // Only forward within (or up to) what's been downloaded; block otherwise.
        self.wait_until(target)?;
        self.pos = target;
        Ok(self.pos)
    }
}

impl MediaSource for GrowingFileSource {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn registry() -> &'static Mutex<HashMap<String, Arc<DashGrowth>>> {
    static R: OnceLock<Mutex<HashMap<String, Arc<DashGrowth>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a growth handle for a DASH temp path (called by the downloader).
pub fn register(path: &str, growth: Arc<DashGrowth>) {
    registry().lock().unwrap().insert(path.to_string(), growth);
}

/// Take the growth handle for a decode path, tolerating the orchestrator's
/// `{path}.decoding` rename. Returns `None` (→ plain-`File` decode) when the flag
/// is off or the file isn't a streaming DASH temp.
pub fn take_for(decode_path: &str) -> Option<Arc<DashGrowth>> {
    let key = decode_path.strip_suffix(".decoding").unwrap_or(decode_path);
    registry().lock().unwrap().remove(key)
}

/// Whether streaming DASH decode is enabled (`TUNE_DASH_STREAM_DECODE`).
pub fn stream_decode_enabled() -> bool {
    std::env::var("TUNE_DASH_STREAM_DECODE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn growing_source_blocks_then_reads_all() {
        // Writer appends in two bursts; a reader thread must see every byte and
        // only hit EOF after finish(), never mid-stream.
        let fichier = crate::test_scratch::scratch_file("tune-growtest", ".bin");
        let path = fichier.to_string_lossy().to_string();
        std::fs::write(&path, b"AAAA").unwrap(); // 4 initial bytes
        let growth = DashGrowth::new(4);

        let g2 = growth.clone();
        let p2 = path.clone();
        let reader = std::thread::spawn(move || {
            let mut src = GrowingFileSource::open(&p2, g2).unwrap();
            let mut out = Vec::new();
            let mut buf = [0u8; 3];
            loop {
                let n = src.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            out
        });

        // Append a second burst and publish it, then finish.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"BBBBBB").unwrap();
            f.flush().unwrap();
        }
        growth.advance(10);
        growth.finish();

        let out = reader.join().unwrap();
        assert_eq!(out, b"AAAABBBBBB");
    }

    #[test]
    fn failed_download_surfaces_error_not_silent_eof() {
        let fichier = crate::test_scratch::scratch_file("tune-growtest-fail", ".bin");
        let path = fichier.to_string_lossy().to_string();
        std::fs::write(&path, b"AAAA").unwrap();
        let growth = DashGrowth::new(4);
        let mut src = GrowingFileSource::open(&path, growth.clone()).unwrap();
        // Consume the 4 available bytes.
        let mut buf = [0u8; 4];
        assert_eq!(src.read(&mut buf).unwrap(), 4);
        // Reader now at the frontier; a failed download must error, not EOF.
        growth.fail();
        assert!(src.read(&mut buf).is_err());
    }

    #[test]
    fn registry_take_strips_decoding_suffix() {
        let g = DashGrowth::new(1);
        register("/tmp/tune-dash-xyz.mp4", g);
        // Orchestrator renamed it to `.decoding` before decoding.
        assert!(take_for("/tmp/tune-dash-xyz.mp4.decoding").is_some());
        // Gone after take.
        assert!(take_for("/tmp/tune-dash-xyz.mp4.decoding").is_none());
    }
}
