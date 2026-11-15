//! #8110 — témoins du repli sur le flux suivant quand le diffuseur refuse le
//! flux joué. Une « station » sur 127.0.0.1 sert, selon le chemin demandé,
//! un refus 403 (liste HLS réservée à un pays), un 404, ou le MP3 de
//! `tests/fixtures/test.mp3`. Aucun son, aucun réseau réel, chaque témoin
//! est borné.
use super::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MP3: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/test.mp3"
));

/// Ce qu'un chemin de la station répond.
#[derive(Clone, Copy)]
enum Reponse {
    Refus403,
    Refus451,
    Introuvable404,
    Mp3,
}

/// Une station sur un port éphémère : chaque chemin répond selon `routes`,
/// et le compteur dit combien de fois chacun a été demandé.
fn station(routes: Vec<(&'static str, Reponse)>) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let demandes: Arc<Mutex<HashMap<String, usize>>> = Default::default();
    let compteur = demandes.clone();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut s in listener.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let n = s.read(&mut buf).unwrap_or(0);
            let requete = String::from_utf8_lossy(&buf[..n]).into_owned();
            let chemin = requete.split_whitespace().nth(1).unwrap_or("/").to_string();
            *compteur.lock().unwrap().entry(chemin.clone()).or_insert(0) += 1;
            let reponse = routes
                .iter()
                .find(|(c, _)| *c == chemin)
                .map(|(_, r)| *r)
                .unwrap_or(Reponse::Introuvable404);
            match reponse {
                Reponse::Refus403 => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
                Reponse::Refus451 => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 451 Unavailable For Legal Reasons\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
                Reponse::Introuvable404 => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
                Reponse::Mp3 => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(MP3);
                }
            }
            let _ = s.flush();
        }
    });
    (format!("http://{addr}"), demandes)
}

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

fn demandes_de(demandes: &Mutex<HashMap<String, usize>>, chemin: &str) -> usize {
    demandes.lock().unwrap().get(chemin).copied().unwrap_or(0)
}

/// Le flux joué est refusé (403 sur la liste HLS réservée au Royaume-Uni) :
/// la lecture passe seule au flux suivant de la fiche et du PCM arrive sur la
/// même session, sans erreur rendue.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_flux_refuse_par_le_diffuseur_passe_au_flux_suivant_de_la_fiche() {
    let (base, demandes) = station(vec![
        ("/uk/master.m3u8", Reponse::Refus403),
        ("/ww/live.mp3", Reponse::Mp3),
    ]);
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let session_pour_le_decodeur = session.clone();
    let data_ready_pour_le_decodeur = data_ready.clone();
    let refuse = format!("{base}/uk/master.m3u8");
    let suivant = format!("{base}/ww/live.mp3");
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm_avec_profondeur(
            refuse,
            vec![suivant],
            tx,
            data_ready_pour_le_decodeur,
            session_pour_le_decodeur,
            None,
            None,
            false,
            None,
            false,
        )
    });

    tokio::time::timeout(Duration::from_secs(10), data_ready.notified())
        .await
        .expect("le flux suivant doit réveiller le consommateur en moins de 10 s : le refus du premier flux n'a pas fait passer au suivant");
    let morceau = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("un morceau de PCM doit arriver")
        .expect("le décodeur ne doit pas fermer le canal");
    assert_eq!(morceau.len(), 32_768);
    assert_eq!(session.detected_output_format(), Some((44_100, 2)));
    assert_eq!(
        demandes_de(&demandes, "/uk/master.m3u8"),
        1,
        "le flux refusé n'est essayé qu'une fois"
    );
    assert!(
        demandes_de(&demandes, "/ww/live.mp3") >= 1,
        "le flux suivant a été ouvert"
    );

    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    assert_eq!(
        verdict,
        Ok(()),
        "aucune erreur n'est rendue quand un flux de repli joue"
    );
}

/// Tous les flux de la fiche sont refusés : chacun est essayé une fois, dans
/// l'ordre, et c'est seulement alors que l'erreur « flux interdit » est
/// rendue, celle que l'auditeur lit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tous_les_flux_refuses_rendent_l_erreur_apres_les_avoir_tous_essayes() {
    let (base, demandes) = station(vec![
        ("/uk/master.m3u8", Reponse::Refus403),
        ("/uk2/master.m3u8", Reponse::Refus451),
        ("/uk3/master.m3u8", Reponse::Refus403),
    ]);
    let session = session_radio().await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let premier = format!("{base}/uk/master.m3u8");
    let replis = vec![
        format!("{base}/uk2/master.m3u8"),
        format!("{base}/uk3/master.m3u8"),
    ];
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm_avec_profondeur(
            premier, replis, tx, data_ready, session, None, None, false, None, false,
        )
    });
    let verdict = tokio::time::timeout(Duration::from_secs(20), decodeur)
        .await
        .expect("le décodeur doit rendre la main")
        .expect("pas de panique");
    let e = verdict.expect_err("tous les flux refusés : l'erreur doit être rendue");
    assert!(e.starts_with(RADIO_HLS_INTERDIT), "{e}");
    for chemin in ["/uk/master.m3u8", "/uk2/master.m3u8", "/uk3/master.m3u8"] {
        assert_eq!(
            demandes_de(&demandes, chemin),
            1,
            "{chemin} essayé une fois"
        );
    }
}

/// Un échec qui n'est PAS un refus du diffuseur (404 sur la liste HLS) garde
/// le comportement d'avant : l'erreur est rendue, le flux suivant n'est
/// jamais ouvert. La reconnexion d'une coupure réseau reste sur le même flux.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_echec_qui_n_est_pas_un_refus_ne_passe_pas_au_flux_suivant() {
    let (base, demandes) = station(vec![
        ("/mort/master.m3u8", Reponse::Introuvable404),
        ("/ww/live.mp3", Reponse::Mp3),
    ]);
    let session = session_radio().await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let premier = format!("{base}/mort/master.m3u8");
    let replis = vec![format!("{base}/ww/live.mp3")];
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm_avec_profondeur(
            premier, replis, tx, data_ready, session, None, None, false, None, false,
        )
    });
    let verdict = tokio::time::timeout(Duration::from_secs(20), decodeur)
        .await
        .expect("le décodeur doit rendre la main")
        .expect("pas de panique");
    let e = verdict.expect_err("un 404 reste un échec rendu");
    assert!(!e.starts_with(RADIO_HLS_INTERDIT), "{e}");
    assert_eq!(
        demandes_de(&demandes, "/ww/live.mp3"),
        0,
        "le flux suivant ne doit pas être ouvert"
    );
}
