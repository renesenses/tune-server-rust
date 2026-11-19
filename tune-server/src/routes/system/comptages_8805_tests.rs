//! #8805 — un comptage lent ne fige plus l'exécuteur.
//!
//! Deux épreuves :
//!
//! * au niveau des ROUTES, sur une vraie base de fichier : un fil à part tient
//!   TOUTES les connexions de lecture (verrou de lecture artificiel), et les
//!   quatre routes sondées en boucle sont appelées pendant ce temps. L'épreuve
//!   tourne sur un exécuteur `current_thread` : un seul fil, donc une route qui
//!   attendrait sa connexion SUR ce fil (ce que faisait `count()` appelé dans
//!   le gestionnaire async) arrêterait aussi le métronome qui tourne à côté ;
//! * au niveau du cache, avec un comptage à délai injecté : un seul comptage en
//!   vol, un relevé lent resservi, un relevé rapide jamais.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::IntoResponse;

use super::{CacheDeComptage, DUREE_DE_RESERVE, SEUIL_DE_LENTEUR, encore_valable};
use crate::state::AppState;

/// Durée pendant laquelle les connexions de lecture sont tenues.
const DETENTION: Duration = Duration::from_millis(2_000);

/// Le plus grand trou toléré entre deux battements du métronome. Bien sous
/// [`DETENTION`] : une route qui bloquerait le fil le creuserait à ~2 s.
const TROU_MAX: Duration = Duration::from_millis(600);

/// Nombre de connexions du pool de lecture SQLite (`READ_POOL_SIZE` de
/// `tune_core::db::sqlite`, privé).
const LECTEURS: usize = 3;

/// Un métronome sur l'exécuteur : il bat toutes les 20 ms et retient le plus
/// grand écart observé entre deux battements.
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

/// Prend toutes les connexions de lecture sur un fil à part et les garde
/// [`DETENTION`] ; rend la main une fois qu'elles sont toutes tenues.
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

#[tokio::test(flavor = "current_thread")]
async fn les_routes_sondees_en_boucle_ne_figent_pas_l_executeur_8805() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();

    let (arret, trou_max_ms, tache) = metronome();
    // Laisser le métronome démarrer.
    tokio::time::sleep(Duration::from_millis(60)).await;
    let lecteurs = tenir_les_lecteurs(&state);
    let debut = Instant::now();

    let s = state.clone();
    let stats = tokio::spawn(async move { super::super::config::stats(State(s)).await.0 });
    let s = state.clone();
    let sante = tokio::spawn(async move {
        super::super::config::health(State(s))
            .await
            .into_response()
            .status()
    });
    let s = state.clone();
    let moniteur =
        tokio::spawn(async move { super::super::diagnostics::health_monitor(State(s)).await.0 });
    let s = state.clone();
    let admin = tokio::spawn(async move { super::super::admin::admin_health(State(s)).await.0 });

    let stats = stats.await.unwrap();
    let sante = sante.await.unwrap();
    let moniteur = moniteur.await.unwrap();
    let admin = admin.await.unwrap();
    let duree = debut.elapsed();

    arret.store(true, Ordering::SeqCst);
    tache.await.unwrap();
    lecteurs.join().unwrap();

    // L'épreuve a bien porté : les routes ont attendu les lecteurs.
    assert!(
        duree >= DETENTION - Duration::from_millis(500),
        "les routes n'ont pas attendu les lecteurs ({duree:?}) : l'épreuve ne prouve rien"
    );
    // Et pendant ce temps, l'exécuteur a continué de servir.
    let trou = Duration::from_millis(trou_max_ms.load(Ordering::SeqCst));
    assert!(
        trou < TROU_MAX,
        "#8805 : l'exécuteur a été figé {trou:?} pendant les comptages"
    );
    // Les réponses restent celles d'une base vide, saine.
    assert_eq!(stats["tracks"], 0);
    assert!(stats.get("tracks_by_source").is_some(), "{stats}");
    assert_eq!(sante, axum::http::StatusCode::OK);
    assert_eq!(moniteur["tracks"], 0);
    assert_eq!(admin["database"]["tracks"], 0);
}

/// Sans verrou ni lenteur, les routes rendent les vrais comptes, et un
/// comptage rapide n'est jamais resservi : une piste ajoutée se voit tout de
/// suite.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_comptage_rapide_n_est_jamais_resservi() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();

    let avant = super::super::config::stats(State(state.clone())).await.0;
    assert_eq!(avant["zones"], 0);
    let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    zones.create("Salon 8805", None, None).unwrap();
    let apres = super::super::config::stats(State(state.clone())).await.0;
    assert_eq!(
        apres["zones"], 1,
        "le compte d'avant a été resservi : {apres}"
    );
}

#[test]
fn seul_un_releve_lent_et_recent_est_resservi() {
    let lent = SEUIL_DE_LENTEUR;
    let rapide = Duration::from_millis(5);
    assert!(encore_valable(lent, Duration::ZERO));
    assert!(encore_valable(
        lent,
        DUREE_DE_RESERVE - Duration::from_millis(1)
    ));
    assert!(!encore_valable(lent, DUREE_DE_RESERVE), "périmé");
    assert!(!encore_valable(rapide, Duration::ZERO), "rapide : recompté");
}

/// Délai injecté : le comptage dort sur le pool bloquant pendant que
/// l'exécuteur (un seul fil) continue de battre ; les appels concurrents
/// partagent CE comptage au lieu d'en lancer d'autres, et le relevé lent est
/// resservi à l'appel suivant.
#[tokio::test(flavor = "current_thread")]
async fn un_comptage_lent_injecte_ne_bloque_pas_et_n_est_lance_qu_une_fois() {
    let cache: Arc<CacheDeComptage<i64>> = Arc::default();
    let lances = Arc::new(AtomicUsize::new(0));
    let (arret, trou_max_ms, tache) = metronome();
    tokio::time::sleep(Duration::from_millis(60)).await;

    let compter = |lances: Arc<AtomicUsize>| {
        move || {
            lances.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(SEUIL_DE_LENTEUR + Duration::from_millis(200));
            120_448_i64
        }
    };
    let mut appels = Vec::new();
    for _ in 0..4 {
        let c = cache.clone();
        let f = compter(lances.clone());
        appels.push(tokio::spawn(async move { c.lire("essai_8805", f).await }));
    }
    for a in appels {
        assert_eq!(a.await.unwrap(), Ok(120_448));
    }
    arret.store(true, Ordering::SeqCst);
    tache.await.unwrap();

    let trou = Duration::from_millis(trou_max_ms.load(Ordering::SeqCst));
    assert!(trou < TROU_MAX, "l'exécuteur a été figé {trou:?}");
    assert_eq!(lances.load(Ordering::SeqCst), 1, "un seul comptage en vol");

    // Lent, donc resservi : aucun nouveau comptage.
    assert_eq!(
        cache.lire("essai_8805", compter(lances.clone())).await,
        Ok(120_448)
    );
    assert_eq!(lances.load(Ordering::SeqCst), 1, "relevé lent resservi");
}

/// Un comptage qui panique ne fait pas tomber la route : `Err`, et le suivant
/// recompte.
#[tokio::test]
async fn un_comptage_qui_panique_rend_une_erreur() {
    let cache: Arc<CacheDeComptage<i64>> = Arc::default();
    let r = cache
        .lire("essai_8805", || -> i64 { panic!("comptage cassé") })
        .await;
    assert!(r.is_err());
    assert_eq!(cache.lire("essai_8805", || 7).await, Ok(7));
}
