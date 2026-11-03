//! #7244 : les extraits cités de Windows/HQPlayer 5.17.2 nomment Stop sans
//! réponse, Status reçu pour PlaylistClear et Pause reçu pour Play. Ce banc
//! parle réellement TCP/XML ; aucun HQPlayer, DAC ni flux audio n'est ouvert.
//! Le délai artificiel du Stop dépasse l'ancienne fenêtre de 400 ms : il ne
//! prétend pas reconstituer un journal brut que l'issue ne contient pas.
use super::*;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

#[derive(Default, Clone)]
struct Options {
    silence: bool,
    mauvaise_reponse_play: bool,
    mauvaise_reponse_clear: bool,
    sortie_refusee: bool,
    stop_lent: bool,
    fragment_et_reponse_en_trop: bool,
}
#[derive(Default)]
struct Etat {
    commandes: Vec<(usize, String)>,
    transport: u8,
    uri: String,
    connexions: usize,
}
struct Banc {
    port: u16,
    etat: Arc<StdMutex<Etat>>,
    tache: tokio::task::JoinHandle<()>,
}
impl Drop for Banc {
    fn drop(&mut self) {
        self.tache.abort();
    }
}
impl Banc {
    async fn nouveau(options: Options) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let etat = Arc::new(StdMutex::new(Etat::default()));
        let partage = etat.clone();
        let tache = tokio::spawn(async move {
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    resultat = listener.accept() => {
                        let Ok((socket, _)) = resultat else { return; };
                        let etat = partage.clone();
                        let id = { let mut e = etat.lock().unwrap(); e.connexions += 1; e.connexions };
                        let options = options.clone();
                        clients.spawn(async move {
                            let mut socket = BufReader::new(socket);
                            let mut acquittement_retarde = false;
                            loop {
                                let mut line = String::new();
                                if socket.read_line(&mut line).await.unwrap_or(0) == 0 { return; }
                                let line = line.trim();
                                if line.is_empty() || line.starts_with("<?xml") { continue; }
                                let root = xml_root(line).unwrap();
                                let reponse = {
                                    let mut e = etat.lock().unwrap();
                                    e.commandes.push((id, line.into()));
                                    match root.as_str() {
                                        "Status" => format!(r#"<Status state="{}" position="0" length="300" title="Beyoncé"><metadata uri="{}" /></Status>"#, e.transport, e.uri),
                                        "Stop" => { e.transport = 0; r#"<Stop result="OK"/>"#.into() },
                                        "PlaylistClear" if options.mauvaise_reponse_clear => r#"<Status state="0" position="0" length="300"/>"#.into(),
                                        "PlaylistClear" => { if e.transport == 0 { e.uri.clear(); } r#"<PlaylistClear result="OK"/>"#.into() },
                                        "PlaylistAdd" => { e.uri = extract_xml_attr(line, "uri").unwrap(); r#"<PlaylistAdd result="OK"/>"#.into() },
                                        "Play" if options.mauvaise_reponse_play => r#"<Pause result="OK"/>"#.into(),
                                        "Play" => { if !options.sortie_refusee { e.transport = 2; } r#"<Play result="OK"/>"#.into() },
                                        "Pause" => { e.transport = 1; r#"<Pause result="OK"/>"#.into() },
                                        _ => format!(r#"<{root} result="OK"/>"#),
                                    }
                                };
                                if options.silence && root != "Status" {
                                    acquittement_retarde = true;
                                    continue;
                                }
                                if root == "Status" && acquittement_retarde {
                                    // L'ancien client reprendrait cette réponse décalée
                                    // pour celle du Status qu'il vient d'envoyer.
                                    if socket.get_mut().write_all(b"<Pause result=\"OK\"/>").await.is_err() { return; }
                                }
                                if root == "Stop" && options.stop_lent {
                                    tokio::time::sleep(std::time::Duration::from_millis(1170)).await;
                                }
                                if root == "Status" && options.fragment_et_reponse_en_trop {
                                    let bytes = reponse.as_bytes();
                                    let coupe = reponse.find('é').unwrap() + 1;
                                    if socket.get_mut().write_all(&bytes[..coupe]).await.is_err() { return; }
                                    tokio::task::yield_now().await;
                                    if socket.get_mut().write_all(&bytes[coupe..]).await.is_err() { return; }
                                    if socket.get_mut().write_all(b"<Pause result=\"OK\"/>").await.is_err() { return; }
                                } else if socket.get_mut().write_all(reponse.as_bytes()).await.is_err() { return; }
                            }
                        });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        });
        Self { port, etat, tache }
    }
    fn sortie(&self) -> HqplayerOutput {
        HqplayerOutput::new(
            "HQPlayer fixture 7244".into(),
            "hqplayer-7244".into(),
            "127.0.0.1".into(),
            self.port,
        )
    }
    fn commandes(&self, nom: &str) -> usize {
        self.etat
            .lock()
            .unwrap()
            .commandes
            .iter()
            .filter(|(_, c)| xml_root(c).as_deref() == Some(nom))
            .count()
    }
}
fn media() -> PlayMedia<'static> {
    PlayMedia {
        url: "http://127.0.0.1:8888/stream/7244.flac",
        mime_type: "audio/flac",
        ..Default::default()
    }
}

#[tokio::test]
async fn pause_ne_peut_pas_acquitter_play_7244() {
    let b = Banc::nouveau(Options {
        mauvaise_reponse_play: true,
        ..Default::default()
    })
    .await;
    let erreur = b
        .sortie()
        .action("<Play />")
        .await
        .expect_err("#7244 : Pause ne doit jamais acquitter Play");
    assert!(
        erreur.contains("hqplayer_control_reply_mismatch"),
        "{erreur}"
    );
}

#[tokio::test]
async fn no_reply_abandonne_la_socket_avant_status_7244() {
    let b = Banc::nouveau(Options {
        silence: true,
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    sortie.action("<Pause />").await.unwrap();
    let statut = sortie
        .get_status()
        .await
        .expect("#7244 : le Status ne doit pas recevoir le Pause retardé");
    assert_eq!(statut.state, TransportState::Paused);
    let e = b.etat.lock().unwrap();
    assert_eq!(
        e.connexions, 2,
        "#7244 : no_reply doit isoler les acquittements tardifs sur l'ancienne socket"
    );
    assert_ne!(e.commandes[0].0, e.commandes[1].0);
}

#[tokio::test]
async fn stop_lent_est_acquitte_avant_les_commandes_suivantes_7244() {
    let b = Banc::nouveau(Options {
        stop_lent: true,
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    sortie.action("<Pause />").await.unwrap(); // v5 a réellement acquitté.
    b.etat.lock().unwrap().transport = 2;
    sortie
        .play_media(&media())
        .await
        .expect("#7244 : l'acquittement Stop tardif ne doit pas décaler les réponses");
    assert_eq!(
        sortie.get_status().await.unwrap().state,
        TransportState::Playing
    );
    assert_eq!(b.commandes("Stop"), 1);
    assert_eq!(b.commandes("Play"), 1);
}

#[tokio::test]
async fn un_etat_cache_arrete_ne_permet_pas_de_sauter_stop_7244() {
    let b = Banc::nouveau(Default::default()).await;
    let sortie = b.sortie();
    *sortie.dernier_etat() = Some(TransportState::Stopped);
    b.etat.lock().unwrap().transport = 2;
    sortie.play_media(&media()).await.unwrap();
    assert_eq!(
        b.commandes("Stop"),
        1,
        "#7244 : l'état frais doit précéder le remplacement, même avec un cache arrêté"
    );
}

#[tokio::test]
async fn play_acquitte_mais_sortie_reste_a_zero_est_un_echec_7244() {
    let b = Banc::nouveau(Options {
        sortie_refusee: true,
        ..Default::default()
    })
    .await;
    let erreur = b
        .sortie()
        .play_media(&media())
        .await
        .expect_err("#7244 : un Play acquitté avec state=0 persistant doit échouer");
    assert!(erreur.contains("hqplayer_output_start_failed"), "{erreur}");
    assert_eq!(
        b.commandes("Play"),
        1,
        "#7244 : ne pas relancer le Play muet"
    );
    assert_eq!(b.commandes("Stop"), 1, "le lancement refusé est arrêté");
}

#[tokio::test]
async fn actions_silencieuses_v6_gardent_une_lecture_verifiee_7244() {
    let b = Banc::nouveau(Options {
        silence: true,
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    sortie
        .play_media(&media())
        .await
        .expect("les actions silencieuses v6 restent compatibles");
    assert_eq!(
        sortie.get_status().await.unwrap().state,
        TransportState::Playing
    );
    assert_eq!(b.commandes("Play"), 1);
    assert!(b.etat.lock().unwrap().connexions >= 4);
}

#[test]
fn le_cadrage_separe_deux_documents_et_garde_les_octets_utf8_7244() {
    let a = "<Status state=\"0\" title=\"Beyoncé\"/>";
    let flux = format!("{a}<Pause result=\"OK\"/>");
    assert_eq!(
        xml_boundary(flux.as_bytes()),
        Some(a.len()),
        "#7244 : le cadre XML doit garder le document suivant pour sa propre réponse"
    );
    let coupe = a.find('é').unwrap() + 1;
    assert_eq!(xml_boundary(&a.as_bytes()[..coupe]), None);
    assert_eq!(xml_root(a).as_deref(), Some("Status"));
}

#[tokio::test]
async fn une_reponse_supplementaire_ne_devient_pas_celle_de_la_requete_suivante_7244() {
    let b = Banc::nouveau(Options {
        fragment_et_reponse_en_trop: true,
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    let a = sortie.command("<Status subscribe=\"0\" />").await.unwrap();
    assert!(
        a.contains("Beyoncé"),
        "le découpage UTF-8 ne doit pas altérer les métadonnées"
    );
    assert!(
        !a.contains("<Pause"),
        "#7244 : une réponse n'en avale pas deux"
    );
    let erreur = sortie
        .command("<Status subscribe=\"0\" />")
        .await
        .expect_err("#7244 : le Pause supplémentaire n'acquitte pas un Status");
    assert!(erreur.contains("hqplayer_control_"), "{erreur}");
    sortie
        .command("<Status subscribe=\"0\" />")
        .await
        .expect("une requête suivante ouvre une connexion propre");
}

#[tokio::test]
async fn status_ne_peut_pas_acquitter_playlistclear_7244() {
    let b = Banc::nouveau(Options {
        mauvaise_reponse_clear: true,
        ..Default::default()
    })
    .await;
    let erreur = b
        .sortie()
        .action("<PlaylistClear />")
        .await
        .expect_err("#7244 : un Status ne doit jamais acquitter PlaylistClear");
    assert!(
        erreur.contains("hqplayer_control_reply_mismatch"),
        "{erreur}"
    );
}
