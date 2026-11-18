//! #9116 — « suspendu = pas de parcours ».

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::extract::State;
use tune_core::taches_de_fond::Tache;

use crate::state::AppState;

#[test]
fn un_releve_n_est_resservi_qu_en_pause() {
    let s = Duration::from_secs;
    assert!(
        !resservir(false, s(0), None),
        "au travail : toujours recompté"
    );
    assert!(!resservir(false, s(0), Some(s(60))));
    assert!(resservir(true, s(3600), None), "en pause : figé");
    assert!(resservir(true, s(59), Some(s(60))));
    assert!(
        !resservir(true, s(60), Some(s(60))),
        "durée échue : recompté"
    );
}

/// Le témoin direct : en pause, le comptage n'est PAS appelé.
#[tokio::test]
async fn suspendu_le_comptage_ne_tourne_plus() {
    let releve: ReleveSuspendu<i64> = ReleveSuspendu::default();
    let appels = Arc::new(AtomicUsize::new(0));
    let compter = |n: i64| {
        let a = appels.clone();
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            n
        }
    };
    // Au travail : chaque appel recompte, comme avant.
    assert_eq!(releve.lire("t", false, None, compter(1)).await, Some(1));
    assert_eq!(releve.lire("t", false, None, compter(2)).await, Some(2));
    assert_eq!(appels.load(Ordering::SeqCst), 2);
    // En pause : le dernier relevé, sans comptage.
    for _ in 0..5 {
        assert_eq!(releve.lire("t", true, None, compter(99)).await, Some(2));
    }
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "en pause, aucune requête de comptage ne doit partir"
    );
    // Reprise : recompté.
    assert_eq!(releve.lire("t", false, None, compter(3)).await, Some(3));
    assert_eq!(appels.load(Ordering::SeqCst), 3);

    // Un serveur qui démarre DÉJÀ en pause : un relevé, un seul, puis figé.
    let neuf: ReleveSuspendu<i64> = ReleveSuspendu::default();
    assert_eq!(neuf.lire("t", true, None, compter(5)).await, Some(5));
    assert_eq!(neuf.lire("t", true, None, compter(6)).await, Some(5));
    assert_eq!(appels.load(Ordering::SeqCst), 4);
}

/// Des sondes simultanées pendant un comptage n'en lancent pas d'autres.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_seul_comptage_en_vol() {
    let releve: Arc<ReleveSuspendu<i64>> = Arc::new(ReleveSuspendu::default());
    let appels = Arc::new(AtomicUsize::new(0));
    let mut taches = Vec::new();
    for _ in 0..8 {
        let r = releve.clone();
        let a = appels.clone();
        taches.push(tokio::spawn(async move {
            r.lire("t", true, None, move || {
                a.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
                42
            })
            .await
        }));
    }
    for t in taches {
        assert_eq!(t.await.unwrap(), Some(42));
    }
    assert_eq!(appels.load(Ordering::SeqCst), 1);
}

/// Une piste candidate au DR (même montage que les témoins de la route).
fn piste_candidate(state: &AppState, id: i64) {
    let chemin = format!("/nulle/part/{id}.flac");
    let params: &[&dyn tune_core::db::backend::ToSqlValue] = &[&id, &chemin];
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, file_path, duration_ms, sample_rate, channels) \
             VALUES (?, 'Joga', ?, 300000, 44100, 2)",
            params,
        )
        .unwrap();
    tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(state.backend.clone())
        .set(id, "rg_analyzed", "1700000000")
        .unwrap();
}

/// Bout en bout, sur la route que l'écran sonde : la plage dynamique en
/// pause, une piste candidate de plus ne change PAS le chiffre (il n'a pas
/// été recompté) ; à la reprise, si.
#[tokio::test]
async fn la_jauge_dr_est_figee_pendant_la_pause() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    piste_candidate(&state, 1);
    let lire = |s: &AppState| {
        let s = s.clone();
        async move {
            let Json(v) =
                crate::routes::system::dynamic_range::dynamic_range_progress(State(s)).await;
            v["candidates"].clone()
        }
    };
    assert_eq!(lire(&state).await, 1);

    tune_core::taches_de_fond::mettre_en_pause(&state.backend, Tache::PlageDynamique).unwrap();
    piste_candidate(&state, 2);
    assert_eq!(
        lire(&state).await,
        1,
        "en pause, la jauge rend son dernier relevé, sans recompter"
    );

    tune_core::taches_de_fond::reprendre(&state.backend, Tache::PlageDynamique).unwrap();
    assert_eq!(lire(&state).await, 2, "à la reprise, la jauge recompte");
}

/// `/library/stats/completeness` : resservi quand TOUT est suspendu, recompté
/// sinon.
#[tokio::test]
async fn la_completude_est_resservie_quand_tout_est_suspendu() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let appels = Arc::new(AtomicUsize::new(0));
    let lire = |n: i64| {
        let a = appels.clone();
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "n": n }))
        }
    };
    let releve = &state.comptages.releves.completude;
    assert_eq!(
        corps_general(releve, "t", lire(1)).await.ok().unwrap()["n"],
        1
    );
    assert_eq!(
        corps_general(releve, "t", lire(2)).await.ok().unwrap()["n"],
        2
    );
    tune_core::taches_de_fond::tout_suspendre(&state.backend).unwrap();
    assert_eq!(
        corps_general(releve, "t", lire(3)).await.ok().unwrap()["n"],
        2
    );
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "tout suspendu : pas de recomptage"
    );
    // Une erreur remonte avec son statut, et n'est pas gardée.
    tune_core::taches_de_fond::tout_reprendre(&state.backend).unwrap();
    let e = corps_general(releve, "t", || {
        Err(crate::error::AppError::bad_request("x"))
    })
    .await
    .unwrap_err();
    assert_eq!(e.status, axum::http::StatusCode::BAD_REQUEST);
}

/// « Tout suspendre » prend effet en mémoire AVANT que la base ait fini
/// d'écrire : une base dont l'écrivain est tenu par un autre (ici une
/// transaction ouverte sur une seconde connexion) ne retarde plus l'arrêt
/// des passes et des jauges.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tout_suspendre_prend_effet_sans_attendre_la_base() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    let tenue = rusqlite::Connection::open(&chemin).unwrap();
    tenue.execute_batch("BEGIN IMMEDIATE").unwrap();

    let b = state.backend.clone();
    let ecriture =
        tokio::task::spawn_blocking(move || tune_core::taches_de_fond::tout_suspendre(&b));
    let debut = std::time::Instant::now();
    while !tune_core::taches_de_fond::tout_est_suspendu() {
        assert!(
            debut.elapsed() < Duration::from_secs(2),
            "la pause doit prendre effet sans attendre l'écriture"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !ecriture.is_finished(),
        "le témoin doit mordre : l'écriture est encore bloquée"
    );
    tenue.execute_batch("ROLLBACK").unwrap();
    ecriture.await.unwrap().unwrap();
    assert!(tune_core::taches_de_fond::tout_est_suspendu());
    // Et la base porte la pause : un redémarrage la retrouve.
    tune_core::taches_de_fond::oublier_pour_les_essais();
    tune_core::taches_de_fond::hydrater(&state.backend);
    assert!(tune_core::taches_de_fond::tout_est_suspendu());
}

/// Une écriture en échec rend les tâches à leur état d'avant : aucune pause
/// en mémoire que la base ne porte pas.
#[tokio::test]
async fn une_pause_qui_ne_s_ecrit_pas_est_retiree() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.backend.execute_batch("DROP TABLE settings").unwrap();
    assert!(tune_core::taches_de_fond::tout_suspendre(&state.backend).is_err());
    assert!(tune_core::taches_de_fond::suspendus().is_empty());
    assert!(tune_core::taches_de_fond::mettre_en_pause(&state.backend, Tache::ReplayGain).is_err());
    assert!(!tune_core::taches_de_fond::est_en_pause(Tache::ReplayGain));
}
