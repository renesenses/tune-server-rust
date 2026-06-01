#[cfg(all(test, feature = "oaat"))]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};

    use oaat_core::Message;
    use oaat_core::codec::FrameCodec;
    use oaat_core::message::*;
    use oaat_core::wire::AUDIO_HEADER_SIZE;

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
            if let Ok((mut stream, _)) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tcp.accept(),
            )
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
        let (got_format, got_play, audio_packets) = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            mock_handle,
        )
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
}
