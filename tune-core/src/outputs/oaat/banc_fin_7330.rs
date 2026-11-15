//! #7330 — banc partagé : un faux endpoint OAAT qui répond à l'horloge, un
//! serveur HTTP qui sert un WAV, et ce que l'endpoint observe en fin de piste.
//!
//! Même horloge des deux côtés (`oaat_core::time::now_ns`, même machine) : le
//! PTS lu dans les en-têtes et l'instant de réception du `stop` sont
//! comparables. `demi_rtt` retarde la réponse d'horloge de façon SYMÉTRIQUE
//! (avant t2 et après t3) : le décalage mesuré reste nul, le RTT vaut deux
//! fois `demi_rtt`, et l'avance de départ (#7331) grandit d'autant.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinHandle;

use oaat_core::Message;
use oaat_core::codec::FrameCodec;
use oaat_core::message::*;
use oaat_core::wire::{AUDIO_HEADER_SIZE, AudioPacketHeader, PacketFlags};

pub(crate) const ENDPOINT_ID: &str = "oaat:mock-ep-7330";

pub(crate) fn wav_16_bits(millis: u32) -> Vec<u8> {
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

#[derive(Debug, Default, Clone)]
pub(crate) struct Observe {
    /// PTS du premier paquet audio, et instant de sa réception.
    pub(crate) premier: Option<(u64, u64)>,
    /// PTS du dernier paquet portant de l'audio.
    pub(crate) dernier_pts_audio: Option<u64>,
    /// Réception du `LAST_PACKET`.
    pub(crate) last_packet_recu_ns: Option<u64>,
    /// Réception du message `stop`.
    pub(crate) stop_recu_ns: Option<u64>,
    /// Fermeture de la connexion de contrôle par Tune.
    pub(crate) ferme_ns: Option<u64>,
}

pub(crate) struct BancFin {
    pub(crate) control_port: u16,
    pub(crate) url: String,
    pub(crate) observe: Arc<Mutex<Observe>>,
    taches: Vec<JoinHandle<()>>,
}

impl Drop for BancFin {
    fn drop(&mut self) {
        for t in &self.taches {
            t.abort();
        }
    }
}

impl BancFin {
    pub(crate) fn photo(&self) -> Observe {
        self.observe.lock().unwrap().clone()
    }

    /// Attend une condition sur l'observation, au plus `limite`.
    pub(crate) async fn attendre(&self, limite: Duration, f: impl Fn(&Observe) -> bool) -> bool {
        let fin = std::time::Instant::now() + limite;
        while std::time::Instant::now() < fin {
            if f(&self.photo()) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        f(&self.photo())
    }

    pub(crate) fn sortie(&self) -> crate::outputs::oaat::OaatOutput {
        crate::outputs::oaat::OaatOutput::new(
            "Mock 7330".into(),
            "127.0.0.1".into(),
            self.control_port,
            ENDPOINT_ID.into(),
        )
    }
}

pub(crate) async fn monter(track_ms: u32, demi_rtt: Duration) -> BancFin {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_port = tcp.local_addr().unwrap().port();
    let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let audio_port = audio_udp.local_addr().unwrap().port();
    let clock_udp = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let clock_port = clock_udp.local_addr().unwrap().port();
    let observe = Arc::new(Mutex::new(Observe::default()));
    let mut taches = Vec::new();

    let obs = observe.clone();
    taches.push(tokio::spawn(async move {
        let mut buf = vec![0u8; 16_384];
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_secs(60), audio_udp.recv(&mut buf)).await
        {
            if n < AUDIO_HEADER_SIZE {
                continue;
            }
            let recu = oaat_core::time::now_ns();
            let hdr: [u8; AUDIO_HEADER_SIZE] = buf[..AUDIO_HEADER_SIZE].try_into().unwrap();
            let Ok(h) = AudioPacketHeader::decode(&hdr) else {
                continue;
            };
            let mut o = obs.lock().unwrap();
            if n > AUDIO_HEADER_SIZE {
                o.premier.get_or_insert((h.pts_ns, recu));
                o.dernier_pts_audio = Some(h.pts_ns);
            }
            if h.flags.contains(PacketFlags::LAST_PACKET) {
                o.last_packet_recu_ns = Some(recu);
            }
        }
    }));

    taches.push(tokio::spawn(async move {
        use oaat_core::wire::{ClockSyncPacket, ClockSyncType};
        let mut buf = [0u8; 64];
        while let Ok((n, peer)) = clock_udp.recv_from(&mut buf).await {
            if n < ClockSyncPacket::SIZE {
                continue;
            }
            let requete: [u8; ClockSyncPacket::SIZE] =
                buf[..ClockSyncPacket::SIZE].try_into().unwrap();
            let Ok(mut reponse) = ClockSyncPacket::decode(&requete) else {
                continue;
            };
            let socket = clock_udp.clone();
            tokio::spawn(async move {
                tokio::time::sleep(demi_rtt).await;
                reponse.kind = ClockSyncType::Response;
                reponse.t2 = oaat_core::time::now_ns();
                reponse.t3 = oaat_core::time::now_ns();
                tokio::time::sleep(demi_rtt).await;
                let mut paquet = [0; ClockSyncPacket::SIZE];
                reponse.encode(&mut paquet);
                let _ = socket.send_to(&paquet, peer).await;
            });
        }
    }));

    let obs = observe.clone();
    taches.push(tokio::spawn(async move {
        let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(Duration::from_secs(20), tcp.accept()).await
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
            let n = match tokio::time::timeout(Duration::from_secs(60), stream.read(&mut read_buf))
                .await
            {
                Ok(Ok(0)) | Ok(Err(_)) => {
                    obs.lock()
                        .unwrap()
                        .ferme_ns
                        .get_or_insert(oaat_core::time::now_ns());
                    break;
                }
                Err(_) => break,
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
                        obs.lock()
                            .unwrap()
                            .stop_recu_ns
                            .get_or_insert(oaat_core::time::now_ns());
                    }
                    _ => {}
                }
            }
        }
    }));

    let wav = wav_16_bits(track_ms);
    let http_tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_tcp.local_addr().unwrap().port();
    taches.push(tokio::spawn(async move {
        while let Ok(Ok((mut s, _))) =
            tokio::time::timeout(Duration::from_secs(60), http_tcp.accept()).await
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
    }));

    BancFin {
        control_port,
        url: format!("http://127.0.0.1:{http_port}/fin-7330.wav"),
        observe,
        taches,
    }
}
