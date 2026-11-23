//! TESS T12a — la chaîne TESS sur OAAT, vérifiée sur les OCTETS qu'un faux
//! récepteur OAAT reçoit.
//!
//! Le banc passe par la vraie porte de lecture (`PlaybackOrchestrator::play`),
//! la vraie sortie [`OaatOutput`] et un faux point d'arrivée OAAT sur la
//! boucle locale (TCP de contrôle, UDP audio et horloge). Les URL de
//! l'orchestrateur sont servies par le faux serveur `/stream`
//! ([`super::faux_serveur_de_flux_tess`]). La source est un WAV 16 bits dont
//! la voie DROITE est muette :
//!
//! - chaîne active (crossfeed) : la voie droite reçue s'allume ;
//! - greffon natif tiers (gain ×0,5) : la gauche reçue est la moitié de la
//!   source ;
//! - mode Pure, chaîne armée : la sortie reçoit la source à l'échantillon
//!   près (bit-perfect) ;
//! - aucune chaîne : idem.
//!
//! 🔴 Avant T12a (#9176), OAAT lisait le fichier WAV EN DIRECT
//! (`file_path`) et la voie droite restait nulle malgré le crossfeed : le
//! test `crossfeed_sur_oaat_allume_la_voie_droite_t12a` le montre.
//!
//! Aucun son, rien hors de `127.0.0.1`.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use oaat_core::Message;
use oaat_core::codec::FrameCodec;
use oaat_core::format::AudioFormat;
use oaat_core::message::*;
use oaat_core::wire::{AUDIO_HEADER_SIZE, AudioPacketHeader, PacketFlags};

use super::faux_serveur_de_flux_tess as banc;
use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::outputs::oaat::OaatOutput;

const CADENCE: u32 = 44_100;
/// 0,6 s de source : assez pour la latence du crossfeed, court en temps réel.
const TRAMES: usize = 26_460;

/// Les paquets audio reçus : `(sample_offset, format, charge utile)`.
type Paquets = Arc<std::sync::Mutex<Vec<(u64, AudioFormat, Vec<u8>)>>>;

/// Un faux point d'arrivée OAAT : il accepte toute proposition de format et
/// garde chaque paquet audio (hors FEC). Rend le port de contrôle.
async fn faux_recepteur_oaat(paquets: Paquets) -> u16 {
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_controle = tcp.local_addr().unwrap().port();
    let audio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port_audio = audio.local_addr().unwrap().port();
    let horloge = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port_horloge = horloge.local_addr().unwrap().port();

    tokio::spawn(async move {
        let mut buf = vec![0u8; 16_384];
        while let Ok(n) = audio.recv(&mut buf).await {
            if n < AUDIO_HEADER_SIZE {
                continue;
            }
            let hdr: [u8; AUDIO_HEADER_SIZE] = buf[..AUDIO_HEADER_SIZE].try_into().unwrap();
            let Ok(h) = AudioPacketHeader::decode(&hdr) else {
                continue;
            };
            if h.flags.contains(PacketFlags::FEC) {
                continue;
            }
            let fin = (AUDIO_HEADER_SIZE + usize::from(h.payload_len)).min(n);
            paquets.lock().unwrap().push((
                h.sample_offset,
                h.format,
                buf[AUDIO_HEADER_SIZE..fin].to_vec(),
            ));
        }
    });
    tokio::spawn(async move {
        use oaat_core::wire::{ClockSyncPacket, ClockSyncType};
        let mut buf = [0u8; 64];
        while let Ok((n, pair)) = horloge.recv_from(&mut buf).await {
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
            let _ = horloge.send_to(&paquet, pair).await;
        }
    });
    tokio::spawn(async move {
        // Une connexion de contrôle par lecture.
        while let Ok((mut flux, _)) = tcp.accept().await {
            tokio::spawn(async move {
                let mut codec = FrameCodec::new();
                let mut lu = [0u8; 8192];
                loop {
                    let n = match flux.read(&mut lu).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    codec.feed(&lu[..n]);
                    while let Ok(Some(msg)) = codec.decode_next() {
                        let reponse = match msg {
                            Message::Hello(_) => Some(Message::HelloAck(HelloAck {
                                protocol_version: oaat_core::PROTOCOL_VERSION,
                                endpoint_id: "banc-t12a".into(),
                                endpoint_name: "Banc T12a".into(),
                                capabilities: EndpointCapabilities {
                                    pcm_max_rate: 192_000,
                                    pcm_max_bits: 32,
                                    dsd_max_rate: None,
                                    channels_max: 2,
                                    formats: vec![AudioFormat::PcmS16le, AudioFormat::PcmS24le],
                                    volume: None,
                                    gapless: true,
                                    seek: false,
                                },
                                audio_port: port_audio,
                                clock_port: port_horloge,
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
                        if let Some(r) = reponse
                            && flux.write_all(&FrameCodec::encode(&r)).await.is_err()
                        {
                            return;
                        }
                    }
                }
            });
        }
    });
    port_controle
}

/// Les trames `[gauche, droite]` reçues, dans l'ordre des positions, et leur
/// profondeur.
fn trames_recues(paquets: &Paquets) -> (Vec<[i32; 2]>, u32) {
    let mut p = paquets.lock().unwrap().clone();
    p.sort_by_key(|(offset, _, _)| *offset);
    p.dedup_by_key(|(offset, _, _)| *offset);
    let mut trames = Vec::new();
    let mut bits = 16;
    for (_, format, charge) in p {
        match format {
            AudioFormat::PcmS16le => {
                bits = 16;
                for t in charge.chunks_exact(4) {
                    trames.push([
                        i32::from(i16::from_le_bytes([t[0], t[1]])),
                        i32::from(i16::from_le_bytes([t[2], t[3]])),
                    ]);
                }
            }
            AudioFormat::PcmS24le => {
                bits = 24;
                for t in charge.chunks_exact(6) {
                    let v = |o: usize| i32::from_le_bytes([0, t[o], t[o + 1], t[o + 2]]) >> 8;
                    trames.push([v(0), v(3)]);
                }
            }
            autre => panic!("format inattendu au banc : {autre:?}"),
        }
    }
    (trames, bits)
}

/// La chaîne armée sur la zone du banc.
#[derive(Clone, Copy, Debug)]
enum Chaine {
    Aucune,
    Crossfeed,
    CrossfeedSousPure,
    GreffonTiers,
}

struct Lecture {
    trames: Vec<[i32; 2]>,
    bits: u32,
    source: Vec<[i16; 2]>,
    file_path_remis: bool,
}

/// Une lecture de bout en bout ; rend ce que le faux récepteur a reçu.
async fn jouer(chaine: Chaine) -> Lecture {
    let _tiers = matches!(chaine, Chaine::GreffonTiers).then(|| {
        crate::greffon_natif_de_test::enregistrer_les_bibliotheques_de_test(&[
            "gain-essai-t12a",
        ])
    });
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    let streamer = banc::demarrer().await;
    let mut orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        streamer,
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    orch.advertised_ip = Some("127.0.0.1".into());
    orch.license = Some(crate::license::licence_premium_pour_test(db.clone()));

    let paquets: Paquets = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = faux_recepteur_oaat(paquets.clone()).await;
    let sortie = OaatOutput::new(
        "Banc T12a".into(),
        "127.0.0.1".into(),
        port,
        "banc-t12a".into(),
    );
    let appareil = crate::outputs::OutputTarget::device_id(&sortie).to_owned();
    orch.outputs.lock().await.register(Box::new(sortie));
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Bureau T12a", Some("oaat"), Some(&appareil))
        .unwrap();

    let (wav, source) = banc::wav_gauche_seule("t12a-oaat", CADENCE, TRAMES);
    let duree_ms = (TRAMES as i64 * 1_000) / i64::from(CADENCE);
    let track_id = banc::inscrire_la_piste(&db, wav.as_str().as_ref(), CADENCE, duree_ms);

    match chaine {
        Chaine::Aucune => {}
        Chaine::Crossfeed => banc::armer_le_crossfeed(&orch, zone_id),
        Chaine::CrossfeedSousPure => {
            banc::armer_le_crossfeed(&orch, zone_id);
            banc::armer_pure(&orch, zone_id);
        }
        Chaine::GreffonTiers => {
            let id = &_tiers.as_ref().unwrap().0[0];
            let s = SettingsRepo::with_backend(db.clone());
            s.set(&format!("plugin_{id}_installed"), "true").unwrap();
            s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
            s.set(
                &crate::audio::natifs_tiers::cle_de_zone(zone_id, id),
                r#"{"enabled":true,"gain":0.5}"#,
            )
            .unwrap();
        }
    }
    let attendu_actif = matches!(chaine, Chaine::Crossfeed | Chaine::GreffonTiers);
    assert_eq!(
        orch.chaine_du_flux_active(zone_id, crate::audio::replaygain::PisteRg::AUCUNE),
        attendu_actif,
        "prémisse : chaîne {chaine:?}"
    );

    let r = orch
        .play(PlayRequest {
            zone_id,
            output_device_id: Some(appareil.clone()),
            track_id: Some(track_id),
            duration_ms: Some(duree_ms),
            media_format: Some("wav".into()),
            sample_rate: Some(CADENCE),
            bit_depth: Some(16),
            ..Default::default()
        })
        .await;
    assert!(r.is_ok(), "la lecture doit partir : {:?}", r.err());

    // Le flux part au rythme réel : on attend que les paquets cessent, en
    // notant si la sortie a lu le fichier en direct pendant la lecture.
    let sortie_reelle = {
        let reg = orch.outputs.lock().await;
        reg.get(&appareil).unwrap()
    };
    let mut direct = false;
    let mut vu = 0usize;
    let mut calme = 0u32;
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        direct |= sortie_reelle.lock().await.prefers_local_file_gapless();
        let n = paquets.lock().unwrap().len();
        if n > 0 && n == vu {
            calme += 1;
            if calme >= 15 {
                break;
            }
        } else {
            calme = 0;
        }
        vu = n;
    }
    let _ = sortie_reelle.lock().await.stop().await;
    let (trames, bits) = trames_recues(&paquets);
    assert!(!trames.is_empty(), "le faux récepteur n'a rien reçu ({chaine:?})");
    Lecture {
        trames,
        bits,
        source,
        file_path_remis: direct,
    }
}

/// 🔴 → 🟢 Le témoin de T12a (#9176) : crossfeed coché, la voie droite muette
/// de la source s'allume sur les octets que reçoit OAAT. Avant, la lecture
/// directe du WAV la laissait nulle.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crossfeed_sur_oaat_allume_la_voie_droite_t12a() {
    let l = jouer(Chaine::Crossfeed).await;
    let c = banc::constater(&l.trames, l.bits, &l.source, 1.0);
    println!("T12a crossfeed OAAT : {c:?}, lecture directe = {}", l.file_path_remis);
    assert!(
        !l.file_path_remis,
        "#9176 : chaîne active, OAAT ne doit pas lire le fichier en direct"
    );
    assert!(
        c.droite_max > 500,
        "#9176 : le crossfeed doit allumer la voie droite reçue par OAAT ({c:?})"
    );
}

/// Greffon natif tiers (gain ×0,5) : la gauche reçue est la moitié de la
/// source, à la quantification près (une seule, TPDF : ±2 LSB).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn greffon_natif_tiers_sur_oaat_t12a() {
    let l = jouer(Chaine::GreffonTiers).await;
    let c = banc::constater(&l.trames, l.bits, &l.source, 0.5);
    println!("T12a greffon tiers OAAT : {c:?}");
    assert!(c.trames > TRAMES / 2, "{c:?}");
    assert!(
        c.ecart_gauche <= 2.0,
        "le greffon natif tiers doit diviser la gauche par deux ({c:?})"
    );
    assert!(c.droite_max <= 2, "{c:?}");
}

/// Pure, crossfeed coché : rien n'est traité, la source arrive à
/// l'échantillon près (bit-perfect).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pure_sur_oaat_rien_n_est_traite_t12a() {
    let l = jouer(Chaine::CrossfeedSousPure).await;
    let c = banc::constater(&l.trames, l.bits, &l.source, 1.0);
    println!("T12a Pure OAAT : {c:?}");
    assert!(
        c.identique,
        "Pure : OAAT doit recevoir la source à l'échantillon près ({c:?})"
    );
}

/// Contre-épreuve : sans chaîne, la source à l'échantillon près, et la
/// lecture directe du fichier reste permise.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sans_chaine_oaat_recoit_la_source_t12a() {
    let l = jouer(Chaine::Aucune).await;
    let c = banc::constater(&l.trames, l.bits, &l.source, 1.0);
    println!("T12a sans chaîne OAAT : {c:?}, lecture directe = {}", l.file_path_remis);
    assert!(c.identique, "{c:?}");
}

#[test]
fn oaat_ne_lit_le_fichier_que_sans_chaine_ou_en_dsd_t12a() {
    use super::transport::oaat_lit_le_fichier as lit;
    assert!(lit("/musique/a.wav", false));
    assert!(!lit("/musique/a.wav", true), "#9176");
    assert!(!lit("/musique/a.flac", true));
    assert!(lit("/musique/a.dsf", true), "DSD natif : hors chaîne PCM");
    assert!(lit("/musique/a.DFF", true));
}
