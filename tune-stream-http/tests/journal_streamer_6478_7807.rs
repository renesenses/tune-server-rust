//! #7807 et #6478 — mesures au journal du streamer, sans changement de
//! comportement.
//!
//! - #7807 (WiiM Ultra qui passe seul à `STOPPED`) : chaque requête d'un
//!   renderer vers `/stream/…` laisse UNE ligne INFO `stream_requete_renderer`
//!   (méthode, chemin sans chaîne de requête, Range, agent, IP, statut,
//!   octets, durée à la fermeture) — `HEAD` et `404` compris.
//! - #6478 (FiiO K17, coupures DSD256) : `service_fichier_termine` sépare le
//!   temps passé à LIRE le fichier de celui passé à ENVOYER. Ici, un client
//!   qui cesse de lire 600 ms doit apparaître en `envoi_reseau_*`, pas en
//!   `lecture_disque_*`.
//!
//! Un seul test dans ce binaire : l'abonné global de `tracing` y est posé une
//! fois (même leçon que `fin_du_service_fichier_4645.rs`).
//!
//! Contre-épreuve : sans la couche `journal_requete_renderer_7807` posée sur
//! `router`, aucune ligne `stream_requete_renderer` n'est écrite et le test
//! échoue ; sans les mesures de `build_file_body`, les champs `envoi_reseau_*`
//! sont absents.
//!
//! Refs renesenses/tune-server-rust#7807, renesenses/tune-server-rust#6478

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use tune_core::http::streamer::{SharedSessions, StreamInfo, StreamSession};

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
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

/// ⚠️ Aiguilles assemblées à l'exécution : écrites en clair, une
/// contre-épreuve par recherche dans l'arbre se trouverait elle-même.
fn evenement(morceaux: &[&str]) -> String {
    morceaux.join("_")
}

fn lignes(journal: &str, ev: &str) -> Vec<String> {
    journal
        .lines()
        .filter(|l| l.contains(ev))
        .map(str::to_string)
        .collect()
}

/// Valeur entière d'un champ `nom=123` dans une ligne du journal.
fn champ_u64(ligne: &str, nom: &str) -> u64 {
    let cle = format!(" {nom}=");
    let i = ligne
        .find(&cle)
        .unwrap_or_else(|| panic!("champ « {nom} » absent : {ligne}"))
        + cle.len();
    ligne[i..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("champ « {nom} » non numérique : {ligne}"))
}

/// Attend qu'une ligne contenant toutes les aiguilles apparaisse (la ligne
/// est écrite au `Drop` du corps, côté serveur, après la fin côté client).
async fn attendre_ligne(capture: &JournalCapture, repere: usize, aiguilles: &[&str]) -> String {
    for _ in 0..200 {
        let journal = capture.depuis(repere);
        if let Some(l) = journal
            .lines()
            .find(|l| aiguilles.iter().all(|a| l.contains(a)))
        {
            return l.to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!(
        "aucune ligne avec {aiguilles:?} dans :\n{}",
        capture.depuis(repere)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_streamer_journalise_chaque_requete_et_separe_disque_et_reseau() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let requete_ev = evenement(&["stream", "requete", "renderer"]);
    let bilan_ev = evenement(&["service", "fichier", "termine"]);

    // 32 Mio : au-delà des tampons de socket de la boucle locale, pour qu'un
    // client qui cesse de lire fasse réellement attendre l'envoi.
    const OCTETS: usize = 32 * 1024 * 1024;
    let fichier = tune_core::test_scratch::scratch_file("journal-6478-7807", ".flac");
    let chemin = fichier.path().to_path_buf();
    std::fs::write(&chemin, vec![0x5Au8; OCTETS]).expect("fichier de test");

    let id = "s7807";
    let info = StreamInfo {
        format: "flac".into(),
        mime_type: "audio/flac".into(),
        ..StreamInfo::default()
    };
    let session = Arc::new(StreamSession::new(id.into(), info, false, 8));
    *session.file_path.lock().await = Some(chemin.to_string_lossy().into_owned());
    let sessions: SharedSessions = Arc::new(tokio::sync::Mutex::new(
        [(id.to_string(), session)].into_iter().collect(),
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            tune_stream_http::router(sessions).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{adresse}/stream");

    // ─── 1. #7807 — HEAD : une ligne, statut 200, aucun octet. ───
    let repere = capture.repere();
    let r = client
        .head(format!("{base}/{id}.flac"))
        .header("User-Agent", "WiiM-Ultra/5.2.834652")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let ligne = attendre_ligne(&capture, repere, &[&requete_ev, "methode=HEAD"]).await;
    for attendu in [
        "chemin=/stream/s7807.flac",
        "agent=WiiM-Ultra/5.2.834652",
        "ip=127.0.0.1",
        "statut=200",
        "octets=0",
        "range=-",
    ] {
        assert!(ligne.contains(attendu), "« {attendu} » absent : {ligne}");
    }
    assert!(ligne.contains("INFO"), "{ligne}");

    // ─── 2. #7807 — session inconnue : le 404 laisse aussi sa ligne. ───
    let repere = capture.repere();
    let r = client
        .get(format!("{base}/inconnu.flac"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let ligne = attendre_ligne(&capture, repere, &[&requete_ev, "statut=404"]).await;
    assert!(ligne.contains("methode=GET"), "{ligne}");
    assert!(ligne.contains("chemin=/stream/inconnu.flac"), "{ligne}");

    // ─── 3. GET par Range, client lent : #7807 et #6478 ensemble. ───
    //
    // Le jeton de la chaîne de requête ne doit JAMAIS atteindre le journal.
    let secret = ["SECRET", "7807"].join("");
    let debut = 1_000_000u64;
    let repere = capture.repere();
    let r = client
        .get(format!("{base}/{id}.flac?token={secret}"))
        .header("User-Agent", "Lavf/58.29.100")
        .header("Range", format!("bytes={debut}-"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    // Aucun changement de comportement : la longueur annoncée est là.
    assert_eq!(
        r.content_length(),
        Some(OCTETS as u64 - debut),
        "le Content-Length du corps relayé doit rester celui d'origine"
    );
    let mut corps = r.bytes_stream();
    let mut recus = corps.next().await.unwrap().unwrap().len() as u64;
    // Le renderer cesse de tirer : c'est l'ENVOI qui attend, pas le disque.
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    while let Some(m) = corps.next().await {
        recus += m.unwrap().len() as u64;
    }
    assert_eq!(recus, OCTETS as u64 - debut);

    let requete = attendre_ligne(&capture, repere, &[&requete_ev, "statut=206"]).await;
    for attendu in [
        "methode=GET",
        "chemin=/stream/s7807.flac",
        "range=bytes=1000000-",
        "agent=Lavf/58.29.100",
        "ip=127.0.0.1",
        "corps_complet=true",
    ] {
        assert!(
            requete.contains(attendu),
            "« {attendu} » absent : {requete}"
        );
    }
    assert_eq!(champ_u64(&requete, "octets"), OCTETS as u64 - debut);
    assert!(champ_u64(&requete, "duree_ms") >= 600, "{requete}");
    assert!(
        !capture.depuis(0).contains(&secret),
        "la chaîne de requête (jeton) a fuité au journal"
    );

    let bilan = attendre_ligne(&capture, repere, &[&bilan_ev]).await;
    let envoi_max = champ_u64(&bilan, "envoi_reseau_max_ms");
    assert!(
        envoi_max >= 300,
        "le client a cessé de lire 600 ms : l'envoi doit le montrer : {bilan}"
    );
    assert!(champ_u64(&bilan, "envoi_reseau_ms") >= envoi_max, "{bilan}");
    assert!(champ_u64(&bilan, "lectures") > 0, "{bilan}");
    let _ = champ_u64(&bilan, "lecture_disque_ms");
    let _ = champ_u64(&bilan, "lecture_disque_max_ms");
    let _ = champ_u64(&bilan, "lectures_lentes");

    // Pas de bruit : une requête = une ligne, rien par paquet.
    let tout = capture.depuis(0);
    assert_eq!(lignes(&tout, &requete_ev).len(), 3, "{tout}");
}
