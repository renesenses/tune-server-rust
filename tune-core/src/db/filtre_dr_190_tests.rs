//! Ticket 190 — la liste des valeurs de Dynamic Range du rail de filtres
//! (`AlbumRepo::dynamic_range_values`, `GET /library/albums/filters`) :
//! `slow_query` de 2,8 puis 7,6 s, `attente_ms=0`, pendant un scan, sur une
//! bibliothèque de quelque 24 000 pistes.
//!
//! Banc : base de FICHIER, profil d'une bibliothèque réelle (une dizaine de
//! clés étendues par piste dans `track_metadata`, des paroles sur une part).
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::album_repo::AlbumRepo;
use super::backend::DbBackend;
use super::engine::Engine;
use super::sqlite::SqliteDb;

pub(super) struct Banc {
    _dossier: crate::test_scratch::ScratchDir,
    pub db: Arc<SqliteDb>,
    pub chemin: String,
}

/// `albums` albums de `pistes` pistes. Une piste sur sept sans DR de piste ;
/// un album sur trois porte le tag d'album sur toutes ses pistes.
pub(super) fn remplir(epreuve: &str, albums: i64, pistes: i64) -> Banc {
    let dossier = crate::test_scratch::scratch_dir(&format!("filtre-dr-190-{epreuve}"));
    let chemin = dossier.join("tune-banc.db").to_string_lossy().into_owned();
    let db = SqliteDb::open(&chemin).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let paroles = "la la la, une ligne de paroles assez longue pour peser. ".repeat(30);
    let commentaire = "Rip EAC, journal vérifié, AccurateRip OK — ".repeat(2);
    let mut sql = String::with_capacity(1 << 24);
    sql.push_str("BEGIN;\n");
    let artistes = (albums / 3).max(10);
    for a in 1..=artistes {
        sql.push_str(&format!(
            "INSERT INTO artists (id, name) VALUES ({a}, 'Artiste {a}');\n"
        ));
    }
    let mut tid = 0_i64;
    for al in 1..=albums {
        let artiste = al % artistes + 1;
        sql.push_str(&format!(
            "INSERT INTO albums (id, title, artist_id, source) VALUES ({al}, 'Album {al}', {artiste}, 'local');\n"
        ));
        let tag_album = al % 3 == 0;
        for n in 1..=pistes {
            tid += 1;
            sql.push_str(&format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
                 file_path, format, sample_rate, bit_depth, source, album_artist) \
                 VALUES ({tid}, 'Piste {n} de l''album {al}', {al}, {artiste}, 1, {n}, \
                 '/music/genre/Artiste {artiste}/Artiste {artiste} - Album {al} (2001) [24-96]/{n:02}. Piste {n}.flac', \
                 'flac', 96000, 24, 'local', 'Artiste {artiste}');\n"
            ));
            let mut cle = |k: &str, v: &str| {
                sql.push_str(&format!(
                    "INSERT INTO track_metadata (track_id, key, value) VALUES ({tid}, '{k}', '{}');\n",
                    v.replace('\'', "''")
                ));
            };
            cle("composer", &format!("Compositeur {}", tid % 997));
            cle("isrc", &format!("FRZ01{tid:07}"));
            cle("label", &format!("Label {}", al % 211));
            cle("catalog_number", &format!("CAT-{al:05}"));
            cle("barcode", &format!("{:013}", al * 7919));
            cle("comment", &commentaire);
            cle("bpm", &format!("{}", 60 + tid % 120));
            cle("rg_track_gain", "-7.85 dB");
            cle("rg_track_peak", "0.988525");
            cle("rg_album_gain", "-8.10 dB");
            cle("rg_album_peak", "0.999969");
            cle("audio_embed_analyzed", "clap-v1");
            if tid % 7 != 0 {
                cle("dr_track", &format!("{}", 5 + tid % 11));
            }
            if tag_album {
                cle("dr_album", &format!("{}", 6 + al % 9));
            }
            if tid % 6 == 0 {
                cle("lyrics", &paroles);
            }
        }
        if sql.len() > (1 << 23) {
            db.execute_batch(&sql).unwrap();
            sql.clear();
        }
    }
    sql.push_str("COMMIT;\nANALYZE;\n");
    db.execute_batch(&sql).unwrap();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    Banc {
        _dossier: dossier,
        db: Arc::new(db),
        chemin,
    }
}

/// Le SQL de la liste des valeurs, tel que le dépôt le joue.
pub(super) fn sql_des_valeurs() -> String {
    format!(
        "SELECT DISTINCT dr.dr FROM albums a {} WHERE dr.dr IS NOT NULL AND {} ORDER BY dr.dr",
        AlbumRepo::dr_album_join(Engine::Sqlite),
        crate::db::facet_filter::hidden_albums_excluded()
    )
}

pub(super) fn plan(db: &SqliteDb, sql: &str) -> Vec<String> {
    db.query_many(&format!("EXPLAIN QUERY PLAN {sql}"), &[])
        .unwrap()
        .iter()
        .map(|r| r.last().and_then(|v| v.as_string()).unwrap_or_default())
        .collect()
}

/// Pages lues hors du cache de page d'une connexion NEUVE, sans `mmap` : ce
/// qu'un disque froid doit rendre, page par page, pour une requête.
pub(super) fn pages_lues_a_froid(chemin: &str, sql: &str) -> (i64, Duration) {
    let c = rusqlite::Connection::open(chemin).unwrap();
    c.execute_batch("PRAGMA mmap_size=0; PRAGMA cache_size=-65536;")
        .unwrap();
    let t0 = Instant::now();
    let mut st = c.prepare(sql).unwrap();
    let n = st.query_map([], |_| Ok(())).unwrap().count();
    let ecoule = t0.elapsed();
    assert!(n > 0);
    let (mut cur, mut hi) = (0i32, 0i32);
    // SAFETY: poignée valide le temps de l'appel ; lecture d'un compteur.
    unsafe {
        rusqlite::ffi::sqlite3_db_status(
            c.handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
            &mut cur,
            &mut hi,
            0,
        );
    }
    (cur as i64, ecoule)
}

fn chrono_chaud(repo: &AlbumRepo) -> (Vec<i64>, Duration) {
    let mut meilleur = Duration::MAX;
    let mut valeurs = Vec::new();
    for _ in 0..5 {
        let t0 = Instant::now();
        valeurs = repo.dynamic_range_values().unwrap();
        meilleur = meilleur.min(t0.elapsed());
    }
    (valeurs, meilleur)
}

/// L'état d'avant : la base sans les deux index du ticket 190.
fn retirer_les_index(db: &SqliteDb) {
    db.execute_batch(
        "DROP INDEX IF EXISTS idx_track_metadata_dr; \
         DROP INDEX IF EXISTS idx_tracks_id_album; ANALYZE;",
    )
    .unwrap();
}

/// La liste des valeurs lit l'index partiel des DR (« COVERING »), et une
/// connexion neuve lit au moins quatre fois moins de pages que sans les
/// index, pour les mêmes valeurs.
#[test]
fn le_rail_dr_ne_lit_que_les_index_190() {
    let b = remplir("epreuve", 300, 9);
    let sql = sql_des_valeurs();
    let repo = AlbumRepo::with_backend(b.db.clone());
    let plan_apres = plan(&b.db, &sql).join(" | ");
    let valeurs_apres = repo.dynamic_range_values().unwrap();
    let (pages_apres, _) = pages_lues_a_froid(&b.chemin, &sql);
    retirer_les_index(&b.db);
    let valeurs_avant = repo.dynamic_range_values().unwrap();
    let (pages_avant, _) = pages_lues_a_froid(&b.chemin, &sql);
    eprintln!("ticket 190 : {pages_avant} pages sans les index, {pages_apres} avec");
    assert_eq!(
        valeurs_apres, valeurs_avant,
        "les index changent les valeurs"
    );
    assert!(!valeurs_apres.is_empty());
    // `idx_tracks_id_album` n'est pas exigé ici : sur une base aussi petite,
    // le planificateur lui préfère la clé primaire. Au banc de 25 200 pistes,
    // il le prend (`banc_190_valeurs_dr_du_rail_de_filtres`).
    assert!(
        plan_apres.contains("COVERING INDEX idx_track_metadata_dr"),
        "la liste des valeurs DR ne lit pas l'index partiel : {plan_apres}"
    );
    assert!(
        pages_apres * 4 <= pages_avant,
        "{pages_apres} pages lues avec les index contre {pages_avant} sans"
    );
}

/// Banc de mesure (ignoré par défaut) : plan, pages lues à froid, temps à
/// chaud, au profil du ticket (2 800 albums de 9 pistes), avant et après.
#[test]
#[ignore = "banc de mesure : cargo test -p tune-core --lib filtre_dr_190 -- --ignored --nocapture"]
fn banc_190_valeurs_dr_du_rail_de_filtres() {
    let b = remplir("banc", 2_800, 9);
    let sql = sql_des_valeurs();
    let repo = AlbumRepo::with_backend(b.db.clone());
    let total: i64 =
        b.db.query_one("SELECT page_count FROM pragma_page_count", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
    let mesurer = |etat: &str| {
        let (valeurs, chaud) = chrono_chaud(&repo);
        let (pages, froid) = pages_lues_a_froid(&b.chemin, &sql);
        eprintln!(
            "banc 190 {etat} : 25 200 pistes, {} valeurs, chaud {chaud:?}, connexion neuve \
             {froid:?}, {pages} pages lues sur {total}",
            valeurs.len()
        );
        for l in plan(&b.db, &sql) {
            eprintln!("  plan : {l}");
        }
    };
    mesurer("après");
    retirer_les_index(&b.db);
    mesurer("avant");
}
