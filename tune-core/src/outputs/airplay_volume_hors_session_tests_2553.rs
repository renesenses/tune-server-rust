//! Fil 2553 (B&W Zeppelin Air, rc9 Windows) : une consigne de volume donnée
//! sans session RTSP était refusée puis perdue, et la session suivante ne
//! portait aucun volume. L'enceinte jouait à son propre niveau pendant que
//! Tune affichait la consigne.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Un faux récepteur : lit UNE requête RTSP, répond 200, rend son texte.
async fn session_et_recepteur() -> (RtspSession, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let recepteur = tokio::spawn(async move {
        let (mut pair, _) = listener.accept().await.unwrap();
        let mut requetes = Vec::new();
        let mut tampon = vec![0u8; 4096];
        loop {
            let lus = match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                pair.read(&mut tampon),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => n,
                _ => break,
            };
            let texte = String::from_utf8_lossy(&tampon[..lus]).to_string();
            let cseq = texte
                .lines()
                .find_map(|l| l.strip_prefix("CSeq: "))
                .unwrap_or("0")
                .trim()
                .to_string();
            requetes.push(texte);
            pair.write_all(format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").as_bytes())
                .await
                .unwrap();
        }
        requetes
    });
    let session = RtspSession::connect("127.0.0.1", port).await.unwrap();
    (session, recepteur)
}

fn sortie() -> AirplayOutput {
    AirplayOutput::new(
        "Zeppelin".into(),
        "airplay:test-2553".into(),
        "127.0.0.1".into(),
        1,
    )
}

#[tokio::test]
async fn une_consigne_donnee_zone_arretee_part_au_debut_de_la_session_suivante_2553() {
    let sortie = sortie();
    sortie
        .set_volume(0.30)
        .await
        .expect("sans session, la consigne doit attendre, pas être refusée");
    assert!(
        (sortie.get_status().await.unwrap().volume - 0.30).abs() < 1e-9,
        "le statut doit porter la consigne retenue"
    );

    let (mut session, recepteur) = session_et_recepteur().await;
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    // Une seconde application n'envoie plus rien : la consigne part une fois.
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    drop(session);
    let requetes = recepteur.await.unwrap();

    let attendu = format!("volume: {:.1}", linear_to_airplay_db(0.30));
    assert_eq!(
        requetes.len(),
        1,
        "une seule commande attendue : {requetes:?}"
    );
    assert!(
        requetes[0].starts_with("SET_PARAMETER ") && requetes[0].contains(&attendu),
        "la session doit porter la consigne : {requetes:?}"
    );
    assert!(sortie.volume_en_attente.lock().await.is_none());
}

#[tokio::test]
async fn sans_consigne_en_attente_la_session_ne_porte_aucun_volume_2395() {
    // #2395 : Tune ne réaffirme pas un volume que personne n'a demandé.
    let sortie = sortie();
    let (mut session, recepteur) = session_et_recepteur().await;
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    drop(session);
    assert!(
        recepteur.await.unwrap().is_empty(),
        "aucune commande de volume ne doit partir sans consigne en attente"
    );
}

#[tokio::test]
async fn une_consigne_acceptee_en_session_vide_l_attente_2553() {
    let sortie = sortie();
    sortie.set_volume(0.80).await.unwrap();
    let (session, recepteur) = session_et_recepteur().await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_volume(0.20).await.unwrap();
    assert!(
        sortie.volume_en_attente.lock().await.is_none(),
        "la consigne envoyée en session remplace l'ancienne attente"
    );
    sortie.rtsp_session.lock().await.take();
    let requetes = recepteur.await.unwrap();
    assert_eq!(requetes.len(), 1, "{requetes:?}");
    assert!(
        requetes[0].contains(&format!("volume: {:.1}", linear_to_airplay_db(0.20))),
        "{requetes:?}"
    );
}

#[tokio::test]
async fn un_refus_du_recepteur_en_session_ne_change_pas_le_statut_confirme() {
    let sortie = sortie();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let session = RtspSession::connect("127.0.0.1", port).await.unwrap();
    // Le récepteur raccroche sans répondre.
    drop(listener.accept().await.unwrap());
    *sortie.rtsp_session.lock().await = Some(session);
    assert!(sortie.set_volume(0.25).await.is_err());
    let status = sortie.get_status().await.unwrap();
    assert_eq!(status.volume, 1.0);
    assert!(sortie.volume_en_attente.lock().await.is_none());
}
