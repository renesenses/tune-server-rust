//! Seekable encrypted CDN source. No full-track buffer, disk cache or redirects.
use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;
use symphonia::core::io::MediaSource;

const IV: [u8; 16] = [
    0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93,
];
const WINDOW: u64 = 128 * 1024;
const MAX_FILE: u64 = 4 * 1024 * 1024 * 1024;

pub(super) struct RangeSource {
    client: reqwest::blocking::Client,
    url: reqwest::Url,
    length: u64,
    position: u64,
    window: io::Cursor<Vec<u8>>,
}

fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, length) = value.strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, length) = (start.parse().ok()?, end.parse().ok()?, length.parse().ok()?);
    (start <= end && end < length && length <= MAX_FILE).then_some((start, end, length))
}

impl RangeSource {
    pub(super) fn open(raw_url: &str) -> Result<Self, String> {
        let url = super::protocol::cdn_url(raw_url)?;
        Self::open_checked_url(url)
    }

    fn open_checked_url(url: reqwest::Url) -> Result<Self, String> {
        let client = crate::http::client::blocking_builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| "Spotify CDN client initialization failed")?;
        let mut source = Self {
            client,
            url,
            length: 0,
            position: 0,
            window: io::Cursor::new(Vec::new()),
        };
        // Small bounded probe before decoding. No account authorization on CDN.
        source.fetch(4096)?;
        Ok(source)
    }

    fn fetch(&mut self, size: u64) -> Result<(), String> {
        let end = (self.position + size - 1).min(if self.length == 0 {
            MAX_FILE - 1
        } else {
            self.length - 1
        });
        let mut response = self
            .client
            .get(self.url.clone())
            .header("Accept-Encoding", "identity")
            .header("Range", format!("bytes={}-{}", self.position, end))
            .send()
            .map_err(|_| "Spotify CDN request failed")?;
        let status = response.status().as_u16();
        if status != 206 {
            return Err(format!("Spotify CDN refused range (HTTP {status})"));
        }
        if response
            .headers()
            .get("Content-Encoding")
            .is_some_and(|h| h != "identity")
        {
            return Err("Spotify CDN transformed encrypted bytes".into());
        }
        let (start, actual_end, length) = response
            .headers()
            .get("Content-Range")
            .and_then(|h| h.to_str().ok())
            .and_then(content_range)
            .ok_or("Spotify CDN range is invalid")?;
        if start != self.position
            || actual_end != end.min(length - 1)
            || (self.length != 0 && self.length != length)
        {
            return Err("Spotify CDN range does not match the requested file window".into());
        }
        let expected = actual_end - start + 1;
        if response.content_length().is_some_and(|n| n != expected) {
            return Err("Spotify CDN range length is inconsistent".into());
        }
        let mut bytes = Vec::with_capacity(expected as usize);
        response
            .by_ref()
            .take(expected + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Spotify CDN body failed")?;
        if bytes.len() as u64 != expected {
            return Err("Spotify CDN body is truncated or oversized".into());
        }
        self.length = length;
        self.window = io::Cursor::new(bytes);
        Ok(())
    }
}

impl Read for RangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.position == self.length {
            return Ok(0);
        }
        if self.window.position() == self.window.get_ref().len() as u64 {
            self.fetch(WINDOW).map_err(io::Error::other)?;
        }
        let size = self.window.read(buf)?;
        self.position += size as u64;
        Ok(size)
    }
}

impl Seek for RangeSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = match from {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(n) => i128::from(self.position) + i128::from(n),
            SeekFrom::End(n) => i128::from(self.length) + i128::from(n),
        };
        if position < 0 || position > i128::from(self.length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Spotify seek is outside the file",
            ));
        }
        let position = position as u64;
        let window_start = self.position - self.window.position();
        let window_end = window_start + self.window.get_ref().len() as u64;
        if (window_start..=window_end).contains(&position) {
            self.window.set_position(position - window_start);
        } else {
            self.window = io::Cursor::new(Vec::new());
        }
        self.position = position;
        Ok(position)
    }
}

impl MediaSource for RangeSource {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.length)
    }
}

pub(super) struct DecryptSource<R> {
    inner: R,
    cipher: ctr::Ctr128BE<Aes128>,
}

impl<R> DecryptSource<R> {
    pub(super) fn new(inner: R, key: &[u8; 16]) -> Self {
        Self {
            inner,
            cipher: ctr::Ctr128BE::new(key.into(), (&IV).into()),
        }
    }
}
impl<R: Read> Read for DecryptSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.inner.read(buf)?;
        self.cipher
            .try_apply_keystream(&mut buf[..size])
            .map_err(|_| io::Error::other("Spotify cipher position overflow"))?;
        Ok(size)
    }
}
impl<R: Seek> Seek for DecryptSource<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let position = self.inner.seek(from)?;
        self.cipher
            .try_seek(position)
            .map_err(|_| io::Error::other("Spotify cipher seek overflow"))?;
        Ok(position)
    }
}
impl<R: MediaSource> MediaSource for DecryptSource<R> {
    fn is_seekable(&self) -> bool {
        self.inner.is_seekable()
    }
    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    fn read_request(socket: &mut std::net::TcpStream) -> String {
        let mut reader = BufReader::new(socket);
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
            assert!(!line.is_empty());
            request.push_str(&line);
        }
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        assert!(!request.to_ascii_lowercase().contains("client-token:"));
        request
    }

    #[test]
    fn native_lossless_real_http_ranges_decrypt_and_seek_without_full_download() {
        let plain: Vec<u8> = (0..200003).map(|n| (n % 251) as u8).collect();
        let mut encrypted = plain.clone();
        let key = [0x39; 16];
        ctr::Ctr128BE::<Aes128>::new((&key).into(), (&IV).into()).apply_keystream(&mut encrypted);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!(
            "http://{}/synthetic?signature=test",
            listener.local_addr().unwrap()
        );
        let server = std::thread::spawn(move || {
            let mut ranges = Vec::new();
            for _ in 0..4 {
                let (mut socket, _) = listener.accept().unwrap();
                let request = read_request(&mut socket).to_ascii_lowercase();
                let range = request
                    .lines()
                    .find_map(|line| line.strip_prefix("range: bytes="))
                    .unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
                assert!(end - start + 1 <= WINDOW as usize);
                ranges.push((start, end));
                write!(socket, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", encrypted.len(), end-start+1).unwrap();
                socket.write_all(&encrypted[start..=end]).unwrap();
            }
            ranges
        });
        // Only tests may bypass the HTTPS/host gate, to use an in-process server.
        let raw = RangeSource::open_checked_url(reqwest::Url::parse(&url).unwrap()).unwrap();
        let mut source = DecryptSource::new(raw, &key);
        let mut prefix = vec![0; 5000];
        source.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, plain[..5000]);
        for position in [180123, 15] {
            source.seek(SeekFrom::Start(position)).unwrap();
            let mut bytes = [0; 41];
            source.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, &plain[position as usize..position as usize + 41]);
        }
        assert_eq!(
            server.join().unwrap(),
            [(0, 4095), (4096, 135167), (180123, 200002), (15, 131086)]
        );
    }

    #[test]
    fn native_lossless_http_refuses_redirects_full_bodies_bad_ranges_and_truncation() {
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/forbidden\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 1\r\n\r\nx",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-4095/5000\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4095/5000\r\nContent-Length: 4096\r\n\r\nx",
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4095/5000\r\nContent-Encoding: gzip\r\nContent-Length: 0\r\n\r\n",
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!(
                "http://{}/private?signature=do-not-log",
                listener.local_addr().unwrap()
            );
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                read_request(&mut socket);
                socket.write_all(response.as_bytes()).unwrap();
            });
            let error = RangeSource::open_checked_url(reqwest::Url::parse(&url).unwrap())
                .err()
                .expect("Invalid transport must be refused");
            assert!(!error.contains("do-not-log") && !error.contains("http://"));
            server.join().unwrap();
        }
    }
    #[test]
    fn native_lossless_ctr_seek_matches_bytes_for_unaligned_positions() {
        let key = [0x17; 16];
        let plain: Vec<u8> = (0..10007).map(|n| (n % 253) as u8).collect();
        let mut encrypted = plain.clone();
        ctr::Ctr128BE::<Aes128>::new((&key).into(), (&IV).into()).apply_keystream(&mut encrypted);
        let mut source = DecryptSource::new(io::Cursor::new(encrypted), &key);
        for pos in [0, 1, 15, 16, 17, 4001, 7999, 2] {
            source.seek(SeekFrom::Start(pos)).unwrap();
            let mut bytes = [0u8; 37];
            source.read_exact(&mut bytes).unwrap();
            assert_eq!(
                &bytes,
                &plain[pos as usize..pos as usize + 37],
                "CTR seek must use the absolute encrypted byte position"
            );
        }
    }
    #[test]
    fn native_lossless_range_parser_rejects_impossible_sizes() {
        assert_eq!(content_range("bytes 0-4095/20000"), Some((0, 4095, 20000)));
        for value in [
            "bytes 0-0/0",
            "bytes 1-0/3",
            "bytes 0-3/3",
            "bytes 0-2/*",
            "bytes 0-2/999999999999",
            "items 0-2/3",
        ] {
            assert!(content_range(value).is_none());
        }
    }
}
