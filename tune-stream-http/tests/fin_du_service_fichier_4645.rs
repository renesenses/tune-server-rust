//! #4645 — `service_fichier_termine … complet=false` ne disait pas QUI avait
//! lâché, et c'est ce qui bloque l'instruction du ticket.
//!
//! # Le fait de terrain
//!
//! Sevy Tabroc, renderer DLNA darTZeel LHC, zone 10. Deux fois, la connexion
//! HTTP qui porte le corps d'un WAV se ferme avant la fin — à 87,7 % du
//! fichier le 21/09 (0.9.159), à 83,6 % le 22/09 (0.9.161) :
//!
//! ```text
//! 15:35:35.776  service_fichier_termine stream_id=e13e4612-… octets=34471936
//!               demande=41235264 premier_octet_ms=0 elapsed_ms=209688
//!               debit_kio_s=160.5 complet=false
//! ```
//!
//! La v0.9.162 a livré deux travaux voisins — la reprise à la position
//! atteinte (#4660) et la mesure du terrain perdu (#4679). Aucun des deux ne
//! dit **pourquoi le flux se ferme**, et le ticket le nomme comme son fond.
//!
//! # Ce que la ligne ne pouvait pas dire
//!
//! Le corps est un générateur `async_stream` que la connexion hyper poursuit
//! elle-même. Il a quatre sorties :
//!
//! 1. il sert tout ce qui était annoncé et rend la main — le cas normal ;
//! 2. le fichier se termine avant la longueur annoncée (`read` → `Ok(0)`) —
//!    **cette sortie-là était entièrement SILENCIEUSE** ;
//! 3. une erreur d'ouverture, de positionnement ou de lecture — une ligne
//!    `file_*_error` la précède ;
//! 4. il ne rend **jamais** la main : hyper détruit le corps au milieu d'un
//!    `yield` parce que la connexion est partie.
//!
//! `ChronoServiceFichier::drop` s'exécutait à l'identique dans les quatre cas.
//! Un serveur à court de fichier et un renderer qui referme sa socket
//! écrivaient la même ligne, au même niveau, avec les mêmes champs. Aucune
//! lecture de journal ne pouvait les départager — et c'est exactement la
//! question ouverte du ticket.
//!
//! # Ce que ce témoin fixe
//!
//! Le champ `fin` de `service_fichier_termine`, et ses trois valeurs
//! observables depuis un test : `complet`, `fichier_plus_court`,
//! `consommateur_parti`.
//!
//! # ⛔ Ce qu'il n'établit PAS
//!
//! Que la cause de la fermeture soit connue. Ce champ sépare « le serveur a
//! cessé d'émettre » de « on a cessé de l'écouter » ; il ne nomme pas le
//! coupable d'un `consommateur_parti` — renderer, lien réseau, ou Tune
//! lui-même poussant une nouvelle URI sur le même appareil. Cette moitié-là se
//! lit dans le journal DLNA au même horodatage, et elle exige une nouvelle
//! occurrence chez le testeur. Aucun comportement n'est modifié par cette PR.
//!
//! # Pourquoi un binaire de test à lui seul, et un seul test dedans
//!
//! `tracing` met en cache POUR TOUT LE PROCESSUS la décision « ce point
//! d'appel intéresse-t-il quelqu'un ? » : un abonné global posé au milieu
//! d'une suite parallèle se voit priver d'évènements de façon imprévisible.
//! Même leçon que `journal_service_fichier_i2352.rs`, et les trois scénarios
//! se jouent donc à la suite DANS le même test.
//!
//! `tune-stream-http` n'a PAS `autotests = false` : ce fichier est compilé
//! sans déclaration `[[test]]`, et la caisse est nommée par la porte de test
//! de la CI.
//!
//! Refs renesenses/tune-server-rust#4645

use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use futures_util::StreamExt;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    #[allow(dead_code)]
    fn lire(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    /// Ce qui s'est écrit depuis le repère donné — les scénarios s'enchaînent
    /// dans le même processus, et chacun doit lire SA ligne.
    fn depuis(&self, repere: usize) -> String {
        let brut = self.0.lock().unwrap();
        String::from_utf8_lossy(&brut[repere.min(brut.len())..]).into_owned()
    }
    fn repere(&self) -> usize {
        self.0.lock().unwrap().len()
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

/// ⚠️ L'aiguille est assemblée à l'exécution : écrite en clair, une
/// contre-épreuve qui la cherche dans l'arbre se trouverait elle-même.
fn ligne_evenement(journal: &str, evenement: &str) -> String {
    journal
        .lines()
        .find(|l| l.contains(evenement))
        .unwrap_or_else(|| panic!("aucune ligne « {evenement} » dans :\n{journal}"))
        .to_string()
}

/// Une session FICHIER posée sur un fichier réel, servie par `handle_stream`.
async fn servir(
    id: &str,
    chemin: &std::path::Path,
    taille_annoncee: Option<u64>,
) -> axum::response::Response {
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        file_size: taille_annoncee,
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 8));
    *session.file_path.lock().await = Some(chemin.to_string_lossy().into_owned());
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session)].into_iter().collect(),
    ));
    tune_stream_http::handle_stream(
        Path(format!("{id}.wav")),
        State(sessions),
        axum::http::HeaderMap::new(),
    )
    .await
}

#[tokio::test]
async fn la_ligne_de_fin_dit_qui_a_lache_le_corps() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let evenement = ["service", "fichier", "termine"].join("_");
    const OCTETS: usize = 400_000;

    let fichier = tune_core::test_scratch::scratch_file("4645-fin", ".wav");
    let chemin = fichier.path().to_path_buf();
    std::fs::write(&chemin, vec![0x5Au8; OCTETS]).expect("fichier de test");

    // ─── 1. Le cas normal : tout ce qui était annoncé est parti. ───
    let repere = capture.repere();
    let reponse = servir("f4645-complet", &chemin, None).await;
    let mut corps = reponse.into_body().into_data_stream();
    let mut recus = 0usize;
    while let Some(m) = corps.next().await {
        recus += m.expect("erreur de flux").len();
    }
    assert_eq!(recus, OCTETS);
    drop(corps);
    tokio::task::yield_now().await;
    let ligne = ligne_evenement(&capture.depuis(repere), &evenement);
    assert!(
        ligne.contains("complet=true"),
        "le service est allé à son terme : {ligne}"
    );
    assert!(
        ligne.contains("fin=\"complet\"") || ligne.contains("fin=complet"),
        "une fin normale doit se nommer : sans ce champ, elle s'écrit comme un \
         abandon (#4645) : {ligne}"
    );

    // ─── 2. LE CAS DE SEVY : on cesse de lire au milieu du corps. ───
    //
    // C'est la fermeture de connexion du darTZeel, reproduite : le corps est
    // détruit alors qu'il lui restait à servir. Avant ce correctif, la ligne
    // était indiscernable du scénario 3 ci-dessous.
    let repere = capture.repere();
    let reponse = servir("f4645-abandon", &chemin, None).await;
    let mut corps = reponse.into_body().into_data_stream();
    // Un seul morceau, puis on lâche — comme une socket qui tombe. Pas de
    // boucle : clippy refuse, à raison, un `while` qui ne tourne jamais.
    let recus = corps
        .next()
        .await
        .expect("le corps devait rendre un premier morceau")
        .expect("erreur de flux")
        .len();
    assert!(
        recus > 0 && recus < OCTETS,
        "le scénario ne vaut que si le corps était INACHEVÉ : {recus}/{OCTETS}"
    );
    drop(corps);
    tokio::task::yield_now().await;
    let ligne = ligne_evenement(&capture.depuis(repere), &evenement);
    assert!(
        ligne.contains("complet=false"),
        "le corps était inachevé : {ligne}"
    );
    assert!(
        ligne.contains("fin=\"consommateur_parti\"") || ligne.contains("fin=consommateur_parti"),
        "🔴 LE POINT DU TICKET : un corps détruit en cours de route doit se \
         distinguer d'un serveur à court de fichier. Sans ce champ, les deux \
         écrivent la même ligne et le journal de Sevy ne peut pas être \
         instruit (#4645) : {ligne}"
    );

    // ─── 3. Le serveur à court de fichier : le corps promet plus. ───
    //
    // `serve_file` annonce le `Content-Length` d'après la taille lue sur le
    // disque, puis le générateur ouvre le fichier à son premier sondage. Entre
    // les deux, le fichier peut avoir rétréci — c'est le profil d'une source
    // posée sur un montage réseau, la seule hypothèse côté serveur que le
    // journal actuel ne pouvait pas écarter. On le reproduit tel quel :
    // réponse construite d'abord, fichier tronqué ensuite, corps vidé après.
    let repere = capture.repere();
    let reponse = servir("f4645-court", &chemin, None).await;
    std::fs::write(&chemin, vec![0x5Au8; 100_000]).expect("troncature");
    let mut corps = reponse.into_body().into_data_stream();
    let mut recus = 0usize;
    while let Some(m) = corps.next().await {
        recus += m.expect("erreur de flux").len();
    }
    drop(corps);
    tokio::task::yield_now().await;
    let ligne = ligne_evenement(&capture.depuis(repere), &evenement);
    assert!(
        recus < OCTETS,
        "le fichier tronqué ne peut pas rendre la longueur annoncée : {recus}"
    );
    assert!(
        ligne.contains("complet=false"),
        "moins que la longueur annoncée : {ligne}"
    );
    assert!(
        ligne.contains("fin=\"fichier_plus_court\"") || ligne.contains("fin=fichier_plus_court"),
        "un fichier plus court que sa promesse doit le DIRE : c'est la seule \
         sortie du générateur qui ne laissait aucune trace (#4645) : {ligne}"
    );

    drop(fichier);
}
