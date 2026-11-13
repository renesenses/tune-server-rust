//! #8624 — AirPlay 1 levait la sourdine de l'enceinte à chaque `set_volume` :
//! une limite de sécurité, une alarme ou le sondeur rendaient le son à une
//! zone toujours affichée en sourdine. Règle de #8165 : seul un geste de
//! volume (consigne puis `set_mute(false)`) lève la sourdine, au niveau du
//! geste. Aucun son : faux récepteur RTSP sur 127.0.0.1 qui note les niveaux.
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
        "Enceinte 8624".into(),
        "airplay:test-8624".into(),
        "127.0.0.1".into(),
        1,
    )
}

/// Les niveaux (`volume: <dB>`) reçus par l'enceinte, dans l'ordre.
fn niveaux(requetes: &[String]) -> Vec<String> {
    requetes
        .iter()
        .filter_map(|r| r.lines().find(|l| l.starts_with("volume: ")))
        .map(|l| l.trim().to_string())
        .collect()
}

fn niveau(volume: f64) -> String {
    format!("volume: {:.1}", linear_to_airplay_db(volume))
}

/// Enceinte en sourdine, session ouverte : un `set_volume` interne (limite,
/// alarme, sondeur, reprise) n'envoie aucun niveau et garde la sourdine.
#[tokio::test]
async fn set_volume_garde_la_sourdine_de_l_enceinte_8624() {
    let sortie = sortie();
    let (session, recepteur) = session_et_recepteur().await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.25).await.unwrap();
    sortie.set_volume(1.0).await.unwrap();
    let statut = sortie.get_status().await.unwrap();
    sortie.rtsp_session.lock().await.take();
    let recus = niveaux(&recepteur.await.unwrap());
    assert!(statut.muted, "#8624 : set_volume a levé la sourdine");
    assert_eq!(
        recus,
        vec![niveau(0.40), "volume: -144.0".to_string()],
        "#8624 : en sourdine, aucun niveau ne doit partir vers l'enceinte"
    );
}

/// Le geste (consigne puis levée) rend le son au niveau du geste, en un seul
/// envoi : pas de passage par l'ancien niveau.
#[tokio::test]
async fn un_geste_leve_la_sourdine_au_niveau_du_geste_8624() {
    let sortie = sortie();
    let (session, recepteur) = session_et_recepteur().await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.20).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    let statut = sortie.get_status().await.unwrap();
    sortie.rtsp_session.lock().await.take();
    let recus = niveaux(&recepteur.await.unwrap());
    assert!(!statut.muted);
    assert_eq!(
        recus,
        vec![niveau(0.40), "volume: -144.0".to_string(), niveau(0.20)]
    );
}

/// Sans session, la consigne donnée en sourdine attend : la session suivante
/// part muette (#8151), puis la levée envoie le niveau du geste.
#[tokio::test]
async fn hors_session_la_sourdine_tient_puis_le_geste_rend_son_niveau_8624() {
    let sortie = sortie();
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.20).await.unwrap();
    assert!(sortie.get_status().await.unwrap().muted);

    let (mut session, recepteur) = session_et_recepteur().await;
    sortie.appliquer_le_volume_en_attente(&mut session).await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_mute(false).await.unwrap();
    sortie.rtsp_session.lock().await.take();
    let recus = niveaux(&recepteur.await.unwrap());
    assert_eq!(recus, vec!["volume: -144.0".to_string(), niveau(0.20)]);
}

/// Hors sourdine, rien ne change : la consigne part tout de suite.
#[tokio::test]
async fn hors_sourdine_la_consigne_part_8624() {
    let sortie = sortie();
    let (session, recepteur) = session_et_recepteur().await;
    *sortie.rtsp_session.lock().await = Some(session);
    sortie.set_volume(0.30).await.unwrap();
    sortie.set_volume(0.50).await.unwrap();
    sortie.rtsp_session.lock().await.take();
    let recus = niveaux(&recepteur.await.unwrap());
    assert_eq!(recus, vec![niveau(0.30), niveau(0.50)]);
}
