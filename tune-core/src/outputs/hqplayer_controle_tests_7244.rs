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
    /// #7869 — octets non sollicités collés à une réponse : après la première
    /// seulement, ou après chacune (`toujours`).
    octets_apres_reponse: Option<(&'static [u8], bool)>,
    /// #7869 — octets poussés dès l'ouverture, sur chaque connexion après la
    /// première.
    octets_a_l_ouverture: Option<&'static [u8]>,
    /// #8395 — faux HQPlayer 3 Desktop : ses `Status` sont au format
    /// `min`/`sec` (journal 3.18.2), et une fois le moteur lancé par `Play`
    /// il pousse un `Status` sur cette connexion, AVANT l'acquittement du
    /// `Play` puis après chaque réponse (le pire ordre envisageable).
    hqplayer_3_pousse_avant_acquittement: bool,
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
                            let mut moteur_lance_ici = false;
                            if let Some(octets) = options.octets_a_l_ouverture
                                && id > 1
                                && socket.get_mut().write_all(octets).await.is_err()
                            {
                                return;
                            }
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
                                        "Status" if options.hqplayer_3_pousse_avant_acquittement => format!(r#"<Status state="{}" track="1" min="0" sec="1" clips="0"/>"#, e.transport),
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
                                if options.hqplayer_3_pousse_avant_acquittement {
                                    let mut trame = Vec::new();
                                    if root == "Play" {
                                        moteur_lance_ici = true;
                                        trame.extend_from_slice(STATUS_POUSSE_HQP3);
                                        trame.extend_from_slice(reponse.as_bytes());
                                    } else {
                                        trame.extend_from_slice(reponse.as_bytes());
                                        if moteur_lance_ici { trame.extend_from_slice(STATUS_POUSSE_HQP3); }
                                    }
                                    if socket.get_mut().write_all(&trame).await.is_err() { return; }
                                    continue;
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
                                } else if let Some((octets, toujours)) = options.octets_apres_reponse {
                                    let premiere = etat.lock().unwrap().commandes.len() == 1;
                                    let mut trame = reponse.into_bytes();
                                    if toujours || premiere { trame.extend_from_slice(octets); }
                                    if socket.get_mut().write_all(&trame).await.is_err() { return; }
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
    // #7869 : le Status suivant n'échoue plus, il est renvoyé sur une
    // connexion neuve ; le Pause en trop n'en devient toujours pas la réponse.
    let b2 = sortie
        .command("<Status subscribe=\"0\" />")
        .await
        .expect("#7869 : la commande suivante réussit après reconnexion");
    assert!(
        b2.starts_with("<Status") && !b2.contains("<Pause"),
        "#7244 : le Pause supplémentaire n'acquitte pas un Status : {b2}"
    );
    let e = b.etat.lock().unwrap();
    assert_eq!(e.connexions, 2, "#7244 : l'ancienne socket est abandonnée");
    assert_eq!(
        e.commandes[1].0, 2,
        "le second Status part sur la connexion neuve"
    );
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

// #7869 — HQPlayer 3 Desktop (testeur, rc10) : la zone reste hors ligne et
// chaque lecture échoue sur `hqplayer_control_unsolicited_response`. Ce que
// HQPlayer 3 envoie en trop n'est pas encore connu (pas de journal) : les
// formes ci-dessous sont des hypothèses compatibles avec ce message, qui ne
// naît que d'octets restés après une réponse ou arrivés entre deux commandes.

const STATUS_POUSSE: &[u8] = br#"<Status state="2" position="12" length="300"/>"#;
/// #8395 — un `Status` poussé par HQPlayer Desktop 3.18.2 (journal public).
const STATUS_POUSSE_HQP3: &[u8] = br#"<Status state="2" track="1" min="0" sec="2" clips="0"/>"#;

#[tokio::test]
async fn octets_non_sollicites_une_fois_la_commande_reussit_apres_reconnexion_7869() {
    let b = Banc::nouveau(Options {
        octets_apres_reponse: Some((STATUS_POUSSE, false)),
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    sortie.action("<Pause />").await.unwrap();
    let statut = sortie
        .get_status()
        .await
        .expect("#7869 : des octets non sollicités ne doivent plus faire échouer la commande");
    assert_eq!(
        statut.state,
        TransportState::Paused,
        "#7244 : le Status poussé (state=2) ne devient pas la réponse"
    );
    let e = b.etat.lock().unwrap();
    assert_eq!(e.connexions, 2, "une seule reconnexion");
    assert_eq!(
        e.commandes.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1, 2],
        "le Status n'est écrit qu'une fois, sur la connexion neuve"
    );
}

#[tokio::test]
async fn octets_non_sollicites_a_chaque_fois_erreur_nommee_apres_la_seconde_tentative_7869() {
    let b = Banc::nouveau(Options {
        octets_apres_reponse: Some((STATUS_POUSSE, true)),
        octets_a_l_ouverture: Some(STATUS_POUSSE),
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    sortie.action("<Pause />").await.unwrap();
    let erreur = sortie
        .get_status()
        .await
        .expect_err("#7869 : la seconde connexion est aussi encombrée");
    assert_eq!(
        erreur,
        format!("{CODE_OCTETS_NON_SOLLICITES}: {MESSAGE_OCTETS_NON_SOLLICITES}")
    );
    assert_eq!(
        crate::orchestrator::PlaybackOrchestrator::message_echec_sortie(
            &erreur,
            "auto",
            None,
            "audio/flac"
        ),
        MESSAGE_OCTETS_NON_SOLLICITES,
        "le message montré ne contient pas le code interne"
    );
    let e = b.etat.lock().unwrap();
    assert_eq!(e.connexions, 2, "deux tentatives, pas plus");
    assert_eq!(e.commandes.len(), 1, "le Status n'est jamais écrit");
}

/// Hypothèse HQPlayer 3 : chaque réponse est suivie d'octets en trop, un
/// octet nul ou un second document. La lecture complète doit aboutir.
#[tokio::test]
async fn hqplayer_3_octets_apres_chaque_reponse_la_lecture_aboutit_7869() {
    for octets in [&b"\0"[..], STATUS_POUSSE] {
        let b = Banc::nouveau(Options {
            octets_apres_reponse: Some((octets, true)),
            ..Default::default()
        })
        .await;
        let sortie = b.sortie();
        sortie
            .play_media(&media())
            .await
            .unwrap_or_else(|e| panic!("#7869 : lecture refusée avec {octets:?} : {e}"));
        assert_eq!(
            b.commandes("Play"),
            1,
            "le Play n'est jamais envoyé deux fois"
        );
        assert_eq!(
            sortie.get_status().await.unwrap().state,
            TransportState::Playing
        );
    }
}

/// #8395 — témoin : un faux HQPlayer 3 qui pousse ses `Status` dès `Play`,
/// avant même l'acquittement. Sans le correctif, `play_media` échouait sur
/// `hqplayer_control_reply_mismatch: expected Play, received <Status …`.
#[tokio::test]
async fn hqplayer_3_status_pousses_des_play_8395() {
    let b = Banc::nouveau(Options {
        hqplayer_3_pousse_avant_acquittement: true,
        ..Default::default()
    })
    .await;
    let sortie = b.sortie();
    let issue = sortie.play_media(&media()).await;
    let play = b.commandes("Play");
    eprintln!("8395 temoin: play_media={issue:?} Play={play}");
    issue.unwrap_or_else(|e| panic!("#8395 : lecture HQPlayer 3 refusée : {e}"));
    assert_eq!(play, 1, "le Play n'est jamais envoyé deux fois");
    let statut = sortie.get_status().await.expect("Status après Play");
    assert_eq!(statut.state, TransportState::Playing);
    sortie.action("<Pause />").await.expect("Pause après Play");
    assert_eq!(
        b.etat.lock().unwrap().connexions,
        1,
        "les Status poussés ne font pas rouvrir la connexion"
    );
}

#[test]
fn seuls_les_status_hqplayer_3_sont_retires_8395() {
    let mut pending = b" <Status state=\"2\" track=\"1\" min=\"0\" sec=\"2\" clips=\"0\"/>\n<Status state=\"2\" min=\"0\" sec=\"3\"/>".to_vec();
    retirer_les_status_pousses_hqplayer_3(&mut pending);
    assert!(pending.iter().all(u8::is_ascii_whitespace), "{pending:?}");
    for reste in [
        STATUS_POUSSE,
        &b"\0"[..],
        &b"<Pause result=\"OK\"/>"[..],
        &b"<Status state=\"2\" min=\"0\""[..],
    ] {
        let mut pending = [STATUS_POUSSE_HQP3, reste].concat();
        retirer_les_status_pousses_hqplayer_3(&mut pending);
        assert_eq!(pending, reste, "seul le Status HQPlayer 3 complet part");
    }
    // Un octet parasite DEVANT le Status : rien n'est retiré, #7869 s'applique.
    let colle = [&b"\0"[..], STATUS_POUSSE_HQP3].concat();
    let mut pending = colle.clone();
    retirer_les_status_pousses_hqplayer_3(&mut pending);
    assert_eq!(pending, colle);
}

#[test]
fn le_debut_des_octets_journalises_est_lisible_et_borne_7869() {
    assert_eq!(debut_des_octets(b"\0<Status/>"), "\\0<Status/>");
    assert_eq!(debut_des_octets(&[b'x'; 500]).len(), DEBUT_JOURNALISE);
    assert_eq!(debut_des_octets(&[0xff, b'a']), "\u{fffd}a");
}
