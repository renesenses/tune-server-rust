//! #8151 — une enceinte AirPlay en sourdine repart muette à la session
//! suivante. `set_mute` sans session RTSP échouait (« requires an active RTSP
//! session ») et la sourdine était perdue : la piste suivante partait au
//! niveau de l'enceinte. Aucun son : faux récepteur RTSP sur 127.0.0.1.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Un faux récepteur : répond 200 à chaque requête RTSP, rend leur texte.
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
        "Enceinte 8151".into(),
        "airplay:test-8151".into(),
        "127.0.0.1".into(),
        1,
    )
}

#[tokio::test]
async fn une_sourdine_donnee_sans_session_part_au_debut_de_la_session_suivante_8151() {
    let sortie = sortie();
    sortie
        .set_mute(true)
        .await
        .expect("#8151 : sans session, la sourdine doit attendre, pas être refusée");

    let (mut session, recepteur) = session_et_recepteur().await;
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    drop(session);
    let requetes = recepteur.await.unwrap();
    assert_eq!(
        requetes.len(),
        1,
        "une seule commande attendue : {requetes:?}"
    );
    assert!(
        requetes[0].starts_with("SET_PARAMETER ") && requetes[0].contains("volume: -144.0"),
        "#8151 : la session suivante doit partir muette : {requetes:?}"
    );
}

#[tokio::test]
async fn une_consigne_en_attente_en_sourdine_part_muette_8151() {
    let sortie = sortie();
    sortie.set_volume(0.30).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    let (mut session, recepteur) = session_et_recepteur().await;
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    drop(session);
    let requetes = recepteur.await.unwrap();
    assert_eq!(requetes.len(), 1, "{requetes:?}");
    assert!(requetes[0].contains("volume: -144.0"), "{requetes:?}");
}

#[tokio::test]
async fn la_sourdine_levee_rend_le_volume_a_la_session_8151() {
    let sortie = sortie();
    sortie.set_volume(0.30).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    let (session, recepteur) = session_et_recepteur().await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_mute(false).await.unwrap();
    sortie.rtsp_session.lock().await.take();
    let requetes = recepteur.await.unwrap();
    let attendu = format!("volume: {:.1}", linear_to_airplay_db(0.30));
    assert!(
        requetes.iter().any(|r| r.contains(&attendu)),
        "sourdine levée : le volume doit être rendu : {requetes:?}"
    );
}
