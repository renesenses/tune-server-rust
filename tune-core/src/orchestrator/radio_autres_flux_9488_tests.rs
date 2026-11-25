//! #9488 — témoins du passage aux autres flux de la station quand le flux par
//! défaut ne démarre pas (fil forum 3056). Une « station » sur 127.0.0.1
//! sert, selon le chemin demandé, un 503, un flux audio vide, ou le MP3 de
//! `tests/fixtures/test.mp3`. Aucun son, aucun réseau réel, chaque témoin est
//! borné.
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
    Indisponible503,
    /// `200 audio/mpeg` sans le moindre octet : un flux vide.
    FluxVide,
    Mp3,
}

/// Une station sur un port éphémère : chaque chemin répond selon `routes`
/// (un chemin inconnu répond 404), et le compteur dit combien de fois chacun
/// a été demandé.
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
            match routes.iter().find(|(c, _)| *c == chemin).map(|(_, r)| *r) {
                Some(Reponse::Indisponible503) => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
                Some(Reponse::FluxVide) => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
                Some(Reponse::Mp3) => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n"
                    );
                    let _ = s.write_all(MP3);
                }
                None => {
                    let _ = write!(
                        s,
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            }
            let _ = s.flush();
        }
    });
    (format!("http://{addr}"), demandes)
}

/// Une adresse où rien n'écoute : la connexion est refusée.
fn adresse_refusee() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    format!("http://127.0.0.1:{port}/ferme.mp3")
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

/// Lance le décodeur sur `premier` puis `replis`, attend le premier morceau
/// de PCM, ferme le consommateur et rend le verdict du décodeur avec les
/// annonces faites.
async fn jouer(premier: String, replis: Vec<String>) -> (Result<(), String>, Vec<RepliRadio>) {
    let session = session_radio().await;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let annonces: Arc<Mutex<Vec<RepliRadio>>> = Default::default();
    let note = annonces.clone();
    let annonce: AnnonceDeRepli =
        Box::new(move |r: &RepliRadio| note.lock().unwrap().push(r.clone()));
    let data_ready_pour_le_decodeur = data_ready.clone();
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm_avec_cadence(
            premier,
            replis,
            tx,
            data_ready_pour_le_decodeur,
            session,
            None,
            None,
            false,
            None,
            false,
            None,
            Some(annonce),
        )
    });
    tokio::time::timeout(Duration::from_secs(15), data_ready.notified())
        .await
        .expect("du PCM doit arriver en moins de 15 s : aucun flux de la station n'a été lu");
    let morceau = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("un morceau de PCM doit arriver")
        .expect("le décodeur ne doit pas fermer le canal");
    assert_eq!(morceau.len(), 32_768);
    drop(rx);
    let verdict = tokio::time::timeout(Duration::from_secs(15), decodeur)
        .await
        .expect("le décodeur doit rendre la main après le départ du consommateur")
        .expect("la tâche de décodage ne doit pas paniquer");
    let annonces = annonces.lock().unwrap().clone();
    (verdict, annonces)
}

/// LE témoin de l'issue : une station à trois flux dont le premier répond
/// 503 est lue sur le deuxième, sans intervention ; le troisième n'est jamais
/// ouvert, et le changement est annoncé UNE fois.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trois_flux_le_premier_en_503_la_lecture_part_sur_le_deuxieme() {
    let (base, demandes) = station(vec![
        ("/256.mp3", Reponse::Indisponible503),
        ("/128.mp3", Reponse::Mp3),
        ("/64.mp3", Reponse::Mp3),
    ]);
    retenir_les_debits_radio(vec![
        (format!("{base}/256.mp3"), 256),
        (format!("{base}/128.mp3"), 128),
        (format!("{base}/64.mp3"), 64),
    ]);
    let (verdict, annonces) = jouer(
        format!("{base}/256.mp3"),
        vec![format!("{base}/128.mp3"), format!("{base}/64.mp3")],
    )
    .await;
    assert_eq!(verdict, Ok(()), "aucune erreur quand un autre flux joue");
    assert_eq!(
        demandes_de(&demandes, "/256.mp3"),
        1,
        "le flux en 503 est essayé une fois"
    );
    assert!(
        demandes_de(&demandes, "/128.mp3") >= 1,
        "le deuxième flux a été ouvert"
    );
    assert_eq!(
        demandes_de(&demandes, "/64.mp3"),
        0,
        "le troisième flux n'est pas ouvert"
    );
    assert_eq!(annonces.len(), 1, "une seule annonce : {annonces:?}");
    let a = &annonces[0];
    assert_eq!(a.url_en_echec, format!("{base}/256.mp3"));
    assert_eq!(a.url_jouee, format!("{base}/128.mp3"));
    assert_eq!((a.debit_en_echec, a.debit_joue), (Some(256), Some(128)));
    assert_eq!(
        a.message(),
        "Flux 256 kbps indisponible, lecture du flux 128 kbps."
    );
}

/// Contre-épreuve : le flux préféré fonctionne. Il est lu, aucun autre flux
/// n'est essayé, et rien n'est annoncé.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_flux_prefere_qui_marche_est_lu_sans_essayer_les_autres() {
    let (base, demandes) = station(vec![
        ("/prefere.mp3", Reponse::Mp3),
        ("/autre1.mp3", Reponse::Mp3),
        ("/autre2.mp3", Reponse::Mp3),
    ]);
    let (verdict, annonces) = jouer(
        format!("{base}/prefere.mp3"),
        vec![format!("{base}/autre1.mp3"), format!("{base}/autre2.mp3")],
    )
    .await;
    assert_eq!(verdict, Ok(()));
    assert!(demandes_de(&demandes, "/prefere.mp3") >= 1);
    assert_eq!(
        demandes_de(&demandes, "/autre1.mp3"),
        0,
        "aucun autre flux essayé"
    );
    assert_eq!(
        demandes_de(&demandes, "/autre2.mp3"),
        0,
        "aucun autre flux essayé"
    );
    assert!(annonces.is_empty(), "rien à annoncer : {annonces:?}");
}

/// Connexion refusée puis flux vide : les deux premiers flux sont écartés
/// chacun une fois, le troisième joue ; une seule annonce, qui nomme le flux
/// par défaut (le premier en échec), pas le deuxième.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connexion_refusee_puis_flux_vide_la_lecture_part_sur_le_troisieme() {
    let (base, demandes) = station(vec![
        ("/vide.mp3", Reponse::FluxVide),
        ("/bon.mp3", Reponse::Mp3),
    ]);
    let ferme = adresse_refusee();
    let (verdict, annonces) = jouer(
        ferme.clone(),
        vec![format!("{base}/vide.mp3"), format!("{base}/bon.mp3")],
    )
    .await;
    assert_eq!(verdict, Ok(()));
    assert_eq!(
        demandes_de(&demandes, "/vide.mp3"),
        1,
        "le flux vide n'est essayé qu'une fois"
    );
    assert!(demandes_de(&demandes, "/bon.mp3") >= 1);
    assert_eq!(annonces.len(), 1, "une seule annonce : {annonces:?}");
    assert_eq!(annonces[0].url_en_echec, ferme);
    assert_eq!(annonces[0].url_jouee, format!("{base}/bon.mp3"));
}

/// Tous les flux échouent : chacun est essayé une fois, puis l'erreur est
/// rendue (celle que l'auditeur lit), sans annonce de repli.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tous_les_flux_en_echec_s_arrete_apres_les_avoir_tous_essayes() {
    let (base, demandes) = station(vec![
        ("/a.mp3", Reponse::Indisponible503),
        ("/b.mp3", Reponse::Indisponible503),
        ("/c.mp3", Reponse::FluxVide),
    ]);
    let session = session_radio().await;
    let (tx, _rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let data_ready = Arc::new(tokio::sync::Notify::new());
    let annonces: Arc<Mutex<Vec<RepliRadio>>> = Default::default();
    let note = annonces.clone();
    let annonce: AnnonceDeRepli =
        Box::new(move |r: &RepliRadio| note.lock().unwrap().push(r.clone()));
    let premier = format!("{base}/a.mp3");
    let replis = vec![format!("{base}/b.mp3"), format!("{base}/c.mp3")];
    let decodeur = tokio::task::spawn_blocking(move || {
        decode_radio_stream_to_pcm_avec_cadence(
            premier,
            replis,
            tx,
            data_ready,
            session,
            None,
            None,
            false,
            None,
            false,
            None,
            Some(annonce),
        )
    });
    let verdict = tokio::time::timeout(Duration::from_secs(30), decodeur)
        .await
        .expect("le décodeur doit rendre la main")
        .expect("pas de panique");
    assert!(
        verdict.is_err(),
        "tous les flux en échec : l'erreur est rendue"
    );
    for chemin in ["/a.mp3", "/b.mp3", "/c.mp3"] {
        assert_eq!(
            demandes_de(&demandes, chemin),
            1,
            "{chemin} essayé une fois"
        );
    }
    assert!(annonces.lock().unwrap().is_empty());
}
