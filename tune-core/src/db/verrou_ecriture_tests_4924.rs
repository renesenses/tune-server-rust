//! Témoins de #4924 : un détenteur long du verrou d'écriture SQLite ne doit
//! plus figer l'exécuteur, et il doit être NOMMÉ pendant qu'il tient.

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use std::io::{Read, Write};
use std::sync::mpsc;

/// Une base SQLite sur fichier, avec une table où écrire.
fn base() -> (Arc<SqliteDb>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("dossier temporaire");
    let chemin = dir.path().join("gel4924.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).expect("ouverture");
    db.execute_batch("CREATE TABLE t (x INTEGER);")
        .expect("table");
    (Arc::new(db), dir)
}

/// Le gel du .18, en petit : un fil tient la connexion d'écriture quatre
/// secondes ; pendant ce temps, quatre tâches veulent écrire sur un exécuteur
/// de DEUX fils. Une requête HTTP doit quand même être servie en moins d'une
/// seconde.
///
/// Avant le correctif, les deux fils de l'exécuteur dorment dans le `futex`
/// du `std::sync::Mutex` : plus personne n'accepte la connexion, la requête
/// attend la libération (4 s) et le témoin rougit sur son délai.
#[test]
fn un_detenteur_long_du_verrou_n_affame_plus_l_executeur() {
    const TENUE: Duration = Duration::from_secs(4);
    const DELAI_HTTP: Duration = Duration::from_secs(1);

    let (db, _dir) = base();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    // Le serveur HTTP minimal, AVANT le détenteur : il écoute déjà.
    let ecoute = rt.block_on(async { tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap() });
    let adresse = ecoute.local_addr().unwrap();
    rt.spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut s, _)) = ecoute.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut tampon = [0u8; 512];
                let _ = s.read(&mut tampon).await;
                let _ = s
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });

    // Le détenteur : un fil ordinaire, comme une passe de fond.
    let (pris_tx, pris_rx) = mpsc::channel();
    let db_detenteur = db.clone();
    let detenteur = std::thread::spawn(move || {
        let _garde = db_detenteur.connection().lock().unwrap();
        pris_tx.send(()).unwrap();
        std::thread::sleep(TENUE);
    });
    pris_rx.recv().unwrap();

    // Plus d'écrivains que de fils d'exécuteur, depuis des tâches async —
    // ce que font les routes et les boucles de fond qui écrivent sans
    // `spawn_blocking`.
    for _ in 0..4 {
        let db = db.clone();
        rt.spawn(async move {
            let _ = DbBackend::execute(&*db, "INSERT INTO t (x) VALUES (1)", &[]);
        });
    }
    std::thread::sleep(Duration::from_millis(200));

    let debut = Instant::now();
    let mut client = std::net::TcpStream::connect(adresse).unwrap();
    client.set_read_timeout(Some(DELAI_HTTP)).unwrap();
    client
        .write_all(b"GET /health HTTP/1.1\r\nhost: x\r\n\r\n")
        .unwrap();
    let mut reponse = String::new();
    let lu = client.read_to_string(&mut reponse);
    let duree = debut.elapsed();

    detenteur.join().unwrap();
    rt.shutdown_timeout(Duration::from_secs(5));

    assert!(
        lu.is_ok() && reponse.contains("200 OK") && duree < DELAI_HTTP,
        "l'exécuteur est resté figé derrière le verrou d'écriture : réponse {:?} en {} ms \
         (délai {} ms, détention {} ms)",
        lu.map(|_| reponse),
        duree.as_millis(),
        DELAI_HTTP.as_millis(),
        TENUE.as_millis()
    );
}

/// Les écritures en attente aboutissent toutes une fois le verrou rendu :
/// sortir l'attente de l'exécuteur ne perd rien.
#[test]
fn les_ecrivains_en_attente_aboutissent_apres_la_liberation() {
    let (db, _dir) = base();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let garde = db.connection().lock().unwrap();
    let taches: Vec<_> = (0..6)
        .map(|_| {
            let db = db.clone();
            rt.spawn(async move { DbBackend::execute(&*db, "INSERT INTO t (x) VALUES (1)", &[]) })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(200));
    drop(garde);
    rt.block_on(async {
        for t in taches {
            t.await.unwrap().expect("insertion");
        }
    });
    let n = DbBackend::query_one(&*db, "SELECT COUNT(*) FROM t", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64();
    assert_eq!(n, Some(6));
}

fn verrou_d_essai(seuil: Duration) -> VerrouEcriture {
    let conn = Connection::open_in_memory().unwrap();
    VerrouEcriture::sans_sentinelle(Arc::new(Mutex::new(conn)), seuil)
}

fn sentinelle_d_essai() -> (Arc<Sentinelle>, mpsc::Receiver<Signalement>) {
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let s = Sentinelle::demarrer(
        Duration::from_millis(10),
        Box::new(move |sig: &Signalement| {
            let _ = tx.lock().unwrap().send(sig.clone());
        }),
    );
    (s, rx)
}

/// Au-delà du seuil, la sentinelle — sur SON fil — nomme le détenteur : le
/// lieu de l'appel à `lock()` (ce fichier-ci), son fil, sa durée.
#[test]
fn la_sentinelle_nomme_une_detention_au_dela_du_seuil() {
    let (sentinelle, rx) = sentinelle_d_essai();
    let verrou = verrou_d_essai(Duration::from_millis(100));
    sentinelle.surveiller(&verrou);

    let fil = std::thread::Builder::new()
        .name("detenteur-4924".into())
        .spawn({
            let verrou = verrou.clone();
            move || {
                let _g = verrou.lock().unwrap();
                std::thread::sleep(Duration::from_millis(400));
            }
        })
        .unwrap();

    let sig = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("aucun signalement pour une détention de 400 ms au seuil de 100 ms");
    fil.join().unwrap();
    assert!(
        sig.detenteur.lieu.contains("verrou_ecriture_tests_4924.rs"),
        "le lieu doit désigner l'appelant de lock(), obtenu : {}",
        sig.detenteur.lieu
    );
    assert_eq!(sig.detenteur.fil, "detenteur-4924");
    assert!(sig.detenteur.depuis >= Duration::from_millis(100));
    // Une seule détention : un seul signalement (le rappel est à 30 s).
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
}

/// En deçà du seuil, la sentinelle se tait.
#[test]
fn la_sentinelle_se_tait_en_deca_du_seuil() {
    let (sentinelle, rx) = sentinelle_d_essai();
    let verrou = verrou_d_essai(Duration::from_millis(300));
    sentinelle.surveiller(&verrou);
    for _ in 0..5 {
        let _g = verrou.lock().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "une détention de 20 ms ne doit rien signaler au seuil de 300 ms"
    );
    assert!(
        verrou.releve().detenteur.is_none(),
        "le relevé doit être vidé à la libération"
    );
}

/// Le relevé dit qui attend, et la sentinelle compte les attentes : c'est ce
/// qui distinguera, au prochain gel, « un écrivain lent » de « tout le monde
/// attend un seul détenteur ».
#[test]
fn le_releve_nomme_ceux_qui_attendent() {
    let (sentinelle, rx) = sentinelle_d_essai();
    let verrou = verrou_d_essai(Duration::from_millis(150));
    sentinelle.surveiller(&verrou);

    let garde = verrou.lock().unwrap();
    let attente = std::thread::Builder::new()
        .name("attente-4924".into())
        .spawn({
            let verrou = verrou.clone();
            move || {
                let _g = verrou.lock().unwrap();
            }
        })
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let releve = verrou.releve();
    assert_eq!(releve.attentes.len(), 1, "{releve}");
    assert_eq!(releve.attentes[0].fil, "attente-4924");
    assert!(
        releve.attentes[0]
            .lieu
            .contains("verrou_ecriture_tests_4924.rs")
    );
    assert!(releve.to_string().contains("en attente : 1"), "{releve}");

    let sig = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("signalement");
    assert_eq!(sig.attentes, 1);
    drop(garde);
    attente.join().unwrap();
    assert!(verrou.releve().attentes.is_empty());
}
