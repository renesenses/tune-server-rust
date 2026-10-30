use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn connexion() -> (AirplayOutput, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let session = RtspSession::connect("127.0.0.1", address.port())
        .await
        .unwrap();
    let (pair, _) = listener.accept().await.unwrap();
    let sortie = AirplayOutput::new(
        "NR1602".into(),
        "airplay:test".into(),
        "127.0.0.1".into(),
        address.port(),
    );
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.playing.store(true, Ordering::SeqCst);
    (sortie, pair)
}

#[tokio::test]
async fn fin_rtsp_arrete_la_lecture_et_remonte_sa_cause_7259() {
    let (sortie, mut pair) = connexion().await;
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
    *sortie.stop_tx.lock().await = Some(stop_tx);
    pair.shutdown().await.unwrap();
    sortie
        .rtsp_session
        .lock()
        .await
        .as_ref()
        .unwrap()
        .stream
        .readable()
        .await
        .unwrap();
    let cause = sortie.take_output_failure();
    assert!(
        cause
            .as_deref()
            .is_some_and(|e| e.contains("fermé la connexion")),
        "FIN RTSP sans diagnostic : {cause:?}"
    );
    assert_eq!(
        sortie.get_status().await.unwrap().state,
        TransportState::Stopped,
        "AirPlay reste en lecture après FIN RTSP"
    );
    assert!(stop_rx.try_recv().is_ok(), "RTP continue après FIN RTSP");
    assert!(
        sortie.take_output_failure().is_none(),
        "l'ancien échec ne doit pas arrêter la piste suivante"
    );
}

#[tokio::test]
async fn relever_rtsp_ne_consomme_pas_une_reponse_et_ne_bloque_pas_7259() {
    let (sortie, mut pair) = connexion().await;
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), sortie.get_status())
            .await
            .expect("le statut attend une réponse RTSP non sollicitée")
            .unwrap()
            .state,
        TransportState::Playing
    );
    pair.write_all(b"R").await.unwrap();
    sortie
        .rtsp_session
        .lock()
        .await
        .as_ref()
        .unwrap()
        .stream
        .readable()
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(1), sortie.get_status())
            .await
            .expect("le statut attend une réponse RTSP non sollicitée")
            .unwrap()
            .state,
        TransportState::Playing
    );
    let mut octet = [0];
    sortie
        .rtsp_session
        .lock()
        .await
        .as_mut()
        .unwrap()
        .stream
        .read_exact(&mut octet)
        .await
        .unwrap();
    assert_eq!(octet, [b'R'], "la sonde a volé une réponse RTSP");
    assert!(sortie.take_output_failure().is_none());
}

struct Refuse;
impl SortieRtp for Refuse {
    async fn envoyer(&self, _: &[u8]) -> Result<(), String> {
        Err("RTP refusé par le banc".into())
    }
}

#[tokio::test]
async fn erreur_rtp_interrompt_avant_de_faire_avancer_la_position_7259() {
    for taille in [BYTES_PER_PACKET * 2, BYTES_PER_FRAME] {
        let (_tx, mut rx) = tokio::sync::oneshot::channel();
        let position = AtomicU64::new(0);
        let issue = diffuser_pcm(
            &vec![0; taille],
            &Refuse,
            &AtomicBool::new(true),
            &AtomicBool::new(false),
            &position,
            &mut rx,
            1,
            &CurseurRtpAirplay::default(),
        )
        .await;
        assert_eq!(
            issue.echec.as_deref(),
            Some("RTP refusé par le banc"),
            "échec RTP perdu par la boucle de diffusion, taille={taille}"
        );
        assert_eq!(
            position.load(Ordering::SeqCst),
            0,
            "la position avance sans paquet audio envoyé"
        );
    }
}

#[tokio::test]
async fn emetteur_reel_ne_jette_pas_une_erreur_udp_7259() {
    let pair = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let audio = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    audio.connect(pair.local_addr().unwrap()).await.unwrap();
    let sortie = controle::EmetteurRaop::new(audio, None);
    // Un datagramme plus grand que la limite UDP échoue sans dépendre d'ICMP.
    let resultat = sortie.envoyer(&vec![0; 65_536]).await;
    assert!(
        resultat.is_err(),
        "l'émetteur RTP masque l'erreur d'envoi UDP"
    );
}
