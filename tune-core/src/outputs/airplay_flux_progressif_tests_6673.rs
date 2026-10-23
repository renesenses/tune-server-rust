//! #6673 — le banc demandé par le diagnostic du 21/10 : un faux récepteur
//! AirPlay 1 qui reçoit AUSSI l'UDP, alimenté par un faux `/stream` lent dont
//! le corps arrive au fil de l'eau (une session WAV à la volée, sans `Range`).
//! Le premier paquet RTP doit partir peu après RECORD, bien avant la fin du
//! corps.
//!
//! Aucun appareil réel : tout tourne sur `127.0.0.1`.

use super::*;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Durée du corps servi, livrée en temps réel par le faux `/stream`.
const DUREE_DU_CORPS: Duration = Duration::from_secs(6);
/// Le délai que le récepteur tolère entre RECORD et le premier RTP.
const DELAI_TOLERE: Duration = Duration::from_millis(2_500);
/// Les échantillons du corps : gauche et droite constants, reconnaissables.
const GAUCHE: i16 = 0x1234;
const DROITE: i16 = -0x1234;

/// Un `/stream` WAV à la volée : en-tête « longueur inconnue », pas de
/// `Content-Length`, pas de `Range`, le PCM livré par blocs de 100 ms au
/// rythme réel. Chaque connexion a sa tâche (la sonde `Range` en ouvre une).
async fn stream_lent() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut requete = Vec::new();
                let mut octet = [0u8; 1];
                while !requete.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut octet).await.unwrap_or(0) == 0 {
                        return;
                    }
                    requete.push(octet[0]);
                }
                let entetes =
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nConnection: close\r\n\r\n";
                if sock.write_all(entetes.as_bytes()).await.is_err() {
                    return;
                }
                let wav = crate::audio::wav::build_wav_header_streaming(2, 44_100, 16);
                if sock.write_all(&wav).await.is_err() {
                    return;
                }
                let trames_par_bloc = 4_410usize;
                let mut bloc = Vec::with_capacity(trames_par_bloc * 4);
                for _ in 0..trames_par_bloc {
                    bloc.extend_from_slice(&GAUCHE.to_le_bytes());
                    bloc.extend_from_slice(&DROITE.to_le_bytes());
                }
                let blocs = DUREE_DU_CORPS.as_millis() / 100;
                for _ in 0..blocs {
                    if sock.write_all(&bloc).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        }
    });
    format!("http://127.0.0.1:{port}/stream/lent.wav")
}

/// Ce que le faux récepteur a vu.
#[derive(Default)]
struct Releve {
    record: Option<Instant>,
    premier_rtp: Option<(Instant, Vec<u8>)>,
}

/// Un faux récepteur AirPlay 1 : RTSP sur TCP, et le port audio UDP que son
/// SETUP annonce. Il note l'instant de sa réponse à RECORD et le premier
/// datagramme audio reçu.
async fn faux_recepteur_avec_udp() -> (u16, Arc<std::sync::Mutex<Releve>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let audio = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port_audio = audio.local_addr().unwrap().port();
    let releve = Arc::new(std::sync::Mutex::new(Releve::default()));

    let releve_udp = releve.clone();
    tokio::spawn(async move {
        let mut tampon = vec![0u8; 2_048];
        while let Ok(n) = audio.recv(&mut tampon).await {
            let mut r = releve_udp.lock().unwrap();
            if r.premier_rtp.is_none() {
                r.premier_rtp = Some((Instant::now(), tampon[..n].to_vec()));
            }
        }
    });

    let releve_rtsp = releve.clone();
    tokio::spawn(async move {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        let mut lecteur = BufReader::new(socket);
        loop {
            let mut ligne = String::new();
            if lecteur.read_line(&mut ligne).await.unwrap_or(0) == 0 {
                return;
            }
            let methode = ligne.split_whitespace().next().unwrap_or("").to_string();
            let mut taille = 0usize;
            loop {
                let mut entete = String::new();
                if lecteur.read_line(&mut entete).await.unwrap_or(0) == 0 {
                    return;
                }
                let entete = entete.trim_end().to_string();
                if entete.is_empty() {
                    break;
                }
                if let Some((cle, valeur)) = entete.split_once(':')
                    && cle.trim().eq_ignore_ascii_case("Content-Length")
                {
                    taille = valeur.trim().parse().unwrap_or(0);
                }
            }
            if taille > 0 {
                let mut corps = vec![0_u8; taille];
                if lecteur.read_exact(&mut corps).await.is_err() {
                    return;
                }
            }
            let supplement = if methode == "SETUP" {
                format!(
                    "Session: 1\r\nTransport: RTP/AVP/UDP;unicast;mode=record;\
                     server_port={port_audio};control_port=0;timing_port=0\r\n"
                )
            } else {
                String::new()
            };
            let reponse =
                format!("RTSP/1.0 200 OK\r\nCSeq: 1\r\n{supplement}Content-Length: 0\r\n\r\n");
            if lecteur
                .get_mut()
                .write_all(reponse.as_bytes())
                .await
                .is_err()
            {
                return;
            }
            if methode == "RECORD" {
                releve_rtsp.lock().unwrap().record = Some(Instant::now());
            }
        }
    });
    (port, releve)
}

/// 🔴 LE témoin de #6673. Avant : `stream_to_airplay` lisait tout le corps
/// (`url_to_local_path`, `resp.bytes()`), soit ici six secondes de silence
/// RTP après RECORD — le récepteur du fil 2261 raccroche avant.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_premier_rtp_part_peu_apres_record_sur_un_stream_lent() {
    let url = stream_lent().await;
    let (port, releve) = faux_recepteur_avec_udp().await;
    let sortie = AirplayOutput::new(
        "Faux AirPlay 1".into(),
        format!("airplay-127.0.0.1-{port}"),
        "127.0.0.1".into(),
        port,
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        sortie.play_media(&PlayMedia {
            url: &url,
            mime_type: "audio/wav",
            ..Default::default()
        }),
    )
    .await
    .expect("play_media ne doit pas rester suspendu")
    .expect("le faux récepteur accepte tout");

    let record = releve
        .lock()
        .unwrap()
        .record
        .expect("le faux récepteur a répondu à RECORD");
    let limite = record + DUREE_DU_CORPS + Duration::from_secs(2);
    let premier = loop {
        if let Some(p) = releve.lock().unwrap().premier_rtp.clone() {
            break Some(p);
        }
        if Instant::now() > limite {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    sortie.stop().await.ok();

    let (instant, paquet) = premier.expect("aucun paquet RTP reçu");
    let delai = instant.saturating_duration_since(record);
    assert!(
        delai < DELAI_TOLERE,
        "premier RTP {delai:?} après RECORD (toléré {DELAI_TOLERE:?}) : la piste \
         est encore téléchargée et décodée en entier avant le premier paquet (#6673)"
    );
    // Le payload est bien le PCM du corps, en L16 gros-boutiste.
    let charge = &paquet[RTP_HEADER_SIZE..];
    assert_eq!(charge.len(), BYTES_PER_PACKET);
    assert_eq!(
        (
            i16::from_be_bytes([charge[0], charge[1]]),
            i16::from_be_bytes([charge[2], charge[3]])
        ),
        (GAUCHE, DROITE),
        "le payload doit être le PCM servi, converti en L16 gros-boutiste"
    );
}

/// Le recalage après une attente du décodeur : pas de rafale, la cadence
/// reprend à ~8 ms par paquet.
#[test]
fn une_attente_du_decodeur_recale_l_horloge() {
    let origine = tokio::time::Instant::now();
    let mut horloge = HorlogeCadencementRtp::new(origine);
    let tolere = Duration::from_millis(100);
    // Dans la tolérance : rien ne bouge.
    assert_eq!(
        horloge.rattraper_un_retard(origine + Duration::from_millis(50), 0, tolere),
        None
    );
    // Le décodeur a fait attendre 2 s avant le paquet 10.
    let trames = 10 * FRAMES_PER_PACKET as u64;
    let maintenant = horloge.echeance(trames) + Duration::from_secs(2);
    assert_eq!(
        horloge.rattraper_un_retard(maintenant, trames, tolere),
        Some(Duration::from_secs(2))
    );
    let suivante = horloge.echeance(trames + FRAMES_PER_PACKET as u64);
    let intervalle = suivante.duration_since(maintenant);
    let un_paquet =
        Duration::from_micros(FRAMES_PER_PACKET as u64 * 1_000_000 / SAMPLE_RATE as u64);
    assert!(
        intervalle >= un_paquet && intervalle <= un_paquet + Duration::from_micros(1),
        "après l'attente, cadence attendue ~8 ms, obtenue {intervalle:?}"
    );
}

/// Une source qui livre en blocs irréguliers donne les mêmes paquets qu'une
/// piste entière : paquets pleins puis le dernier partiel (#2237).
#[tokio::test]
async fn des_blocs_irreguliers_donnent_les_memes_paquets() {
    struct Blocs(Vec<Vec<u8>>);
    impl SourcePcmAirplay for Blocs {
        async fn suivant(&mut self) -> Option<Vec<u8>> {
            if self.0.is_empty() {
                None
            } else {
                Some(self.0.remove(0))
            }
        }
    }
    let total = 3 * BYTES_PER_PACKET + 100 * BYTES_PER_FRAME;
    let pcm = vec![0u8; total];
    let tailles = [1usize, 7, 1_000, 3, BYTES_PER_PACKET * 2, 17];
    let mut blocs = Vec::new();
    let mut i = 0;
    let mut k = 0;
    while i < total {
        let n = tailles[k % tailles.len()].min(total - i);
        blocs.push(pcm[i..i + n].to_vec());
        i += n;
        k += 1;
    }
    #[derive(Default)]
    struct Sortie(std::sync::Mutex<Vec<usize>>);
    impl SortieRtp for Sortie {
        async fn envoyer(&self, pkt: &[u8]) {
            self.0.lock().unwrap().push(pkt.len() - RTP_HEADER_SIZE);
        }
    }
    let sortie = Sortie::default();
    let playing = AtomicBool::new(true);
    let paused = AtomicBool::new(false);
    let position = AtomicU64::new(0);
    let rtp = CurseurRtpAirplay::default();
    let (_stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    let issue = diffuser_flux(
        &mut Blocs(blocs),
        &sortie,
        &playing,
        &paused,
        &position,
        &mut stop_rx,
        1,
        &rtp,
    )
    .await;
    assert!(!issue.interrompu);
    assert_eq!(
        sortie.0.lock().unwrap().clone(),
        vec![
            BYTES_PER_PACKET,
            BYTES_PER_PACKET,
            BYTES_PER_PACKET,
            100 * BYTES_PER_FRAME
        ]
    );
}
