//! #7330 — banc du vrai chemin HTTP d'`OaatOutput` : en fin NATURELLE, le
//! `stop` (qui purge la file du récepteur) n'arrive plus avant l'échéance du
//! dernier paquet audio.
//!
//! Le faux endpoint répond à l'horloge comme un vrai (même horloge
//! `oaat_core::time::now_ns` des deux côtés, sur la même machine) : le PTS lu
//! dans les en-têtes et l'instant de réception du `stop` sont donc
//! comparables. Sans le correctif, la piste de 500 ms part en rafale (sous le
//! seuil de cadencement) et le `stop` arrive avant le PTS du dernier paquet,
//! qui porte encore l'avance de départ (0,5 s au moins, #7331).

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use oaat_core::Message;
use oaat_core::codec::FrameCodec;
use oaat_core::message::*;
use oaat_core::wire::{AUDIO_HEADER_SIZE, AudioPacketHeader, PacketFlags};

use crate::outputs::oaat::OaatOutput;
use crate::outputs::traits::{OutputTarget, PlayMedia};

fn wav_16_bits(millis: u32) -> Vec<u8> {
    let sr = 44_100u32;
    let ch = 2u16;
    let bits = 16u16;
    let data_size = sr * millis / 1000 * ch as u32 * (bits as u32 / 8);
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_size).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&ch.to_le_bytes());
    b.extend_from_slice(&sr.to_le_bytes());
    b.extend_from_slice(&(sr * ch as u32 * bits as u32 / 8).to_le_bytes());
    b.extend_from_slice(&(ch * bits / 8).to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_size.to_le_bytes());
    b.resize(b.len() + data_size as usize, 0);
    b
}

#[derive(Debug, Default)]
struct Observe {
    /// PTS du dernier paquet portant de l'audio.
    dernier_pts_audio: Option<u64>,
    /// Instant (horloge oaat) de réception du `LAST_PACKET`.
    last_packet_recu_ns: Option<u64>,
    /// Instant (horloge oaat) de réception du `stop`.
    stop_recu_ns: Option<u64>,
}

#[tokio::test]
async fn la_fin_naturelle_n_envoie_pas_stop_avant_le_dernier_pts_7330() {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_port = tcp.local_addr().unwrap().port();
    let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let audio_port = audio_udp.local_addr().unwrap().port();
    let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let clock_port = clock_udp.local_addr().unwrap().port();
    let observe = Arc::new(Mutex::new(Observe::default()));

    let obs_audio = observe.clone();
    let _audio = tokio::spawn(async move {
        let mut buf = vec![0u8; 16_384];
        while let Ok(Ok(n)) =
            tokio::time::timeout(std::time::Duration::from_secs(20), audio_udp.recv(&mut buf)).await
        {
            if n < AUDIO_HEADER_SIZE {
                continue;
            }
            let recu = oaat_core::time::now_ns();
            let hdr: [u8; AUDIO_HEADER_SIZE] = buf[..AUDIO_HEADER_SIZE].try_into().unwrap();
            let Ok(h) = AudioPacketHeader::decode(&hdr) else {
                continue;
            };
            let mut o = obs_audio.lock().unwrap();
            if n > AUDIO_HEADER_SIZE {
                o.dernier_pts_audio = Some(h.pts_ns);
            }
            if h.flags.contains(PacketFlags::LAST_PACKET) {
                o.last_packet_recu_ns = Some(recu);
            }
        }
    });
    let _clock = tokio::spawn(async move {
        use oaat_core::wire::{ClockSyncPacket, ClockSyncType};
        let mut buf = [0u8; 64];
        while let Ok((n, peer)) = clock_udp.recv_from(&mut buf).await {
            if n < ClockSyncPacket::SIZE {
                continue;
            }
            let t2 = oaat_core::time::now_ns();
            let requete: [u8; ClockSyncPacket::SIZE] =
                buf[..ClockSyncPacket::SIZE].try_into().unwrap();
            let Ok(mut reponse) = ClockSyncPacket::decode(&requete) else {
                continue;
            };
            reponse.kind = ClockSyncType::Response;
            reponse.t2 = t2;
            reponse.t3 = oaat_core::time::now_ns();
            let mut paquet = [0; ClockSyncPacket::SIZE];
            reponse.encode(&mut paquet);
            let _ = clock_udp.send_to(&paquet, peer).await;
        }
    });

    let obs_ctrl = observe.clone();
    let endpoint = tokio::spawn(async move {
        let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(std::time::Duration::from_secs(10), tcp.accept()).await
        else {
            return;
        };
        let mut codec = FrameCodec::new();
        let mut read_buf = [0u8; 8192];
        let n = stream.read(&mut read_buf).await.unwrap_or(0);
        codec.feed(&read_buf[..n]);
        if !matches!(codec.decode_next(), Ok(Some(Message::Hello(_)))) {
            return;
        }
        let ack = Message::HelloAck(HelloAck {
            protocol_version: oaat_core::PROTOCOL_VERSION,
            endpoint_id: "mock-ep-7330".into(),
            endpoint_name: "Mock 7330".into(),
            capabilities: EndpointCapabilities {
                pcm_max_rate: 192_000,
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
            features: vec![],
        });
        let _ = stream.write_all(&FrameCodec::encode(&ack)).await;
        loop {
            let n = match tokio::time::timeout(
                std::time::Duration::from_secs(20),
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
                        let accept = Message::FormatAccept(FormatAccept {
                            stream_id: fp.stream_id,
                        });
                        let _ = stream.write_all(&FrameCodec::encode(&accept)).await;
                    }
                    Message::Stop(_) => {
                        let mut o = obs_ctrl.lock().unwrap();
                        o.stop_recu_ns.get_or_insert(oaat_core::time::now_ns());
                    }
                    _ => {}
                }
            }
        }
    });

    // 500 ms : sous le seuil de cadencement, la piste part en rafale.
    let track_ms = 500u32;
    let wav = wav_16_bits(track_ms);
    let http_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_tcp.local_addr().unwrap().port();
    let http = tokio::spawn(async move {
        while let Ok(Ok((mut s, _))) =
            tokio::time::timeout(std::time::Duration::from_secs(20), http_tcp.accept()).await
        {
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                match s.read(&mut byte).await {
                    Ok(1) => req.push(byte[0]),
                    _ => break,
                }
            }
            let hdr = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: audio/wav\r\n\r\n",
                wav.len()
            );
            let _ = s.write_all(hdr.as_bytes()).await;
            let _ = s.write_all(&wav).await;
            let _ = s.shutdown().await;
        }
    });

    let output = OaatOutput::new(
        "Mock 7330".into(),
        "127.0.0.1".into(),
        control_port,
        "mock-ep-7330".into(),
    );
    let url = format!("http://127.0.0.1:{http_port}/fin-7330.wav");
    output
        .play_media(&PlayMedia {
            url: &url,
            mime_type: "audio/wav",
            title: Some("Fin 7330"),
            duration_ms: Some(track_ms as u64),
            ..Default::default()
        })
        .await
        .expect("play_media");

    // Attendre le `stop` de la fin naturelle (borné).
    let limite = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while std::time::Instant::now() < limite && observe.lock().unwrap().stop_recu_ns.is_none() {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let o = std::mem::take(&mut *observe.lock().unwrap());
    let dernier_pts = o
        .dernier_pts_audio
        .expect("aucun paquet audio reçu : le banc n'a rien prouvé");
    let last_packet = o
        .last_packet_recu_ns
        .expect("aucun LAST_PACKET : la fin de piste n'a pas été atteinte");
    let stop = o.stop_recu_ns.expect("aucun stop reçu en fin naturelle");
    assert!(
        dernier_pts > last_packet,
        "le banc suppose que le dernier paquet est encore à jouer au LAST_PACKET \
         (PTS {} ms après sa réception)",
        (dernier_pts as i128 - last_packet as i128) / 1_000_000
    );
    assert!(
        stop >= dernier_pts,
        "stop reçu {} ms AVANT l'échéance du dernier paquet : le récepteur purge \
         une file encore à jouer, la fin de la piste est coupée (#7330)",
        (dernier_pts - stop) / 1_000_000
    );
    assert!(
        stop.saturating_sub(last_packet)
            <= super::fin_jouee_7330::PLAFOND_ATTENTE.as_nanos() as u64 + 1_000_000_000,
        "l'attente avant stop doit rester bornée"
    );

    output.stop().await.ok();
    endpoint.abort();
    http.abort();
}

/// Sans paquet audio envoyé, rien à attendre : le `stop` part tout de suite.
#[tokio::test]
async fn l_attente_de_fin_n_existe_pas_sans_paquet() {
    let fin = super::fin_jouee_7330::FinJouee::default();
    let debut = std::time::Instant::now();
    fin.attendre_avant_stop("banc").await;
    assert!(debut.elapsed() < std::time::Duration::from_millis(50));
}
