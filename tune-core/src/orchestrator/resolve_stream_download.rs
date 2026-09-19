//! Telechargement local/OAAT lie a la session qui le consommera (#4234).
//!
//! Supprimer une session ne ferme pas forcement son receveur : un corps HTTP
//! peut encore en retenir un Arc. On observe donc le registre, pas seulement
//! Sender::closed(). Une pause ou un prechargement sans lecteur reste vivant.
use crate::http::streamer::AudioStreamer;
use std::path::Path;
use std::time::Duration;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;

pub(super) async fn telecharger_pour_session(
    streamer: &AudioStreamer,
    session_id: &str,
    upstream: &str,
    entetes: &[(String, String)],
    codec: &str,
    directory: &Path,
) -> Result<Option<(NamedTempFile, u64)>, String> {
    let cancelled = async {
        while streamer.session_alive(session_id).await {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    };
    let transfer = async {
        let client = crate::http::client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|e| format!("upstream client: {e}"))?;
        // #4366 — les en-têtes du résolveur (yt-dlp) : sans eux, `googlevideo`
        // refuse l'URL en 403. Vide pour tout autre service.
        let mut requete = client.get(upstream);
        for (nom, valeur) in entetes {
            requete = requete.header(nom, valeur);
        }
        let mut response = requete
            .send()
            .await
            .map_err(|e| format!("upstream fetch: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("upstream HTTP {}", response.status()));
        }
        // Creation exclusive et suppression RAII, y compris si select! detruit
        // cette future pendant l'attente des en-tetes ou d'un morceau du corps.
        let temporary = tempfile::Builder::new()
            .prefix(&format!("tune-stream-{session_id}-"))
            .suffix(&format!(".{codec}"))
            .tempfile_in(directory)
            .map_err(|e| format!("tmp create: {e}"))?;
        let mut file =
            tokio::fs::File::from_std(temporary.reopen().map_err(|e| format!("tmp open: {e}"))?);
        let mut bytes = 0;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| format!("download read: {e}"))?
        {
            file.write_all(&chunk)
                .await
                .map_err(|e| format!("download write: {e}"))?;
            bytes += chunk.len() as u64;
        }
        file.flush()
            .await
            .map_err(|e| format!("download flush: {e}"))?;
        drop(file);
        Ok((temporary, bytes))
    };
    tokio::select! {
        biased;
        () = cancelled => Ok(None),
        result = transfer => {
            // La fin reseau et le retrait peuvent arriver au meme tour.
            if streamer.session_alive(session_id).await {
                result.map(Some)
            } else {
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::streamer::StreamInfo;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    struct Task<T>(tokio::task::JoinHandle<T>);
    impl<T> Drop for Task<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    struct Peer {
        url: String,
        ready: Option<oneshot::Receiver<()>>,
        closed: Option<oneshot::Receiver<()>>,
        _task: Task<()>,
    }

    impl Peer {
        async fn new(response: Option<Vec<u8>>, truncate: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/track", listener.local_addr().unwrap());
            let (ready_tx, ready) = oneshot::channel();
            let (closed_tx, closed) = oneshot::channel();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0; 1];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 16_384);
                }
                if let Some(response) = response {
                    socket.write_all(&response).await.unwrap();
                }
                let _ = ready_tx.send(());
                if truncate {
                    socket.shutdown().await.unwrap();
                } else {
                    let mut byte = [0; 1];
                    match socket.read(&mut byte).await {
                        Ok(0) => {}
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::BrokenPipe
                            ) => {}
                        other => panic!("upstream should observe a closed connection: {other:?}"),
                    }
                }
                let _ = closed_tx.send(());
            });
            Self {
                url,
                ready: Some(ready),
                closed: Some(closed),
                _task: Task(task),
            }
        }

        async fn ready(&mut self) {
            tokio::time::timeout(Duration::from_secs(2), self.ready.take().unwrap())
                .await
                .expect("HTTP request did not arrive")
                .unwrap();
        }

        async fn closed(&mut self) {
            tokio::time::timeout(Duration::from_secs(1), self.closed.take().unwrap())
                .await
                .expect("cancelled download must close the real upstream socket")
                .unwrap();
        }
    }

    fn response(status: u16, length: usize, body: &[u8]) -> Vec<u8> {
        let mut result = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        result.extend_from_slice(body);
        result
    }

    async fn cancel_pending(body_started: bool) {
        let streamer = Arc::new(AudioStreamer::new(0));
        let (sid, tx, _) = streamer
            .create_session(StreamInfo::default(), false, 8)
            .await;
        // A real HTTP body can retain this Arc after remove_session. Merely
        // waiting for tx.closed() would leave this download running.
        let held = streamer.sessions_state().lock().await[&sid].clone();
        let dir = tempfile::tempdir().unwrap();
        let reply = body_started.then(|| response(200, 100_000, b"partial audio"));
        let mut peer = Peer::new(reply, false).await;
        let cloned = streamer.clone();
        let task_sid = sid.clone();
        let upstream = peer.url.clone();
        let path = dir.path().to_path_buf();
        let mut task = Task(tokio::spawn(async move {
            telecharger_pour_session(&cloned, &task_sid, &upstream, &[], "flac", &path).await
        }));
        peer.ready().await;
        if body_started {
            tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if std::fs::read_dir(dir.path())
                        .unwrap()
                        .any(|entry| entry.unwrap().metadata().unwrap().len() > 0)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("partial download must really reach disk before cancellation");
        }
        streamer.remove_session(&sid).await;
        assert!(
            !tx.is_closed(),
            "fixture must retain the removed session receiver"
        );
        let outcome = tokio::time::timeout(Duration::from_secs(1), &mut task.0)
            .await
            .expect("removed session must cancel its pending HTTP download")
            .unwrap()
            .unwrap();
        assert!(
            outcome.is_none(),
            "cancellation is not a completed download"
        );
        peer.closed().await;
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "cancelled download must remove its partial temporary file"
        );
        drop(held);
    }

    #[tokio::test]
    async fn i4234_cancel_before_http_headers_closes_upstream() {
        cancel_pending(false).await;
    }

    #[tokio::test]
    async fn i4234_cancel_partial_body_with_retained_session_removes_file() {
        cancel_pending(true).await;
    }

    #[tokio::test]
    async fn i4234_removed_session_never_starts_a_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let streamer = AudioStreamer::new(0);
        let dir = tempfile::tempdir().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            telecharger_pour_session(
                &streamer,
                "already-removed",
                &format!("http://{}/track", listener.local_addr().unwrap()),
                &[],
                "flac",
                dir.path(),
            ),
        )
        .await
        .expect("an already removed session must finish without contacting upstream")
        .unwrap();
        assert!(result.is_none());
        assert!(
            tokio::time::timeout(Duration::from_millis(75), listener.accept())
                .await
                .is_err(),
            "a superseded session must not open an upstream connection"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn i4234_live_preload_without_consumers_keeps_all_bytes() {
        let streamer = AudioStreamer::new(0);
        let (sid, _tx, _) = streamer
            .create_session(StreamInfo::default(), false, 8)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let payload = (0..250_000).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        let mut peer = Peer::new(Some(response(200, payload.len(), &payload)), false).await;
        let (file, bytes) =
            telecharger_pour_session(&streamer, &sid, &peer.url, &[], "flac", dir.path())
                .await
                .unwrap()
                .expect("live preloaded/paused session must not be cancelled");
        assert_eq!(bytes, payload.len() as u64);
        assert_eq!(std::fs::read(file.path()).unwrap(), payload);
        let path = file.path().to_owned();
        drop(file);
        assert!(
            !path.exists(),
            "successful download must also release its owned file"
        );
        peer.closed().await;
    }

    #[tokio::test]
    async fn i4234_http_refusal_is_an_error_without_temp_file() {
        let streamer = AudioStreamer::new(0);
        let (sid, _tx, _) = streamer
            .create_session(StreamInfo::default(), false, 8)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let mut peer = Peer::new(Some(response(403, 0, b"")), false).await;
        let error = telecharger_pour_session(&streamer, &sid, &peer.url, &[], "flac", dir.path())
            .await
            .unwrap_err();
        assert!(error.contains("upstream HTTP 403"), "{error}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        peer.closed().await;
    }

    #[tokio::test]
    async fn i4234_incomplete_http_body_is_an_error_and_is_removed() {
        let streamer = AudioStreamer::new(0);
        let (sid, _tx, _) = streamer
            .create_session(StreamInfo::default(), false, 8)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let peer = Peer::new(Some(response(200, 100_000, b"truncated")), true).await;
        let error = telecharger_pour_session(&streamer, &sid, &peer.url, &[], "flac", dir.path())
            .await
            .unwrap_err();
        assert!(error.contains("download read:"), "{error}");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "failed body must not leave a tune-stream file behind"
        );
    }

    fn task(
        streamer: Arc<AudioStreamer>,
        sid: String,
        upstream: String,
    ) -> super::super::TranscodageEnTache {
        super::super::TranscodageEnTache {
            is_dash_local: upstream.starts_with("file://"),
            upstream_url: upstream,
            upstream_headers: Vec::new(),
            codec: "wav".into(),
            sr: 44100,
            bd: 16,
            ev_bus: None,
            playback: Arc::new(crate::playback::PlaybackManager::new()),
            zone_id: 4234,
            streamer_for_eof: streamer,
            session_id_for_eof: sid,
            attach_levels: false,
            seek_s: 0.0,
            use_http_range: false,
        }
    }

    #[tokio::test]
    async fn i4234_real_transcode_task_exits_when_session_is_removed() {
        let streamer = Arc::new(AudioStreamer::new(0));
        let (sid, tx, ready) = streamer
            .create_session(StreamInfo::default(), false, 8)
            .await;
        let held = streamer.sessions_state().lock().await[&sid].clone();
        let mut peer = Peer::new(None, false).await;
        let captured = task(streamer.clone(), sid.clone(), peer.url.clone());
        let mut producer = Task(tokio::spawn(
            super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(captured, tx, ready),
        ));
        peer.ready().await;
        streamer.remove_session(&sid).await;
        tokio::time::timeout(Duration::from_secs(1), &mut producer.0)
            .await
            .expect("real local/OAAT producer must exit without waiting for obsolete download")
            .unwrap();
        peer.closed().await;
        assert!(
            held.recv_chunk().await.is_none(),
            "cancelled producer must emit no WAV bytes"
        );
    }

    #[tokio::test]
    async fn i4234_real_http_refusal_removes_the_unusable_session() {
        let streamer = Arc::new(AudioStreamer::new(0));
        let (sid, tx, ready) = streamer
            .create_session(StreamInfo::default(), false, 8)
            .await;
        let session = streamer.sessions_state().lock().await[&sid].clone();
        let mut peer = Peer::new(Some(response(403, 0, b"")), false).await;
        tokio::time::timeout(
            Duration::from_secs(2),
            super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(
                task(streamer.clone(), sid.clone(), peer.url.clone()),
                tx,
                ready,
            ),
        )
        .await
        .unwrap();
        assert!(
            !streamer.session_alive(&sid).await,
            "failed download must not leave a dead session eligible for gapless (#3287)"
        );
        assert!(session.recv_chunk().await.is_none());
        peer.closed().await;
    }

    fn wav() -> Vec<u8> {
        let data_len = 4410_u32 * 4;
        let mut data = Vec::new();
        data.extend_from_slice(b"RIFF");
        data.extend_from_slice(&(36 + data_len).to_le_bytes());
        data.extend_from_slice(b"WAVEfmt ");
        data.extend_from_slice(&16_u32.to_le_bytes());
        data.extend_from_slice(&1_u16.to_le_bytes());
        data.extend_from_slice(&2_u16.to_le_bytes());
        data.extend_from_slice(&44100_u32.to_le_bytes());
        data.extend_from_slice(&(44100_u32 * 4).to_le_bytes());
        data.extend_from_slice(&4_u16.to_le_bytes());
        data.extend_from_slice(&16_u16.to_le_bytes());
        data.extend_from_slice(b"data");
        data.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..4410_i16 {
            data.extend_from_slice(&(i % 250).to_le_bytes());
            data.extend_from_slice(&(-(i % 250)).to_le_bytes());
        }
        data
    }

    async fn successful_producer(upstream: String) {
        let streamer = Arc::new(AudioStreamer::new(0));
        let (sid, tx, ready) = streamer
            .create_session(StreamInfo::default(), false, 256)
            .await;
        let session = streamer.sessions_state().lock().await[&sid].clone();
        tokio::time::timeout(
            Duration::from_secs(5),
            super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(
                task(streamer.clone(), sid.clone(), upstream),
                tx,
                ready,
            ),
        )
        .await
        .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(chunk) = session.recv_chunk().await {
                bytes.extend(chunk);
            }
        })
        .await
        .expect("finite transcode must close its session input");
        assert!(bytes.starts_with(b"RIFF"), "real decoder must produce WAV");
        assert!(
            bytes.len() >= 44 + 4410 * 4,
            "real PCM payload must be present"
        );
        assert!(
            streamer.session_alive(&sid).await,
            "successful session remains readable"
        );
    }

    #[tokio::test]
    async fn i4234_real_http_transcode_still_decodes_and_ends_input() {
        let payload = wav();
        let mut peer = Peer::new(Some(response(200, payload.len(), &payload)), false).await;
        successful_producer(peer.url.clone()).await;
        peer.closed().await;
    }

    #[tokio::test]
    async fn i4234_dash_cache_file_survives_real_transcode() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), wav()).unwrap();
        let before = std::fs::read(file.path()).unwrap();
        successful_producer(format!("file://{}", file.path().display())).await;
        assert_eq!(
            std::fs::read(file.path()).unwrap(),
            before,
            "a DASH cache file must never be deleted or modified by transcode cleanup"
        );
    }

    // ── #4366 : la sortie locale/OAAT rejoue les en-têtes du résolveur ──
    //
    // Un amont qui, comme `googlevideo`, refuse en 403 l'URL rendue par yt-dlp
    // à qui ne rejoue pas les en-têtes associés. Il sert le corps en 200, ou
    // en 206 sur `Range` (la sonde et la lecture de `HttpRangeSource`).
    const ENTETE_4366: (&str, &str) = ("x-temoin-4366", "yt-dlp");

    struct Gardien {
        url: String,
        /// (en-tête présent, `Range` demandé) pour chaque requête reçue.
        requetes: Arc<std::sync::Mutex<Vec<(bool, bool)>>>,
        _task: Task<()>,
    }

    impl Gardien {
        async fn new(corps: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/track.wav", listener.local_addr().unwrap());
            let requetes = Arc::new(std::sync::Mutex::new(Vec::new()));
            let journal = requetes.clone();
            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let mut brut = Vec::new();
                    while !brut.ends_with(b"\r\n\r\n") {
                        let mut octet = [0; 1];
                        if socket.read_exact(&mut octet).await.is_err() {
                            break;
                        }
                        brut.push(octet[0]);
                    }
                    let texte = String::from_utf8_lossy(&brut).to_ascii_lowercase();
                    let autorise = texte.contains(&format!("{}: {}", ENTETE_4366.0, ENTETE_4366.1));
                    let range = texte
                        .lines()
                        .find_map(|l| l.strip_prefix("range: bytes="))
                        .map(|r| r.trim().to_string());
                    journal.lock().unwrap().push((autorise, range.is_some()));
                    let reponse = if !autorise {
                        response(403, 0, b"")
                    } else if let Some(r) = range {
                        let (debut, fin) = r.split_once('-').unwrap();
                        let debut: usize = debut.parse().unwrap();
                        let fin: usize = if fin.is_empty() {
                            corps.len() - 1
                        } else {
                            fin.parse::<usize>().unwrap().min(corps.len() - 1)
                        };
                        let tranche = &corps[debut..=fin];
                        let mut r = format!(
                            "HTTP/1.1 206 Partial\r\nContent-Length: {}\r\nContent-Range: bytes {debut}-{fin}/{}\r\nConnection: close\r\n\r\n",
                            tranche.len(),
                            corps.len()
                        )
                        .into_bytes();
                        r.extend_from_slice(tranche);
                        r
                    } else {
                        response(200, corps.len(), &corps)
                    };
                    let _ = socket.write_all(&reponse).await;
                    let _ = socket.shutdown().await;
                }
            });
            Self {
                url,
                requetes,
                _task: Task(task),
            }
        }
    }

    /// Fait tourner la VRAIE tâche locale/OAAT contre le gardien ; rend les
    /// octets WAV reçus par la session (vide si l'amont a refusé).
    async fn produire_4366(
        upstream: String,
        entetes: Vec<(String, String)>,
        use_http_range: bool,
    ) -> Vec<u8> {
        let streamer = Arc::new(AudioStreamer::new(0));
        let (sid, tx, ready) = streamer
            .create_session(StreamInfo::default(), false, 256)
            .await;
        let session = streamer.sessions_state().lock().await[&sid].clone();
        let mut tache = task(streamer.clone(), sid.clone(), upstream);
        tache.upstream_headers = entetes;
        tache.use_http_range = use_http_range;
        tokio::time::timeout(
            Duration::from_secs(5),
            super::super::PlaybackOrchestrator::transcoder_le_flux_en_wav(tache, tx, ready),
        )
        .await
        .unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(chunk) = session.recv_chunk().await {
                bytes.extend(chunk);
            }
        })
        .await
        .expect("la tâche doit fermer l'entrée de sa session");
        bytes
    }

    fn entetes_4366() -> Vec<(String, String)> {
        vec![(ENTETE_4366.0.to_string(), ENTETE_4366.1.to_string())]
    }

    /// Contre-épreuve du banc : sans les en-têtes, le gardien refuse bien —
    /// sinon les deux témoins suivants ne prouveraient rien.
    #[tokio::test]
    async fn i4366_le_gardien_refuse_une_requete_nue() {
        let gardien = Gardien::new(wav()).await;
        let octets = produire_4366(gardien.url.clone(), Vec::new(), false).await;
        assert!(
            octets.is_empty(),
            "requête nue : l'amont doit refuser (403)"
        );
        assert!(gardien.requetes.lock().unwrap().iter().all(|(ok, _)| !ok));
    }

    /// Chemin par téléchargement complet (repli historique).
    #[tokio::test]
    async fn i4366_le_telechargement_local_rejoue_les_entetes_du_resolveur() {
        let gardien = Gardien::new(wav()).await;
        let octets = produire_4366(gardien.url.clone(), entetes_4366(), false).await;
        assert!(
            octets.starts_with(b"RIFF") && octets.len() >= 44 + 4410 * 4,
            "l'URL servie à qui rejoue ses en-têtes doit se décoder (reçu {} octets)",
            octets.len()
        );
    }

    /// Chemin par `Range` — celui que prend un M4A YouTube sur sortie locale
    /// (#1885) : la sonde ET les lectures doivent rejouer les en-têtes, et
    /// c'est bien ce chemin qui a servi (pas le repli par fichier).
    #[tokio::test]
    async fn i4366_la_source_range_rejoue_les_entetes_du_resolveur() {
        let gardien = Gardien::new(wav()).await;
        let octets = produire_4366(gardien.url.clone(), entetes_4366(), true).await;
        assert!(
            octets.starts_with(b"RIFF") && octets.len() >= 44 + 4410 * 4,
            "la source Range doit décoder l'URL gardée (reçu {} octets)",
            octets.len()
        );
        let requetes = gardien.requetes.lock().unwrap().clone();
        assert!(
            requetes.len() >= 2 && requetes.iter().all(|&(ok, range)| ok && range),
            "sonde + lecture, toutes en Range et toutes avec les en-têtes : {requetes:?}"
        );
    }
}
