//! Vrais canaux OAAT, récepteur fictif local, aucune sortie sonore.
use super::*;
use oaat_core::Message;
use oaat_core::codec::FrameCodec;
use oaat_core::message::{
    EndpointCapabilities, FormatAccept, HelloAck, VolumeCapability, VolumeType,
};
use oaat_core::wire::{ClockSyncPacket, ClockSyncType};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;

struct Banc {
    port: u16,
    annonce: Arc<std::sync::Mutex<Option<VolumeCapability>>>,
    messages: mpsc::Receiver<Message>,
    taches: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Banc {
    fn drop(&mut self) {
        for tache in &self.taches {
            tache.abort();
        }
    }
}

impl Banc {
    async fn nouveau(volume: Option<VolumeCapability>) -> Self {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = tcp.local_addr().unwrap().port();
        let audio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio_port = audio.local_addr().unwrap().port();
        let horloge = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let clock_port = horloge.local_addr().unwrap().port();
        let annonce = Arc::new(std::sync::Mutex::new(volume));
        let annonce_tache = annonce.clone();
        let (tx, messages) = mpsc::channel(64);
        let controle = tokio::spawn(async move {
            while let Ok((mut socket, _)) = tcp.accept().await {
                let mut codec = FrameCodec::new();
                let mut buf = [0; 8192];
                loop {
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    codec.feed(&buf[..n]);
                    while let Ok(Some(message)) = codec.decode_next() {
                        let reponse = match &message {
                            Message::Hello(_) => Some(Message::HelloAck(HelloAck {
                                protocol_version: oaat_core::PROTOCOL_VERSION,
                                endpoint_id: "volume-7333-fictif".into(),
                                endpoint_name: "Banc volume 7333".into(),
                                capabilities: EndpointCapabilities {
                                    pcm_max_rate: 192_000,
                                    pcm_max_bits: 32,
                                    dsd_max_rate: None,
                                    channels_max: 2,
                                    formats: vec![oaat_core::format::AudioFormat::PcmS16le],
                                    volume: annonce_tache.lock().unwrap().clone(),
                                    gapless: false,
                                    seek: false,
                                },
                                audio_port,
                                clock_port,
                                buffer_size_ms: 100,
                                features: vec![],
                            })),
                            Message::FormatPropose(f) => {
                                Some(Message::FormatAccept(FormatAccept {
                                    stream_id: f.stream_id.clone(),
                                }))
                            }
                            _ => None,
                        };
                        if let Some(reponse) = reponse {
                            if socket
                                .write_all(&FrameCodec::encode(&reponse))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        if tx.send(message).await.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        let horloge = tokio::spawn(async move {
            let mut buf = [0; ClockSyncPacket::SIZE];
            while let Ok((n, peer)) = horloge.recv_from(&mut buf).await {
                if n != buf.len() {
                    continue;
                }
                if let Ok(mut paquet) = ClockSyncPacket::decode(&buf) {
                    paquet.kind = ClockSyncType::Response;
                    paquet.t2 = oaat_core::time::now_ns();
                    paquet.t3 = oaat_core::time::now_ns();
                    paquet.encode(&mut buf);
                    let _ = horloge.send_to(&buf, peer).await;
                }
            }
        });
        let audio = tokio::spawn(async move {
            let mut buf = [0; 65_536];
            while audio.recv(&mut buf).await.is_ok() {}
        });
        Self {
            port,
            annonce,
            messages,
            taches: vec![controle, horloge, audio],
        }
    }

    async fn attendre(&mut self, predicate: impl Fn(&Message) -> bool) -> Message {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let message = self.messages.recv().await.expect("endpoint fictif vivant");
                if predicate(&message) {
                    break message;
                }
            }
        })
        .await
        .expect("commande attendue par le faux endpoint OAAT")
    }

    async fn sans_volume(&mut self) {
        let resultat = tokio::time::timeout(Duration::from_millis(250), async {
            while let Some(message) = self.messages.recv().await {
                assert!(
                    !matches!(message, Message::VolumeSet(_) | Message::Mute(_)),
                    "#7333 : commande distante envoyée à un endpoint fixed/none : {message:?}"
                );
            }
        })
        .await;
        assert!(
            resultat.is_err(),
            "le canal du banc doit rester ouvert pendant l'observation"
        );
    }
}

fn annonce(vol_type: VolumeType) -> VolumeCapability {
    VolumeCapability {
        vol_type,
        range: [5, 80],
        step: 7,
    }
}

fn wav(path: &std::path::Path) {
    let taille = 44_100_u32 * 2 * 2 * 30;
    let mut buf = Vec::with_capacity(taille as usize + 44);
    buf.extend(b"RIFF");
    buf.extend((taille + 36).to_le_bytes());
    buf.extend(b"WAVEfmt ");
    buf.extend(16_u32.to_le_bytes());
    buf.extend(1_u16.to_le_bytes());
    buf.extend(2_u16.to_le_bytes());
    buf.extend(44_100_u32.to_le_bytes());
    buf.extend(176_400_u32.to_le_bytes());
    buf.extend(4_u16.to_le_bytes());
    buf.extend(16_u16.to_le_bytes());
    buf.extend(b"data");
    buf.extend(taille.to_le_bytes());
    buf.resize(taille as usize + 44, 0);
    std::fs::write(path, buf).unwrap();
}

async fn lancer(output: &OaatOutput, banc: &mut Banc, path: &std::path::Path) {
    output
        .play_media(&PlayMedia {
            url: "http://127.0.0.1:1/jamais-requis.wav",
            mime_type: "audio/wav",
            file_path: path.to_str(),
            duration_ms: Some(30_000),
            ..Default::default()
        })
        .await
        .unwrap();
    banc.attendre(|m| matches!(m, Message::Play(_))).await;
}

#[tokio::test]
async fn le_txt_fixed_none_interdit_le_volume_avant_connexion_7333() {
    for annonce in ["fixed", "none"] {
        let output = OaatOutput::new("Fictif".into(), "127.0.0.1".into(), 1, "7333".into())
            .avec_volume_annonce(Some(annonce));
        assert!(
            !output.capabilities().can_set_volume,
            "#7333 : TXT {annonce} pilotable à tort"
        );
        assert!(!output.capabilities().can_mute);
        assert!(output.set_volume(0.25).await.is_err());
        assert!(output.set_mute(true).await.is_err());
        assert_eq!(
            output.volume.load(Ordering::SeqCst),
            100,
            "le refus ne change pas le niveau"
        );
        output
            .volume_annonce
            .lock()
            .unwrap()
            .annoncer_hello(Some(&self::annonce(VolumeType::Hw)));
        assert!(
            !output.capabilities().can_set_volume,
            "une annonce contradictoire ne lève pas TXT fixed/none"
        );
    }
}

#[tokio::test]
async fn le_vrai_hello_fixed_none_inhibe_meme_une_commande_en_file_7333() {
    let dossier = crate::test_scratch::scratch_dir("oaat-volume-fixe-7333");
    let path = dossier.join("silence.wav");
    wav(&path);
    for vol_type in [VolumeType::Fixed, VolumeType::None] {
        let mut banc = Banc::nouveau(Some(annonce(vol_type))).await;
        let output = OaatOutput::new(
            "Fictif".into(),
            "127.0.0.1".into(),
            banc.port,
            "7333".into(),
        );
        lancer(&output, &mut banc, &path).await;
        assert!(
            !output.capabilities().can_set_volume,
            "#7333 : hello fixed/none ignoré"
        );
        assert!(output.set_volume(0.33).await.is_err());
        // Une commande mise en file AVANT une nouvelle annonce doit être
        // revalidée aussi par la boucle de transport, pas seulement à l'API.
        output
            .command_tx
            .lock()
            .await
            .as_ref()
            .unwrap()
            .send(OaatCommand::SetVolume(33))
            .await
            .unwrap();
        output
            .command_tx
            .lock()
            .await
            .as_ref()
            .unwrap()
            .send(OaatCommand::Mute(true))
            .await
            .unwrap();
        let avant = output.diag.packets_sent.load(Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let vide = output
                    .command_tx
                    .lock()
                    .await
                    .as_ref()
                    .is_some_and(|tx| tx.capacity() == tx.max_capacity());
                if vide && output.diag.packets_sent.load(Ordering::SeqCst) > avant {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("la boucle doit vider sa commande et continuer les paquets audio fictifs");
        banc.sans_volume().await;
        output.stop().await.unwrap();
    }
}

#[tokio::test]
async fn les_anciennes_annonces_restent_compatibles_et_une_plage_invalide_ne_devient_pas_pilotable_7333()
 {
    let output = OaatOutput::new("Fictif".into(), "127.0.0.1".into(), 1, "7333".into());
    assert!(output.capabilities().can_set_volume);
    assert!(output.capabilities().volume_percent_range.is_none());
    output.set_volume(0.28).await.unwrap();
    assert_eq!(output.volume.load(Ordering::SeqCst), 28);
    let mut v = annonce(VolumeType::Hw);
    v.range = [90, 20];
    output
        .volume_annonce
        .lock()
        .unwrap()
        .annoncer_hello(Some(&v));
    assert!(!output.capabilities().can_set_volume);
    assert!(output.set_volume(0.28).await.is_err());
    // Le champ ajouté est facultatif pour les anciennes charges utiles.
    let mut ancien =
        serde_json::to_value(OutputCapabilities::v1(true, true, true, true, true, false)).unwrap();
    ancien
        .as_object_mut()
        .unwrap()
        .remove("volume_percent_range");
    let relu: OutputCapabilities = serde_json::from_value(ancien).unwrap();
    assert!(relu.volume_percent_range.is_none());
}

#[tokio::test]
async fn hw_sw_envoient_la_plage_et_le_pas_annonces_sur_le_vrai_fil_7333() {
    let dossier = crate::test_scratch::scratch_dir("oaat-volume-reglable-7333");
    let path = dossier.join("silence.wav");
    wav(&path);
    for vol_type in [VolumeType::Hw, VolumeType::Sw] {
        let mut banc = Banc::nouveau(Some(annonce(vol_type))).await;
        let output = OaatOutput::new(
            "Fictif".into(),
            "127.0.0.1".into(),
            banc.port,
            "7333".into(),
        );
        lancer(&output, &mut banc, &path).await;
        let json = serde_json::to_value(output.capabilities()).unwrap();
        assert_eq!(json["can_set_volume"], true);
        assert_eq!(
            json["volume_percent_range"],
            serde_json::json!({"min":5,"max":80,"step":7})
        );
        for demande in [0.0, 1.0] {
            assert!(
                output.set_volume(demande).await.is_err(),
                "une consigne hors plage doit être refusée"
            );
        }
        banc.sans_volume().await;
        for (demande, attendu) in [(0.05, 5), (0.28, 26), (0.80, 75)] {
            output.set_volume(demande).await.unwrap();
            let message = banc.attendre(|m| matches!(m, Message::VolumeSet(_))).await;
            let Message::VolumeSet(volume) = message else {
                unreachable!()
            };
            assert_eq!(
                volume.level, attendu,
                "#7333 : niveau hors de la grille réellement annoncée"
            );
            assert_eq!(output.volume.load(Ordering::SeqCst), u32::from(attendu));
        }
        output.stop().await.unwrap();
    }
}

#[tokio::test]
async fn une_reconnexion_actualise_hw_vers_fixed_et_ne_reactive_pas_un_txt_fixe_7333() {
    let dossier = crate::test_scratch::scratch_dir("oaat-volume-reconnexion-7333");
    let path = dossier.join("silence.wav");
    wav(&path);
    let mut banc = Banc::nouveau(Some(annonce(VolumeType::Hw))).await;
    let output = OaatOutput::new(
        "Fictif".into(),
        "127.0.0.1".into(),
        banc.port,
        "7333".into(),
    )
    .avec_volume_annonce(Some("hw"));
    lancer(&output, &mut banc, &path).await;
    assert!(
        output.capabilities().can_set_volume,
        "hw est pilotable quand les annonces concordent"
    );
    output.stop().await.unwrap();
    *banc.annonce.lock().unwrap() = Some(annonce(VolumeType::Fixed));
    lancer(&output, &mut banc, &path).await;
    assert!(
        !output.capabilities().can_set_volume,
        "#7333 : capacité périmée après reconnexion"
    );
    assert!(output.set_volume(0.20).await.is_err());
    output.stop().await.unwrap();
    let output = output.avec_volume_annonce(Some("fixed"));
    *banc.annonce.lock().unwrap() = None;
    lancer(&output, &mut banc, &path).await;
    assert!(
        !output.capabilities().can_set_volume,
        "l'annonce ancienne incomplète conserve TXT fixed"
    );
    banc.sans_volume().await;
    output.stop().await.unwrap();
}
