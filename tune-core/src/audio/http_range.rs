//! Source HTTP seekable par requetes `Range`.
//!
//! Un M4A place souvent son atome `moov` a la fin du fichier. Le telecharger
//! sequentiellement dans un temporaire oblige donc a attendre le dernier octet
//! avant de pouvoir decoder la premiere note. Cette source donne a Symphonia un
//! vrai [`MediaSource`] seekable : chaque seek ferme la reponse courante et la
//! lecture suivante repart exactement a l'octet demande par HTTP Range.

use std::io::{self, Read, Seek, SeekFrom};

use reqwest::header::{ACCEPT_ENCODING, CONTENT_RANGE, RANGE};
use symphonia::core::io::MediaSource;

pub struct HttpRangeSource {
    client: reqwest::blocking::Client,
    url: String,
    len: u64,
    pos: u64,
    response: Option<reqwest::blocking::Response>,
}

impl HttpRangeSource {
    /// Sonde le support Range avec un seul octet. Un serveur qui ignore Range
    /// est refuse : le chemin appelant conserve alors son repli historique par
    /// telechargement complet.
    pub fn open(url: &str) -> Result<Self, String> {
        let client = crate::http::client::blocking_builder()
            .timeout(None)
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| format!("http range client: {e}"))?;
        let response = client
            .get(url)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, "bytes=0-0")
            .send()
            .map_err(|e| format!("http range probe: {}", describe_http_error(&e)))?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(format!(
                "http range unsupported: probe returned {}",
                response.status()
            ));
        }
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .ok_or("http range probe: Content-Range absent")?;
        let (start, _end, len) = parse_content_range(content_range)
            .ok_or_else(|| format!("http range probe: Content-Range invalide: {content_range}"))?;
        if start != 0 || len == 0 {
            return Err(format!(
                "http range probe incoherente: debut={start}, taille={len}"
            ));
        }

        Ok(Self {
            client,
            url: url.to_string(),
            len,
            pos: 0,
            // La reponse de sonde ne contient qu'un octet. La premiere vraie
            // lecture ouvre `bytes=0-` et peut donc avancer sans requete par
            // paquet.
            response: None,
        })
    }

    fn open_at_current_position(&mut self) -> io::Result<()> {
        if self.pos >= self.len {
            self.response = None;
            return Ok(());
        }
        let range = format!("bytes={}-", self.pos);
        let response = self
            .client
            .get(&self.url)
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, range)
            .send()
            .map_err(|e| io::Error::other(describe_http_error(&e)))?;
        if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(io::Error::other(format!(
                "range resume at {} returned {}",
                self.pos,
                response.status()
            )));
        }
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range)
            .ok_or_else(|| io::Error::other("range response has no valid Content-Range"))?;
        if content_range.0 != self.pos || content_range.2 != self.len {
            return Err(io::Error::other(format!(
                "range response mismatch: requested {}, got {}-{}/{}",
                self.pos, content_range.0, content_range.1, content_range.2
            )));
        }
        self.response = Some(response);
        Ok(())
    }
}

/// Ne jamais recopier l'URL signee du CDN depuis `reqwest::Error` dans les
/// journaux. Elle porte des parametres d'autorisation et doit rester secrete.
fn describe_http_error(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_body() {
        "response body failed"
    } else if error.is_decode() {
        "response decode failed"
    } else {
        "request failed"
    }
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, len) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse().ok()?;
    let end = end.parse().ok()?;
    let len = len.parse().ok()?;
    (start <= end && end < len).then_some((start, end, len))
}

impl Read for HttpRangeSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }

        // Une coupure de corps HTTP peut etre reprise une fois a l'octet exact.
        // Au second echec, remonter l'erreur : une boucle silencieuse livrerait
        // une piste tronquee.
        for attempt in 0..2 {
            if self.response.is_none() {
                self.open_at_current_position()?;
            }
            let read = self.response.as_mut().unwrap().read(buf);
            match read {
                Ok(0) if self.pos < self.len && attempt == 0 => {
                    self.response = None;
                }
                Ok(0) if self.pos < self.len => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("range body ended at {} before {}", self.pos, self.len),
                    ));
                }
                Ok(n) => {
                    self.pos += n as u64;
                    return Ok(n);
                }
                Err(_) if attempt == 0 => {
                    self.response = None;
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

impl Seek for HttpRangeSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => i128::from(n),
            SeekFrom::Current(delta) => i128::from(self.pos) + i128::from(delta),
            SeekFrom::End(delta) => i128::from(self.len) + i128::from(delta),
        };
        if target < 0 || target > i128::from(self.len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("range seek outside source: {target}/{}", self.len),
            ));
        }
        self.pos = target as u64;
        // Le prochain read ouvre une requete exactement a la nouvelle position.
        self.response = None;
        Ok(self.pos)
    }
}

impl MediaSource for HttpRangeSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    struct RangeServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
        full_body_completed: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl RangeServer {
        fn start(body: &'static [u8]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let full_body_completed = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let requests_bg = requests.clone();
            let full_bg = full_body_completed.clone();
            let stop_bg = stop.clone();
            let thread = std::thread::spawn(move || {
                while !stop_bg.load(Ordering::SeqCst) {
                    let Ok((stream, _)) = listener.accept() else {
                        continue;
                    };
                    if stop_bg.load(Ordering::SeqCst) {
                        break;
                    }
                    serve_range(stream, body, &requests_bg, &full_bg);
                }
            });
            Self {
                url: format!("http://{address}/test.m4a"),
                requests,
                full_body_completed,
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for RangeServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(
                self.url
                    .trim_start_matches("http://")
                    .replace("/test.m4a", ""),
            );
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn serve_range(
        mut stream: TcpStream,
        body: &[u8],
        requests: &Mutex<Vec<String>>,
        full_body_completed: &AtomicBool,
    ) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            lines.push(line);
        }
        let range = lines
            .iter()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("range")
                    .then(|| value.trim().to_string())
            })
            .unwrap_or_else(|| format!("bytes=0-{}", body.len() - 1));
        requests.lock().unwrap().push(range.clone());
        let spec = range.strip_prefix("bytes=").unwrap();
        let (start, end) = spec.split_once('-').unwrap();
        let start: usize = start.parse().unwrap();
        let end = if end.is_empty() {
            body.len() - 1
        } else {
            end.parse::<usize>().unwrap().min(body.len() - 1)
        };
        let len = end - start + 1;
        write!(
            stream,
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {len}\r\nContent-Range: bytes {start}-{end}/{}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        let mut complete = true;
        for chunk in body[start..=end].chunks(1_024) {
            if stream.write_all(chunk).is_err() {
                complete = false;
                break;
            }
            // Rendre observable la propriete recherchee : le decodeur doit
            // produire du PCM pendant que le corps principal arrive encore.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if complete && start == 0 && end + 1 == body.len() {
            full_body_completed.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn content_range_exige_des_bornes_coherentes() {
        assert_eq!(parse_content_range("bytes 4-9/12"), Some((4, 9, 12)));
        assert_eq!(parse_content_range("bytes 9-4/12"), None);
        assert_eq!(parse_content_range("bytes 4-12/12"), None);
        assert_eq!(parse_content_range("4-9/12"), None);
    }

    fn m4a_avec_espace_final() -> &'static [u8] {
        const FREE_SIZE: usize = 1024 * 1024;
        let mut media = include_bytes!("../../tests/fixtures/test.m4a").to_vec();
        media.extend_from_slice(&(FREE_SIZE as u32).to_be_bytes());
        media.extend_from_slice(b"free");
        media.resize(media.len() + FREE_SIZE - 8, 0);
        Box::leak(media.into_boxed_slice())
    }

    #[test]
    fn les_seeks_ouvrent_une_range_a_la_position_exacte() {
        static M4A: &[u8] = include_bytes!("../../tests/fixtures/test.m4a");
        let server = RangeServer::start(M4A);
        let mut source = HttpRangeSource::open(&server.url).unwrap();
        let mut head = [0u8; 4];
        source.read_exact(&mut head).unwrap();
        assert_eq!(head, M4A[..4]);
        source.seek(SeekFrom::End(-4)).unwrap();
        let mut tail = [0u8; 4];
        source.read_exact(&mut tail).unwrap();
        assert_eq!(tail, M4A[M4A.len() - 4..]);

        let requests = server.requests.lock().unwrap().clone();
        assert!(requests.iter().any(|r| r == "bytes=0-0"));
        assert!(requests.iter().any(|r| r == "bytes=0-"));
        assert!(
            requests
                .iter()
                .any(|r| r == &format!("bytes={}-", M4A.len() - 4))
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn le_premier_pcm_arrive_avant_la_fin_du_m4a() {
        let m4a = m4a_avec_espace_final();
        let server = RangeServer::start(m4a);
        let url = server.url.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let ready = Arc::new(tokio::sync::Notify::new());
        let (levels_tx, _levels_rx) = tokio::sync::mpsc::unbounded_channel();
        let decoder = tokio::task::spawn_blocking(move || {
            let source = HttpRangeSource::open(&url).unwrap();
            crate::audio::decode::decode_http_range_to_pcm_streaming_seeked(
                source,
                "m4a",
                Some(44_100),
                Some(2),
                Some(32),
                tx,
                1_024,
                ready,
                levels_tx,
                0.0,
            )
        });

        let header = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("entete WAV sans attendre le media entier")
            .expect("entete WAV");
        assert_eq!(header.len(), 44);
        assert_eq!(&header[20..22], &1u16.to_le_bytes(), "PCM entier");
        assert_eq!(&header[34..36], &32u16.to_le_bytes(), "32 bits annonces");

        let pcm = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("premier PCM sans attendre le media entier")
            .expect("premier bloc PCM");
        assert!(!pcm.is_empty());
        assert!(pcm.iter().any(|byte| *byte != 0));
        assert!(
            !server.full_body_completed.load(Ordering::SeqCst),
            "l'ancien chemin telechargeait tout le M4A avant le premier PCM; ranges={:?}",
            server.requests.lock().unwrap()
        );

        while rx.recv().await.is_some() {}
        assert_eq!(decoder.await.unwrap().unwrap(), (32, 44_100));
    }
}
