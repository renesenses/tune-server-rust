//! L'album qui n'avançait pas : la piste suivante atteint-elle HQPlayer ? (#4023)
//!
//! ## La question, et pourquoi elle se pose ici
//!
//! Steve Taylor (fil 1773) : *« I only get one song »*. Le journal joint
//! porte `set_queue_ok zone_id=9 n=1`, ce qui a d'abord été lu comme une file
//! d'album amputée. **C'est une mauvaise lecture** : cette ligne ne sort que
//! si `outcome.has_loss()` est faux (une perte part en `set_queue_incomplet`,
//! niveau `warn` — comptabilité de #2394, `tune-server/src/routes/
//! playback.rs`). Donc `inserted == requested == 1` : le serveur a reçu une
//! demande d'UNE piste et n'en a perdu aucune. La même zone 9 avait d'ailleurs
//! porté une file de 42 pistes le matin même
//! (`queue_metadata_restored zone_id=9 queue_len=42`).
//!
//! Il ne reste donc pas un défaut de construction de file, mais la vraie
//! plainte : **la file n'AVANCE pas**. Et l'avance, sur une sortie sans
//! enchaînement (`can_gapless = false` depuis #2196), repasse par le même
//! endroit que le démarrage :
//!
//! ```text
//! poller/tick.rs  Stopped × STOPPED_TICKS_THRESHOLD → natural_end
//!   → handle_track_end  (poller/fin_de_piste.rs)
//!   → avancer_avec_reprises → orchestrator.play_from_queue
//!   → transport.rs  envoyer_a_la_sortie
//!   → OutputTarget::play_media   ← ICI
//! ```
//!
//! Rien n'exclut `hqplayer` de ce chemin : les seules gardes typées du
//! sondeur nomment `"dlna"` et `"chromecast"`, et `can_gapless = false` joue
//! **en faveur** de l'avancement (`gapless_skipped_exclusive_output`, puis
//! `awaiting_dlna_transition = false`).
//!
//! L'instance de sortie, elle, est celle du registre : les deux pistes
//! passent donc par **la même** `HqplayerOutput`, donc par la même connexion
//! de contrôle persistante.
//!
//! ## Ce que ce témoin mesure
//!
//! Un faux HQPlayer enregistre les octets d'un album qui avance : deux
//! `play_media` d'affilée sur une seule sortie, soit **quatre** commandes sur
//! **une** connexion. On vérifie sur le fil que les quatre arrivent, dans
//! l'ordre, sous une seule déclaration XML.
//!
//! C'est la mesure demandée : avant le correctif de #4050, seule la première
//! commande de la connexion était analysable par HQPlayer — l'avance ne
//! pouvait donc pas aboutir même quand le sondeur la décidait. Ce témoin
//! l'établit au niveau du protocole, sans HQPlayer réel.
//!
//! ## Second temps : ce qui reste fragile, et qui est maintenant DIT
//!
//! `parse_state_from_xml` retombe sur `Stopped` pour toute réponse `<Status>`
//! dont il ne reconnaît aucun mot d'état. Or `Stopped` n'est pas neutre :
//! c'est lui qui fait avancer la file après cinq sondes et arrêter la zone
//! après trente. Une réponse d'une forme inattendue produirait donc
//! exactement « l'album s'arrête après une piste », **sans une ligne pour le
//! dire**. Le comportement ne change pas — le repli reste `Stopped` — mais il
//! sort désormais un `warn`, **une seule fois par sortie** (le sondage tourne
//! en boucle, cf. #4025).
//!
//! Rien ici ne prouve que HQPlayer v5 réponde dans une forme non reconnue :
//! ce n'est pas mesuré, et ce témoin ne le prétend pas. Il garde la ligne qui
//! permettra de le mesurer chez le prochain testeur.
//!
//! ⚠️ `tune-core` porte `autotests = false` : sans sa cible `[[test]]` dans
//! `tune-core/Cargo.toml`, ce fichier ne serait JAMAIS compilé.
//!
//! ## Un binaire à lui seul, un seul essai dedans
//!
//! Le second temps pose un abonné `tracing` **global au processus**, dont la
//! décision « ce point d'appel intéresse-t-il quelqu'un ? » est mise en cache.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tune_core::outputs::hqplayer::HqplayerOutput;
use tune_core::outputs::traits::{OutputTarget, PlayMedia, TransportState};

const PISTE_1: &str = "http://192.168.0.20:8888/stream/aaaa1111.flac";
const PISTE_2: &str = "http://192.168.0.20:8888/stream/bbbb2222.flac";

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Lit tout ce que le client écrit, jusqu'à ce qu'il ferme (ou 10 s de garde).
async fn octets_recus(mut socket: TcpStream) -> String {
    let mut vus: Vec<u8> = Vec::new();
    let mut tampon = [0u8; 4096];
    let garde = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(garde, socket.read(&mut tampon)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => vus.extend_from_slice(&tampon[..n]),
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&vus).into_owned()
}

fn media(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/flac",
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_album_qui_avance_pose_quatre_commandes_lisibles_sur_une_connexion() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    // ── Temps 1 : l'album avance, et les quatre commandes arrivent ───────
    let ecoute = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("écoute locale");
    let port = ecoute.local_addr().expect("adresse locale").port();
    let faux_hqplayer = tokio::spawn(async move {
        let (socket, _) = ecoute.accept().await.expect("connexion de contrôle");
        octets_recus(socket).await
    });

    // UNE sortie pour les deux pistes : c'est l'instance du registre que
    // `envoyer_a_la_sortie` reprend à chaque piste.
    let sortie = HqplayerOutput::new(
        "HQPlayer".into(),
        format!("hqplayer-127.0.0.1:{port}"),
        "127.0.0.1".into(),
        port,
    );

    sortie.play_media(&media(PISTE_1)).await.expect("piste 1");
    // Ce que fait `handle_track_end` → `play_from_queue` en fin de piste.
    sortie.play_media(&media(PISTE_2)).await.expect("piste 2");
    drop(sortie);

    let sur_le_fil = faux_hqplayer.await.expect("tâche du faux HQPlayer");

    // Une connexion de contrôle = UN flux XML = UNE déclaration, même quand
    // l'album y pose quatre commandes.
    let declarations = sur_le_fil.matches("<?xml").count();
    assert_eq!(
        declarations, 1,
        "quatre commandes sur une connexion, une seule déclaration XML : \
         au-delà de la première, HQPlayer n'analyse plus rien et l'avance de \
         file ne peut pas aboutir (#4023). Compté {declarations} :\n{sur_le_fil}"
    );

    // Les deux pistes, dans l'ordre, chacune suivie de son Play.
    let attendu = format!(
        "<PlaylistAdd uri=\"{PISTE_1}\" queued=\"0\" clear=\"1\"></PlaylistAdd>\n\
         <Play />\n\
         <PlaylistAdd uri=\"{PISTE_2}\" queued=\"0\" clear=\"1\"></PlaylistAdd>\n\
         <Play />\n"
    );
    assert!(
        sur_le_fil.ends_with(&attendu),
        "l'avance d'album doit poser PlaylistAdd/Play pour la piste 1 PUIS \
         pour la piste 2, sur la même connexion.\nAttendu en fin de \
         fil :\n{attendu}\nReçu :\n{sur_le_fil}"
    );

    // ── Temps 2 : une réponse `Status` non reconnue est DITE, une fois ───
    //
    // Le repli `Stopped` ne change pas ; ce qui change, c'est qu'il ne soit
    // plus muet. Sans cette ligne, une forme de réponse inattendue produit
    // « l'album s'arrête après une piste » et rien ne l'explique.
    let ecoute2 = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("écoute locale 2");
    let port2 = ecoute2.local_addr().expect("adresse locale 2").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = ecoute2.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut tampon = [0u8; 4096];
                // Valeur hors enum Signalyst (0..=3) : le diagnostic
                // des états réellement inconnus doit rester présent.
                while socket.read(&mut tampon).await.unwrap_or(0) > 0 {
                    let reponse = r#"<Status state="99" position="12.5" duration="300.0"/>"#;
                    if socket.write_all(reponse.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let sortie2 = HqplayerOutput::new(
        "HQPlayer".into(),
        format!("hqplayer-127.0.0.1:{port2}"),
        "127.0.0.1".into(),
        port2,
    );
    let etat1 = sortie2.get_status().await.expect("status 1");
    let etat2 = sortie2.get_status().await.expect("status 2");

    // Le comportement ne bouge pas : le repli reste `Stopped`.
    assert_eq!(etat1.state, TransportState::Stopped);
    assert_eq!(etat2.state, TransportState::Stopped);
    // Et le reste de la réponse est bien lu — la ligne ne dénonce pas une
    // réponse illisible, seulement un ÉTAT non reconnu.
    assert_eq!(etat1.position_ms, 12_500);

    let texte = capture.texte();
    let dits = texte
        .lines()
        .filter(|l| l.contains("hqplayer_status_etat_inconnu"))
        .count();
    assert_eq!(
        dits, 1,
        "deux sondages sur une réponse dont l'état n'est pas reconnu : une \
         seule ligne, mais UNE (le sondage tourne en boucle, #4025). \
         Journal :\n{texte}"
    );
    assert!(
        texte.contains("state=\\\"99\\\"") || texte.contains("state=\"99\""),
        "la ligne doit citer la réponse reçue, sinon elle ne mesure rien :\n{texte}"
    );
}
