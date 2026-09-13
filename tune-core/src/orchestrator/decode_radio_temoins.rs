//! Témoins de `decode_radio_stream_to_pcm` : un flux radio factice, décodé
//! JUSQU'AU PCM (REF-2 phase 2, #2219).
//!
//! Avant ces témoins, la seule épreuve qui traversait le décodeur de
//! production s'arrêtait à la porte HLS, avant le moindre octet de réseau.
//! Rien ne prouvait que la boucle `'reconnect` — connexion, sonde
//! Symphonia, décodage, découpe en morceaux de 32 768 octets, réveil du
//! consommateur, reconnexion en fin de flux — rende ce qu'elle rend. Ce
//! fichier sert la fixture `tests/fixtures/test.mp3` (44,1 kHz stéréo,
//! 192 kbit/s) depuis un serveur HTTP local et regarde ce qui sort.
//!
//! Chaque témoin est BORNÉ : un décodeur qui ne rend plus la main fait
//! tomber le test sur un message nommé, jamais sur un blocage de la CI.
use super::{RADIO_NOT_AUDIO, decode_radio_stream_to_pcm};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MP3: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test.mp3"
));

/// Une « station » sur un port éphémère : à la n-ième connexion elle sert la
/// n-ième réponse (type MIME, corps), puis répète la dernière indéfiniment.
/// Rend l'URL et le compteur de connexions acceptées. Pas de
/// `Content-Length` : comme une vraie radio, le corps finit quand le serveur
/// ferme.
fn station_factice(reponses: Vec<(&'static str, Vec<u8>)>) -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let connexions = Arc::new(AtomicUsize::new(0));
    let compteur = connexions.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut s in listener.incoming().flatten() {
            let n = compteur.fetch_add(1, Ordering::SeqCst);
            let (content_type, corps) = &reponses[n.min(reponses.len() - 1)];
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nConnection: close\r\n\r\n"
            );
            let _ = s.write_all(corps);
            let _ = s.flush();
        }
    });
    (format!("http://{addr}/live"), connexions)
}

/// La session de production, pour `publish_detected_output_format`. Le canal
/// PCM et le réveil sont créés par chaque témoin : `create_radio_session`
/// garde un émetteur de maintien à l'intérieur de la session, et c'est la
/// chute du RÉCEPTEUR, tenu par le test, qui doit dire au décodeur que le
/// consommateur est parti.
async fn session_radio() -> Arc<crate::http::streamer::StreamSession> {
    use crate::http::streamer::{AudioStreamer, StreamInfo};
    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "wav".to_string(),
        mime_type: "audio/wav".to_string(),
        ..StreamInfo::default()
    };
    let (_id, _tx, _data_ready, session) = streamer.create_radio_session(info, 4).await;
    session
}

/// Le chemin nominal : la station sert du MP3, le décodeur publie le format
/// réellement sondé, réveille le consommateur au premier morceau, sert des
/// morceaux de 32 768 octets de PCM 16 bits qui ne sont pas du silence, et
/// tape les VU-mètres au format de sortie. Quand le consommateur part, il
/// rend `Ok(())` sans attendre la fin de la station.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_decodeur_radio_rend_le_pcm_du_mp3_servi() {
    let (url, _connexions) = station_factice(vec![("audio/mpeg", MP3.to_vec())]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let (levels_tx, mut levels_rx) = tokio::sync::mpsc::unbounded_channel();
    let session_pour_le_decodeur = session.clone();
    let data_ready_pour_le_decodeur = data_ready.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(
            url,
            tx,
            data_ready_pour_le_decodeur,
            session_pour_le_decodeur,
            None,
            Some(levels_tx),
        )
    });

    tokio::time::timeout(Duration::from_secs(10), data_ready.notified())
        .await
        .expect("le premier morceau doit réveiller le consommateur en moins de 10 s");
    assert_eq!(
        session.detected_output_format(),
        Some((44_100, 2)),
        "le format publié doit être celui sondé dans le MP3 (44,1 kHz stéréo)"
    );

    let mut morceaux = 0usize;
    let mut octets = 0usize;
    let mut non_nuls = 0usize;
    while morceaux < 3 {
        let morceau = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("un morceau de PCM doit arriver en moins de 10 s")
            .expect("le décodeur ne doit pas fermer le canal avant trois morceaux");
        assert_eq!(morceau.len(), 32_768, "chaque morceau fait 32 768 octets");
        octets += morceau.len();
        non_nuls += morceau.iter().filter(|b| **b != 0).count();
        morceaux += 1;
    }
    assert!(octets >= 3 * 32_768);
    assert!(
        non_nuls > 1_000,
        "le PCM décodé ne doit pas être du silence : {non_nuls} octets non nuls sur {octets}"
    );

    let fenetre = tokio::time::timeout(Duration::from_secs(5), levels_rx.recv())
        .await
        .expect("les VU-mètres doivent recevoir une fenêtre")
        .expect("le bus des niveaux est ouvert");
    assert_eq!(fenetre.sample_rate, 44_100);
    assert_eq!(fenetre.channels, 2);
    assert_eq!(fenetre.bit_depth, 16);

    // Le consommateur s'en va : le décodeur doit rendre la main, `Ok(())`.
    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main en moins de 15 s après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    assert_eq!(verdict, Ok(()));
}

/// La fin du flux amont n'est PAS la fin de la lecture : la boucle se
/// reconnecte en place et continue de nourrir la MÊME session (Xavier, les
/// coupures de Radio France). Preuve : la station voit une seconde
/// connexion après avoir servi son MP3 en entier, et le PCM continue
/// d'arriver après elle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_fin_du_flux_amont_reconnecte_la_meme_session() {
    let (url, connexions) = station_factice(vec![("audio/mpeg", MP3.to_vec())]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let session_pour_le_decodeur = session.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(url, tx, data_ready, session_pour_le_decodeur, None, None)
    });

    // On draine tout ce qui vient, en comptant, jusqu'à ce que la station ait
    // accepté une seconde connexion ET qu'au moins un morceau soit arrivé
    // après elle.
    let borne = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut morceaux_avant = 0usize;
    let mut morceaux_apres = 0usize;
    loop {
        let reste = borne.saturating_duration_since(tokio::time::Instant::now());
        assert!(
            !reste.is_zero(),
            "aucune seconde connexion suivie de PCM en 20 s : la fin du flux amont \
             n'a pas provoqué de reconnexion (connexions={}, morceaux avant={morceaux_avant}, \
             après={morceaux_apres})",
            connexions.load(Ordering::SeqCst)
        );
        match tokio::time::timeout(reste, rx.recv()).await {
            Ok(Some(_morceau)) => {
                if connexions.load(Ordering::SeqCst) >= 2 {
                    morceaux_apres += 1;
                } else {
                    morceaux_avant += 1;
                }
                if morceaux_apres >= 1 {
                    break;
                }
            }
            Ok(None) => panic!(
                "le décodeur a fermé le canal au lieu de se reconnecter (connexions={}, \
                 morceaux avant={morceaux_avant})",
                connexions.load(Ordering::SeqCst)
            ),
            Err(_) => continue,
        }
    }
    assert!(
        morceaux_avant >= 1,
        "la première connexion doit déjà avoir livré du PCM"
    );

    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main en moins de 15 s après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    assert_eq!(verdict, Ok(()));
}

/// Une station qui, à la reconnexion, répond une page web : l'erreur est
/// DITE tout de suite (`radio_not_audio`), pas avalée par trente tentatives
/// (#1960, #3756). Le consommateur reste branché : c'est bien le décodeur
/// qui décide de rendre, et il rend `Err`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn une_page_web_a_la_reconnexion_est_dite_sans_attendre() {
    let (url, connexions) = station_factice(vec![
        ("audio/mpeg", MP3.to_vec()),
        (
            "text/html; charset=utf-8",
            b"<html>La station a ferme</html>".to_vec(),
        ),
    ]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    // Un consommateur qui lit tout, pour que le décodeur ne bloque jamais
    // sur un canal plein.
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let session_pour_le_decodeur = session.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm(url, tx, data_ready, session_pour_le_decodeur, None, None)
    });

    let verdict = tokio::time::timeout(Duration::from_secs(20), decodeur)
        .await
        .expect("le décodeur doit rendre en moins de 20 s : une page web ne se réessaie pas")
        .expect("la tâche de décodage ne doit pas paniquer");
    let erreur = verdict.expect_err("une page web à la reconnexion doit rendre Err");
    assert!(
        erreur.starts_with(RADIO_NOT_AUDIO),
        "l'erreur doit nommer radio_not_audio : {erreur}"
    );
    assert_eq!(
        connexions.load(Ordering::SeqCst),
        2,
        "exactement deux connexions : le MP3, puis la page web refusée"
    );
    drain
        .await
        .expect("le drain se termine quand le canal se ferme");
}
