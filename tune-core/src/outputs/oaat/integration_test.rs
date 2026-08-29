#[cfg(all(test, feature = "oaat"))]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};

    use oaat_core::Message;
    use oaat_core::codec::FrameCodec;
    use oaat_core::message::*;
    use oaat_core::wire::{AUDIO_HEADER_SIZE, AudioPacketHeader};

    use crate::outputs::oaat::OaatOutput;
    use crate::outputs::traits::{OutputTarget, PlayMedia};
    #[tokio::test]
    async fn oaat_connect_and_stream() {
        // Bind control (TCP), audio (UDP), clock (UDP) on random ports
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        // Start mock endpoint
        let mock_handle = tokio::spawn(async move {
            let mut got_format = false;
            let mut got_play = false;

            // Audio receiver
            let audio_socket = std::sync::Arc::new(audio_udp);
            let audio_rx = {
                let s = audio_socket.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut count = 0u32;
                    loop {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            s.recv(&mut buf),
                        )
                        .await
                        {
                            Ok(Ok(n)) if n >= AUDIO_HEADER_SIZE => count += 1,
                            _ => break,
                        }
                        if count >= 10 {
                            break;
                        }
                    }
                    count
                })
            };

            // Clock responder
            let _clock_handle = tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    match clock_udp.recv_from(&mut buf).await {
                        Ok((n, peer)) if n >= 28 => {
                            // Echo back as response (simplified)
                            let _ = clock_udp.send_to(&buf[..n], peer).await;
                        }
                        _ => break,
                    }
                }
            });

            // Accept TCP
            if let Ok((mut stream, _)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), tcp.accept())
                    .await
                    .unwrap_or(Err(std::io::Error::other("timeout")))
            {
                let mut codec = FrameCodec::new();
                let mut read_buf = [0u8; 8192];

                // Read Hello
                let n = stream.read(&mut read_buf).await.unwrap_or(0);
                if n > 0 {
                    codec.feed(&read_buf[..n]);
                    if let Ok(Some(Message::Hello(_))) = codec.decode_next() {
                        // Send HelloAck
                        let ack = Message::HelloAck(HelloAck {
                            protocol_version: oaat_core::PROTOCOL_VERSION,
                            endpoint_id: "mock-ep-001".into(),
                            endpoint_name: "Mock DAC".into(),
                            capabilities: EndpointCapabilities {
                                pcm_max_rate: 192000,
                                pcm_max_bits: 32,
                                dsd_max_rate: None,
                                channels_max: 2,
                                formats: vec![
                                    oaat_core::format::AudioFormat::PcmS16le,
                                    oaat_core::format::AudioFormat::PcmS24le,
                                ],
                                volume: None,
                                gapless: true,
                                seek: false,
                            },
                            audio_port,
                            clock_port,
                            buffer_size_ms: 100,
                        });
                        let _ = stream.write_all(&FrameCodec::encode(&ack)).await;

                        // Read control messages until disconnected
                        loop {
                            let n = match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                stream.read(&mut read_buf),
                            )
                            .await
                            {
                                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                Ok(Ok(n)) => n,
                            };
                            codec.feed(&read_buf[..n]);
                            while let Ok(Some(msg)) = codec.decode_next() {
                                match msg {
                                    Message::FormatPropose(fp) => {
                                        got_format = true;
                                        let accept = Message::FormatAccept(FormatAccept {
                                            stream_id: fp.stream_id,
                                        });
                                        let _ =
                                            stream.write_all(&FrameCodec::encode(&accept)).await;
                                    }
                                    Message::Play(_) => got_play = true,
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }

            let audio_count = audio_rx.await.unwrap_or(0);
            (got_format, got_play, audio_count)
        });

        // Start HTTP server with a short WAV
        let wav = make_test_wav();
        let http_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_tcp.local_addr().unwrap().port();
        let http_handle = tokio::spawn(async move {
            if let Ok((mut s, _)) = http_tcp.accept().await {
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                    wav.len()
                );
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&wav).await;
            }
        });

        // Create OaatOutput and play
        let output = OaatOutput::new(
            "Mock DAC".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-ep-001".into(),
        );

        let url = format!("http://127.0.0.1:{http_port}/test.wav");
        let result = output
            .play_media(&PlayMedia {
                url: &url,
                mime_type: "audio/wav",
                title: Some("Test"),
                ..Default::default()
            })
            .await;
        assert!(result.is_ok());

        // Wait for mock to finish
        let (got_format, got_play, audio_packets) =
            tokio::time::timeout(std::time::Duration::from_secs(8), mock_handle)
                .await
                .expect("mock timed out")
                .expect("mock panicked");

        assert!(got_format, "endpoint should receive FormatPropose");
        assert!(got_play, "endpoint should receive Play");
        assert!(
            audio_packets >= 5,
            "expected >=5 audio packets, got {audio_packets}"
        );

        // Verify diagnostics show activity
        let snap = output.diagnostics_snapshot();
        assert!(snap["packets_sent"].as_u64().unwrap() > 0);

        output.stop().await.ok();
        http_handle.abort();
    }

    /// #2239 — le contrat négocié doit décrire les octets UDP, pas seulement
    /// les variables de paquetisation.
    ///
    /// Le témoin contient 50 ms de vrai PCM 24-bit / 96 kHz / stéréo. Les
    /// canaux sont opposés, donc le downmix 0,5 L + 0,5 R attendu est exactement
    /// nul. L'endpoint contre-propose 16-bit / 48 kHz / mono ; on décode ensuite
    /// chaque en-tête UDP et on vérifie format, frontières, PTS, offsets, durée
    /// et payload byte-for-byte.
    #[tokio::test]
    async fn format_counter_convertit_reellement_le_payload_pcm() {
        use oaat_core::format::{AudioFormat, ChannelLayout};
        use oaat_core::wire::PacketFlags;

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        let endpoint = tokio::spawn(async move {
            let audio_rx = tokio::spawn(async move {
                let mut packets = Vec::new();
                let mut datagram = vec![0u8; 8192];
                loop {
                    let n = tokio::time::timeout(
                        std::time::Duration::from_secs(8),
                        audio_udp.recv(&mut datagram),
                    )
                    .await
                    .expect("audio UDP timeout")
                    .expect("audio UDP receive");
                    assert!(n >= AUDIO_HEADER_SIZE);
                    let header_bytes: [u8; AUDIO_HEADER_SIZE] =
                        datagram[..AUDIO_HEADER_SIZE].try_into().unwrap();
                    let header = AudioPacketHeader::decode(&header_bytes).unwrap();
                    let payload = datagram[AUDIO_HEADER_SIZE..n].to_vec();
                    assert_eq!(payload.len(), header.payload_len as usize);
                    let last = header.flags.contains(PacketFlags::LAST_PACKET);
                    packets.push((header, payload));
                    if last {
                        break;
                    }
                }
                packets
            });

            let _clock = tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok((n, peer)) = clock_udp.recv_from(&mut buf).await {
                    if n >= 28 {
                        let _ = clock_udp.send_to(&buf[..n], peer).await;
                    }
                }
            });

            let (mut stream, _) =
                tokio::time::timeout(std::time::Duration::from_secs(5), tcp.accept())
                    .await
                    .expect("control accept timeout")
                    .expect("control accept");
            let mut codec = FrameCodec::new();
            let mut read_buf = [0u8; 8192];
            let n = stream.read(&mut read_buf).await.unwrap();
            codec.feed(&read_buf[..n]);
            assert!(matches!(codec.decode_next(), Ok(Some(Message::Hello(_)))));
            let ack = Message::HelloAck(HelloAck {
                protocol_version: oaat_core::PROTOCOL_VERSION,
                endpoint_id: "mock-counter-pcm".into(),
                endpoint_name: "Mock Counter PCM".into(),
                capabilities: EndpointCapabilities {
                    pcm_max_rate: 96_000,
                    pcm_max_bits: 24,
                    dsd_max_rate: None,
                    channels_max: 2,
                    formats: vec![AudioFormat::PcmS16le, AudioFormat::PcmS24le],
                    volume: None,
                    gapless: true,
                    seek: false,
                },
                audio_port,
                clock_port,
                buffer_size_ms: 100,
            });
            stream.write_all(&FrameCodec::encode(&ack)).await.unwrap();

            let mut propositions = Vec::new();
            let mut got_play = false;
            let mut stopped = false;
            loop {
                let n = match tokio::time::timeout(
                    std::time::Duration::from_secs(8),
                    stream.read(&mut read_buf),
                )
                .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(n)) => n,
                };
                codec.feed(&read_buf[..n]);
                while let Ok(Some(message)) = codec.decode_next() {
                    match message {
                        Message::FormatPropose(proposee) => {
                            propositions.push(proposee.clone());
                            let contre = Message::FormatCounter(FormatCounter {
                                stream_id: proposee.stream_id,
                                format: AudioFormat::PcmS16le,
                                sample_rate: 48_000,
                                channels: 1,
                                channel_layout: ChannelLayout::Mono,
                                bits_per_sample: 16,
                                dsd_rate: None,
                            });
                            stream
                                .write_all(&FrameCodec::encode(&contre))
                                .await
                                .unwrap();
                        }
                        Message::Play(_) => got_play = true,
                        Message::Stop(_) => {
                            stopped = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if stopped {
                    break;
                }
            }
            let packets = audio_rx.await.unwrap();
            (propositions, got_play, packets)
        });

        let frames = 4_800u32;
        let mut pcm = Vec::with_capacity(frames as usize * 6);
        for frame in 0..frames {
            let left =
                (((frame as i32 * 997) & 0x7f_ffff) - 0x40_0000).clamp(-0x7f_ffff, 0x7f_ffff);
            for sample in [left, -left] {
                pcm.extend_from_slice(&sample.to_le_bytes()[..3]);
            }
        }
        let mut wav = Vec::with_capacity(44 + pcm.len());
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&96_000u32.to_le_bytes());
        wav.extend_from_slice(&(96_000u32 * 6).to_le_bytes());
        wav.extend_from_slice(&6u16.to_le_bytes());
        wav.extend_from_slice(&24u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
        wav.extend_from_slice(&pcm);

        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http.local_addr().unwrap().port();
        let http_task = tokio::spawn(async move {
            let (mut socket, _) = http.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                wav.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&wav).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let output = OaatOutput::new(
            "Mock Counter PCM".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-counter-pcm".into(),
        );
        let url = format!("http://127.0.0.1:{http_port}/counter.wav");
        output
            .play_media(&PlayMedia {
                url: &url,
                mime_type: "audio/wav",
                title: Some("Counter contract"),
                duration_ms: Some(50),
                ..Default::default()
            })
            .await
            .unwrap();

        let (propositions, got_play, packets) =
            tokio::time::timeout(std::time::Duration::from_secs(12), endpoint)
                .await
                .expect("endpoint did not finish")
                .unwrap();
        http_task.await.unwrap();

        assert_eq!(propositions.len(), 1);
        assert_eq!(propositions[0].format, AudioFormat::PcmS24le);
        assert_eq!(propositions[0].sample_rate, 96_000);
        assert_eq!(propositions[0].bits_per_sample, 24);
        assert_eq!(propositions[0].channels, 2);
        assert!(got_play, "la conversion doit être prête avant Play");

        let audio: Vec<_> = packets
            .iter()
            .filter(|(_, payload)| !payload.is_empty())
            .collect();
        assert!(!audio.is_empty());
        let first_pts = audio[0].0.pts_ns;
        let mut payload = Vec::new();
        for (header, bytes) in &audio {
            assert_eq!(header.format, AudioFormat::PcmS16le);
            assert_eq!(bytes.len() % 2, 0, "paquet mono 16-bit mal aligné");
            let expected_delta = header.sample_offset * 1_000_000_000 / 48_000;
            assert!(
                header.pts_ns.abs_diff(first_pts + expected_delta) <= 1,
                "PTS incohérent pour offset {}",
                header.sample_offset
            );
            payload.extend_from_slice(bytes);
        }
        assert_eq!(payload.len(), 2_400 * 2, "50 ms à 48 kHz mono 16-bit");
        assert!(
            payload.iter().all(|byte| *byte == 0),
            "L=-R doit donner un downmix mono nul byte-for-byte"
        );
        let (last, last_payload) = packets.last().unwrap();
        assert!(last.flags.contains(PacketFlags::LAST_PACKET));
        assert!(
            !last_payload.is_empty(),
            "LAST doit etre porte par le dernier payload audio reel"
        );
        assert_eq!(last_payload.len() % 2, 0);
        assert_eq!(
            last.sample_offset + (last_payload.len() / 2) as u64,
            2_400,
            "l offset de debut et le dernier payload doivent couvrir exactement 50 ms"
        );
        output.stop().await.ok();
    }

    #[tokio::test]
    async fn format_counter_impossible_reste_fail_closed_jusqu_au_reseau() {
        use oaat_core::format::{AudioFormat, ChannelLayout};

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        let endpoint = tokio::spawn(async move {
            let audio_rx = tokio::spawn(async move {
                let mut datagram = [0u8; 8192];
                usize::from(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        audio_udp.recv(&mut datagram),
                    )
                    .await
                    .is_ok(),
                )
            });
            let _clock = tokio::spawn(async move {
                let mut buf = [0u8; 64];
                while let Ok((n, peer)) = clock_udp.recv_from(&mut buf).await {
                    if n >= 28 {
                        let _ = clock_udp.send_to(&buf[..n], peer).await;
                    }
                }
            });

            let (mut stream, _) = tcp.accept().await.unwrap();
            let mut codec = FrameCodec::new();
            let mut read_buf = [0u8; 8192];
            let n = stream.read(&mut read_buf).await.unwrap();
            codec.feed(&read_buf[..n]);
            assert!(matches!(codec.decode_next(), Ok(Some(Message::Hello(_)))));
            let ack = Message::HelloAck(HelloAck {
                protocol_version: oaat_core::PROTOCOL_VERSION,
                endpoint_id: "mock-impossible".into(),
                endpoint_name: "Mock Impossible".into(),
                capabilities: EndpointCapabilities {
                    pcm_max_rate: 192_000,
                    pcm_max_bits: 32,
                    dsd_max_rate: None,
                    channels_max: 2,
                    formats: vec![AudioFormat::Flac],
                    volume: None,
                    gapless: false,
                    seek: false,
                },
                audio_port,
                clock_port,
                buffer_size_ms: 100,
            });
            stream.write_all(&FrameCodec::encode(&ack)).await.unwrap();

            let mut got_play = false;
            loop {
                let n = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    stream.read(&mut read_buf),
                )
                .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(n)) => n,
                };
                codec.feed(&read_buf[..n]);
                while let Ok(Some(message)) = codec.decode_next() {
                    match message {
                        Message::FormatPropose(proposee) => {
                            let impossible = Message::FormatCounter(FormatCounter {
                                stream_id: proposee.stream_id,
                                format: AudioFormat::Flac,
                                sample_rate: proposee.sample_rate,
                                channels: proposee.channels,
                                channel_layout: ChannelLayout::Stereo,
                                bits_per_sample: proposee.bits_per_sample,
                                dsd_rate: None,
                            });
                            stream
                                .write_all(&FrameCodec::encode(&impossible))
                                .await
                                .unwrap();
                        }
                        Message::Play(_) => got_play = true,
                        _ => {}
                    }
                }
            }
            (got_play, audio_rx.await.unwrap())
        });

        let wav = make_test_wav();
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http.local_addr().unwrap().port();
        let http_task = tokio::spawn(async move {
            let (mut socket, _) = http.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                wav.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&wav).await.unwrap();
            socket.shutdown().await.unwrap();
        });

        let output = OaatOutput::new(
            "Mock Impossible".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-impossible".into(),
        );
        let url = format!("http://127.0.0.1:{http_port}/impossible.wav");
        output
            .play_media(&PlayMedia {
                url: &url,
                mime_type: "audio/wav",
                title: Some("Impossible counter"),
                ..Default::default()
            })
            .await
            .unwrap();

        let (got_play, audio_datagrams) =
            tokio::time::timeout(std::time::Duration::from_secs(10), endpoint)
                .await
                .expect("endpoint did not finish")
                .unwrap();
        http_task.await.unwrap();
        assert!(
            !got_play,
            "un FormatCounter sans pipeline ne doit jamais atteindre Play"
        );
        assert_eq!(
            audio_datagrams, 0,
            "aucun octet audio ne doit partir après le refus"
        );
        let failure = output
            .take_output_failure()
            .expect("le refus doit remonter au poller/client");
        assert!(failure.contains("ne passe pas par le convertisseur PCM entier"));
        output.stop().await.ok();
    }

    /// A gapless transition that CHANGES FORMAT must restart the stream.
    ///
    /// Proposing a format makes the endpoint tear its output down and clear its
    /// `started` flag; from then on it drops every packet until a `Play`
    /// arrives. Tune used to renegotiate and keep streaming without one, so the
    /// first transition between two different formats killed the sound for
    /// good — while the position kept advancing and neither log said anything
    /// (.18, 8 Aug 2026: silent from the first 44.1 -> 96 kHz transition).
    ///
    /// The assertion is on the ORDER: a Play must follow the second
    /// FormatPropose, not merely appear somewhere in the session.
    /// RÉACTIVÉ le 2026-08-13 — la course de #1358 est comprise et corrigée.
    ///
    /// Deux défauts distincts, prouvés par sondes horodatées :
    ///
    /// 1. LE MOCK (cause principale des échecs) : il ne lisait jamais la
    ///    requête GET avant d'écrire sa réponse et de lâcher la socket. Fermer
    ///    avec des octets reçus non lus fait répondre un RST, et un RST
    ///    détruit les octets de réponse encore en vol : le client perdait la
    ///    queue du corps (~34 Ko mesurés) et n'obtenait JAMAIS l'EOF —
    ///    `stream.next()` restait pendu, la transition n'arrivait pas, et le
    ///    mock rendait son verdict sur son délai d'inactivité (les ~9 s des
    ///    passes en échec = dernier message + 6 s). La taille de la perte
    ///    dépendait du retard de consommation (lecture pacée en temps réel) :
    ///    ~1/5 en local, systématique sur un runner lent. D'où l'échec des
    ///    tentatives passées : allonger le délai du mock n'apportait rien (la
    ///    queue ne viendra jamais), et retarder la mise en file aggravait (plus
    ///    d'octets en attente au moment du close).
    ///
    /// 2. LA PRODUCTION (durcie au passage) : le résultat du préchargement
    ///    transite par un oneshot vers un bras du `select!` ; mesuré prêt 2 s
    ///    avant l'EOF et jamais consommé — le tirage aléatoire servait le bras
    ///    flux à chaque itération et l'EOF ne consultait que `next_track`,
    ///    encore vide. Corrigé par `biased` (bras commande/prefetch avant le
    ///    flux) + rattrapage borné d'un préchargement en vol à l'EOF.
    ///
    /// Validé : 40 passes consécutives vertes (5,02-5,07 s, l'ancien mode 9 s
    /// a disparu). NE PAS affaiblir les deux assertions : elles gardent une
    /// panne totale et silencieuse du son (#1333).
    #[tokio::test]
    async fn oaat_format_change_gapless_transition_restarts_the_stream() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("tune_core::outputs::oaat=debug")
            .with_test_writer()
            .try_init();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        // Records the control messages in the order they arrive.
        let mock_handle = tokio::spawn(async move {
            use oaat_core::format::AudioFormat;
            use oaat_core::wire::PacketFlags;

            let mut seen: Vec<String> = Vec::new();

            let audio_rx = tokio::spawn(async move {
                let mut datagram = vec![0u8; 8192];
                let mut packets = Vec::new();
                let mut last_count = 0;
                loop {
                    let Ok(Ok(n)) = tokio::time::timeout(
                        std::time::Duration::from_secs(8),
                        audio_udp.recv(&mut datagram),
                    )
                    .await
                    else {
                        break;
                    };
                    if n < AUDIO_HEADER_SIZE {
                        continue;
                    }
                    let header_bytes: [u8; AUDIO_HEADER_SIZE] =
                        datagram[..AUDIO_HEADER_SIZE].try_into().unwrap();
                    let header = AudioPacketHeader::decode(&header_bytes).unwrap();
                    let payload = datagram[AUDIO_HEADER_SIZE..n].to_vec();
                    if header.flags.contains(PacketFlags::LAST_PACKET) {
                        last_count += 1;
                    }
                    packets.push((header, payload));
                    if last_count == 2 {
                        break;
                    }
                }
                packets
            });

            let _clock_handle = tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    match clock_udp.recv_from(&mut buf).await {
                        Ok((n, peer)) if n >= 28 => {
                            let _ = clock_udp.send_to(&buf[..n], peer).await;
                        }
                        _ => break,
                    }
                }
            });

            if let Ok((mut stream, _)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), tcp.accept())
                    .await
                    .unwrap_or(Err(std::io::Error::other("timeout")))
            {
                let mut codec = FrameCodec::new();
                let mut read_buf = [0u8; 8192];

                let n = stream.read(&mut read_buf).await.unwrap_or(0);
                if n > 0 {
                    codec.feed(&read_buf[..n]);
                    if let Ok(Some(Message::Hello(_))) = codec.decode_next() {
                        let ack = Message::HelloAck(HelloAck {
                            protocol_version: oaat_core::PROTOCOL_VERSION,
                            endpoint_id: "mock-ep-001".into(),
                            endpoint_name: "Mock DAC".into(),
                            capabilities: EndpointCapabilities {
                                pcm_max_rate: 192000,
                                pcm_max_bits: 32,
                                dsd_max_rate: None,
                                channels_max: 2,
                                formats: vec![
                                    oaat_core::format::AudioFormat::PcmS16le,
                                    oaat_core::format::AudioFormat::PcmS24le,
                                ],
                                volume: None,
                                gapless: true,
                                seek: false,
                            },
                            audio_port,
                            clock_port,
                            buffer_size_ms: 100,
                        });
                        let _ = stream.write_all(&FrameCodec::encode(&ack)).await;

                        let mut proposal_count = 0;
                        let mut stopped = false;
                        loop {
                            let n = match tokio::time::timeout(
                                std::time::Duration::from_secs(6),
                                stream.read(&mut read_buf),
                            )
                            .await
                            {
                                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                Ok(Ok(n)) => n,
                            };
                            codec.feed(&read_buf[..n]);
                            while let Ok(Some(msg)) = codec.decode_next() {
                                match msg {
                                    Message::FormatPropose(fp) => {
                                        seen.push("FormatPropose".into());
                                        proposal_count += 1;
                                        let response = if proposal_count == 1 {
                                            Message::FormatAccept(FormatAccept {
                                                stream_id: fp.stream_id,
                                            })
                                        } else {
                                            // La seconde source est 24-bit. Le
                                            // renderer garde le contrat 16-bit
                                            // de la première piste : le chemin
                                            // gapless doit convertir AVANT Play.
                                            Message::FormatCounter(FormatCounter {
                                                stream_id: fp.stream_id,
                                                format: AudioFormat::PcmS16le,
                                                sample_rate: fp.sample_rate,
                                                channels: fp.channels,
                                                channel_layout: fp.channel_layout,
                                                bits_per_sample: 16,
                                                dsd_rate: None,
                                            })
                                        };
                                        let _ =
                                            stream.write_all(&FrameCodec::encode(&response)).await;
                                    }
                                    Message::Play(_) => seen.push("Play".into()),
                                    Message::Stop(_) => {
                                        stopped = true;
                                        break;
                                    }
                                    _ => {}
                                }
                            }
                            if stopped {
                                break;
                            }
                        }
                    }
                }
            }
            let packets = audio_rx.await.unwrap();
            (seen, packets)
        });

        // Track 1 is 16-bit; track 2 is 24-bit, so the transition must
        // renegotiate. One listener serves both requests in order.
        let wav16 = make_test_wav_sized(16, 2_000);
        let wav24 = make_test_wav_sized(24, 500);
        let http_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_tcp.local_addr().unwrap().port();
        let http_handle = tokio::spawn(async move {
            for body in [wav16, wav24] {
                if let Ok((mut s, _)) = http_tcp.accept().await {
                    // Consommer la requête AVANT de répondre et de fermer.
                    // C'était la vraie instabilité de ce test (#1358) : fermer
                    // une socket avec des octets reçus jamais lus (le GET)
                    // fait répondre un RST par le noyau, et un RST détruit les
                    // octets de réponse encore en vol — le client perdait la
                    // queue du corps (~34 Ko mesurés) et n'obtenait jamais
                    // l'EOF, donc jamais la transition. Le rythme de
                    // consommation côté client (pacé en temps réel) décidait
                    // de la taille de la perte : ~1 passe sur 5 en local,
                    // systématique sur un runner lent.
                    let mut req = [0u8; 1024];
                    let mut head: Vec<u8> = Vec::new();
                    loop {
                        match s.read(&mut req).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                head.extend_from_slice(&req[..n]);
                                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(hdr.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                    // FIN propre (pas de drop brutal) : le client lit tout le
                    // corps puis voit une fin de flux normale.
                    let _ = s.shutdown().await;
                }
            }
        });

        let output = OaatOutput::new(
            "Mock DAC".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-ep-001".into(),
        );

        let url1 = format!("http://127.0.0.1:{http_port}/one.wav");
        output
            .play_media(&PlayMedia {
                url: &url1,
                mime_type: "audio/wav",
                title: Some("One"),
                ..Default::default()
            })
            .await
            .expect("first track should start");

        // Stage the differing next track, then let the first one run out.
        let url2 = format!("http://127.0.0.1:{http_port}/two.wav");
        output
            .set_next_media(&PlayMedia {
                url: &url2,
                mime_type: "audio/wav",
                title: Some("Two"),
                ..Default::default()
            })
            .await
            .ok();

        let (seen, packets) = tokio::time::timeout(std::time::Duration::from_secs(12), mock_handle)
            .await
            .expect("mock timed out")
            .expect("mock panicked");

        let proposals = seen.iter().filter(|m| *m == "FormatPropose").count();
        assert!(
            proposals >= 2,
            "the format change should have been renegotiated, saw: {seen:?}"
        );

        let last_proposal = seen
            .iter()
            .rposition(|m| m == "FormatPropose")
            .expect("checked above");
        assert!(
            seen[last_proposal..].iter().any(|m| m == "Play"),
            "a Play must follow the format renegotiation, or the endpoint drops \
             every packet and plays nothing — saw: {seen:?}"
        );

        let first_last = packets
            .iter()
            .position(|(header, _)| {
                header
                    .flags
                    .contains(oaat_core::wire::PacketFlags::LAST_PACKET)
            })
            .expect("la première piste doit se fermer");
        let second_packets = &packets[first_last + 1..];
        let second_audio: Vec<_> = second_packets
            .iter()
            .filter(|(_, payload)| !payload.is_empty())
            .collect();
        assert!(
            !second_audio.is_empty(),
            "la piste gapless doit émettre du PCM"
        );
        assert!(
            second_audio.iter().all(|(header, payload)| {
                header.format == oaat_core::format::AudioFormat::PcmS16le && payload.len() % 4 == 0
            }),
            "la seconde piste 24-bit doit être réellement paquetisée en 16-bit stéréo"
        );
        assert_eq!(second_audio[0].0.sample_offset, 0);
        let second_payload_len: usize = second_audio.iter().map(|(_, payload)| payload.len()).sum();
        assert_eq!(
            second_payload_len, 88_200,
            "500 ms à 44,1 kHz, 16-bit stéréo doivent produire exactement 88 200 octets"
        );
        assert!(
            second_audio
                .iter()
                .all(|(_, payload)| payload.iter().all(|byte| *byte == 0)),
            "le témoin silencieux doit rester nul byte-for-byte après conversion"
        );
        let (second_last, second_last_payload) = second_packets.last().expect("LAST piste 2");
        assert!(
            second_last
                .flags
                .contains(oaat_core::wire::PacketFlags::LAST_PACKET)
        );
        assert!(
            !second_last_payload.is_empty(),
            "LAST piste 2 doit etre porte par le dernier payload audio reel"
        );
        assert_eq!(second_last_payload.len() % 4, 0);
        assert_eq!(
            second_last.sample_offset + (second_last_payload.len() / 4) as u64,
            22_050,
            "l offset de debut et le dernier payload doivent couvrir exactement 500 ms"
        );

        output.stop().await.ok();
        http_handle.abort();
    }

    #[tokio::test]
    async fn oaat_diagnostics_initial_state() {
        let output = OaatOutput::new("Test".into(), "127.0.0.1".into(), 9999, "id".into());
        let d = output.diagnostics_snapshot();
        assert_eq!(d["packets_sent"], 0);
        assert_eq!(d["bytes_sent"], 0);
        assert!(!d["connected"].as_bool().unwrap());
        assert!(!d["playing"].as_bool().unwrap());
        assert!(!d["stall_detected"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn oaat_is_available_always_true() {
        let output = OaatOutput::new("Test".into(), "127.0.0.1".into(), 9999, "id".into());
        assert!(output.is_available().await);
    }

    /// Xavier / Zicmu native-DSD album gapless: OAAT always supports internal
    /// gapless (both PCM/HTTP url-swap and native-DSD reader-swap), so the poller
    /// arms the transition in both modes. While streaming native DSD it ALSO
    /// prefers the local-file gapless path (so prepare_gapless stages the next
    /// `.dsf` directly, with no orphaned DSD->PCM transcode). stop() (run before
    /// every play_media) clears native-DSD mode so a following PCM track goes
    /// back to the url-prefetch path.
    #[tokio::test]
    async fn oaat_gapless_capabilities_reflect_native_dsd() {
        let output = OaatOutput::new("Test".into(), "127.0.0.1".into(), 9999, "id".into());

        // OAAT always reports internal gapless (the poller must arm it).
        assert!(output.supports_internal_gapless());

        // Default (PCM/HTTP mode): stage the next track as a transcoded URL.
        assert!(
            !output.prefers_local_file_gapless(),
            "PCM/HTTP mode must use the url-prefetch gapless path"
        );

        // Native DSD streaming: stage the next track as a local .dsf file.
        // Et tant qu'on est en PCM, la sortie mesure : les VU ont une source.
        assert!(
            !output.is_native_dsd_active(),
            "en PCM la sortie n'est pas en DSD natif"
        );
        output.set_native_dsd_active_for_test(true);
        // DSD natif : plus personne ne décode, donc plus aucune fenêtre de
        // niveaux. C'est ce que `levels_available` doit annoncer à l'écran,
        // faute de quoi l'aiguille reste figée et se lit comme une panne.
        assert!(
            output.is_native_dsd_active(),
            "le DSD natif doit se déclarer : sans mesure, l'écran doit le dire"
        );
        assert!(output.supports_internal_gapless());

        assert!(
            output.prefers_local_file_gapless(),
            "native DSD must use the local-file gapless path (no transcode session)"
        );

        // PCM direct-file playback: the loop now stages the next local file and
        // swaps buffers at EOF, so gapless must be armed AND the next track
        // staged as a local file — a transcode URL would be useless to it.
        output.set_direct_pcm_active_for_test(true);
        assert!(
            output.supports_internal_gapless(),
            "direct PCM playback chains internally — gapless must be armed"
        );
        assert!(
            output.prefers_local_file_gapless(),
            "direct PCM playback swaps local PCM buffers, not transcode URLs"
        );

        // The #1006 guarantee, in its new form: when the loop reaches an end
        // with nothing to chain into (next track not local, format change,
        // decode failure) it raises `chain_exhausted`, and the queue returns to
        // the poller's natural-end advance. Without this the poller would sit
        // out its guard waiting for a transition that cannot come — silence,
        // then the same track replayed (local→local on an OAAT zone, 29/07).
        output.set_chain_exhausted_for_test(true);
        assert!(
            !output.supports_internal_gapless(),
            "an exhausted chain must hand the queue back to the poller"
        );

        output.set_chain_exhausted_for_test(false);
        output.set_direct_pcm_active_for_test(false);
        assert!(output.supports_internal_gapless());

        // stop() leaves native-DSD mode (runs before the next play_media).
        output.stop().await.ok();
        assert!(
            !output.prefers_local_file_gapless(),
            "after stop(), the next (PCM) track must return to url-prefetch gapless"
        );
    }

    /// La fin d'un morceau ne doit pas être prise pour une panne de flux — et
    /// une boucle qui s'arrête doit le DIRE au poller.
    ///
    /// Xavier Joly (#1323), 7 août 2026, endpoint OAAT + DAC SMSL : Toccata puis
    /// Fugue, ALAC transcodé en WAV progressif. Le gapless était armé
    /// (`next track prefetched (gapless ready)`), puis contourné, et il
    /// s'écoulait ~83 s de silence avant que la Fugue reparte à froid.
    ///
    /// Le banc reproduit la cause exacte : `StreamInfo::wav_content_length()`
    /// annonce une taille PRÉDITE depuis la durée en bibliothèque, le décodeur
    /// produit le nombre exact d'échantillons du fichier, et les deux ne
    /// coïncident jamais. Le corps se termine donc AVANT son `Content-Length`
    /// annoncé, ce qui remonte en `error decoding response body` — au moment
    /// précis de la fin du morceau. Le mock annonce ici 20 000 octets de plus
    /// qu'il n'en envoie, puis ferme proprement.
    ///
    /// Deux propriétés, une par défaut :
    ///
    /// 1. **Une seule requête HTTP, sans `Range`.** Une reprise par `Range`
    ///    prouverait que la fin de piste est classée comme panne. Elle ne peut
    ///    rien resservir (les octets demandés n'existent pas), échoue, et fait
    ///    sortir la boucle par un chemin qui saute la transition gapless.
    ///
    /// 2. **`supports_internal_gapless()` rend `false` une fois la boucle
    ///    terminée.** Le poller relit cette réponse pendant qu'il attend ; tant
    ///    qu'elle vaut `true`, il attend une transition d'une tâche qui n'existe
    ///    plus — les 34 s entre `gapless_natural_end_waiting_for_transition`
    ///    (16:34:06) et `oaat: stop` (16:34:40) dans son journal.
    ///
    /// Le mock ferme par `shutdown()` APRÈS avoir lu la requête : fermer avec
    /// des octets non lus déclenche un RST, qui détruit la réponse en vol et
    /// rend le test instable (#1358).
    #[tokio::test]
    async fn track_end_is_not_a_stream_failure_and_releases_the_poller() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        // Endpoint factice : Hello/HelloAck, puis FormatAccept sur proposition.
        let endpoint_handle = tokio::spawn(async move {
            let _audio_drain = tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                while (tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    audio_udp.recv(&mut buf),
                )
                .await)
                    .is_ok()
                {}
            });
            let _clock = tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    match clock_udp.recv_from(&mut buf).await {
                        Ok((n, peer)) if n >= 28 => {
                            let _ = clock_udp.send_to(&buf[..n], peer).await;
                        }
                        _ => break,
                    }
                }
            });

            let Ok(Ok((mut stream, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(10), tcp.accept()).await
            else {
                return;
            };
            let mut codec = FrameCodec::new();
            let mut read_buf = [0u8; 8192];
            let n = stream.read(&mut read_buf).await.unwrap_or(0);
            if n == 0 {
                return;
            }
            codec.feed(&read_buf[..n]);
            if !matches!(codec.decode_next(), Ok(Some(Message::Hello(_)))) {
                return;
            }
            let ack = Message::HelloAck(HelloAck {
                protocol_version: oaat_core::PROTOCOL_VERSION,
                endpoint_id: "mock-ep-1323".into(),
                endpoint_name: "Mock SMSL".into(),
                capabilities: EndpointCapabilities {
                    pcm_max_rate: 192000,
                    pcm_max_bits: 32,
                    dsd_max_rate: None,
                    channels_max: 2,
                    formats: vec![
                        oaat_core::format::AudioFormat::PcmS16le,
                        oaat_core::format::AudioFormat::PcmS24le,
                    ],
                    volume: None,
                    gapless: true,
                    seek: false,
                },
                audio_port,
                clock_port,
                buffer_size_ms: 100,
            });
            let _ = stream.write_all(&FrameCodec::encode(&ack)).await;

            loop {
                let n = match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    stream.read(&mut read_buf),
                )
                .await
                {
                    Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                    Ok(Ok(n)) => n,
                };
                codec.feed(&read_buf[..n]);
                while let Ok(Some(msg)) = codec.decode_next() {
                    if let Message::FormatPropose(fp) = msg {
                        let accept = Message::FormatAccept(FormatAccept {
                            stream_id: fp.stream_id,
                        });
                        let _ = stream.write_all(&FrameCodec::encode(&accept)).await;
                    }
                }
            }
        });

        // 500 ms de PCM 44,1/16/2 : sous le seuil de 50 paquets qui déclenche
        // le cadencement temps réel, donc le morceau s'écoule vite.
        let track_ms = 500u32;
        let wav = make_test_wav_sized(16, track_ms);

        let http_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_tcp.local_addr().unwrap().port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requests_srv = requests.clone();
        let http_handle = tokio::spawn(async move {
            loop {
                let Ok(Ok((mut s, _))) =
                    tokio::time::timeout(std::time::Duration::from_secs(20), http_tcp.accept())
                        .await
                else {
                    break;
                };
                // Lire la requête ENTIÈRE avant de répondre (sinon RST, #1358).
                let mut req = Vec::new();
                let mut byte = [0u8; 1];
                while !req.ends_with(b"\r\n\r\n") {
                    match s.read(&mut byte).await {
                        Ok(1) => req.push(byte[0]),
                        _ => break,
                    }
                }
                requests_srv
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req).into_owned());

                // Content-Length PRÉDIT : plus long que le corps réellement
                // servi, exactement comme wav_content_length() le calcule
                // depuis la durée en bibliothèque.
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                    wav.len() + 20_000
                );
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&wav).await;
                let _ = s.shutdown().await;
            }
        });

        let output = OaatOutput::new(
            "Mock SMSL".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-ep-1323".into(),
        );
        assert!(
            output.supports_internal_gapless(),
            "au repos, la sortie doit annoncer le gapless interne — sinon le poller ne l'arme jamais"
        );

        let url = format!("http://127.0.0.1:{http_port}/toccata.wav");
        output
            .play_media(&PlayMedia {
                url: &url,
                mime_type: "audio/wav",
                title: Some("Toccata"),
                // La durée vient de la BIBLIOTHÈQUE, pas de l'en-tête : c'est
                // elle qui permet de reconnaître la fin (#1365).
                duration_ms: Some(track_ms as u64),
                ..Default::default()
            })
            .await
            .expect("play_media");

        // Attendre la fin de la lecture (borné).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
        while std::time::Instant::now() < deadline {
            if !output.diagnostics_snapshot()["playing"]
                .as_bool()
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        let seen = requests.lock().unwrap().clone();
        assert!(
            !seen.is_empty(),
            "le flux n'a jamais été demandé — le banc n'a rien prouvé"
        );
        assert!(
            !seen
                .iter()
                .any(|r| r.to_ascii_lowercase().contains("range:")),
            "la fin du morceau a été prise pour une panne : une reprise par Range a été tentée ({} requêtes) — {seen:?}",
            seen.len()
        );
        assert_eq!(
            seen.len(),
            1,
            "une fin propre ne doit provoquer aucune re-requête — {seen:?}"
        );

        assert!(
            !output.supports_internal_gapless(),
            "la boucle est terminée : sans le dire au poller, il attend une transition d'une tâche qui n'existe plus (34 s puis redémarrage à froid, #1323)"
        );

        output.stop().await.ok();
        assert!(
            output.supports_internal_gapless(),
            "stop() doit réarmer le gapless pour la lecture suivante"
        );
        endpoint_handle.abort();
        http_handle.abort();
    }

    fn make_test_wav() -> Vec<u8> {
        let sr = 44100u32;
        let ch = 2u16;
        let bits = 16u16;
        let duration_samples = sr / 5; // 200ms
        let data_size = duration_samples * ch as u32 * (bits as u32 / 8);
        let byte_rate = sr * ch as u32 * bits as u32 / 8;
        let block_align = ch * bits / 8;

        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_size).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&sr.to_le_bytes());
        b.extend_from_slice(&byte_rate.to_le_bytes());
        b.extend_from_slice(&block_align.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_size.to_le_bytes());
        b.resize(b.len() + data_size as usize, 0);
        b
    }

    /// Same shape as `make_test_wav`, with the bit depth and length chosen by
    /// the caller: a transition between two different depths forces a format
    /// renegotiation, and the first track must last long enough for the next
    /// one to finish prefetching before it ends.
    fn make_test_wav_sized(bits: u16, millis: u32) -> Vec<u8> {
        let sr = 44100u32;
        let ch = 2u16;
        let duration_samples = sr * millis / 1000;
        let data_size = duration_samples * ch as u32 * (bits as u32 / 8);
        let byte_rate = sr * ch as u32 * bits as u32 / 8;
        let block_align = ch * bits / 8;

        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + data_size).to_le_bytes());
        b.extend_from_slice(b"WAVE");
        b.extend_from_slice(b"fmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&sr.to_le_bytes());
        b.extend_from_slice(&byte_rate.to_le_bytes());
        b.extend_from_slice(&block_align.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&data_size.to_le_bytes());
        b.resize(b.len() + data_size as usize, 0);
        b
    }

    /// Sustained streaming test: 5 seconds of audio, verify no packet loss
    /// and position tracking stays accurate.
    /// Ignored in CI — timing-sensitive, flaky on shared runners.
    #[tokio::test]
    #[ignore]
    async fn oaat_sustained_stream_no_drift() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        let audio_socket = std::sync::Arc::new(audio_udp);
        let packet_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pc = packet_count.clone();

        // Audio receiver — count packets until done
        let audio_rx = {
            let s = audio_socket.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match tokio::time::timeout(std::time::Duration::from_secs(10), s.recv(&mut buf))
                        .await
                    {
                        Ok(Ok(n)) if n >= AUDIO_HEADER_SIZE => {
                            pc.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                        _ => break,
                    }
                }
            })
        };

        // Clock responder
        let _clock = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                match clock_udp.recv_from(&mut buf).await {
                    Ok((n, peer)) if n >= 28 => {
                        let _ = clock_udp.send_to(&buf[..n], peer).await;
                    }
                    _ => break,
                }
            }
        });

        // Mock endpoint
        let mock = tokio::spawn(async move {
            if let Ok((mut stream, _)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), tcp.accept())
                    .await
                    .unwrap_or(Err(std::io::Error::other("timeout")))
            {
                let mut codec = FrameCodec::new();
                let mut read_buf = [0u8; 8192];
                let n = stream.read(&mut read_buf).await.unwrap_or(0);
                if n > 0 {
                    codec.feed(&read_buf[..n]);
                    if let Ok(Some(Message::Hello(_))) = codec.decode_next() {
                        let ack = Message::HelloAck(HelloAck {
                            protocol_version: oaat_core::PROTOCOL_VERSION,
                            endpoint_id: "mock-ep".into(),
                            endpoint_name: "Mock DAC".into(),
                            capabilities: EndpointCapabilities {
                                pcm_max_rate: 192000,
                                pcm_max_bits: 32,
                                dsd_max_rate: None,
                                channels_max: 2,
                                formats: vec![oaat_core::format::AudioFormat::PcmS16le],
                                volume: None,
                                gapless: true,
                                seek: false,
                            },
                            audio_port,
                            clock_port,
                            buffer_size_ms: 100,
                        });
                        let _ = stream.write_all(&FrameCodec::encode(&ack)).await;

                        // Read control messages until disconnect
                        loop {
                            let n = match tokio::time::timeout(
                                std::time::Duration::from_secs(15),
                                stream.read(&mut read_buf),
                            )
                            .await
                            {
                                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                Ok(Ok(n)) => n,
                            };
                            codec.feed(&read_buf[..n]);
                            while let Ok(Some(msg)) = codec.decode_next() {
                                if let Message::FormatPropose(fp) = msg {
                                    let accept = Message::FormatAccept(FormatAccept {
                                        stream_id: fp.stream_id,
                                    });
                                    let _ = stream.write_all(&FrameCodec::encode(&accept)).await;
                                }
                            }
                        }
                    }
                }
            }
        });

        // Generate 5 seconds of WAV audio
        let sr = 44100u32;
        let duration_s = 5u32;
        let data_size = sr * 4 * duration_s; // 16-bit stereo
        let wav = {
            let mut b = Vec::new();
            b.extend_from_slice(b"RIFF");
            b.extend_from_slice(&(36 + data_size).to_le_bytes());
            b.extend_from_slice(b"WAVE");
            b.extend_from_slice(b"fmt ");
            b.extend_from_slice(&16u32.to_le_bytes());
            b.extend_from_slice(&1u16.to_le_bytes());
            b.extend_from_slice(&2u16.to_le_bytes());
            b.extend_from_slice(&sr.to_le_bytes());
            b.extend_from_slice(&(sr * 4).to_le_bytes());
            b.extend_from_slice(&4u16.to_le_bytes());
            b.extend_from_slice(&16u16.to_le_bytes());
            b.extend_from_slice(b"data");
            b.extend_from_slice(&data_size.to_le_bytes());
            b.resize(b.len() + data_size as usize, 0);
            b
        };

        // HTTP server
        let http_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_tcp.local_addr().unwrap().port();
        let http = tokio::spawn(async move {
            if let Ok((mut s, _)) = http_tcp.accept().await {
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                    wav.len()
                );
                let _ = s.write_all(hdr.as_bytes()).await;
                let _ = s.write_all(&wav).await;
            }
        });

        let output = OaatOutput::new(
            "Mock DAC".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-ep".into(),
        );

        let url = format!("http://127.0.0.1:{http_port}/test.wav");
        output
            .play_media(&PlayMedia {
                url: &url,
                mime_type: "audio/wav",
                title: Some("Load Test"),
                ..Default::default()
            })
            .await
            .unwrap();

        // Wait for streaming to complete (5s audio + margin)
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;

        let packets = packet_count.load(std::sync::atomic::Ordering::Relaxed);
        let diag = output.diagnostics_snapshot();

        // At 44100Hz / 480 samples per packet = ~91.9 packets/sec → ~460 packets for 5s
        assert!(
            packets >= 400,
            "expected >=400 packets for 5s stream, got {packets}"
        );

        // Position should be near 5000ms
        let pos = diag["position_ms"].as_u64().unwrap_or(0);
        assert!(pos >= 4000, "position should be near 5000ms, got {pos}ms");

        // No stall detected
        assert!(
            !diag["stall_detected"].as_bool().unwrap_or(true),
            "stall detected during sustained stream"
        );

        output.stop().await.ok();
        http.abort();
        mock.abort();
        audio_rx.abort();
    }

    /// #1475 — la boucle de connexion d'une lecture doit CESSER à l'arrêt.
    ///
    /// `play_media` détache une tâche qui tente la connexion quinze fois. Rien
    /// ne l'arrêtait : `stop()` envoyait bien un signal, mais la boucle ne
    /// l'écoute pas — c'est un `for` avec des pauses. Elle continuait donc à
    /// réclamer l'endpoint pendant une quarantaine de secondes, et volait la
    /// connexion à la lecture suivante. Sur un endpoint mono-client, deux
    /// boucles concurrentes font repartir la piste de zéro en boucle
    /// (constaté sur .42 le 2026-08-11 : `attempt=1` et `attempt=9` à la même
    /// seconde, deux sockets vers le même endpoint).
    ///
    /// Le test compte les connexions entrantes : elles doivent se figer après
    /// `stop()`. Sans le correctif, le compteur continue de monter.
    #[tokio::test]
    async fn stop_cancels_the_connection_retry_loop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        // Un faux endpoint qui ACCEPTE puis raccroche aussitôt : la connexion
        // échoue vite (pas d'attente de 3 s), et chaque tentative est comptée.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let attempts = Arc::new(AtomicU32::new(0));
        let counter = attempts.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => break,
                }
            }
        });

        let output = OaatOutput::new("Mock".into(), "127.0.0.1".into(), port, "ep".into());
        let _ = output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:1/none.wav",
                mime_type: "audio/wav",
                title: Some("Test"),
                ..Default::default()
            })
            .await;

        // Laisser la boucle enchaîner quelques tentatives.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let before_stop = attempts.load(Ordering::SeqCst);
        assert!(
            before_stop >= 2,
            "la boucle devrait avoir tenté plusieurs connexions, vu {before_stop}"
        );

        let _ = output.stop().await;

        // Après l'arrêt, plus AUCUNE nouvelle tentative.
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
        let after_stop = attempts.load(Ordering::SeqCst);
        assert_eq!(
            after_stop, before_stop,
            "la boucle a continué après stop() : {before_stop} -> {after_stop}"
        );
    }

    // ------------------------------------------------------------------
    // #1513 — fidélité au bit près du chemin DSD natif.
    //
    // DEvir : bruit blanc en DSD64 sur endpoint OAAT (0.9.68), DAC affichant
    // pourtant DSD64, paquets reçus sans FEC/XRUN, fichier local comme UNC.
    // Le code de ce chemin est identique entre 0.9.64 et 0.9.68 ; ces tests
    // établissent donc ce que le serveur envoie RÉELLEMENT sur le fil, au
    // lieu de le supposer : chaque octet DSD reçu en UDP doit être celui du
    // fichier, dans l'ordre, sans trou ni doublon.
    // ------------------------------------------------------------------

    /// Écrit un `.dsf` synthétique : blocs par canal de `block_size` octets,
    /// motif déterministe distinct par octet (tout réordonnancement,
    /// troncature ou mélange de canaux casse la comparaison exacte).
    fn write_test_dsf(path: &std::path::Path, blocks_per_channel: usize, seed: u32) -> Vec<u8> {
        const BLOCK: usize = 4096;
        let channels = 2usize;
        let data_len = blocks_per_channel * BLOCK * channels;
        let mut data = Vec::with_capacity(data_len);
        for i in 0..data_len {
            data.push(((i as u32).wrapping_add(seed).wrapping_mul(2654435761) >> 16) as u8);
        }
        let total_samples = (blocks_per_channel * BLOCK * 8) as u64; // bits par canal

        let mut buf = Vec::new();
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&28u64.to_le_bytes());
        buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes());
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&52u64.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&2u32.to_le_bytes());
        buf.extend_from_slice(&(channels as u32).to_le_bytes());
        buf.extend_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&total_samples.to_le_bytes());
        buf.extend_from_slice(&(BLOCK as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&data);
        std::fs::write(path, &buf).unwrap();

        // Vérité terrain : ce que le lecteur produit (entrelacé par octet).
        let info = crate::audio::dsf::parse_dsf(path.to_str().unwrap()).unwrap();
        let mut reader =
            crate::audio::dsf::DsfStreamReader::open(path.to_str().unwrap(), info).unwrap();
        let mut expected = Vec::with_capacity(data_len);
        while let Ok(Some(chunk)) = reader.next_chunk() {
            expected.extend_from_slice(&chunk);
        }
        expected
    }

    /// Faux endpoint compatible DSD : HelloAck (dsd_max_rate 512), accepte
    /// tout FormatPropose, répond à l'horloge, et capture les datagrammes
    /// audio (en-tête décodé + payload) jusqu'à `quiet` sans paquet.
    async fn spawn_dsd_mock(
        quiet: std::time::Duration,
    ) -> (
        u16,
        tokio::task::JoinHandle<Vec<(oaat_core::wire::AudioPacketHeader, Vec<u8>)>>,
    ) {
        use oaat_core::wire::AudioPacketHeader;

        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_port = tcp.local_addr().unwrap().port();
        let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio_udp.local_addr().unwrap().port();
        let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = clock_udp.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let audio_rx = tokio::spawn(async move {
                let mut packets: Vec<(AudioPacketHeader, Vec<u8>)> = Vec::new();
                let mut buf = vec![0u8; 65536];
                loop {
                    // La poignée de main (Hello → horloge → FormatPropose) peut
                    // prendre plusieurs secondes AVANT le premier paquet ; le
                    // délai court de fin de flux ne vaut qu'après lui.
                    let wait = if packets.is_empty() {
                        std::time::Duration::from_secs(15)
                    } else {
                        quiet
                    };
                    match tokio::time::timeout(wait, audio_udp.recv(&mut buf)).await {
                        Ok(Ok(n)) if n >= AUDIO_HEADER_SIZE => {
                            let hdr_bytes: [u8; AUDIO_HEADER_SIZE] =
                                buf[..AUDIO_HEADER_SIZE].try_into().unwrap();
                            if let Ok(hdr) = AudioPacketHeader::decode(&hdr_bytes) {
                                packets.push((hdr, buf[AUDIO_HEADER_SIZE..n].to_vec()));
                            }
                        }
                        _ => break,
                    }
                }
                packets
            });

            let _clock = tokio::spawn(async move {
                let mut buf = [0u8; 64];
                loop {
                    match clock_udp.recv_from(&mut buf).await {
                        Ok((n, peer)) if n >= 28 => {
                            let _ = clock_udp.send_to(&buf[..n], peer).await;
                        }
                        _ => break,
                    }
                }
            });

            // Contrôle TCP : ce mock accepte les reconnexions successives —
            // une lecture qui en remplace une autre rouvre une session, comme
            // le vrai endpoint mono-client.
            let _control = tokio::spawn(async move {
                loop {
                    let Ok((mut stream, _)) = tcp.accept().await else {
                        break;
                    };
                    let audio_port = audio_port;
                    let clock_port = clock_port;
                    tokio::spawn(async move {
                        let mut codec = FrameCodec::new();
                        let mut read_buf = [0u8; 16384];
                        loop {
                            let n = match tokio::time::timeout(
                                std::time::Duration::from_secs(10),
                                stream.read(&mut read_buf),
                            )
                            .await
                            {
                                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
                                Ok(Ok(n)) => n,
                            };
                            codec.feed(&read_buf[..n]);
                            while let Ok(Some(msg)) = codec.decode_next() {
                                match msg {
                                    Message::Hello(_) => {
                                        let ack = Message::HelloAck(HelloAck {
                                            protocol_version: oaat_core::PROTOCOL_VERSION,
                                            endpoint_id: "mock-dsd-ep".into(),
                                            endpoint_name: "Mock DSD DAC".into(),
                                            capabilities: EndpointCapabilities {
                                                pcm_max_rate: 384000,
                                                pcm_max_bits: 32,
                                                dsd_max_rate: Some(512),
                                                channels_max: 2,
                                                formats: vec![
                                                    oaat_core::format::AudioFormat::PcmS16le,
                                                    oaat_core::format::AudioFormat::PcmS24le,
                                                    oaat_core::format::AudioFormat::DsdU8,
                                                ],
                                                volume: None,
                                                gapless: true,
                                                seek: false,
                                            },
                                            audio_port,
                                            clock_port,
                                            buffer_size_ms: 100,
                                        });
                                        let _ = stream.write_all(&FrameCodec::encode(&ack)).await;
                                    }
                                    Message::FormatPropose(fp) => {
                                        let accept = Message::FormatAccept(FormatAccept {
                                            stream_id: fp.stream_id,
                                        });
                                        let _ =
                                            stream.write_all(&FrameCodec::encode(&accept)).await;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    });
                }
            });

            audio_rx.await.unwrap_or_default()
        });

        (control_port, handle)
    }

    /// #1513 — lecture fraîche : les octets UDP sont EXACTEMENT ceux du
    /// fichier, dans l'ordre, sans trou ni doublon, et les `sample_offset`
    /// des en-têtes sont cohérents avec la position réelle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_dsd_bytes_reach_the_endpoint_unaltered() {
        let dir = std::env::temp_dir().join(format!("tune-dsd-proof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dsf_path = dir.join("proof.dsf");
        // 9 blocs/canal ≈ 0,21 s de DSD64 : assez court pour un test,
        // assez long pour plusieurs paquets (73 728 octets ≈ 18 paquets).
        let expected = write_test_dsf(&dsf_path, 9, 0xD5D_0001);

        let (control_port, mock) = spawn_dsd_mock(std::time::Duration::from_secs(3)).await;
        let output = OaatOutput::new(
            "Mock DSD DAC".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-dsd-ep".into(),
        );

        let path_str = dsf_path.to_str().unwrap();
        let result = output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:1/unused.wav",
                mime_type: "audio/x-dsf",
                title: Some("Proof"),
                file_path: Some(path_str),
                ..Default::default()
            })
            .await;
        assert!(result.is_ok(), "play_media: {result:?}");

        let packets = tokio::time::timeout(std::time::Duration::from_secs(20), mock)
            .await
            .expect("mock timed out")
            .expect("mock panicked");
        let _ = output.stop().await;

        assert!(
            !packets.is_empty(),
            "aucun paquet audio reçu — le chemin natif ne s'est pas engagé"
        );

        let mut received = Vec::with_capacity(expected.len());
        for (i, (hdr, payload)) in packets.iter().enumerate() {
            assert_eq!(
                hdr.format,
                oaat_core::format::AudioFormat::DsdU8,
                "paquet #{i} : pas du DSD_U8"
            );
            // sample_offset = bits par canal déjà envoyés ; les octets reçus
            // jusqu'ici couvrent received.len()/2 octets par canal.
            assert_eq!(
                hdr.sample_offset,
                (received.len() / 2) as u64 * 8,
                "sample_offset incohérent au paquet #{i} — trou ou doublon"
            );
            received.extend_from_slice(payload);
        }

        assert_eq!(
            received.len(),
            expected.len(),
            "volume reçu != volume du fichier"
        );
        assert_eq!(
            received, expected,
            "les octets DSD reçus diffèrent du fichier — corruption sur le chemin d'envoi"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1513 — lectures enchaînées (le vrai usage de DEvir : cinq sessions en
    /// douze minutes). Depuis #1481, `play_media` ANNULE la tâche précédente ;
    /// le second flux doit rester exact au bit près malgré l'abort du premier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_dsd_second_play_is_bit_exact_after_aborting_the_first() {
        let dir = std::env::temp_dir().join(format!("tune-dsd-proof2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path_a = dir.join("a.dsf");
        let path_b = dir.join("b.dsf");
        // A long (2 s) pour être sûr d'être interrompu en plein envoi ;
        // B court, au motif distinct.
        let _expected_a = write_test_dsf(&path_a, 86, 0xAAAA_0001);
        let expected_b = write_test_dsf(&path_b, 9, 0xBBBB_0002);

        let (control_port, mock) = spawn_dsd_mock(std::time::Duration::from_secs(3)).await;
        let output = OaatOutput::new(
            "Mock DSD DAC".into(),
            "127.0.0.1".into(),
            control_port,
            "mock-dsd-ep".into(),
        );

        let a = path_a.to_str().unwrap().to_owned();
        let r = output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:1/unused.wav",
                mime_type: "audio/x-dsf",
                title: Some("A"),
                file_path: Some(&a),
                ..Default::default()
            })
            .await;
        assert!(r.is_ok());

        // Laisser A émettre, puis le remplacer en plein vol.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;

        let b = path_b.to_str().unwrap().to_owned();
        let r = output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:1/unused.wav",
                mime_type: "audio/x-dsf",
                title: Some("B"),
                file_path: Some(&b),
                ..Default::default()
            })
            .await;
        assert!(r.is_ok());

        let packets = tokio::time::timeout(std::time::Duration::from_secs(25), mock)
            .await
            .expect("mock timed out")
            .expect("mock panicked");
        let _ = output.stop().await;

        // Les paquets de B = le dernier stream_id observé.
        let last_stream = packets.last().expect("aucun paquet").0.stream_id;
        let received_b: Vec<u8> = packets
            .iter()
            .filter(|(h, _)| h.stream_id == last_stream)
            .flat_map(|(_, p)| p.iter().copied())
            .collect();

        assert_eq!(
            received_b.len(),
            expected_b.len(),
            "volume du second flux != fichier B (stream {last_stream})"
        );
        assert_eq!(
            received_b, expected_b,
            "le second flux est corrompu après l'annulation du premier (#1481)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Le contrat de reference des tests de negociation : PCM 24/96 stereo.
    #[cfg(feature = "oaat")]
    fn contrat_pcm() -> crate::outputs::oaat::output::ContratPropose {
        use oaat_core::format::{AudioFormat, ChannelLayout};
        crate::outputs::oaat::output::ContratPropose {
            stream_id: "flux-1".into(),
            format: AudioFormat::PcmS24le,
            sample_rate: 96_000,
            channels: 2,
            channel_layout: ChannelLayout::Stereo,
            bits_per_sample: 24,
            dsd_rate: None,
        }
    }

    #[cfg(feature = "oaat")]
    fn contre_de(
        contrat: &crate::outputs::oaat::output::ContratPropose,
    ) -> oaat_core::message::FormatCounter {
        oaat_core::message::FormatCounter {
            stream_id: contrat.stream_id.clone(),
            format: contrat.format,
            sample_rate: contrat.sample_rate,
            channels: contrat.channels,
            channel_layout: contrat.channel_layout,
            bits_per_sample: contrat.bits_per_sample as u8,
            dsd_rate: contrat.dsd_rate,
        }
    }

    /// La regression #2239 : une contre-proposition etait ADOPTEE sans qu'aucune
    /// conversion ne touche au payload.
    ///
    /// Et #2283 : le predicat ne comparait que codec/cadence/bits, si bien
    /// qu'une contre-proposition MONO face a une proposition stereo passait pour
    /// identique (JP Robbe). On fait donc varier CHAQUE champ negocie, un a la
    /// fois, depuis une contre-proposition par ailleurs exactement conforme.
    #[test]
    #[cfg(feature = "oaat")]
    fn chaque_champ_negocie_suffit_a_rendre_une_contre_proposition_inacceptable() {
        use crate::outputs::oaat::output::{
            PolitiqueAdaptation, ReponseNegociation, juger_reponse,
        };
        use oaat_controller::EndpointResponse;
        use oaat_core::format::{AudioFormat, ChannelLayout};

        let contrat = contrat_pcm();

        // Identique en TOUT point : il n'y a rien a faire, on peut jouer.
        let conforme = EndpointResponse::FormatCounter(contre_de(&contrat));
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&conforme),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_ok(),
            "une contre-proposition identique a la proposition n'a rien a refuser"
        );

        // Un champ change a la fois. Chacun doit suffire.
        #[allow(clippy::type_complexity)]
        let ecarts: Vec<(&str, Box<dyn Fn(&mut oaat_core::message::FormatCounter)>)> = vec![
            (
                "codec",
                Box::new(|c: &mut _| c.format = AudioFormat::PcmS16le),
            ),
            ("cadence", Box::new(|c: &mut _| c.sample_rate = 44_100)),
            // LA contre-epreuve de JP : mono contre stereo.
            ("canaux", Box::new(|c: &mut _| c.channels = 1)),
            (
                "disposition",
                Box::new(|c: &mut _| c.channel_layout = ChannelLayout::Mono),
            ),
            ("profondeur", Box::new(|c: &mut _| c.bits_per_sample = 16)),
            ("DSD", Box::new(|c: &mut _| c.dsd_rate = Some(64))),
        ];

        for (nom, modifier) in ecarts {
            let mut contre = contre_de(&contrat);
            modifier(&mut contre);
            let reponse = EndpointResponse::FormatCounter(contre);
            assert!(
                juger_reponse(
                    &contrat,
                    ReponseNegociation::Recue(&reponse),
                    PolitiqueAdaptation::ExacteSeulement,
                )
                .is_err(),
                "un ecart de {nom} doit suffire a refuser : Tune enverrait des \
                 octets que l'etiquette ne decrit plus (#2283)"
            );
        }
    }

    #[test]
    #[cfg(feature = "oaat")]
    fn politique_pcm_n_accepte_que_les_contre_propositions_reellement_convertibles() {
        use crate::outputs::oaat::output::{
            PolitiqueAdaptation, ReponseNegociation, juger_reponse,
        };
        use oaat_controller::EndpointResponse;
        use oaat_core::format::{AudioFormat, ChannelLayout};

        let contrat = contrat_pcm();
        let mut pcm = contre_de(&contrat);
        pcm.format = AudioFormat::PcmS16le;
        pcm.sample_rate = 48_000;
        pcm.channels = 1;
        pcm.channel_layout = ChannelLayout::Mono;
        pcm.bits_per_sample = 16;
        let cible = juger_reponse(
            &contrat,
            ReponseNegociation::Recue(&EndpointResponse::FormatCounter(pcm)),
            PolitiqueAdaptation::PcmEntier,
        )
        .expect("24/96/stéréo vers 16/48/mono passe par le convertisseur PCM");
        assert_eq!(cible.format, AudioFormat::PcmS16le);
        assert_eq!(cible.sample_rate, 48_000);
        assert_eq!(cible.channels, 1);
        assert_eq!(cible.bits_per_sample, 16);

        let mut compressee = contre_de(&contrat);
        compressee.format = AudioFormat::Flac;
        let refus = juger_reponse(
            &contrat,
            ReponseNegociation::Recue(&EndpointResponse::FormatCounter(compressee)),
            PolitiqueAdaptation::PcmEntier,
        )
        .expect_err("un encodeur FLAC ne doit pas être inventé dans le paquetiseur");
        assert!(
            refus
                .raison
                .contains("ne passe pas par le convertisseur PCM entier")
        );

        let mut disposition_mensongere = contre_de(&contrat);
        disposition_mensongere.channels = 1;
        disposition_mensongere.channel_layout = ChannelLayout::Stereo;
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(
                    &EndpointResponse::FormatCounter(disposition_mensongere,)
                ),
                PolitiqueAdaptation::PcmEntier,
            )
            .is_err(),
            "mono annoncé avec une disposition stéréo doit rester fail-closed"
        );
    }

    #[test]
    #[cfg(feature = "oaat")]
    fn piste_directe_gapless_est_preparee_dans_le_contrat_deja_negocie() {
        use crate::outputs::oaat::helpers::StagedDirectTrack;
        use crate::outputs::oaat::output::{ContratPropose, adapter_piste_directe_gapless};
        use oaat_core::format::{AudioFormat, ChannelLayout};

        let frames = 4_800usize;
        let mut pcm = Vec::with_capacity(frames * 6);
        for frame in 0..frames {
            let left = ((frame as i32 * 997) & 0x7f_ffff) - 0x40_0000;
            for sample in [left, -left] {
                pcm.extend_from_slice(&sample.to_le_bytes()[..3]);
            }
        }
        let piste = StagedDirectTrack {
            pcm,
            format: AudioFormat::PcmS24le,
            sample_rate: 96_000,
            bits_per_sample: 24,
            channels: 2,
            title: "suivante".into(),
            artist: String::new(),
            album: String::new(),
            cover_url: None,
            duration_ms: 50,
        };
        let cible = ContratPropose {
            stream_id: "flux-direct".into(),
            format: AudioFormat::PcmS16le,
            sample_rate: 48_000,
            channels: 1,
            channel_layout: ChannelLayout::Mono,
            bits_per_sample: 16,
            dsd_rate: None,
        };

        let piste = adapter_piste_directe_gapless(piste, &cible)
            .expect("la préparation se fait avant la frontière de piste");
        assert_eq!(piste.format, AudioFormat::PcmS16le);
        assert_eq!(piste.sample_rate, 48_000);
        assert_eq!(piste.bits_per_sample, 16);
        assert_eq!(piste.channels, 1);
        assert_eq!(piste.pcm.len(), 2_400 * 2);
        assert!(piste.pcm.iter().all(|byte| *byte == 0));
    }

    /// `dsd_rate` se COMPARE, il ne s'exige pas absent.
    ///
    /// Exiger `None` refusait une contre-proposition DSD64 rigoureusement
    /// identique a la proposition DSD64 — or les trois chemins qui posent un
    /// `FormatPropose` a la main envoient bien un multiplicateur. Le garde-fou
    /// de #2289 cassait donc le DSD natif (#2283, JP Robbe).
    #[test]
    #[cfg(feature = "oaat")]
    fn une_contre_proposition_dsd_identique_est_honorable() {
        use crate::outputs::oaat::output::{
            PolitiqueAdaptation, ReponseNegociation, juger_reponse,
        };
        use oaat_controller::EndpointResponse;

        let mut contrat = contrat_pcm();
        contrat.format = oaat_core::format::AudioFormat::DsdU32le;
        contrat.bits_per_sample = 1;
        contrat.dsd_rate = Some(64);

        let identique = EndpointResponse::FormatCounter(contre_de(&contrat));
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&identique),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_ok(),
            "une contre-proposition DSD64 identique a la proposition DSD64 \
             decrit exactement ce qu'on allait envoyer (#2283)"
        );

        // Et un multiplicateur DIFFERENT reste un refus.
        let mut autre = contre_de(&contrat);
        autre.dsd_rate = Some(128);
        let autre = EndpointResponse::FormatCounter(autre);
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&autre),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_err(),
            "DSD128 contre DSD64 n'est pas le meme flux"
        );
    }

    /// Les huit issues de la negociation, jugees pour ce qu'elles DECIDENT.
    ///
    /// Mon test de #2291 lisait le texte source du helper et verifiait la
    /// presence de la chaine `FormatReject` : il restait vert quand on
    /// remplacait le bras `FormatReject => Err(..)` par `Ok(())` (#2297, JP
    /// Robbe). Ici la decision est appelee, pas relue.
    #[test]
    #[cfg(feature = "oaat")]
    fn juger_reponse_decide_les_huit_issues() {
        use crate::outputs::oaat::output::{
            PolitiqueAdaptation, ReponseNegociation, juger_reponse,
        };
        use oaat_controller::EndpointResponse;

        let contrat = contrat_pcm();
        let flux = contrat.stream_id.clone();

        // 1. accord sur le bon flux -> on joue.
        let accord = EndpointResponse::FormatAccept(oaat_core::message::FormatAccept {
            stream_id: flux.clone(),
        });
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&accord),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_ok()
        );

        // 2. accord sur un AUTRE flux -> refus. Une reponse en retard prise
        //    pour la bonne decale toute la suite (#2282).
        let accord_etranger = EndpointResponse::FormatAccept(oaat_core::message::FormatAccept {
            stream_id: "flux-precedent".into(),
        });
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&accord_etranger),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_err(),
            "un accord pour un autre flux n'est pas notre accord"
        );

        // 3. contre-proposition identique -> on joue.
        let conforme = EndpointResponse::FormatCounter(contre_de(&contrat));
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&conforme),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_ok()
        );

        // 4. contre-proposition ecartee -> refus (couvert champ par champ
        //    ci-dessus, repris ici pour l'exhaustivite de l'enumeration).
        let mut ecartee = contre_de(&contrat);
        ecartee.sample_rate = 48_000;
        let ecartee = EndpointResponse::FormatCounter(ecartee);
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&ecartee),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_err()
        );

        // 5. refus explicite -> refus, avec le motif de l'endpoint.
        let refus = EndpointResponse::FormatReject(oaat_core::message::FormatReject {
            stream_id: flux.clone(),
            reason: "cadence non supportee".into(),
        });
        let r = juger_reponse(
            &contrat,
            ReponseNegociation::Recue(&refus),
            PolitiqueAdaptation::ExacteSeulement,
        )
        .expect_err("un FormatReject interdit la lecture (#2282)");
        assert!(
            r.raison.contains("cadence non supportee"),
            "le motif de l'endpoint doit remonter tel quel, il finit sous les \
             yeux de l'utilisateur (#2294) — recu : {}",
            r.raison
        );
        assert_eq!(r.stream_id, flux, "le refus doit nommer le flux concerne");

        // 6. reponse hors sujet -> refus : on ne joue pas sur une reponse
        //    qu'on n'a pas comprise.
        let hors_sujet = EndpointResponse::NextTrackReady(oaat_core::message::NextTrackReady {
            stream_id: flux.clone(),
        });
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Recue(&hors_sujet),
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_err()
        );

        // 7. endpoint ferme.
        assert!(
            juger_reponse(
                &contrat,
                ReponseNegociation::Fermee,
                PolitiqueAdaptation::ExacteSeulement,
            )
            .is_err()
        );

        // 8. silence.
        let r = juger_reponse(
            &contrat,
            ReponseNegociation::Timeout,
            PolitiqueAdaptation::ExacteSeulement,
        )
        .expect_err("le silence n'est pas un accord");
        assert!(!r.raison.is_empty(), "un refus sans motif ne sert a rien");
    }
}
