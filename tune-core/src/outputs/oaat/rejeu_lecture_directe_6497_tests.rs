//! #6497 (fil 2332), second défaut — après un enchaînement interne de la
//! lecture directe OAAT, la sortie rapportait encore l'URI de la piste FINIE.
//!
//! Le journal du signalement (zone OAAT, album WAV, 18/10) :
//!
//! ```text
//! 20:58:37.788 oaat: gapless transition (direct file) … title=That's Our Sun …
//! 20:58:38.590 gapless_position_reset_detected prev_pos=158280 new_pos=805
//! 20:58:38.595 WARN gapless_retombee_sur_la_piste_finie_surveillee position_ms=805
//!              uri=Some(".../stream/95d3dfd1-….wav") flux_fini=Some("95d3dfd1-…") flux_arme=None
//! 20:58:42.590 WARN gapless_adoption_horloge_infirmee_relance … preuve=RejeuDeLaPisteFinie
//! 20:58:42.622 oaat: stop device=red [Verox OAAT]
//! 20:58:44.624 oaat: play_media … title=That's Our Sun …
//! ```
//!
//! La sortie avait bien enchaîné la piste suivante en interne, mais son
//! `current_uri` nommait toujours le flux de la piste finie : le chemin direct
//! ne le remettait pas à zéro, alors que les enchaînements DSD natif et HTTP de
//! la même sortie le font. Le garde-fou du rejeu (#5411) concluait donc que le
//! lecteur rejouait la piste finie, et relançait la suivante depuis zéro après
//! quatre secondes : compteur qui hésite, silence, puis reprise à 0:00.
//!
//! Le banc rejoue la séquence avec un faux endpoint (aucune sortie audio
//! réelle) : un WAV lu en direct, la suivante préparée en direct, la bascule,
//! puis la décision du sondeur sur ce que la sortie rapporte.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use oaat_core::Message;
use oaat_core::codec::FrameCodec;
use oaat_core::message::*;

use crate::outputs::oaat::OaatOutput;
use crate::outputs::traits::{OutputTarget, PlayMedia};
use crate::poller::decisions;

const FLUX_FINI: &str = "95d3dfd1-000e-4bcd-96da-8ceef446eb47";

/// WAV canonique 16 bits, 44,1 kHz, stéréo, de `millis` ms, au contenu non nul.
pub(super) fn wav(millis: u32) -> Vec<u8> {
    let trames = 44_100 * millis / 1000;
    let data_size = trames * 4;
    let mut b = b"RIFF".to_vec();
    b.extend_from_slice(&(36 + data_size).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&44_100u32.to_le_bytes());
    b.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    b.extend_from_slice(&4u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_size.to_le_bytes());
    b.extend((0..data_size).map(|i| (i % 7) as u8));
    b
}

/// Faux endpoint PCM : HelloAck, accepte tout FormatPropose, répond à
/// l'horloge, avale l'audio. Rien n'est joué.
pub(super) async fn faux_endpoint() -> u16 {
    faux_endpoint_avec(false).await
}

/// Le même, qui annonce aussi le DSD natif (`DsdU8`, jusqu'au DSD512) quand
/// `dsd` est vrai.
pub(super) async fn faux_endpoint_avec(dsd: bool) -> u16 {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_port = tcp.local_addr().unwrap().port();
    let audio_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let audio_port = audio_udp.local_addr().unwrap().port();
    let clock_udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let clock_port = clock_udp.local_addr().unwrap().port();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65536];
        while audio_udp.recv(&mut buf).await.is_ok() {}
    });
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        while let Ok((n, peer)) = clock_udp.recv_from(&mut buf).await {
            if n >= 28 {
                let _ = clock_udp.send_to(&buf[..n], peer).await;
            }
        }
    });
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = tcp.accept().await {
            tokio::spawn(async move {
                let mut codec = FrameCodec::new();
                let mut lu = [0u8; 16384];
                loop {
                    let n = match stream.read(&mut lu).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    codec.feed(&lu[..n]);
                    while let Ok(Some(msg)) = codec.decode_next() {
                        let reponse = match msg {
                            Message::Hello(_) => Some(Message::HelloAck(HelloAck {
                                protocol_version: oaat_core::PROTOCOL_VERSION,
                                endpoint_id: "faux-verox".into(),
                                endpoint_name: "Faux Verox".into(),
                                capabilities: EndpointCapabilities {
                                    pcm_max_rate: 384_000,
                                    pcm_max_bits: 32,
                                    dsd_max_rate: dsd.then_some(512),
                                    channels_max: 2,
                                    formats: if dsd {
                                        vec![
                                            oaat_core::format::AudioFormat::PcmS16le,
                                            oaat_core::format::AudioFormat::PcmS24le,
                                            oaat_core::format::AudioFormat::DsdU8,
                                        ]
                                    } else {
                                        vec![
                                            oaat_core::format::AudioFormat::PcmS16le,
                                            oaat_core::format::AudioFormat::PcmS24le,
                                        ]
                                    },
                                    volume: None,
                                    gapless: true,
                                    seek: false,
                                },
                                audio_port,
                                clock_port,
                                buffer_size_ms: 100,
                                features: vec![],
                            })),
                            Message::FormatPropose(fp) => {
                                Some(Message::FormatAccept(FormatAccept {
                                    stream_id: fp.stream_id,
                                }))
                            }
                            _ => None,
                        };
                        if let Some(r) = reponse {
                            let _ = stream.write_all(&FrameCodec::encode(&r)).await;
                        }
                    }
                }
            });
        }
    });
    control_port
}

/// Fin de piste WAV en lecture directe → piste suivante préparée en direct →
/// bascule interne. Ce que la sortie rapporte ensuite ne doit plus nommer le
/// flux de la piste finie, et le sondeur ne doit pas y voir un rejeu.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l_enchainement_direct_ne_rapporte_plus_le_flux_de_la_piste_finie() {
    let dir = tempfile::TempDir::new().unwrap();
    let piste_1 = dir.path().join("01 - The Chrysalis.wav");
    let piste_2 = dir.path().join("02 - That's Our Sun.wav");
    std::fs::write(&piste_1, wav(1_500)).unwrap();
    std::fs::write(&piste_2, wav(3_000)).unwrap();

    let port = faux_endpoint().await;
    let sortie = OaatOutput::new(
        "red [Verox OAAT]".into(),
        "127.0.0.1".into(),
        port,
        "faux-verox".into(),
    );
    let url_1 = format!("http://127.0.0.1:1/stream/{FLUX_FINI}.wav");
    let chemin_1 = piste_1.to_str().unwrap().to_owned();
    sortie
        .play_media(&PlayMedia {
            url: &url_1,
            mime_type: "audio/wav",
            title: Some("The Chrysalis"),
            file_path: Some(&chemin_1),
            duration_ms: Some(1_500),
            ..Default::default()
        })
        .await
        .expect("play_media");

    // La lecture directe doit s'être engagée avant d'armer la suivante : le
    // sondeur n'arme un FICHIER que dans ce cas.
    let engagee = async {
        while !sortie.prefers_local_file_gapless() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), engagee)
        .await
        .expect("la lecture directe ne s'est pas engagée");

    let chemin_2 = piste_2.to_str().unwrap().to_owned();
    sortie
        .set_next_media(&PlayMedia {
            url: "http://127.0.0.1:1/stream/jamais-tire.wav",
            mime_type: "audio/wav",
            title: Some("That's Our Sun"),
            file_path: Some(&chemin_2),
            duration_ms: Some(3_000),
            ..Default::default()
        })
        .await
        .expect("set_next_media");

    // La bascule interne : le titre passe à la piste 2.
    let bascule = async {
        loop {
            let s = sortie.get_status().await.unwrap();
            if s.track_title.as_deref() == Some("That's Our Sun") {
                return s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let statut = tokio::time::timeout(std::time::Duration::from_secs(20), bascule)
        .await
        .expect("l'enchaînement direct n'a pas eu lieu");
    let _ = sortie.stop().await;

    assert!(
        !decisions::uri_nomme_le_flux(statut.current_uri.as_deref(), Some(FLUX_FINI)),
        "après la bascule interne, la sortie nomme encore le flux de la piste finie : {:?}",
        statut.current_uri
    );
    // La décision du journal : flux fini connu, aucun flux armé (le chemin
    // direct n'en crée pas), pas de répétition.
    assert!(
        !decisions::renderer_rejoue_la_piste_finie(
            statut.current_uri.as_deref(),
            Some(FLUX_FINI),
            None,
            false,
        ),
        "le garde-fou #5411 prendrait l'enchaînement direct pour un rejeu et relancerait la piste"
    );
}
