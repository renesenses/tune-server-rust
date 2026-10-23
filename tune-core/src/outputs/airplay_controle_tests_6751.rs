//! #6751 — une session RAOP simulée de bout en bout : le faux récepteur
//! annonce ses ports audio ET de contrôle au SETUP (un même socket), reçoit l'audio, les sync,
//! et réclame un paquet par une demande 0x55. Aucun appareil réel.

use super::*;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Ce que le faux récepteur a reçu, avec l'instant de réception.
#[derive(Default)]
struct Releve {
    audio: Vec<(Instant, Vec<u8>)>,
    sync: Vec<(Instant, Vec<u8>)>,
    retransmis: Vec<Vec<u8>>,
    /// Le port de contrôle de l'ÉMETTEUR, lu dans sa requête SETUP.
    controle_emetteur: Option<u16>,
}

type Partage = Arc<std::sync::Mutex<Releve>>;

/// Faux récepteur AirPlay 1. `fermer_au_teardown` : il ferme le canal RTSP à
/// réception du TEARDOWN, sans répondre (le Marantz du fil 2261).
async fn faux_recepteur(fermer_au_teardown: bool) -> (u16, Partage, Arc<tokio::net::UdpSocket>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // UN seul socket UDP reçoit l'audio ET le contrôle : le SETUP l'annonce
    // comme `server_port` et comme `control_port`. L'ordre d'arrivée sur un
    // même socket est l'ordre d'envoi ; deux sockets lus par deux tâches ne
    // le garantissent pas (le témoin d'ordre en devenait instable).
    let controle = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let port_controle = controle.local_addr().unwrap().port();
    let port_audio = port_controle;
    let releve: Partage = Arc::default();

    let r = releve.clone();
    let c = controle.clone();
    tokio::spawn(async move {
        let mut t = vec![0u8; 4_096];
        while let Ok((n, _)) = c.recv_from(&mut t).await {
            let p = t[..n].to_vec();
            let mut r = r.lock().unwrap();
            match p.get(1).map(|b| b & 0x7f) {
                Some(0x54) => r.sync.push((Instant::now(), p)),
                Some(0x56) => r.retransmis.push(p),
                Some(0x60) => r.audio.push((Instant::now(), p)),
                _ => {}
            }
        }
    });
    let r = releve.clone();
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
                if let Some((cle, valeur)) = entete.split_once(':') {
                    if cle.trim().eq_ignore_ascii_case("Content-Length") {
                        taille = valeur.trim().parse().unwrap_or(0);
                    }
                    if cle.trim().eq_ignore_ascii_case("Transport") {
                        r.lock().unwrap().controle_emetteur = valeur
                            .split(';')
                            .find_map(|p| p.trim().strip_prefix("control_port="))
                            .and_then(|p| p.parse().ok());
                    }
                }
            }
            if taille > 0 {
                let mut corps = vec![0_u8; taille];
                if lecteur.read_exact(&mut corps).await.is_err() {
                    return;
                }
            }
            if methode == "TEARDOWN" && fermer_au_teardown {
                return;
            }
            let supplement = if methode == "SETUP" {
                format!(
                    "Session: 1\r\nTransport: RTP/AVP/UDP;unicast;mode=record;\
                     server_port={port_audio};control_port={port_controle};timing_port=0\r\n"
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
        }
    });
    (port, releve, controle)
}

async fn jouer(port: u16) -> AirplayOutput {
    let sortie = AirplayOutput::new(
        "Faux RAOP".into(),
        format!("airplay-127.0.0.1-{port}"),
        "127.0.0.1".into(),
        port,
    );
    let chemin = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.wav");
    let url = format!("file://{chemin}");
    tokio::time::timeout(
        Duration::from_secs(10),
        sortie.play_media(&PlayMedia {
            url: &url,
            mime_type: "audio/wav",
            ..Default::default()
        }),
    )
    .await
    .expect("play_media ne doit pas rester suspendu")
    .expect("le faux récepteur accepte tout");
    sortie
}

async fn attendre(releve: &Partage, condition: impl Fn(&Releve) -> bool) -> bool {
    let limite = Instant::now() + Duration::from_secs(10);
    while Instant::now() < limite {
        if condition(&releve.lock().unwrap()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// 🔴 Le témoin de #6751, point 1 et 4 : au moins un sync, marqué « premier »,
/// AVANT le premier paquet audio, et dont l'horodatage est celui de ce paquet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_sync_premier_part_avant_le_premier_paquet_audio() {
    let (port, releve, _controle) = faux_recepteur(false).await;
    let sortie = jouer(port).await;
    assert!(
        attendre(&releve, |r| r.audio.len() >= 3).await,
        "le faux récepteur doit recevoir de l'audio"
    );
    sortie.stop().await.ok();
    let r = releve.lock().unwrap();
    let (instant_sync, sync) = r.sync.first().cloned().expect(
        "aucun paquet sync reçu sur le port de contrôle : le récepteur ne peut \
         pas ancrer le flux (#6751)",
    );
    let (instant_audio, audio) = r.audio.first().cloned().unwrap();
    assert!(
        instant_sync <= instant_audio,
        "le premier sync doit précéder le premier paquet audio"
    );
    assert_eq!(
        &sync[..4],
        &[0x90, 0xd4, 0x00, 0x07],
        "premier sync : {sync:?}"
    );
    assert_eq!(
        &sync[16..20],
        &audio[4..8],
        "le sync porte l'horodatage RTP du premier paquet"
    );
}

/// 🔴 Point 2 : une demande 0x55 pour un paquet déjà émis reçoit sa
/// retransmission 0x56, qui porte le paquet d'origine à l'octet près.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_demande_de_retransmission_recoit_le_paquet_d_origine() {
    let (port, releve, controle) = faux_recepteur(false).await;
    let sortie = jouer(port).await;
    assert!(attendre(&releve, |r| r.audio.len() >= 5).await);
    let (original, port_emetteur) = {
        let r = releve.lock().unwrap();
        (
            r.audio[2].1.clone(),
            r.controle_emetteur.expect("SETUP sans control_port"),
        )
    };
    let mut demande = [0x80u8, 0xd5, 0x00, 0x01, 0, 0, 0x00, 0x01];
    demande[4..6].copy_from_slice(&original[2..4]);
    controle
        .send_to(&demande, ("127.0.0.1", port_emetteur))
        .await
        .unwrap();
    let recu = attendre(&releve, |r| !r.retransmis.is_empty()).await;
    sortie.stop().await.ok();
    assert!(
        recu,
        "la demande 0x55 est restée sans réponse : Tune ignore les \
         retransmissions (#6751)"
    );
    let r = releve.lock().unwrap();
    let reponse = &r.retransmis[0];
    assert_eq!(&reponse[..2], &[0x80, 0xd6]);
    assert_eq!(
        &reponse[4..],
        &original[..],
        "la 0x56 porte le paquet d'origine"
    );
}

/// Point 3 : un appareil qui ferme le canal au TEARDOWN ne « refuse » rien —
/// le journal disait `TEARDOWN refused by device: 0`.
#[tokio::test]
async fn un_canal_ferme_au_teardown_n_est_pas_un_refus_de_code_0() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut requete = Vec::new();
        let mut octet = [0_u8; 1];
        while !requete.ends_with(b"\r\n\r\n") {
            socket.read_exact(&mut octet).await.unwrap();
            requete.push(octet[0]);
        }
        // Fermeture sans réponse.
    });
    let mut session = RtspSession::connect("127.0.0.1", address.port())
        .await
        .unwrap();
    let erreur = tokio::time::timeout(Duration::from_secs(2), session.teardown())
        .await
        .expect("TEARDOWN ne doit pas rester suspendu")
        .unwrap_err();
    assert!(
        !erreur.contains("refused by device: 0") && erreur.contains("connection closed"),
        "obtenu : {erreur}"
    );
}
