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

// ── #7619 / #7634 : une écoute = une ligne, jamais multipliée par une jointure ──

/// Le texte de la requête AVANT le correctif (contre-épreuve : il doit gonfler).
const ANCIENNE_REQUETE: &str = "SELECT h.title, h.artist_name, COUNT(*) as plays, \
     COALESCE(MAX(t.id), MAX(t3.id), MAX(h.track_id)) as track_id, \
     COALESCE(MAX(al.cover_path), MAX(al2.cover_path), MAX(h.cover_url)) as cover_path, \
     COALESCE(MAX(al.title), MAX(h.album_title)) as album_title, \
     MAX(h.source) \
     FROM listen_history h \
     LEFT JOIN tracks t ON h.track_id = t.id \
     LEFT JOIN tracks t3 ON t3.title = h.title \
     LEFT JOIN albums al ON t.album_id = al.id \
     LEFT JOIN albums al2 ON al2.title = h.album_title AND al2.cover_path IS NOT NULL \
     WHERE h.source != 'radio' \
     GROUP BY h.title, h.artist_name \
     ORDER BY plays DESC LIMIT 100000";

fn base_vide(epreuve: &str) -> Banc {
    let dossier = crate::test_scratch::scratch_dir(&format!("top-titres-dup-{epreuve}"));
    let chemin = dossier.join("tune-dup.db").to_string_lossy().into_owned();
    let db = SqliteDb::open(&chemin).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Banc {
        _dossier: dossier,
        db: Arc::new(db),
    }
}

fn top(b: &Banc) -> Vec<(String, i64, Option<i64>)> {
    let backend: Arc<dyn DbBackend> = b.db.clone();
    HistoryRepo::with_backend(backend)
        .top_tracks(100)
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["title"].as_str().unwrap().to_string(),
                l["plays"].as_i64().unwrap(),
                l["track_id"].as_i64(),
            )
        })
        .collect()
}

/// « Intro » existe sur 3 albums (3 pistes) dont 2 albums homonymes avec
/// pochette ; 4 écoutes d'« Intro », 2 de « Solo » : le compte est 4 et 2.
#[test]
fn les_plus_ecoutes_comptent_une_ecoute_par_ligne_malgre_les_homonymes() {
    let b = base_vide("homonymes");
    b.db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'A'), (2, 'B');
         INSERT INTO albums (id, title, artist_id, cover_path) VALUES
           (1, 'Disque', 1, 'a.jpg'), (2, 'Disque', 2, 'b.jpg'), (3, 'Autre', 1, 'c.jpg');
         INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format) VALUES
           (10, 'Intro', 1, 1, 1, '/m/1.flac', 'flac'),
           (11, 'Intro', 2, 2, 1, '/m/2.flac', 'flac'),
           (12, 'Intro', 3, 1, 1, '/m/3.flac', 'flac'),
           (13, 'Solo', 3, 1, 2, '/m/4.flac', 'flac');
         INSERT INTO listen_history (title, artist_name, album_title, source) VALUES
           ('Intro', 'A', 'Disque', 'local'), ('Intro', 'A', 'Disque', 'local'),
           ('Intro', 'A', 'Disque', 'local'), ('Intro', 'A', 'Disque', 'local'),
           ('Solo', 'A', 'Autre', 'local'), ('Solo', 'A', 'Autre', 'local'),
           ('Radio', 'R', NULL, 'radio');",
    )
    .unwrap();
    let t = top(&b);
    assert_eq!(t.len(), 2, "la radio est exclue : {t:?}");
    assert_eq!((t[0].0.as_str(), t[0].1), ("Intro", 4), "{t:?}");
    assert_eq!((t[1].0.as_str(), t[1].1), ("Solo", 2), "{t:?}");
    assert_eq!(t[1].2, Some(13), "la piste se résout toujours par le titre");

    // Contre-épreuve : l'ancienne requête gonfle bien ce même jeu (4 écoutes
    // d'« Intro » x 3 pistes homonymes x 2 albums à pochette = 24).
    let rows = b.db.query_many(ANCIENNE_REQUETE, &[]).unwrap();
    let ancien: i64 = rows
        .iter()
        .find(|r| r[0].as_string().as_deref() == Some("Intro"))
        .and_then(|r| r[2].as_i64())
        .unwrap();
    assert!(ancien > 4, "la contre-épreuve ne reproduit plus : {ancien}");
}

/// Sans doublon, la requête rend les mêmes lignes et le même ordre qu'avant.
#[test]
fn les_plus_ecoutes_sans_doublon_rendent_les_memes_lignes_et_le_meme_ordre() {
    let b = base_vide("sans-doublon");
    b.db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'A');
         INSERT INTO albums (id, title, artist_id, cover_path) VALUES
           (1, 'Un', 1, 'u.jpg'), (2, 'Deux', 1, NULL);
         INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, format) VALUES
           (1, 'T1', 1, 1, 1, '/m/1.flac', 'flac'),
           (2, 'T2', 1, 1, 2, '/m/2.flac', 'flac'),
           (3, 'T3', 2, 1, 1, '/m/3.flac', 'flac');
         INSERT INTO listen_history (title, artist_name, album_title, source, cover_url) VALUES
           ('T1', 'A', 'Un', 'local', NULL), ('T1', 'A', 'Un', 'local', NULL),
           ('T1', 'A', 'Un', 'local', NULL),
           ('T2', 'A', 'Un', 'local', NULL), ('T2', 'A', 'Un', 'local', NULL),
           ('T3', 'A', 'Deux', 'local', NULL),
           ('Web', 'W', 'Ailleurs', 'qobuz', 'http://c/x.jpg');",
    )
    .unwrap();
    let nouveau = lignes(&b.db);
    let backend: Arc<dyn DbBackend> = b.db.clone();
    let ordre: Vec<(String, i64)> = HistoryRepo::with_backend(backend)
        .top_tracks(100)
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["title"].as_str().unwrap().into(),
                l["plays"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ordre,
        vec![
            ("T1".to_string(), 3),
            ("T2".to_string(), 2),
            ("T3".to_string(), 1),
            ("Web".to_string(), 1)
        ]
    );
    let mut ancien: Vec<String> =
        b.db.query_many(ANCIENNE_REQUETE, &[])
            .unwrap()
            .iter()
            .map(|c| {
                serde_json::json!({
                    "title": c[0].as_string().unwrap_or_default(),
                    "artist_name": c[1].as_string(),
                    "plays": c[2].as_i64().unwrap_or(0),
                    "track_id": c[3].as_i64(),
                    "cover_path": c[4].as_string(),
                    "album_title": c[5].as_string(),
                    "source": c[6].as_string(),
                })
                .to_string()
            })
            .collect();
    ancien.sort();
    assert_eq!(
        nouveau, ancien,
        "les lignes diffèrent de l'ancienne requête"
    );
}
