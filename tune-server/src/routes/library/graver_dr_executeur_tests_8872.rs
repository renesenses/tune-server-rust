//! #8872 (fil 2863, 572 000 pistes, Windows) — `GET /library/dr/gravure`
//! tenait un fil de l'exécuteur 66 s : l'inventaire des pistes mesurées et le
//! compte des DR déjà gravés étaient lus en synchrone dans le gestionnaire
//! async. `POST` (lancer) lisait le même inventaire de la même façon.
//!
//! L'épreuve est celle de #8805 : sur une vraie base de fichier, un fil à part
//! tient TOUTES les connexions de lecture pendant [`DETENTION`], et les routes
//! sont appelées pendant ce temps sur un exécuteur `current_thread`. Une route
//! qui attendrait sa connexion SUR ce fil arrêterait aussi le métronome qui
//! tourne à côté.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::IntoResponse;

use crate::state::AppState;

/// Durée pendant laquelle les connexions de lecture sont tenues.
const DETENTION: Duration = Duration::from_millis(2_000);
/// Le plus grand trou toléré entre deux battements du métronome.
const TROU_MAX: Duration = Duration::from_millis(600);
/// Connexions du pool de lecture SQLite (`READ_POOL_SIZE`, privé).
const LECTEURS: usize = 3;

fn metronome() -> (Arc<AtomicBool>, Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
    let arret = Arc::new(AtomicBool::new(false));
    let trou_max_ms = Arc::new(AtomicU64::new(0));
    let (a, t) = (arret.clone(), trou_max_ms.clone());
    let tache = tokio::spawn(async move {
        let mut dernier = Instant::now();
        while !a.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let ecart = dernier.elapsed().as_millis() as u64;
            t.fetch_max(ecart, Ordering::SeqCst);
            dernier = Instant::now();
        }
    });
    (arret, trou_max_ms, tache)
}

fn tenir_les_lecteurs(state: &AppState) -> std::thread::JoinHandle<()> {
    let tenant = state.clone();
    let (pris_tx, pris_rx) = std::sync::mpsc::channel();
    let fil = std::thread::spawn(move || {
        let db = tenant.db.as_ref().expect("moteur SQLite");
        let gardes: Vec<_> = (0..LECTEURS).map(|_| db.read_connection()).collect();
        pris_tx.send(()).unwrap();
        std::thread::sleep(DETENTION);
        drop(gardes);
    });
    pris_rx.recv().expect("les lecteurs sont tenus");
    fil
}

/// Mesure le plus grand trou du métronome pendant `route`, lancée alors que
/// les lecteurs sont tenus. Rend `(trou, durée de la route, réponse)`.
async fn sous_lecteurs_tenus<F, Fut, T>(state: &AppState, route: F) -> (Duration, Duration, T)
where
    F: FnOnce(AppState) -> Fut,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (arret, trou_max_ms, tache) = metronome();
    tokio::time::sleep(Duration::from_millis(60)).await;
    let lecteurs = tenir_les_lecteurs(state);
    let debut = Instant::now();
    let reponse = tokio::spawn(route(state.clone())).await.unwrap();
    let duree = debut.elapsed();
    arret.store(true, Ordering::SeqCst);
    tache.await.unwrap();
    lecteurs.join().unwrap();
    (
        Duration::from_millis(trou_max_ms.load(Ordering::SeqCst)),
        duree,
        reponse,
    )
}

fn base() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);
    (dir, state)
}

/// TÉMOIN #8872 — le statut de la gravure ne fige pas l'exécuteur pendant
/// que l'inventaire attend la base.
#[tokio::test(flavor = "current_thread")]
async fn le_statut_de_la_gravure_ne_fige_pas_l_executeur_8872() {
    let (_dir, state) = base();
    let (trou, duree, v) =
        sous_lecteurs_tenus(&state, |s| async move { super::statut(State(s)).await.0 }).await;
    assert!(
        duree >= DETENTION - Duration::from_millis(500),
        "la route n'a pas attendu les lecteurs ({duree:?}) : l'épreuve ne prouve rien"
    );
    assert!(
        trou < TROU_MAX,
        "#8872 : GET /library/dr/gravure a figé l'exécuteur {trou:?}"
    );
    // La réponse reste entière : rien à graver sur une base vide.
    assert_eq!(v["status"], "idle", "{v}");
    assert_eq!(v["a_graver"], 0, "{v}");
    assert_eq!(v["hors_format"], 0, "{v}");
    assert_eq!(v["dans_les_fichiers"], 0, "{v}");
}

/// TÉMOIN #8872 — lancer la gravure lit le même inventaire, hors de
/// l'exécuteur lui aussi, et la passe démarre toujours.
#[tokio::test(flavor = "current_thread")]
async fn lancer_la_gravure_ne_fige_pas_l_executeur_8872() {
    let (_dir, state) = base();
    let (trou, duree, statut) = sous_lecteurs_tenus(&state, |s| async move {
        super::lancer(State(s)).await.into_response().status()
    })
    .await;
    assert!(
        duree >= DETENTION - Duration::from_millis(500),
        "la route n'a pas attendu les lecteurs ({duree:?}) : l'épreuve ne prouve rien"
    );
    assert!(
        trou < TROU_MAX,
        "#8872 : POST /library/dr/gravure a figé l'exécuteur {trou:?}"
    );
    assert_eq!(statut, axum::http::StatusCode::ACCEPTED);
}
