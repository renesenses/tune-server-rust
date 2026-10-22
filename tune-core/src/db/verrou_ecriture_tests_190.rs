//! Ticket 190 : la mise en texte d'une pile ne retient pas le verrou.
//! Le formateur reste bloqué par le banc jusqu'à ce que le détenteur ait
//! rendu puis repris la connexion. Aucun rapport de durées n'est supposé.
use super::{Sentinelle, VerrouEcriture, armer_les_piles};
use rusqlite::Connection;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

struct Blocage {
    commence: mpsc::Sender<()>,
    libre: Mutex<bool>,
    reveil: Condvar,
}
static RETARDS: Mutex<Vec<(&'static str, Arc<Blocage>)>> = Mutex::new(Vec::new());

pub(super) fn retard_de_formatage(fil: &str) {
    let blocage = RETARDS
        .lock()
        .unwrap()
        .iter()
        .find(|(nom, _)| *nom == fil)
        .map(|(_, b)| b.clone());
    if let Some(b) = blocage {
        let _ = b.commence.send(());
        let mut libre = b.libre.lock().unwrap();
        while !*libre {
            libre = b.reveil.wait(libre).unwrap();
        }
    }
}

fn eprouver(fil: &'static str, sentinelle: bool) {
    armer_les_piles();
    let verrou = VerrouEcriture::sans_sentinelle(
        Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
        Duration::ZERO,
    );
    let (commence_tx, commence_rx) = mpsc::channel();
    let blocage = Arc::new(Blocage {
        commence: commence_tx,
        libre: Mutex::new(false),
        reveil: Condvar::new(),
    });
    RETARDS.lock().unwrap().push((fil, blocage.clone()));
    let (pris_tx, pris_rx) = mpsc::channel();
    let (rendre_tx, rendre_rx) = mpsc::channel();
    let (fini_tx, fini_rx) = mpsc::channel();
    let v = verrou.clone();
    let detenteur = std::thread::Builder::new()
        .name(fil.into())
        .spawn(move || {
            let tenue = v.lock().unwrap();
            pris_tx.send(()).unwrap();
            rendre_rx.recv().unwrap();
            drop(tenue);
            let suivante = v.lock().unwrap();
            suivante.execute_batch("SELECT 1").unwrap();
            drop(suivante);
            fini_tx.send(()).unwrap();
        })
        .unwrap();
    pris_rx.recv().unwrap();
    let v = verrou.clone();
    let lecture = std::thread::spawn(move || {
        if sentinelle {
            let s = Sentinelle {
                surveilles: Mutex::new(Vec::new()),
                puits: Box::new(move |sig| {
                    assert_eq!(sig.detenteur.fil, fil);
                    assert!(sig.detenteur.pile.is_some());
                }),
            };
            s.surveiller(&v);
            s.tour();
        } else {
            let photo = v.releve().detenteur.expect("le relevé nomme le détenteur");
            assert_eq!(photo.fil, fil);
            assert!(photo.pile.is_some());
        }
    });
    let commence = commence_rx.recv_timeout(Duration::from_secs(15)).is_ok();
    rendre_tx.send(()).unwrap();
    let rendu = fini_rx.recv_timeout(Duration::from_secs(15)).is_ok();
    // Libérer même si le défaut gardé a bloqué le détenteur.
    *blocage.libre.lock().unwrap() = true;
    blocage.reveil.notify_all();
    lecture.join().unwrap();
    detenteur.join().unwrap();
    RETARDS.lock().unwrap().retain(|(nom, _)| *nom != fil);
    assert!(
        commence,
        "le formateur de pile doit être entré avant le rendu"
    );
    assert!(
        rendu,
        "la pile se met encore en texte sous le verrou : rendre puis reprendre la connexion attend le formateur"
    );
}

#[test]
fn rendre_la_connexion_n_attend_pas_la_mise_en_texte_de_la_pile_190() {
    eprouver("epreuve-190-detenteur", true);
}

#[test]
fn le_releve_d_un_gel_ne_retient_pas_le_detenteur_190() {
    eprouver("epreuve-190-releve", false);
}
