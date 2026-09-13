//! Le sondeur HQPlayer cesse de crier — et de jeter sa connexion (#4025).
//!
//! ## Ce qui était mesuré chez le testeur
//!
//! Export de journal de Steve Taylor (fil 1773, 0.9.147 Linux, #4023). La
//! ligne de rééquilibrage dit ce qui a été jeté :
//!
//! ```text
//! log_export_rebalanced lues=8000 rendues=1000
//!   ecartees={"tune_core::outputs::hqplayer": 135, …,
//!             "tune_server::routes::hqplayer": 266}
//! ```
//!
//! **401 des 416 lignes écartées viennent des deux modules HQPlayer.** Et ce
//! qui restait tournait en rond, une fois par minute, trois heures durant :
//!
//! ```text
//! 21:54:45.819  INFO …outputs::hqplayer: hqplayer_port_detected host="192.168.0.20" port=4321
//! 21:54:45.819  INFO …routes::hqplayer:  hqplayer_output_registered name=HQPlayer id=hqplayer-192.168.0.20 …
//! 21:54:45.820  INFO …routes::hqplayer:  hqplayer_zone_reconnected name=HQPlayer id=hqplayer-192.168.0.20
//! 21:55:45.877  INFO …outputs::hqplayer: hqplayer_port_detected host="192.168.0.20" port=4321
//! 21:55:45.877  INFO …routes::hqplayer:  hqplayer_output_registered …
//! 21:55:45.877  INFO …routes::hqplayer:  hqplayer_zone_reconnected …
//! ```
//!
//! Trois lignes INFO par minute, **4 320 par jour**, pour dire que rien n'a
//! changé. #2566 avait silencé la ligne du sondeur lui-même
//! (`hqplayer_poll_registered`, un seul exemplaire dans tout l'export) mais
//! pas les trois qui sont à l'intérieur de l'appel qu'il fait à chaque tour.
//!
//! ## Le second dégât, celui qui ne se voit pas dans le journal
//!
//! `OutputRegistry::register` **écrase** l'entrée existante. Ré-enregistrer à
//! chaque tour détruisait donc l'objet `HqplayerOutput` précédent toutes les
//! 60 s, **avec la connexion TCP de contrôle persistante qu'il portait**.
//!
//! ## Ce que ce témoin éprouve
//!
//! Un faux HQPlayer sur une socket locale, et **trois tours** de
//! `discover_and_register` — ce que le sondeur fait en trois minutes. On
//! compte les lignes INFO, et on vérifie l'identité de l'objet enregistré :
//!
//! 1. `hqplayer_output_registered` : **une** fois, pas trois ;
//! 2. `hqplayer_zone_reconnected` : **zéro** fois — la zone n'a jamais été
//!    hors ligne, donc il n'y a rien à « reconnecter » ;
//! 3. `hqplayer_port_detected` : **zéro** ligne INFO (passée en `debug!`) ;
//! 4. l'objet de sortie du registre est **le même** aux trois tours
//!    (`Arc::ptr_eq`) : sa connexion de contrôle survit au sondage.
//!
//! La première découverte, elle, reste dite : c'est le point 1, qui vaut 1 et
//! non 0.
//!
//! ⚠️ `tune-server` porte `autotests = false` : sans sa cible `[[test]]` dans
//! `tune-server/Cargo.toml`, ce fichier ne serait JAMAIS compilé et la porte
//! rendrait un vert contre rien.
//!
//! ## Pourquoi un binaire de test à lui seul
//!
//! Même raison que `journal_sondage_hqplayer.rs`, sa cible sœur : l'abonné
//! `tracing` est **global au processus** et sa décision « ce point d'appel
//! intéresse-t-il quelqu'un ? » est mise en cache. Un abonné posé au milieu
//! d'un binaire multi-essais se voit priver d'évènements sans prévenir.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Combien de tours du sondeur on rejoue (60 s chacun en service).
const TOURS: usize = 3;

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

fn lignes_info(texte: &str, marqueur: &str) -> usize {
    texte
        .lines()
        .filter(|l| l.contains(marqueur) && l.contains("INFO"))
        .count()
}

/// Un faux HQPlayer : il répond au `<GetInfo />` de la sonde comme un v5, et
/// referme. Il accepte autant de connexions qu'on lui en ouvre.
async fn faux_hqplayer(ecoute: TcpListener) {
    loop {
        let Ok((mut socket, _)) = ecoute.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let mut tampon = [0u8; 4096];
            // Une seule lecture suffit : la sonde n'envoie qu'un GetInfo.
            if socket.read(&mut tampon).await.unwrap_or(0) == 0 {
                return;
            }
            let reponse = concat!(
                r#"<?xml version="1.0" encoding="UTF-8"?>"#,
                r#"<Info name="HQPlayer" product="HQPlayer Embedded" version="5.11.0"/>"#,
            );
            let _ = socket.write_all(reponse.as_bytes()).await;
        });
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trois_tours_de_sondage_ne_font_pas_trois_lignes_ni_trois_sorties() {
    let capture = JournalCapture::default();
    // DEBUG et non INFO : on veut VOIR les lignes de remplacement passées en
    // `debug!`. Le comptage, lui, exige « INFO » sur la ligne — un abonné
    // posé trop haut rendrait ce test vert pour la mauvaise raison.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    // Port éphémère : ce témoin tourne sur une machine de compilation
    // partagée, il ne peut pas réserver 4321. C'est le port CONFIGURÉ, et la
    // sonde doit désormais l'essayer avant les deux standards.
    let ecoute = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("écoute locale");
    let port = ecoute.local_addr().expect("adresse locale").port();
    tokio::spawn(faux_hqplayer(ecoute));

    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("hqplayer_host", "127.0.0.1").expect("hôte");
    settings
        .set("hqplayer_port", &port.to_string())
        .expect("port");
    settings.set("hqplayer_enabled", "true").expect("activé");

    let mut instances = Vec::new();
    for tour in 0..TOURS {
        tune_server::routes::hqplayer::discover_and_register(&state)
            .await
            .unwrap_or_else(|e| panic!("tour {tour} : le faux HQPlayer doit être joignable : {e}"));
        let sortie = {
            let reg = state.outputs.lock().await;
            reg.get("hqplayer-127.0.0.1")
                .unwrap_or_else(|| panic!("tour {tour} : la sortie doit être enregistrée"))
        };
        instances.push(sortie);
    }

    let texte = capture.texte();

    // 1. La découverte est dite UNE fois, pas une par minute.
    assert_eq!(
        lignes_info(&texte, "hqplayer_output_registered"),
        1,
        "{TOURS} tours de sondage annoncent la sortie UNE fois ; \
         une ligne par tour, c'est 1 440 lignes INFO par jour pour dire que \
         rien n'a changé (#4025) — journal :\n{texte}"
    );

    // 2. « Reconnected » n'a de sens que si la zone était hors ligne.
    assert_eq!(
        lignes_info(&texte, "hqplayer_zone_reconnected"),
        0,
        "la zone n'a jamais été hors ligne : rien à reconnecter (#4025) — \
         journal :\n{texte}"
    );

    // 3. Le port détecté ne sort plus en INFO à chaque sondage.
    assert_eq!(
        lignes_info(&texte, "hqplayer_port_detected"),
        0,
        "`hqplayer_port_detected` est un `debug!` : il sortait à chaque tour \
         alors que le port n'avait pas bougé (#4025) — journal :\n{texte}"
    );

    // 4. La zone EST créée au premier tour : on borne le bruit, on ne rend
    //    pas le sondeur muet.
    assert_eq!(
        lignes_info(&texte, "hqplayer_zone_auto_created"),
        1,
        "la création de la zone reste annoncée, une fois :\n{texte}"
    );

    // 5. Le dégât invisible : l'objet de sortie — et donc sa connexion TCP de
    //    contrôle persistante — doit survivre aux tours suivants.
    for (tour, instance) in instances.iter().enumerate().skip(1) {
        assert!(
            Arc::ptr_eq(&instances[0], instance),
            "tour {tour} : `register` ÉCRASE l'entrée du registre ; \
             ré-enregistrer à chaque sondage détruit l'`HqplayerOutput` \
             précédent avec sa connexion de contrôle (#4025)"
        );
    }
}
