//! Audit des performances de la base (31/10/2026) — « Les plus écoutés ».
//!
//! [`super::history_repo::sql::top_tracks`] résout l'identifiant d'une écoute
//! par son titre (`LEFT JOIN tracks t3 ON t3.title = h.title`). Sans index sur
//! `tracks.title`, SQLite bâtissait à chaque requête un index AUTOMATIQUE sur
//! toute la table des pistes. Sur la copie de la base du .18 (96 496 pistes,
//! 3 484 écoutes) : 226 ms par requête, 30 ms avec `idx_tracks_title`, pour
//! les mêmes lignes.
//!
//! Témoins : le plan lit `idx_tracks_title` (et plus d'index automatique), et
//! les lignes rendues sont les mêmes avec et sans l'index.

use std::sync::Arc;

use super::backend::DbBackend;
use super::engine::SqliteDialect;
use super::history_repo::HistoryRepo;
use super::sqlite::SqliteDb;

struct Banc {
    _dossier: crate::test_scratch::ScratchDir,
    db: Arc<SqliteDb>,
}

/// Une base de FICHIER migrée : `albums` albums de 12 pistes, des titres qui
/// se répètent d'un album à l'autre (« Intro », « Untitled »…), et des écoutes
/// sans `track_id` — c'est ce qu'écrit l'orchestrateur —, dont une part ne
/// correspond à aucune piste locale.
fn remplir(epreuve: &str, albums: i64) -> Banc {
    let dossier = crate::test_scratch::scratch_dir(&format!("top-titres-index-{epreuve}"));
    let chemin = dossier.join("tune-banc.db").to_string_lossy().into_owned();
    let db = SqliteDb::open(&chemin).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let mut sql = String::from("BEGIN;\n");
    for a in 1..=20 {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    let mut tid = 0_i64;
    for al in 1..=albums {
        let artiste = al % 20 + 1;
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, cover_path) VALUES ({al}, 'Album {al}', {artiste}, 'c{al}.jpg');\n"
        ));
        for n in 1..=12 {
            tid += 1;
            // Un titre sur quatre est partagé par plusieurs albums.
            let titre = if n % 4 == 0 {
                format!("Commun {}", n + (al % 3))
            } else {
                format!("Titre {al}-{n}")
            };
            sql.push_str(&format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format) \
                 VALUES ({tid}, '{titre}', {al}, {artiste}, {n}, '/m/{al}/{n}.flac', 'flac');\n"
            ));
        }
    }
    for e in 0..(albums * 3) {
        let al = e % albums + 1;
        let n = e % 12 + 1;
        let (titre, artiste) = match e % 5 {
            // Une écoute de service : aucun titre local ne lui répond.
            0 => (format!("Ailleurs {}", e % 40), "Service".to_string()),
            1 => (
                format!("Commun {}", 4 + (e % 3)),
                format!("Artiste {}", e % 20 + 1),
            ),
            _ => (
                format!("Titre {al}-{n}"),
                format!("Artiste {}", al % 20 + 1),
            ),
        };
        sql.push_str(&format!(
            "INSERT INTO listen_history (title, artist_name, album_title, source) \
             VALUES ('{titre}', '{artiste}', 'Album {al}', 'local');\n"
        ));
    }
    sql.push_str("COMMIT; ANALYZE;");
    db.execute_batch(&sql).unwrap();
    Banc {
        _dossier: dossier,
        db: Arc::new(db),
    }
}

fn sql_top_titres() -> String {
    // Le texte que le dépôt joue sur SQLite, la limite inscrite en clair.
    super::history_repo::sql::top_tracks(&SqliteDialect).replace('?', "100000")
}

fn plan(db: &SqliteDb, sql: &str) -> String {
    db.query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
        .unwrap()
        .iter()
        .map(|r| r.last().and_then(|v| v.as_string()).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Les lignes de `top_tracks`, triées : l'ordre des ex aequo de `plays` n'est
/// pas garanti par la requête, ni avant ni après.
fn lignes(db: &Arc<SqliteDb>) -> Vec<String> {
    let backend: Arc<dyn DbBackend> = db.clone();
    let mut v: Vec<String> = HistoryRepo::with_backend(backend)
        .top_tracks(100_000)
        .unwrap()
        .iter()
        .map(|l| l.to_string())
        .collect();
    v.sort();
    v
}

/// La passe de démarrage pose `idx_tracks_title`, le plan de « Les plus
/// écoutés » le lit au lieu de bâtir un index automatique, et les lignes
/// rendues sont celles d'avant l'index.
#[test]
fn les_plus_ecoutes_lisent_l_index_des_titres() {
    let b = remplir("epreuve", 300);
    let sql = sql_top_titres();
    let plan_apres = plan(&b.db, &sql);
    let lignes_apres = lignes(&b.db);
    assert!(
        plan_apres.contains("INDEX idx_tracks_title (title=?)"),
        "« Les plus écoutés » ne lit pas idx_tracks_title : {plan_apres}"
    );
    assert!(
        !plan_apres.contains("AUTOMATIC"),
        "« Les plus écoutés » bâtit encore un index automatique : {plan_apres}"
    );

    b.db.execute_batch("DROP INDEX IF EXISTS idx_tracks_title; ANALYZE;")
        .unwrap();
    let plan_avant = plan(&b.db, &sql);
    let lignes_avant = lignes(&b.db);
    eprintln!("avant : {plan_avant}\naprès : {plan_apres}");
    assert!(
        lignes_apres.len() > 40,
        "banc trop maigre : {}",
        lignes_apres.len()
    );
    assert_eq!(
        lignes_apres, lignes_avant,
        "l'index des titres change les lignes de « Les plus écoutés »"
    );
}

/// La passe est idempotente : rejouée sur une base qui porte déjà l'index,
/// elle ne change rien et ne le double pas.
#[test]
fn l_index_des_titres_se_rejoue_sans_effet() {
    let b = remplir("rejeu", 100);
    crate::db::migrations::run_migrations(&b.db).unwrap();
    let n =
        b.db.query_one(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_tracks_title'",
            &[],
        )
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0);
    assert_eq!(n, 1, "idx_tracks_title absent ou doublé après rejeu");
}
