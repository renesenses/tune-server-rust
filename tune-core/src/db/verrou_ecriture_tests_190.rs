//! Ticket 190 — la sentinelle ne met plus la pile du détenteur en texte en
//! tenant le verrou de relevé.
//!
//! Le journal du terrain montrait, à la même milliseconde,
//! `ecriture_sqlite_toujours_tenue tenue_ms=1163` et
//! `ecriture_sqlite_detention_longue tenue_ms=5414` pour la MÊME prise : la
//! sentinelle avait relevé la prise à 1,16 s, puis passé plus de quatre
//! secondes à résoudre les symboles de sa pile sous `detention`. Le
//! détenteur, qui rendait la connexion, attendait ce verrou ; l'écrivain
//! suivant aussi, connexion en main.
//!
//! L'épreuve rend la mise en texte lente à volonté (1,5 s), pour la seule
//! pile d'un fil nommé, et mesure ce que paient le détenteur qui rend et
//! l'écrivain qui suit.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::{Sentinelle, Signalement, VerrouEcriture, armer_les_piles};

/// Les fils dont la mise en texte de la pile est retardée, et de combien.
/// Une entrée par épreuve : elles tournent en parallèle.
static RETARDS: Mutex<Vec<(&'static str, Duration)>> = Mutex::new(Vec::new());

fn retarder(fil: &'static str, d: Option<Duration>) {
    let mut r = RETARDS.lock().unwrap_or_else(|e| e.into_inner());
    r.retain(|(f, _)| *f != fil);
    if let Some(d) = d {
        r.push((fil, d));
    }
}

/// Appelé par `formater_la_pile` sous `cfg(test)`.
pub(super) fn retard_de_formatage(fil_du_detenteur: &str) {
    let retard = RETARDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(fil, _)| *fil == fil_du_detenteur)
        .map(|(_, d)| *d);
    if let Some(d) = retard {
        std::thread::sleep(d);
    }
}

const FIL: &str = "epreuve-190-detenteur";
const LENTEUR: Duration = Duration::from_millis(1500);

/// Le détenteur tient 200 ms, la sentinelle (seuil 50 ms, tour de 20 ms)
/// relève sa prise et met sa pile en texte en 1,5 s. Rendre la connexion,
/// puis la reprendre et la rendre, doit rester bref.
#[test]
fn rendre_la_connexion_n_attend_pas_la_mise_en_texte_de_la_pile_190() {
    armer_les_piles();
    let verrou = VerrouEcriture::sans_sentinelle(
        Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
        Duration::from_millis(50),
    );
    let signalements = Arc::new(AtomicUsize::new(0));
    let compte = signalements.clone();
    let sentinelle = Sentinelle::demarrer(
        Duration::from_millis(20),
        Box::new(move |s: &Signalement| {
            if s.detenteur.fil == FIL {
                compte.fetch_add(1, Ordering::SeqCst);
            }
        }),
    );
    sentinelle.surveiller(&verrou);
    retarder(FIL, Some(LENTEUR));

    let v = verrou.clone();
    let (rendre, suivant) = std::thread::Builder::new()
        .name(FIL.into())
        .spawn(move || {
            let tenue = v.lock().unwrap();
            tenue.execute_batch("SELECT 1").unwrap();
            // Le temps que la sentinelle relève la prise et commence la
            // mise en texte de la pile.
            std::thread::sleep(Duration::from_millis(200));
            let t0 = Instant::now();
            drop(tenue);
            let rendre = t0.elapsed();
            // L'écrivain suivant : prendre, écrire, rendre.
            let t1 = Instant::now();
            let suivante = v.lock().unwrap();
            suivante.execute_batch("SELECT 1").unwrap();
            drop(suivante);
            (rendre, t1.elapsed())
        })
        .unwrap()
        .join()
        .unwrap();
    retarder(FIL, None);
    eprintln!("ticket 190 : rendre {rendre:?}, écrivain suivant {suivant:?}");

    // Contre-épreuve : la mise en texte a bien eu lieu, et elle a bien été
    // lente. Sinon l'épreuve passerait sans rien prouver.
    let limite = Instant::now() + Duration::from_secs(5);
    while signalements.load(Ordering::SeqCst) == 0 && Instant::now() < limite {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        signalements.load(Ordering::SeqCst),
        1,
        "la sentinelle n'a pas signalé la prise du fil {FIL}"
    );
    let borne = Duration::from_millis(300);
    assert!(
        rendre < borne && suivant < borne,
        "rendre la connexion a pris {rendre:?} et l'écrivain suivant {suivant:?} \
         (borne {borne:?}) : la pile se met encore en texte sous le verrou de relevé"
    );
}

/// Le relevé d'un gel (`releve`) suit la même règle : pendant qu'il met en
/// texte la pile du détenteur, le détenteur rend la connexion sans attendre.
#[test]
fn le_releve_d_un_gel_ne_retient_pas_le_detenteur_190() {
    armer_les_piles();
    let verrou = VerrouEcriture::sans_sentinelle(
        Arc::new(Mutex::new(Connection::open_in_memory().unwrap())),
        Duration::from_secs(60),
    );
    const FIL_RELEVE: &str = "epreuve-190-releve";
    let v = verrou.clone();
    let (pris_tx, pris_rx) = std::sync::mpsc::channel();
    let detenteur = std::thread::Builder::new()
        .name(FIL_RELEVE.into())
        .spawn(move || {
            let tenue = v.lock().unwrap();
            pris_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let t0 = Instant::now();
            drop(tenue);
            t0.elapsed()
        })
        .unwrap();
    pris_rx.recv().unwrap();
    retarder(FIL_RELEVE, Some(LENTEUR));
    let releve = verrou.releve();
    retarder(FIL_RELEVE, None);
    let rendre = detenteur.join().unwrap();
    let photo = releve.detenteur.expect("le relevé nomme le détenteur");
    assert_eq!(photo.fil, FIL_RELEVE);
    assert!(photo.pile.is_some(), "piles armées : la pile est au relevé");
    assert!(
        rendre < Duration::from_millis(300),
        "rendre la connexion a pris {rendre:?} pendant le relevé : la pile se \
         met encore en texte sous le verrou de relevé"
    );
}
