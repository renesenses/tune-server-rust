//! La politique de cession à la lecture (#4681, suite de #4699) : chaque frein
//! ralentit la passe pendant une lecture SIMULÉE, la laisse repartir à
//! l'arrêt, et ne fait rien quand la politique est désarmée (contre-épreuve
//! dans chaque témoin).
//!
//! La lecture est simulée par le point de production qui l'alimente
//! (`noter_etat_de_lecture`, appelé par `ZoneRepo::save_play_state`), la
//! politique par le point qu'emprunte `PATCH /system/config` (`regler`).
//!
//! Un binaire à lui seul, et des témoins sérialisés : le témoin de lecture et
//! la politique sont des états de PROCESSUS.
//!
//! Le banc (`banc_lecture_de_premier_plan_sous_charge_de_fond`, ignoré par
//! défaut) mesure l'attente d'une lecture SQLite de premier plan pendant que
//! des passes de fond saturent le pool, freins armés puis désarmés :
//!
//! ```text
//! cargo test -p tune-core --test cession_a_la_lecture_4681 -- --ignored --nocapture
//! ```
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use tune_core::db::sqlite::SqliteDb;
use tune_core::scanner::walker::{EcrituresDuLot, ScannedFile, lire_le_lot, scan_files_batched};
use tune_core::taches_de_fond::priorite::politique::{
    self, DEFAUT, Politique, marquer_le_fil_de_fond, regler, releve_des_freins,
};
use tune_core::taches_de_fond::priorite::{
    ID_SCAN, noter_etat_de_lecture, oublier_la_lecture_pour_les_essais, releve,
};

static VERROU: Mutex<()> = Mutex::new(());

fn a_neuf() {
    oublier_la_lecture_pour_les_essais();
}

fn jouer() {
    noter_etat_de_lecture(7, "playing");
}

fn arreter() {
    noter_etat_de_lecture(7, "stopped");
}

fn desarmee() -> Politique {
    Politique {
        enabled: false,
        ..DEFAUT
    }
}

// ---------------------------------------------------------------------------
// Le scan
// ---------------------------------------------------------------------------

/// Combien de fichiers `lire_le_lot` lit à la fois : chaque lecture simulée
/// dure 10 ms et note le maximum en vol.
fn largeur_observee(n: usize) -> (usize, Duration) {
    let chemins: Vec<std::path::PathBuf> = (0..n)
        .map(|i| format!("/nulle/part/{i}.flac").into())
        .collect();
    let lot: Vec<&Path> = chemins.iter().map(|p| p.as_path()).collect();
    let en_vol = AtomicUsize::new(0);
    let max = AtomicUsize::new(0);
    let lire_un = |p: &&Path| {
        let k = en_vol.fetch_add(1, Ordering::SeqCst) + 1;
        max.fetch_max(k, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(10));
        en_vol.fetch_sub(1, Ordering::SeqCst);
        ScannedFile {
            path: p.display().to_string(),
            metadata: None,
            unsupported: None,
            audio_hash: None,
            file_size: 0,
            mtime: 0.0,
        }
    };
    let debut = Instant::now();
    let lus = lire_le_lot(&lot, &|| false, &lire_un);
    assert_eq!(lus.len(), n, "aucun fichier perdu");
    (max.load(Ordering::SeqCst), debut.elapsed())
}

/// Pendant la lecture, le scan lit au plus `scan_width` fichiers à la fois et
/// marque une pause entre deux paquets ; à l'arrêt, il reprend toute sa
/// largeur. Contre-épreuve : freins désarmés, la lecture ne change rien.
#[test]
fn le_scan_reduit_sa_largeur_pendant_la_lecture_et_la_reprend_ensuite() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    // Le pool rayon global doit pouvoir lire plus de deux fichiers à la fois,
    // sans quoi ce témoin ne distinguerait rien.
    let coeurs = std::thread::available_parallelism().map_or(1, |n| n.get());
    if coeurs < 4 {
        eprintln!("témoin sauté : {coeurs} cœurs, la largeur de repos n'excède pas 2");
        return;
    }
    regler(Politique {
        scan_width: 2,
        pause_between_files_ms: 20,
        ..DEFAUT
    });

    let (repos, duree_repos) = largeur_observee(40);
    assert!(repos > 2, "au repos, le scan lit large ({repos})");

    jouer();
    let (lecture, duree_lecture) = largeur_observee(40);
    assert!(
        lecture <= 2,
        "une zone joue : au plus 2 fichiers à la fois, mesuré {lecture}"
    );
    // 20 paquets de 2 × 10 ms + 19 pauses de 20 ms ≥ 580 ms.
    assert!(
        duree_lecture >= Duration::from_millis(500),
        "une zone joue : la passe doit ralentir ({duree_lecture:?}, repos {duree_repos:?})"
    );
    assert!(duree_lecture > duree_repos * 3);
    assert_eq!(releve_des_freins().scan_files_throttled, 38);
    assert!(releve().throttled.contains(&ID_SCAN));

    // Contre-épreuve : freins désarmés, même lecture en cours.
    regler(desarmee());
    let (desarme, _) = largeur_observee(40);
    assert!(
        desarme > 2,
        "freins désarmés : la largeur revient ({desarme})"
    );
    assert_eq!(
        releve_des_freins().scan_files_throttled,
        38,
        "freins désarmés : aucun fichier cédé de plus"
    );

    // La lecture s'arrête : le scan reprend sa largeur.
    regler(DEFAUT);
    arreter();
    let (apres, duree_apres) = largeur_observee(40);
    assert!(apres > 2, "après la lecture, le scan relit large ({apres})");
    assert!(duree_apres < duree_lecture);
    a_neuf();
}

/// Le même frein, par la fonction de production `scan_files_batched` : UN lot
/// de 12 fichiers (un seul dossier), lu en 6 paquets de 2 pendant la lecture,
/// donc 5 pauses — et aucune au repos.
#[test]
fn le_scan_de_production_cede_entre_deux_fichiers_pendant_la_lecture() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    let dossier = tempfile::tempdir().expect("dossier");
    let fichiers: Vec<std::path::PathBuf> = (0..12)
        .map(|i| {
            let f = dossier.path().join(format!("{i}.flac"));
            std::fs::write(&f, b"pas du flac").expect("fichier");
            f
        })
        .collect();
    regler(Politique {
        scan_width: 2,
        pause_between_files_ms: 100,
        ..DEFAUT
    });
    let scanner = || {
        let debut = Instant::now();
        let mut lots = 0usize;
        scan_files_batched(&fichiers, false, 500, |_, _, _| {
            lots += 1;
            EcrituresDuLot::SANS_PERTE
        });
        assert_eq!(lots, 1, "un seul dossier, un seul lot");
        debut.elapsed()
    };
    let repos = scanner();
    assert!(repos < Duration::from_millis(500), "au repos : {repos:?}");
    assert_eq!(releve_des_freins().scan_files_throttled, 0);

    jouer();
    let lecture = scanner();
    assert!(
        lecture >= Duration::from_millis(500),
        "une zone joue : 5 pauses de 100 ms attendues, mesuré {lecture:?}"
    );
    assert_eq!(releve_des_freins().scan_files_throttled, 10);

    arreter();
    let apres = scanner();
    assert!(
        apres < Duration::from_millis(500),
        "après la lecture : {apres:?}"
    );
    a_neuf();
}

// ---------------------------------------------------------------------------
// SQLite : la réserve de lecture
// ---------------------------------------------------------------------------

fn base_sur_disque() -> (tempfile::TempDir, Arc<SqliteDb>) {
    let dossier = tempfile::tempdir().expect("dossier");
    let chemin = dossier.path().join("tune.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).expect("base");
    db.execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER); INSERT INTO t VALUES (1);")
        .expect("schéma");
    (dossier, Arc::new(db))
}

/// Trois fils de fond tiennent chacun une lecture 300 ms ; une lecture de
/// premier plan arrive ensuite. Rend son attente, et combien de fils de fond
/// ont dû attendre leur connexion.
fn attente_du_premier_plan(db: &Arc<SqliteDb>) -> (Duration, usize) {
    let depart = Arc::new(Barrier::new(4));
    let differes = Arc::new(AtomicUsize::new(0));
    let fils: Vec<_> = (0..3)
        .map(|_| {
            let db = db.clone();
            let depart = depart.clone();
            let differes = differes.clone();
            std::thread::spawn(move || {
                let _fond = marquer_le_fil_de_fond();
                depart.wait();
                // L'attente de la RÉSERVE précède celle d'une connexion libre
                // (`attente()`) : on mesure l'emprunt entier.
                let debut = Instant::now();
                let c = db.read_connection();
                if debut.elapsed() >= Duration::from_millis(20) {
                    differes.fetch_add(1, Ordering::SeqCst);
                }
                let _: i64 = c.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
                std::thread::sleep(Duration::from_millis(300));
            })
        })
        .collect();
    depart.wait();
    std::thread::sleep(Duration::from_millis(50));
    let debut = Instant::now();
    {
        let c = db.read_connection();
        let _: i64 = c.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
    }
    let attente = debut.elapsed();
    for f in fils {
        f.join().unwrap();
    }
    (attente, differes.load(Ordering::SeqCst))
}

/// Pendant la lecture, une connexion de lecture reste libre pour le premier
/// plan : il n'attend pas les passes de fond. Au repos, ou freins désarmés,
/// il attend qu'une d'elles rende la sienne (la contre-épreuve).
#[test]
fn une_connexion_de_lecture_reste_libre_pour_le_premier_plan_pendant_la_lecture() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    let (_d, db) = base_sur_disque();

    // Au repos : les trois fils de fond prennent tout, le premier plan attend.
    let (repos, differes_repos) = attente_du_premier_plan(&db);
    assert!(
        repos >= Duration::from_millis(150),
        "au repos, rien n'est réservé : le premier plan attend ({repos:?})"
    );
    assert_eq!(differes_repos, 0);

    jouer();
    let (lecture, differes_lecture) = attente_du_premier_plan(&db);
    assert!(
        lecture < Duration::from_millis(100),
        "une zone joue : la connexion réservée sert le premier plan tout de suite ({lecture:?})"
    );
    assert_eq!(
        differes_lecture, 1,
        "une zone joue : le troisième fil de fond attend la réserve"
    );
    assert!(releve_des_freins().sqlite_reads_deferred >= 1);

    // Contre-épreuve : freins désarmés pendant la même lecture.
    regler(desarmee());
    let (desarme, _) = attente_du_premier_plan(&db);
    assert!(
        desarme >= Duration::from_millis(150),
        "freins désarmés : plus de réserve ({desarme:?})"
    );

    // La lecture s'arrête : plus de réserve non plus.
    regler(DEFAUT);
    arreter();
    let (apres, differes_apres) = attente_du_premier_plan(&db);
    assert!(
        apres >= Duration::from_millis(150),
        "après la lecture : {apres:?}"
    );
    assert_eq!(differes_apres, 0);
    a_neuf();
}

/// Un fil de fond qui tient déjà une lecture n'est pas plafonné pour la
/// suivante (emprunts imbriqués) : pas d'interblocage entre fils de fond.
#[test]
fn un_emprunt_imbrique_de_fond_ne_s_interbloque_pas() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    let (_d, db) = base_sur_disque();
    jouer();
    let depart = Arc::new(Barrier::new(2));
    let fils: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            let depart = depart.clone();
            std::thread::spawn(move || {
                let _fond = marquer_le_fil_de_fond();
                let a = db.read_connection();
                depart.wait();
                let debut = Instant::now();
                let b = db.read_connection();
                let _: i64 = b.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
                drop((a, b));
                debut.elapsed()
            })
        })
        .collect();
    for f in fils {
        let d = f.join().unwrap();
        assert!(d < Duration::from_millis(1000), "imbriqué : {d:?}");
    }
    a_neuf();
}

// ---------------------------------------------------------------------------
// SQLite : l'écrivain de fond laisse passer
// ---------------------------------------------------------------------------

/// Le verrou d'écriture est tenu ; un écrivain de premier plan attend ; un
/// écrivain de fond arrive. Pendant la lecture, le fond laisse passer le
/// premier plan, et le relevé le compte. Contre-épreuve : freins désarmés,
/// aucune cession comptée.
#[test]
fn l_ecrivain_de_fond_laisse_passer_le_premier_plan_pendant_la_lecture() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    let (_d, db) = base_sur_disque();

    let une_course = |db: &Arc<SqliteDb>| -> Vec<&'static str> {
        let ordre = Arc::new(Mutex::new(Vec::new()));
        let tenue = db.connection().lock().unwrap();
        let premier_plan = {
            let (db, ordre) = (db.clone(), ordre.clone());
            std::thread::spawn(move || {
                let c = db.connection().lock().unwrap();
                ordre.lock().unwrap().push("premier_plan");
                std::thread::sleep(Duration::from_millis(20));
                drop(c);
            })
        };
        // Le premier plan est inscrit dans l'attente avant que le fond arrive.
        std::thread::sleep(Duration::from_millis(50));
        let fond = {
            let (db, ordre) = (db.clone(), ordre.clone());
            std::thread::spawn(move || {
                let _f = marquer_le_fil_de_fond();
                let c = db.connection().lock().unwrap();
                ordre.lock().unwrap().push("fond");
                drop(c);
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(tenue);
        premier_plan.join().unwrap();
        fond.join().unwrap();
        Arc::try_unwrap(ordre).unwrap().into_inner().unwrap()
    };

    jouer();
    let ordre = une_course(&db);
    assert_eq!(ordre, vec!["premier_plan", "fond"]);
    let freins = releve_des_freins();
    assert_eq!(freins.sqlite_writes_deferred, 1, "{freins:?}");

    regler(desarmee());
    let _ = une_course(&db);
    assert_eq!(
        releve_des_freins().sqlite_writes_deferred,
        1,
        "freins désarmés : aucune cession de plus"
    );

    regler(DEFAUT);
    arreter();
    let _ = une_course(&db);
    assert_eq!(
        releve_des_freins().sqlite_writes_deferred,
        1,
        "après la lecture : aucune cession de plus"
    );
    a_neuf();
}

// ---------------------------------------------------------------------------
// Priorité d'E/S
// ---------------------------------------------------------------------------

/// Linux : pendant la lecture, la garde place le fil en best-effort 7, puis
/// lui rend sa priorité d'avant ; au repos, ou freins d'E/S désarmés, elle ne
/// touche à rien.
#[cfg(target_os = "linux")]
#[test]
fn la_priorite_d_e_s_baisse_pendant_la_lecture_et_revient_ensuite() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    std::thread::spawn(|| {
        let avant = politique::priorite_d_e_s_du_fil().expect("ioprio_get");
        {
            let g = politique::baisser_pendant_la_lecture();
            assert!(!g.appliquee(), "au repos : rien");
            assert_eq!(politique::priorite_d_e_s_du_fil(), Some(avant));
        }
        jouer();
        {
            let g = politique::baisser_pendant_la_lecture();
            assert!(g.appliquee(), "une zone joue : la priorité baisse");
            assert_eq!(politique::priorite_d_e_s_du_fil(), Some((2, 7)));
        }
        assert_eq!(
            politique::priorite_d_e_s_du_fil(),
            Some(avant),
            "la garde rend la priorité d'avant"
        );
        regler(Politique {
            low_io_priority: false,
            ..DEFAUT
        });
        {
            let g = politique::baisser_pendant_la_lecture();
            assert!(!g.appliquee(), "désarmée : rien");
        }
        assert_eq!(releve_des_freins().low_io_priority_applied, 1);
    })
    .join()
    .unwrap();
    a_neuf();
}

/// Le relevé publie la politique en vigueur et les compteurs des freins.
#[test]
fn le_releve_publie_la_politique_et_les_freins() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    regler(Politique {
        pause_between_items_ms: 1_234,
        ..DEFAUT
    });
    let json = serde_json::to_value(releve()).unwrap();
    assert_eq!(json["pause_between_items_ms"], 1_234);
    assert_eq!(json["policy"]["pause_between_items_ms"], 1_234);
    assert_eq!(json["policy"]["enabled"], true);
    assert_eq!(json["policy"]["scan_width"], DEFAUT.scan_width);
    for cle in [
        "scan_files_throttled",
        "sqlite_reads_deferred",
        "sqlite_reads_deferred_ms",
        "sqlite_writes_deferred",
        "sqlite_writes_deferred_ms",
        "low_io_priority_applied",
    ] {
        assert!(json["brakes"][cle].is_u64(), "{cle} : {json}");
    }
    a_neuf();
}

// ---------------------------------------------------------------------------
// Le banc
// ---------------------------------------------------------------------------

/// Six fils de fond enchaînent des lectures lourdes pendant 3 s ; un fil de
/// premier plan fait une lecture triviale toutes les 20 ms et note son temps
/// total. Freins armés, puis désarmés, une zone jouant dans les deux cas.
#[test]
#[ignore = "banc de mesure : --ignored --nocapture"]
fn banc_lecture_de_premier_plan_sous_charge_de_fond() {
    let _g = VERROU.lock().unwrap_or_else(|e| e.into_inner());
    a_neuf();
    let (_d, db) = base_sur_disque();
    db.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200000)
         INSERT INTO t SELECT i FROM n;",
    )
    .unwrap();
    jouer();
    let mesurer = |politique: Politique| -> Vec<Duration> {
        regler(politique);
        let fin = Instant::now() + Duration::from_secs(3);
        let fond: Vec<_> = (0..6)
            .map(|_| {
                let db = db.clone();
                std::thread::spawn(move || {
                    let _f = marquer_le_fil_de_fond();
                    while Instant::now() < fin {
                        let c = db.read_connection();
                        let _: i64 = c
                            .query_row("SELECT sum(x * x % 7) FROM t", [], |r| r.get(0))
                            .unwrap();
                    }
                })
            })
            .collect();
        let mut mesures = Vec::new();
        while Instant::now() < fin {
            let debut = Instant::now();
            {
                let c = db.read_connection();
                let _: i64 = c.query_row("SELECT 1", [], |r| r.get(0)).unwrap();
            }
            mesures.push(debut.elapsed());
            std::thread::sleep(Duration::from_millis(20));
        }
        for f in fond {
            f.join().unwrap();
        }
        mesures.sort();
        mesures
    };
    let resume = |nom: &str, m: &[Duration]| {
        let q = |p: f64| m[((m.len() - 1) as f64 * p) as usize];
        println!(
            "{nom:>16} : n={} p50={:?} p95={:?} p99={:?} max={:?}",
            m.len(),
            q(0.50),
            q(0.95),
            q(0.99),
            m[m.len() - 1]
        );
    };
    let armes = mesurer(DEFAUT);
    let desarmes = mesurer(desarmee());
    resume("freins armés", &armes);
    resume("freins désarmés", &desarmes);
    println!("{:?}", releve_des_freins());
    a_neuf();
}
