//! #5612 — un balayage des sorties audio qui ne rend plus la main ne doit pas
//! entraîner ses appelants avec lui.
//!
//! Le core dump de Belkadi Yacine (fil 2093, 1.0.0-rc1 Linux, DENAFRIPS) montre
//! le balayage arrêté dans le greffon ALSA de PipeWire
//! (`_snd_pcm_pipewire_open` → `pw_thread_loop_stop` → `pthread_join`), sous
//! `list_audio_devices_uncached`, donc verrou d'énumération TENU. Les appelants
//! suivants le prenaient par `lock()` : ils attendaient pour toujours, et
//! `GET /devices/audio` le faisait depuis un fil de l'ordonnanceur tokio.
//!
//! Ces témoins n'ouvrent aucun matériel : le verrou est un verrou LOCAL tenu
//! par un fil du test (le balayage bloqué), et l'énumérateur est injecté.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

type Garde = Mutex<Option<(Instant, Vec<AudioDevice>)>>;

/// Un fil qui tient le verrou comme le ferait un balayage bloqué dans le
/// greffon PipeWire, jusqu'à ce que le test le libère (ou au plus `duree`).
fn balayage_bloque(
    verrou: Arc<Garde>,
    duree: Duration,
) -> (mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (pret_tx, pret_rx) = mpsc::channel();
    let (fin_tx, fin_rx) = mpsc::channel::<()>();
    let fil = std::thread::spawn(move || {
        let _garde = verrou.lock().unwrap();
        pret_tx.send(()).unwrap();
        let _ = fin_rx.recv_timeout(duree);
    });
    pret_rx.recv().unwrap();
    (fin_tx, fil)
}

#[test]
fn un_appelant_n_attend_pas_indefiniment_un_balayage_bloque() {
    let verrou: Arc<Garde> = Arc::new(Mutex::new(None));
    // Le balayage « bloqué » tient le verrou 10 s ; l'appelant ne doit en
    // attendre que 200 ms.
    let (fin, fil) = balayage_bloque(verrou.clone(), Duration::from_secs(10));

    let enumere = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel();
    {
        let verrou = verrou.clone();
        let enumere = enumere.clone();
        std::thread::spawn(move || {
            let debut = Instant::now();
            let _ = lister_sous_le_verrou_de_balayage(&verrou, Duration::from_millis(200), || {
                enumere.store(true, Ordering::SeqCst);
                Vec::new()
            });
            let _ = tx.send(debut.elapsed());
        });
    }

    let ecoule = rx.recv_timeout(Duration::from_secs(3));
    let _ = fin.send(());
    let _ = fil.join();
    let ecoule = ecoule.expect(
        "l'appelant attend encore le balayage bloqué après 3 s : chaque appel parque un fil \
         pour toujours, et depuis un gestionnaire async c'est un fil de l'ordonnanceur (#5612)",
    );
    assert!(
        ecoule >= Duration::from_millis(200),
        "rendu avant l'échéance : {ecoule:?}"
    );
    assert!(
        !enumere.load(Ordering::SeqCst),
        "un appelant qui n'a pas obtenu le verrou ne doit pas lancer un second balayage"
    );
}

#[test]
fn un_balayage_voisin_qui_finit_a_temps_est_attendu() {
    let verrou: Arc<Garde> = Arc::new(Mutex::new(None));
    let (fin, fil) = balayage_bloque(verrou.clone(), Duration::from_secs(10));
    let liberateur = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        let _ = fin.send(());
    });
    // Le verrou se libère à ~100 ms : `prendre_avant` doit l'obtenir, pas
    // abandonner.
    let garde = prendre_avant(&verrou, Duration::from_secs(5));
    assert!(
        garde.is_some(),
        "le verrou libéré dans le délai doit être pris"
    );
    drop(garde);
    liberateur.join().unwrap();
    fil.join().unwrap();
}

#[test]
fn un_verrou_empoisonne_reste_utilisable() {
    let verrou: Arc<Garde> = Arc::new(Mutex::new(None));
    let v = verrou.clone();
    let _ = std::thread::spawn(move || {
        let _g = v.lock().unwrap();
        panic!("balayage qui panique en tenant le verrou");
    })
    .join();
    assert!(verrou.is_poisoned());
    assert!(prendre_avant(&verrou, Duration::from_millis(50)).is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l_enumeration_async_rend_la_main_a_l_echeance() {
    let (fin_tx, fin_rx) = mpsc::channel::<()>();
    let debut = Instant::now();
    let parc = enumeration_bornee(Duration::from_millis(200), move || {
        // Un balayage qui ne rend pas la main (au plus 5 s, pour que le test
        // se termine même sans le correctif).
        let _ = fin_rx.recv_timeout(Duration::from_secs(5));
        Vec::new()
    })
    .await;
    let ecoule = debut.elapsed();
    let _ = fin_tx.send(());
    assert!(parc.is_none(), "un balayage bloqué ne rend pas de parc");
    assert!(
        ecoule < Duration::from_secs(2),
        "l'appelant async a attendu le balayage bloqué {ecoule:?} (#5612)"
    );
}

#[tokio::test]
async fn l_enumeration_async_qui_panique_ne_tue_pas_l_appelant() {
    let parc =
        enumeration_bornee(Duration::from_secs(5), || panic!("sonde ALSA qui panique")).await;
    assert!(parc.is_none());
}

#[tokio::test]
async fn l_enumeration_async_rend_le_parc_quand_elle_repond() {
    let parc = enumeration_bornee(Duration::from_secs(5), Vec::new).await;
    assert_eq!(parc.map(|p| p.len()), Some(0));
}
