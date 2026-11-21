//! Banc #9116 (fil 2935) — la vue Répertoires et les jauges d'avancement sur
//! une base SYNTHÉTIQUE d'environ 500 000 pistes.
//!
//! ```text
//! cargo run --release -p tune-core --example banc_repertoires_9116 -- /tmp/banc.db [pistes]
//! ```
//!
//! Crée la base si elle n'existe pas (ÉCRIT dans le fichier donné). Imprime :
//! le compte d'une racine AVANT (`file_path LIKE`, toute la table) et APRÈS
//! (plage sur `idx_tracks_chemin_dossier`), leurs plans, les comptes par
//! sous-dossier à la racine et à un artiste, et ce que coûtent les jauges
//! d'avancement que l'écran État du serveur sondait toutes les 5 s.

use std::sync::Arc;
use std::time::Instant;

use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::engine::Engine;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo as tr;

fn ms<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed().as_secs_f64() * 1000.0)
}

/// Médiane de `n` mesures (la première, à froid, est imprimée à part).
fn mediane(n: usize, mut f: impl FnMut() -> f64) -> f64 {
    let mut v: Vec<f64> = (0..n).map(|_| f()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[n / 2]
}

fn plan(db: &SqliteDb, sql: &str, p: &[&dyn ToSqlValue]) -> Vec<String> {
    db.query_many(&format!("EXPLAIN QUERY PLAN {sql}"), p)
        .unwrap()
        .iter()
        .filter_map(|r| r.get(3).and_then(|v| v.as_string()))
        .collect()
}

fn main() {
    let chemin = std::env::args().nth(1).expect("chemin de la base de banc");
    let n: i64 = std::env::args()
        .nth(2)
        .map(|s| s.parse().unwrap())
        .unwrap_or(500_000);
    let neuve = !std::path::Path::new(&chemin).exists();
    let db = SqliteDb::open(&chemin).expect("ouverture");
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).expect("migrations");
    if neuve {
        // 5 000 artistes × 10 albums × 10 pistes, sous /srv/musique ; la
        // moitié porte `rg_analyzed` (candidates DR), un quart `dr_track`.
        let (_, t) = ms(|| {
            db.execute_batch(&format!(
                "BEGIN; \
                 WITH RECURSIVE g(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM g WHERE i < {n} - 1) \
                 INSERT INTO tracks (title, file_path, format, source, track_number, duration_ms, sample_rate, channels) \
                 SELECT 'Piste ' || i, '/srv/musique/Artiste ' || (i / 100) || '/Album ' || ((i / 10) % 10) \
                 || '/' || (i % 10) || ' - piste.flac', 'flac', 'local', i % 10, 240000, 44100, 2 FROM g; \
                 INSERT INTO track_metadata (track_id, key, value) \
                 SELECT id, 'rg_analyzed', '1700000000' FROM tracks WHERE id % 2 = 0; \
                 INSERT INTO track_metadata (track_id, key, value) \
                 SELECT id, 'dr_track', '12' FROM tracks WHERE id % 4 = 0; \
                 COMMIT; ANALYZE;"
            ))
            .unwrap()
        });
        println!("base créée : {n} pistes en {t:.0} ms");
    }
    let db = Arc::new(db);
    let b: Arc<dyn DbBackend> = db.clone();
    let total = b
        .query_one("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    println!("pistes en base : {total}");

    let racine = "/srv/musique";
    let motif = tr::folder_like_pattern(racine);
    let ancienne = format!(
        "SELECT COUNT(*) FROM tracks WHERE file_path LIKE ?1{}",
        tr::like_escape_clause()
    );
    // La variante bornée sur l'index du chemin (retenue pour PostgreSQL
    // seulement) : mesurée ici pour justifier le choix SQLite.
    let nouvelle = format!(
        "SELECT COUNT(*) FROM tracks WHERE {} AND file_path LIKE ?1{}",
        tr::plage_du_dossier(Engine::Sqlite, tr::CHEMIN_DE_LA_PISTE, "?2", "?3"),
        tr::like_escape_clause()
    );
    let (bas, haut) = tr::bornes_du_dossier(racine);
    println!(
        "plan AVANT : {:?}",
        plan(&db, &ancienne, &[&motif as &dyn ToSqlValue])
    );
    println!(
        "plan PLAGE : {:?}",
        plan(&db, &nouvelle, &[&motif as &dyn ToSqlValue, &bas, &haut])
    );
    let avant = || {
        b.query_one(&ancienne, &[&motif as &dyn ToSqlValue])
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap()
    };
    let (n_avant, t_froid) = ms(avant);
    let t_avant = mediane(7, || ms(avant).1);
    println!("RACINE AVANT : {n_avant} pistes, froid {t_froid:.0} ms, médiane {t_avant:.0} ms");
    let plage = || {
        b.query_one(&nouvelle, &[&motif as &dyn ToSqlValue, &bas, &haut])
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap()
    };
    let (n_plage, t_froid) = ms(plage);
    let t_plage = mediane(7, || ms(plage).1);
    println!("RACINE PLAGE : {n_plage} pistes, froid {t_froid:.0} ms, médiane {t_plage:.0} ms");
    let apres = || tr::compter_pistes_sous_dossier(b.as_ref(), racine).unwrap();
    let (n_apres, t_froid) = ms(apres);
    let t_apres = mediane(7, || ms(apres).1);
    println!(
        "RACINE APRÈS (1er appel) : {n_apres} pistes, froid {t_froid:.0} ms, médiane {t_apres:.0} ms"
    );
    let rev = || tune_core::db::library_revision::lire(b.as_ref());
    let t_rev = mediane(7, || ms(rev).1);
    println!(
        "RACINE APRÈS, servie par révision ({:?}) : {t_rev:.2} ms (lecture de la révision seule)",
        rev()
    );

    // Ouvrir la racine (5 000 sous-dossiers) puis un artiste (10 albums).
    for dossier in [racine.to_string(), format!("{racine}/Artiste 2500")] {
        let f = || tr::compter_pistes_par_sous_dossier(b.as_ref(), &dossier).unwrap();
        let (c, t_froid) = ms(f);
        let t = mediane(5, || ms(f).1);
        println!(
            "SOUS-DOSSIERS de {dossier} : {} peuplés, froid {t_froid:.0} ms, médiane {t:.0} ms \
             (servi par révision ensuite : {t_rev:.2} ms)",
            c.nb_dossiers_peuples()
        );
    }

    // #9116, suite — l'arbre des dossiers : construit une fois, puis chaque
    // niveau de la descente lu en mémoire. En regard, ce que la route d'avant
    // payait par niveau : comptes des sous-dossiers + pistes directes (plage
    // récursive), sans cache.
    use tune_core::db::arbre_des_dossiers as ad;
    let (arbre, t_froid) = ms(|| ad::construire(b.as_ref()).unwrap());
    let t_constr = mediane(3, || ms(|| ad::construire(b.as_ref()).unwrap()).1);
    println!(
        "ARBRE : {} chemins, {} dossiers, {:.1} Mio ({:.1} octets/chemin), construit froid {t_froid:.0} ms, médiane {t_constr:.0} ms",
        arbre.nb_chemins(),
        arbre.nb_dossiers(),
        arbre.octets() as f64 / 1_048_576.0,
        arbre.octets() as f64 / arbre.nb_chemins() as f64
    );
    for dossier in [
        racine.to_string(),
        format!("{racine}/Artiste 2500"),
        format!("{racine}/Artiste 2500/Album 3"),
    ] {
        let sep = std::path::MAIN_SEPARATOR.to_string();
        let motif = tr::folder_like_pattern(&dossier);
        let (bas, haut) = tr::bornes_du_dossier(&dossier);
        let sql = tr::sql_pistes_du_dossier(Engine::Sqlite, dossier.chars().count() + 2);
        let avant = || {
            let c = tr::compter_pistes_par_sous_dossier(b.as_ref(), &dossier).unwrap();
            let p = b
                .query_many(&sql, &[&motif as &dyn ToSqlValue, &sep, &bas, &haut])
                .unwrap();
            (c.nb_dossiers_peuples(), p.len())
        };
        let t_avant = mediane(3, || ms(avant).1);
        let apres = || {
            let v = arbre.dossier(&dossier);
            let ids = v.pistes_directes().to_vec();
            let p = if ids.is_empty() {
                0
            } else {
                let q = tr::sql_pistes_par_ids(Engine::Sqlite, ids.len());
                let params: Vec<&dyn ToSqlValue> =
                    ids.iter().map(|i| i as &dyn ToSqlValue).collect();
                b.query_many(&q, &params).unwrap().len()
            };
            (v.nb_sous_dossiers(), p)
        };
        let t_apres = mediane(5, || ms(apres).1);
        println!(
            "NIVEAU {dossier} : avant {t_avant:.1} ms {:?}, arbre {t_apres:.3} ms {:?}",
            avant(),
            apres()
        );
    }

    // Les jauges d'avancement : ce que coûtait CHAQUE sondage (5 s) avec
    // toutes les passes en pause, et que la pause fige désormais.
    use tune_core::audio::replaygain as rg;
    let jauges: Vec<(&str, Box<dyn Fn() -> String>)> = vec![
        (
            "replaygain/progress candidats",
            Box::new(|| rg::compter_les_candidats_replaygain(&b).to_string()),
        ),
        (
            "replaygain/progress reportées",
            Box::new(|| rg::compter_les_reportees_par_chemin(&b).to_string()),
        ),
        (
            "replaygain/progress bibliothèque",
            Box::new(|| {
                format!(
                    "{:?}",
                    rg::bibliotheque::compter_la_bibliotheque_replaygain(&b).map(|x| x.total)
                )
            }),
        ),
        (
            "dynamic-range/progress candidats",
            Box::new(|| rg::compter_les_candidats_dr(&b).to_string()),
        ),
        (
            "completeness : DR traitées",
            Box::new(|| format!("{:?}", rg::compter_les_pistes_traitees_dr(&b).is_some())),
        ),
    ];
    let mut somme = 0.0;
    for (nom, f) in &jauges {
        let (r, t) = ms(f);
        somme += t;
        println!("JAUGE {nom} : {r} en {t:.0} ms (en pause désormais : 0 requête)");
    }
    println!("JAUGES : {somme:.0} ms de lecture par sondage avant, 0 en pause après");
}
