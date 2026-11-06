//! Banc #7619 (L1) — la liste des pistes AVANT et APRÈS la clé de tri
//! indexée, sur une COPIE de base réelle.
//!
//! ```text
//! sqlite3 /chemin/tune_v2.db ".backup /tmp/copie.db"     # jamais la base en service
//! cargo run --release -p tune-core --example banc_liste_pistes_7619 -- /tmp/copie.db
//! ```
//!
//! ⚠️ ÉCRIT dans la copie (passe de démarrage : colonne `cle_tri` et son
//! index, puis les clés). N'imprime que des durées et des effectifs : aucun
//! titre, aucun nom.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::cle_de_tri::Curseur;
use tune_core::db::track_repo::{ColonneDeTri, DemandeDePistes, TrackRepo};

fn ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

fn ids(p: &[tune_core::db::models::Track]) -> Vec<i64> {
    p.iter().filter_map(|t| t.id).collect()
}

fn main() {
    let chemin = std::env::args()
        .nth(1)
        .expect("chemin de la COPIE de la base");
    let db = SqliteDb::open(&chemin).expect("ouverture");
    let (_, t_mig) = ms(|| tune_core::db::migrations::run_migrations(&db).expect("migrations"));
    let db = Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    println!("passe de démarrage (colonne + index) : {t_mig:.0} ms");

    if std::env::var("BANC_PLAN").is_ok() {
        use tune_core::db::backend::DbBackend;
        let e = db.engine();
        let socle = tune_core::db::facet_filter::socle_ensembliste_de_la_vue_des_pistes(e);
        let ecartees = tune_core::db::facet_filter::sql_pistes_ecartees_par_le_socle(e);
        let (r, t) = ms(|| db.query_many(&ecartees, &[]).unwrap().len());
        println!("socle : {r} pistes écartées résolues en {t:.0} ms");
        let page = format!(
            "SELECT t.id, t.cle_tri{} WHERE {} AND (t.cle_tri, t.id) > ('m', 0) \
             ORDER BY t.cle_tri, t.id LIMIT 200 OFFSET 0",
            tune_core::db::track_repo::sql::track_from(),
            socle.join(" AND ")
        );
        for l in db
            .query_many(&format!("EXPLAIN QUERY PLAN {page}"), &[])
            .unwrap()
        {
            println!("  plan: {:?}", l.get(3).and_then(|v| v.as_string()));
        }
        let (_, t) = ms(|| db.query_many(&page, &[]).unwrap().len());
        println!("page de 200 après 'm' : {t:.0} ms");
        return;
    }
    // ── AVANT ────────────────────────────────────────────────────────────
    // Clients en service : `getAllTracks`, pages de 2 000, ancien chemin.
    let (premiere, t1) = ms(|| repo.list_visible_avec_total(2000, 0).unwrap());
    let total_avant = premiere.1.unwrap_or(0);
    println!("AVANT  total visible = {total_avant} ; 1re page de 2 000 (froid) : {t1:.0} ms");
    let mut avant: Vec<i64> = ids(&premiere.0);
    let (_, t_tout) = ms(|| {
        let mut off = 2000;
        while off < total_avant {
            avant.extend(ids(&repo.list_visible_avec_total(2000, off).unwrap().0));
            off += 2000;
        }
    });
    println!(
        "AVANT  onglet entier par getAllTracks ({} pages) : {:.0} ms",
        (total_avant + 1999) / 2000,
        t1 + t_tout
    );
    let page200 = DemandeDePistes {
        limit: 200,
        ..Default::default()
    };
    for (nom, d) in [
        ("1re page triée de 200 (onglet Titres)", page200.clone()),
        (
            "page triée de 200 à l'offset 40 000",
            DemandeDePistes {
                offset: 40_000,
                ..page200.clone()
            },
        ),
        (
            "1re page de 200 triée par titre",
            DemandeDePistes {
                tri: Some(ColonneDeTri::Titre),
                ..page200.clone()
            },
        ),
    ] {
        let (_, a) = ms(|| repo.page_de_pistes_par_decalage(&d).unwrap());
        let (_, b) = ms(|| repo.page_de_pistes_par_decalage(&d).unwrap());
        println!("AVANT  {nom} : {a:.0} ms puis {b:.0} ms");
    }

    // ── Les clés ─────────────────────────────────────────────────────────
    let (n, t_cles) = ms(|| repo.rafraichir_cles_de_tri(false).unwrap());
    println!("clés : {n} calculées et écrites en {t_cles:.0} ms (premier démarrage)");
    let (n, t_cles2) = ms(|| repo.rafraichir_cles_de_tri(false).unwrap());
    println!("clés : rafraîchissement complet sans changement : {n} écrites, {t_cles2:.0} ms");
    let (_, t_assurer) = ms(|| repo.assurer_les_cles_de_tri());
    println!("clés : contrôle avant lecture : {t_assurer:.1} ms");
    // Laisser passer le rafraîchissement de fond qu'a pu lancer le contrôle.
    std::thread::sleep(std::time::Duration::from_secs(3));

    // ── APRÈS ────────────────────────────────────────────────────────────
    for (nom, d) in [
        ("1re page de 200 (onglet Titres)", page200.clone()),
        (
            "page de 200 à l'offset 40 000",
            DemandeDePistes {
                offset: 40_000,
                ..page200.clone()
            },
        ),
        (
            "1re page de 200 triée par titre",
            DemandeDePistes {
                tri: Some(ColonneDeTri::Titre),
                ..page200.clone()
            },
        ),
    ] {
        let (_, a) = ms(|| repo.page_de_pistes(&d).unwrap());
        let (_, b) = ms(|| repo.page_de_pistes(&d).unwrap());
        println!("APRÈS  {nom} : {a:.0} ms puis {b:.0} ms");
    }

    // Parcours complet par curseur, pages de 200 puis de 2 000.
    for limite in [200_i64, 2000] {
        let mut tous = Vec::new();
        let mut apres: Option<Curseur> = None;
        let mut pages = 0;
        let mut pire: f64 = 0.0;
        let mut total = 0;
        let (_, t) = ms(|| {
            loop {
                let (p, tp) = ms(|| {
                    repo.page_de_pistes(&DemandeDePistes {
                        limit: limite,
                        apres: apres.clone(),
                        ..Default::default()
                    })
                    .unwrap()
                });
                pire = pire.max(tp);
                pages += 1;
                total = p.total;
                tous.extend(ids(&p.pistes));
                match p.curseur_suivant {
                    Some(c) => apres = Some(Curseur::decoder(&c).unwrap()),
                    None => break,
                }
            }
        });
        let uniques: HashSet<i64> = tous.iter().copied().collect();
        let avant_set: HashSet<i64> = avant.iter().copied().collect();
        println!(
            "APRÈS  parcours par curseur, pages de {limite} : {pages} pages, {t:.0} ms en tout, \
             pire page {pire:.0} ms ; rendues {} (uniques {}), total annoncé {total}, \
             identiques à l'ancien chemin : {}",
            tous.len(),
            uniques.len(),
            uniques == avant_set && tous.len() == avant.len()
        );
        if limite == 2000 {
            let rangs_differents = tous
                .iter()
                .zip(avant.iter())
                .filter(|(a, b)| a != b)
                .count();
            println!(
                "ordre : {rangs_differents} rangs sur {} diffèrent de l'ancien ordre LOWER() \
                 (accents pliés, départage par id)",
                tous.len()
            );
        }
    }
}
