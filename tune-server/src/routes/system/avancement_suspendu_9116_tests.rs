//! #9116 — « suspendu = pas de parcours ».

use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::extract::State;
use tune_core::taches_de_fond::Tache;

use crate::state::AppState;

/// Une révision fixe : la bibliothèque n'a pas bougé.
fn fixe() -> Revision {
    Some((7, 1))
}

#[test]
fn un_releve_n_est_resservi_qu_en_pause_et_sous_la_meme_revision() {
    let s = Duration::from_secs;
    let r = Some((7, 1));
    assert!(
        !resservir(false, r, r, s(0), None),
        "au travail : toujours recompté"
    );
    assert!(!resservir(false, r, r, s(0), Some(s(60))));
    assert!(resservir(true, r, r, s(3600), None), "en pause : figé");
    assert!(
        !resservir(true, r, Some((7, 2)), s(0), None),
        "la bibliothèque a bougé : recompté"
    );
    assert!(
        !resservir(true, r, Some((8, 1)), s(0), None),
        "une autre base : recompté"
    );
    assert!(
        !resservir(true, None, None, s(0), None),
        "révision illisible : jamais resservi"
    );
    assert!(!resservir(true, r, None, s(0), None));
    assert!(resservir(true, r, r, s(59), Some(s(60))));
    assert!(
        !resservir(true, r, r, s(60), Some(s(60))),
        "durée échue : recompté"
    );
}

#[test]
fn au_repos_seulement_tout_suspendu_et_sans_scan() {
    assert!(au_repos_selon(true, false));
    assert!(!au_repos_selon(true, true), "un scan tourne");
    assert!(!au_repos_selon(false, false), "une passe travaille");
    assert!(!au_repos_selon(false, true));
}

#[test]
fn un_releve_commence_apres_l_arrivee_se_reprend() {
    let t0 = Instant::now();
    let t1 = t0 + Duration::from_millis(5);
    assert!(compte_apres_l_arrivee(t1, t0));
    assert!(compte_apres_l_arrivee(t0, t0));
    assert!(!compte_apres_l_arrivee(t0, t1), "commencé avant : périmé");
}

/// Le témoin direct : en pause, sous la même révision, le comptage n'est PAS
/// appelé.
#[tokio::test]
async fn suspendu_le_comptage_ne_tourne_plus() {
    let releve: Arc<ReleveSuspendu<i64>> = Arc::default();
    let appels = Arc::new(AtomicUsize::new(0));
    let compter = |n: i64| {
        let a = appels.clone();
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            n
        }
    };
    // Au travail : chaque appel recompte, comme avant.
    assert_eq!(releve.lire("t", false, fixe, compter(1)).await, Some(1));
    assert_eq!(releve.lire("t", false, fixe, compter(2)).await, Some(2));
    assert_eq!(appels.load(Ordering::SeqCst), 2);
    // En pause : le dernier relevé, sans comptage.
    for _ in 0..5 {
        assert_eq!(releve.lire("t", true, fixe, compter(99)).await, Some(2));
    }
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "en pause, aucune requête de comptage ne doit partir"
    );
    // Reprise : recompté.
    assert_eq!(releve.lire("t", false, fixe, compter(3)).await, Some(3));
    assert_eq!(appels.load(Ordering::SeqCst), 3);

    // Un serveur qui démarre DÉJÀ en pause : un relevé, un seul, puis figé.
    let neuf: Arc<ReleveSuspendu<i64>> = Arc::default();
    assert_eq!(neuf.lire("t", true, fixe, compter(5)).await, Some(5));
    assert_eq!(neuf.lire("t", true, fixe, compter(6)).await, Some(5));
    assert_eq!(appels.load(Ordering::SeqCst), 4);

    // Sans révision lisible : jamais resservi.
    let sans: Arc<ReleveSuspendu<i64>> = Arc::default();
    assert_eq!(sans.lire("t", true, || None, compter(8)).await, Some(8));
    assert_eq!(sans.lire("t", true, || None, compter(9)).await, Some(9));
}

/// B1 de la relecture : un relevé pris pendant que la passe tourne, une
/// piste ajoutée, puis la pause — la première lecture en pause RECOMPTE
/// (l'ancien code resservait le relevé de lundi mercredi). Puis figé.
#[tokio::test]
async fn la_premiere_lecture_en_pause_recompte_si_la_bibliotheque_a_bouge() {
    let releve: Arc<ReleveSuspendu<i64>> = Arc::default();
    let revision = Arc::new(std::sync::atomic::AtomicI64::new(1));
    let rev = || {
        let r = revision.clone();
        move || Some((7, r.load(Ordering::SeqCst)))
    };
    let appels = Arc::new(AtomicUsize::new(0));
    let compter = |n: i64| {
        let a = appels.clone();
        move || {
            a.fetch_add(1, Ordering::SeqCst);
            n
        }
    };
    assert_eq!(releve.lire("t", false, rev(), compter(1)).await, Some(1));
    revision.fetch_add(1, Ordering::SeqCst); // une piste ajoutée
    assert_eq!(
        releve.lire("t", true, rev(), compter(2)).await,
        Some(2),
        "en pause, mais la bibliothèque a bougé depuis le relevé : recompté"
    );
    assert_eq!(releve.lire("t", true, rev(), compter(3)).await, Some(2));
    assert_eq!(appels.load(Ordering::SeqCst), 2);
    revision.fetch_add(1, Ordering::SeqCst); // une suppression pendant la pause
    assert_eq!(releve.lire("t", true, rev(), compter(4)).await, Some(4));
}

/// Des sondes simultanées pendant un comptage n'en lancent pas d'autres.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn un_seul_comptage_en_vol() {
    let releve: Arc<ReleveSuspendu<i64>> = Arc::default();
    let appels = Arc::new(AtomicUsize::new(0));
    let mut taches = Vec::new();
    for _ in 0..8 {
        let r = releve.clone();
        let a = appels.clone();
        taches.push(tokio::spawn(async move {
            r.lire("t", true, fixe, move || {
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

/// C1 de la relecture : HORS pause, sept sondes arrivées pendant un comptage
/// ne refont pas chacune un comptage complet à la file. Elles en partagent
/// UN, commencé après leur arrivée : deux comptages en tout, pas huit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hors_pause_les_attendants_partagent_un_comptage() {
    let releve: Arc<ReleveSuspendu<i64>> = Arc::default();
    let appels = Arc::new(AtomicUsize::new(0));
    let sonde = |r: Arc<ReleveSuspendu<i64>>, a: Arc<AtomicUsize>| {
        tokio::spawn(async move {
            r.lire("t", false, fixe, move || {
                let n = a.fetch_add(1, Ordering::SeqCst) as i64 + 1;
                std::thread::sleep(Duration::from_millis(150));
                n
            })
            .await
        })
    };
    let premiere = sonde(releve.clone(), appels.clone());
    tokio::time::sleep(Duration::from_millis(30)).await;
    let suivantes: Vec<_> = (0..7)
        .map(|_| sonde(releve.clone(), appels.clone()))
        .collect();
    assert_eq!(premiere.await.unwrap(), Some(1));
    for t in suivantes {
        assert_eq!(
            t.await.unwrap(),
            Some(2),
            "le comptage commencé après l'arrivée"
        );
    }
    assert_eq!(appels.load(Ordering::SeqCst), 2);
}

/// C3 de la relecture : un client qui abandonne pendant le comptage ne le
/// perd pas — la sonde suivante reprend le relevé au lieu d'en relancer un.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_client_qui_abandonne_ne_perd_pas_le_releve() {
    let releve: Arc<ReleveSuspendu<i64>> = Arc::default();
    let appels = Arc::new(AtomicUsize::new(0));
    let a = appels.clone();
    let abandon = tokio::time::timeout(
        Duration::from_millis(30),
        releve.lire("t", true, fixe, move || {
            a.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            7
        }),
    )
    .await;
    assert!(abandon.is_err(), "le client a abandonné");
    let a = appels.clone();
    let v = releve
        .lire("t", true, fixe, move || {
            a.fetch_add(1, Ordering::SeqCst);
            99
        })
        .await;
    assert_eq!(v, Some(7), "le relevé abandonné est arrivé quand même");
    assert_eq!(appels.load(Ordering::SeqCst), 1, "pas de second comptage");
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

/// Bout en bout, sur la route que l'écran sonde (B1) : un relevé pris
/// pendant que la passe tourne, une piste candidate de plus, puis la pause —
/// la première lecture en pause voit la piste. Ensuite, sans écriture, la
/// jauge est figée ; à la reprise, elle recompte.
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

    piste_candidate(&state, 2);
    tune_core::taches_de_fond::mettre_en_pause(&state.backend, Tache::PlageDynamique).unwrap();
    assert_eq!(
        lire(&state).await,
        2,
        "la bibliothèque a bougé depuis le relevé : la première lecture en pause recompte"
    );

    // Une piste candidate de plus SANS que la révision bouge (déclencheurs
    // retirés) : si la jauge recomptait, elle la verrait.
    for table in ["tracks", "track_metadata"] {
        for suffixe in ["ins", "upd", "del"] {
            state
                .backend
                .execute_batch(&format!(
                    "DROP TRIGGER IF EXISTS library_revision_{table}_{suffixe}"
                ))
                .unwrap();
        }
    }
    piste_candidate(&state, 3);
    assert_eq!(
        lire(&state).await,
        2,
        "en pause, même révision : la jauge rend son dernier relevé, sans recompter"
    );

    tune_core::taches_de_fond::reprendre(&state.backend, Tache::PlageDynamique).unwrap();
    assert_eq!(lire(&state).await, 3, "à la reprise, la jauge recompte");
}

/// `/library/stats/completeness` : resservi quand TOUT est suspendu, recompté
/// sinon.
#[tokio::test]
async fn la_completude_est_resservie_quand_tout_est_suspendu() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // État « au repos » fixé : le vrai suit des compteurs du processus (pauses,
    // scans) que d'autres tests font bouger en parallèle (#9116, CI PG).
    let au_repos = true;
    let au_travail = false;
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
        corps_general_selon(releve, "t", au_travail, fixe, lire(1))
            .await
            .ok()
            .unwrap()["n"],
        1
    );
    assert_eq!(
        corps_general_selon(releve, "t", au_travail, fixe, lire(2))
            .await
            .ok()
            .unwrap()["n"],
        2
    );
    assert_eq!(
        corps_general_selon(releve, "t", au_repos, fixe, lire(3))
            .await
            .ok()
            .unwrap()["n"],
        2
    );
    assert_eq!(
        appels.load(Ordering::SeqCst),
        2,
        "tout suspendu : pas de recomptage"
    );
    // Une erreur remonte avec son statut, et n'est pas gardée.
    let e = corps_general_selon(releve, "t", au_travail, fixe, || {
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

/// Le vrai prédicat, bout en bout : « tout suspendre » → le corps est compté
/// une fois puis resservi ; « tout reprendre » → recompté.
#[tokio::test]
async fn tout_suspendu_le_corps_general_est_resservi_puis_recompte_a_la_reprise() {
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
    let releve = state.comptages.releves.completude.clone();
    tune_core::taches_de_fond::tout_suspendre(&state.backend).unwrap();
    // Un scan lancé par un autre témoin, en parallèle, rendrait la
    // bibliothèque « au travail » : on recommence la mesure dans ce cas.
    let mut essais = 0;
    loop {
        essais += 1;
        // La première lecture compte (ou reprend le relevé d'un essai
        // précédent) ; la seconde, sans scan entre les deux, ne compte pas.
        let scan = tune_core::scanner::activite::scan_bibliotheque_en_cours;
        let v1 = corps_general(&releve, "t", &state.backend, lire(1))
            .await
            .ok()
            .unwrap();
        let apres_v1 = appels.load(Ordering::SeqCst);
        let sans_scan = !scan();
        let v2 = corps_general(&releve, "t", &state.backend, lire(2))
            .await
            .ok()
            .unwrap();
        if sans_scan && !scan() {
            assert!(apres_v1 >= 1, "un relevé a été pris après la pause");
            assert_eq!(v1, v2, "tout suspendu, même révision : resservi");
            assert_eq!(appels.load(Ordering::SeqCst), apres_v1);
            break;
        }
        assert!(essais < 50, "un scan tourne sans fin dans le processus");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    tune_core::taches_de_fond::tout_reprendre(&state.backend).unwrap();
    let avant = appels.load(Ordering::SeqCst);
    let v = corps_general(&releve, "t", &state.backend, lire(3))
        .await
        .ok()
        .unwrap();
    assert_eq!(v["n"], 3, "à la reprise : recompté");
    assert_eq!(appels.load(Ordering::SeqCst), avant + 1);
}

/// C2 de la relecture : « tout suspendre » échoue à mi-chemin pendant qu'une
/// pause de la DERNIÈRE tâche réussit. Le retour en arrière ne doit pas
/// effacer de la mémoire une pause que la base porte.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_tout_suspendre_en_echec_n_efface_pas_une_pause_concurrente() {
    let _pauses = crate::pauses_de_fond_tests::isoler().await;
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    let refusee = Tache::TOUTES[1].cle_reglage();
    let x = Tache::TOUTES[Tache::TOUTES.len() - 1];
    state
        .backend
        .execute_batch(&format!(
            "CREATE TRIGGER refus_9116_ins BEFORE INSERT ON settings WHEN NEW.key = '{refusee}' \
             BEGIN SELECT RAISE(ABORT, 'refus'); END; \
             CREATE TRIGGER refus_9116_upd BEFORE UPDATE ON settings WHEN NEW.key = '{refusee}' \
             BEGIN SELECT RAISE(ABORT, 'refus'); END;"
        ))
        .unwrap();
    // L'écrivain tenu : les deux écritures attendent, les bits sont posés.
    let tenue = rusqlite::Connection::open(&chemin).unwrap();
    tenue.execute_batch("BEGIN IMMEDIATE").unwrap();

    let b = state.backend.clone();
    let tout = tokio::task::spawn_blocking(move || tune_core::taches_de_fond::tout_suspendre(&b));
    let debut = std::time::Instant::now();
    while !tune_core::taches_de_fond::tout_est_suspendu() {
        assert!(debut.elapsed() < Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let b = state.backend.clone();
    let une =
        tokio::task::spawn_blocking(move || tune_core::taches_de_fond::mettre_en_pause(&b, x));
    tokio::time::sleep(Duration::from_millis(200)).await;
    tenue.execute_batch("ROLLBACK").unwrap();

    assert!(tout.await.unwrap().is_err(), "le témoin doit mordre");
    une.await.unwrap().unwrap();

    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    for tache in Tache::TOUTES {
        let en_base = reglages.get(tache.cle_reglage()).unwrap().as_deref() == Some("true");
        assert_eq!(
            tune_core::taches_de_fond::est_en_pause(tache),
            en_base,
            "{} : la mémoire doit suivre la base",
            tache.id()
        );
    }
    assert!(tune_core::taches_de_fond::est_en_pause(x));
}
