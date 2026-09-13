//! Le `Play` de HQPlayer natif doit rester lisible par son analyseur (#4023).
//!
//! ## Ce que Steve Taylor a mesuré
//!
//! Fil forum 1773, Tune 0.9.147 Linux, HQPlayer Embedded v5 sur la même
//! machine. Il a joint **les deux** journaux, ce qui est rare et ce qui tranche.
//!
//! Côté Tune, `play_media` déclare avoir tout envoyé :
//!
//! ```text
//! 21:55:14.399  hqplayer_play device=HQPlayer url="http://…/stream/ee876363-….flac"
//! 21:55:14.917  output_play_sent device_id=hqplayer-192.168.0.20
//! ```
//!
//! Côté HQPlayer, au même instant, la première commande est exécutée et la
//! seconde n'existe pas :
//!
//! ```text
//! & 2026/09/12 21:55:14 Playlist clear
//! & 2026/09/12 21:55:14 Playlist add URI: http://…/stream/ee876363-….flac
//! - 2026/09/12 21:55:20 Control ended from 192.168.0.20:45966
//! ```
//!
//! Alors qu'un `Play` déclenché depuis son interface web **est** journalisé,
//! preuve que HQPlayer sait tracer cette commande-là :
//!
//! ```text
//! & 2026/09/12 21:58:01 Play (-1/0)
//! + 2026/09/12 21:58:01 Playback engine running
//! ```
//!
//! ## Ce qui se passait
//!
//! `send_inner` préfixait **chaque** message de la déclaration XML, y compris
//! le second envoyé sur une connexion de contrôle déjà ouverte. HQPlayer lit
//! une connexion de contrôle comme UN flux XML : une déclaration au milieu du
//! flux est une erreur fatale pour un lecteur en flux. La première commande
//! passait, tout ce qui suivait sur la même connexion était jeté — et
//! `play_media` est le seul chemin qui pose DEUX commandes d'affilée
//! (`PlaylistAdd` puis `Play`). D'où le symptôme, exactement : la piste est
//! chargée, la lecture ne part pas.
//!
//! Le chronométrage du testeur le confirme : 518 ms entre `hqplayer_play` et
//! `output_play_sent`, soit un acquittement rapide sur le `PlaylistAdd` (v5
//! acquitte) puis les 400 ms pleines de `drain_brief` sur le `Play` — silence
//! total, parce que l'analyseur était déjà mort.
//!
//! ## Ce que ce témoin éprouve
//!
//! Un faux HQPlayer sur une socket locale enregistre les octets réellement
//! écrits par un `play_media` complet, et l'on vérifie sur le fil :
//!
//! 1. **une seule** déclaration XML sur la connexion ;
//! 2. le `Play` bien présent, **après** le `PlaylistAdd`.
//!
//! Le point 1 est le garde-fou : avant le correctif il en comptait deux.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tune_core::outputs::hqplayer::HqplayerOutput;
use tune_core::outputs::traits::{OutputTarget, PlayMedia};

/// Lit tout ce que le client écrit, jusqu'à ce qu'il ferme (ou 10 s de garde).
async fn octets_recus(mut socket: TcpStream) -> String {
    let mut vus: Vec<u8> = Vec::new();
    let mut tampon = [0u8; 4096];
    let garde = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match tokio::time::timeout_at(garde, socket.read(&mut tampon)).await {
            Ok(Ok(0)) => break,                               // fermeture propre
            Ok(Ok(n)) => vus.extend_from_slice(&tampon[..n]), // des octets
            Ok(Err(_)) => break,                              // socket cassée
            Err(_) => break,                                  // garde de 10 s
        }
    }
    String::from_utf8_lossy(&vus).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_play_suit_le_playlistadd_sans_seconde_declaration_xml() {
    // Un faux HQPlayer : il accepte, il enregistre, il ne répond jamais — ce
    // qui est aussi le cas de v6, donc le chemin fire-and-forget est le vrai.
    let ecoute = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("écoute locale");
    let port = ecoute.local_addr().expect("adresse locale").port();

    let faux_hqplayer = tokio::spawn(async move {
        let (socket, _) = ecoute.accept().await.expect("connexion de contrôle");
        octets_recus(socket).await
    });

    let sortie = HqplayerOutput::new(
        "HQPlayer".into(),
        format!("hqplayer-127.0.0.1:{port}"),
        "127.0.0.1".into(),
        port,
    );

    let media = PlayMedia {
        url: "http://192.168.0.20:8888/stream/ee876363-f149-4e8e-a6c5-0d0fdde57130.flac",
        mime_type: "audio/flac",
        title: Some("Euphoria: Der Traum"),
        ..Default::default()
    };

    sortie.play_media(&media).await.expect("play_media");

    // Fermer la connexion de contrôle : le faux HQPlayer voit l'EOF et rend
    // les octets sans attendre sa garde.
    drop(sortie);

    let sur_le_fil = faux_hqplayer.await.expect("tâche du faux HQPlayer");

    // 1. Une connexion de contrôle = UN flux XML = UNE déclaration.
    let declarations = sur_le_fil.matches("<?xml").count();
    assert_eq!(
        declarations, 1,
        "une connexion de contrôle HQPlayer ne porte qu'une déclaration XML ; \
         une seconde au milieu du flux tue son analyseur et le `Play` est perdu \
         (#4023). Compté {declarations}. Sur le fil :\n{sur_le_fil}"
    );

    // 2. Le `Play` est bien là, et il suit le `PlaylistAdd`.
    //    (chercher `<Play />` et non `<Play` : `<PlaylistAdd` commence pareil.)
    let pos_add = sur_le_fil
        .find("<PlaylistAdd")
        .unwrap_or_else(|| panic!("pas de PlaylistAdd sur le fil :\n{sur_le_fil}"));
    let pos_play = sur_le_fil
        .find("<Play />")
        .unwrap_or_else(|| panic!("pas de Play sur le fil :\n{sur_le_fil}"));
    assert!(
        pos_add < pos_play,
        "le Play doit suivre le PlaylistAdd, pas le précéder :\n{sur_le_fil}"
    );

    // 3. L'URL du flux est bien celle qu'on a demandée (garde-fou de framing :
    //    les deux commandes ne doivent pas se coller l'une à l'autre).
    assert!(
        sur_le_fil.contains("ee876363-f149-4e8e-a6c5-0d0fdde57130.flac"),
        "l'URL du flux doit partir intacte :\n{sur_le_fil}"
    );
    assert!(
        sur_le_fil.contains("</PlaylistAdd>\n<Play />"),
        "chaque commande est terminée par un saut de ligne, pour que deux \
         commandes consécutives restent distinctes dans le flux :\n{sur_le_fil}"
    );
}
