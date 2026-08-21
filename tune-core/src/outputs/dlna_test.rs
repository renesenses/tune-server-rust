#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use tokio::sync::Mutex;

    use crate::outputs::dlna::DlnaOutput;
    use crate::outputs::traits::{OutputTarget, PlayMedia, TransportState};

    #[derive(Clone)]
    struct MockState {
        play_count: Arc<AtomicU32>,
        pause_count: Arc<AtomicU32>,
        stop_count: Arc<AtomicU32>,
        seek_count: Arc<AtomicU32>,
        set_next_count: Arc<AtomicU32>,
        volume_count: Arc<AtomicU32>,
        transport_state: Arc<Mutex<String>>,
        last_seek_target: Arc<Mutex<String>>,
    }

    impl Default for MockState {
        fn default() -> Self {
            Self {
                play_count: Arc::new(AtomicU32::new(0)),
                pause_count: Arc::new(AtomicU32::new(0)),
                stop_count: Arc::new(AtomicU32::new(0)),
                seek_count: Arc::new(AtomicU32::new(0)),
                set_next_count: Arc::new(AtomicU32::new(0)),
                volume_count: Arc::new(AtomicU32::new(0)),
                transport_state: Arc::new(Mutex::new("STOPPED".into())),
                last_seek_target: Arc::new(Mutex::new(String::new())),
            }
        }
    }

    fn extract_action(body: &str) -> String {
        // Find <u:ACTION in the SOAP body
        if let Some(start) = body.find("<u:") {
            let rest = &body[start + 3..];
            if let Some(end) = rest.find(|c: char| c == ' ' || c == '>') {
                return rest[..end].to_string();
            }
        }
        String::new()
    }

    fn extract_tag(xml: &str, tag: &str) -> String {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        if let Some(s) = xml.find(&open) {
            let s = s + open.len();
            if let Some(e) = xml[s..].find(&close) {
                return xml[s..s + e].to_string();
            }
        }
        String::new()
    }

    fn soap_ok(action: &str, inner: &str) -> String {
        format!(
            r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">{inner}</u:{action}Response></s:Body></s:Envelope>"#
        )
    }

    async fn av_handler(State(state): State<MockState>, body: String) -> String {
        let action = extract_action(&body);
        match action.as_str() {
            "SetAVTransportURI" => soap_ok("SetAVTransportURI", ""),
            "Play" => {
                state.play_count.fetch_add(1, Ordering::Relaxed);
                *state.transport_state.lock().await = "PLAYING".into();
                soap_ok("Play", "")
            }
            "Pause" => {
                state.pause_count.fetch_add(1, Ordering::Relaxed);
                *state.transport_state.lock().await = "PAUSED_PLAYBACK".into();
                soap_ok("Pause", "")
            }
            "Stop" => {
                state.stop_count.fetch_add(1, Ordering::Relaxed);
                *state.transport_state.lock().await = "STOPPED".into();
                soap_ok("Stop", "")
            }
            "Seek" => {
                state.seek_count.fetch_add(1, Ordering::Relaxed);
                *state.last_seek_target.lock().await = extract_tag(&body, "Target");
                soap_ok("Seek", "")
            }
            "SetNextAVTransportURI" => {
                state.set_next_count.fetch_add(1, Ordering::Relaxed);
                soap_ok("SetNextAVTransportURI", "")
            }
            "GetTransportInfo" => {
                let ts = state.transport_state.lock().await.clone();
                soap_ok(
                    "GetTransportInfo",
                    &format!(
                        "<CurrentTransportState>{ts}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>"
                    ),
                )
            }
            "GetPositionInfo" => soap_ok(
                "GetPositionInfo",
                "<Track>1</Track><TrackDuration>0:05:00</TrackDuration><TrackMetaData></TrackMetaData><TrackURI></TrackURI><RelTime>0:01:30</RelTime><AbsTime>0:01:30</AbsTime><RelCount>0</RelCount><AbsCount>0</AbsCount>",
            ),
            _ => soap_ok(&action, ""),
        }
    }

    async fn rc_handler(State(state): State<MockState>, body: String) -> String {
        let action = extract_action(&body);
        match action.as_str() {
            "SetVolume" => {
                state.volume_count.fetch_add(1, Ordering::Relaxed);
                soap_ok("SetVolume", "")
            }
            "GetVolume" => soap_ok("GetVolume", "<CurrentVolume>50</CurrentVolume>"),
            "SetMute" => soap_ok("SetMute", ""),
            "GetMute" => soap_ok("GetMute", "<CurrentMute>0</CurrentMute>"),
            _ => soap_ok(&action, ""),
        }
    }

    async fn start_mock(state: MockState) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/AVTransport", post(av_handler))
            .route("/RenderingControl", post(rc_handler))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn make_dlna(base: &str) -> DlnaOutput {
        DlnaOutput::new(
            "Mock Renderer".into(),
            "mock-dlna-001".into(),
            "127.0.0.1".into(),
            format!("{base}/AVTransport"),
            format!("{base}/RenderingControl"),
            None,
        )
    }

    #[tokio::test]
    async fn dlna_play_and_status() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .play_media(&PlayMedia {
                url: "http://example.com/track.wav",
                mime_type: "audio/wav",
                title: Some("Test Track"),
                ..Default::default()
            })
            .await
            .unwrap();

        assert!(state.play_count.load(Ordering::Relaxed) >= 1);
        let status = output.get_status().await.unwrap();
        assert_eq!(status.state, TransportState::Playing);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_pause_resume_stop() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .play_media(&PlayMedia {
                url: "http://example.com/t.wav",
                mime_type: "audio/wav",
                ..Default::default()
            })
            .await
            .unwrap();

        output.pause().await.unwrap();
        assert_eq!(state.pause_count.load(Ordering::Relaxed), 1);

        output.resume().await.unwrap();

        output.stop().await.unwrap();
        assert!(state.stop_count.load(Ordering::Relaxed) >= 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_seek() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output.seek(90_000).await.unwrap();
        assert_eq!(state.seek_count.load(Ordering::Relaxed), 1);
        assert_eq!(*state.last_seek_target.lock().await, "0:01:30");
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_set_volume() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output.set_volume(0.75).await.unwrap();
        assert_eq!(state.volume_count.load(Ordering::Relaxed), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_set_next_gapless() {
        let state = MockState::default();
        let (base, handle) = start_mock(state.clone()).await;
        let output = make_dlna(&base);

        output
            .set_next_media(&PlayMedia {
                url: "http://example.com/next.wav",
                mime_type: "audio/wav",
                title: Some("Next"),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(state.set_next_count.load(Ordering::Relaxed), 1);
        handle.abort();
    }

    #[tokio::test]
    async fn dlna_get_position() {
        let state = MockState::default();
        *state.transport_state.lock().await = "PLAYING".into();
        let (base, handle) = start_mock(state).await;
        let output = make_dlna(&base);

        let status = output.get_status().await.unwrap();
        assert_eq!(status.state, TransportState::Playing);
        assert_eq!(status.position_ms, 90_000);
        assert_eq!(status.duration_ms, 300_000);
        handle.abort();
    }

    // ---- #1984 : le renderer raccroche avant d'avoir fini de répondre --------

    /// Lit une requête HTTP entière (en-têtes + corps annoncé par
    /// `Content-Length`) pour ne pas répondre avant que le client ait fini
    /// d'écrire — sinon la réponse part dans un socket que le client est encore
    /// en train de remplir, et le test échouerait pour une raison qui n'est pas
    /// celle qu'il mesure.
    async fn lire_requete_entiere(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut brut = Vec::new();
        let mut tampon = [0u8; 4096];
        loop {
            let n = sock.read(&mut tampon).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            brut.extend_from_slice(&tampon[..n]);
            let texte = String::from_utf8_lossy(&brut).to_string();
            let Some(fin_entetes) = texte.find("\r\n\r\n") else {
                continue;
            };
            let attendu: usize = texte
                .lines()
                .find_map(|l| {
                    let (nom, valeur) = l.split_once(':')?;
                    nom.eq_ignore_ascii_case("content-length")
                        .then(|| valeur.trim().parse().ok())?
                })
                .unwrap_or(0);
            if brut.len() >= fin_entetes + 4 + attendu {
                return texte;
            }
        }
        String::from_utf8_lossy(&brut).to_string()
    }

    /// Renderer qui se comporte comme le Marantz ND8006 de Jean Valjean : la
    /// **première** connexion est acceptée, la requête lue, puis le socket
    /// refermé sans le moindre octet de réponse — le symptôme d'une pile HTTP
    /// embarquée qui a raccroché sur une connexion mutualisée. Les connexions
    /// suivantes répondent normalement.
    ///
    /// Renvoie l'URL de base et un compteur de connexions acceptées.
    async fn renderer_qui_raccroche_une_fois()
    -> (String, Arc<AtomicU32>, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connexions = Arc::new(AtomicU32::new(0));
        let compteur = connexions.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let rang = compteur.fetch_add(1, Ordering::SeqCst);
                if rang == 0 {
                    // Le socket mort : on lit, on ne répond pas, on raccroche.
                    let _ = lire_requete_entiere(&mut sock).await;
                    drop(sock);
                    continue;
                }
                let _ = lire_requete_entiere(&mut sock).await;
                let corps = concat!(
                    r#"<?xml version="1.0"?>"#,
                    r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">"#,
                    "<s:Body><u:GetProtocolInfoResponse><Sink>",
                    "http-get:*:audio/L16;rate=44100;channels=2:*,",
                    "http-get:*:audio/L24;rate=96000;channels=2:*,",
                    "http-get:*:audio/flac:*",
                    "</Sink></u:GetProtocolInfoResponse></s:Body></s:Envelope>",
                );
                let reponse = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{corps}",
                    corps.len()
                );
                let _ = sock.write_all(reponse.as_bytes()).await;
                let _ = sock.flush().await;
                // Laisser le client lire avant de fermer.
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        (format!("http://127.0.0.1:{port}"), connexions, handle)
    }

    /// #1984 — « Impossible de lire les capacités du renderer » alors que
    /// l'appareil est allumé, détecté et en train de jouer.
    ///
    /// La sonde `GetProtocolInfo` tombait sur une connexion que le renderer
    /// avait refermée. `reqwest` rend alors une erreur qui n'est ni
    /// `is_connect()` ni `is_timeout()` : la garde de `soap_action` ne la
    /// reconnaissait pas, sortait par le bras « erreur définitive », et
    /// n'essayait pas une seconde fois. `caps` restait vide, et le bouton
    /// « WAV 24 bits » restait grisé alors que le Sink annonce `audio/L24`.
    #[tokio::test]
    async fn les_capacites_se_lisent_malgre_une_connexion_raccrochee() {
        let (base, connexions, handle) = renderer_qui_raccroche_une_fois().await;
        let output = DlnaOutput::new(
            "Marantz ND8006".into(),
            "uuid:56fcb4ae".into(),
            "127.0.0.1".into(),
            format!("{base}/upnp/control/renderer_dvc/AVTransport"),
            format!("{base}/upnp/control/renderer_dvc/RenderingControl"),
            Some(format!(
                "{base}/upnp/control/renderer_dvc/ConnectionManager"
            )),
        );

        let caps = output.probe_capabilities().await;

        assert!(
            caps.probed,
            "la sonde doit aboutir : le renderer répond dès la deuxième connexion"
        );
        assert!(
            caps.lpcm24,
            "audio/L24 est annoncé — le bouton 24 bits doit s'armer"
        );
        assert!(caps.flac, "audio/flac est annoncé");
        assert_eq!(
            connexions.load(Ordering::SeqCst),
            2,
            "il faut exactement une seconde tentative, sur une connexion neuve"
        );
        handle.abort();
    }
}
