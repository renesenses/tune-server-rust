//! Fil du forum 3013 — banc de mesure : défilement de la grille des albums
//! (tri Titre, lettre « R ») sur une GRANDE bibliothèque.
//!
//! Base SQLite synthétique sur disque (WAL, comme en production), écrite par
//! requêtes préparées dans une transaction. `TUNE_BANC_ALBUMS` (défaut 100 000) fixe le
//! nombre d'albums ; `TUNE_BANC_DIR` le dossier de la base. Lancé à la main :
//! `cargo test -p tune-core --release banc_3013 -- --ignored --nocapture`.

use super::*;
use std::time::{Duration, Instant};

const PREFIXES: [&str; 12] = [
    "Requiem", "Rhapsody", "Abbey", "Blue", "Concerto", "Dark", "Été", "Live", "Rondo", "Symphony",
    "The", "Revolver",
];

pub(super) fn base_synthetique(dossier: &std::path::Path, nb_albums: usize) -> SqliteDb {
    let chemin = dossier.join("banc-3013.db");
    let _ = std::fs::remove_file(&chemin);
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let nb_artistes = (nb_albums / 8).max(1);
    db.write(|c| {
        c.execute_batch("BEGIN")?;
        {
            let mut artiste = c.prepare("INSERT INTO artists (id, name) VALUES (?1, ?2)")?;
            for a in 1..=nb_artistes {
                artiste.execute(rusqlite::params![a as i64, format!("Artiste {a:06}")])?;
            }
            let mut album = c.prepare(
                "INSERT INTO albums (id, title, artist_id, track_count, format, folder_path) \
                 VALUES (?1, ?2, ?3, 3, 'flac', ?4)",
            )?;
            let mut piste = c.prepare(
                "INSERT INTO tracks (id, title, album_id, artist_id, track_number, duration_ms, \
                 file_path, format, sample_rate, bit_depth) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 200000, ?6, 'flac', 44100, 16)",
            )?;
            let mut id_piste = 0i64;
            for i in 1..=nb_albums {
                let prefixe = PREFIXES[i % PREFIXES.len()];
                let artiste = ((i % nb_artistes) + 1) as i64;
                album.execute(rusqlite::params![
                    i as i64,
                    format!("{prefixe} {i:06}"),
                    artiste,
                    format!("/Musique/{i}")
                ])?;
                for n in 1..=3i64 {
                    id_piste += 1;
                    piste.execute(rusqlite::params![
                        id_piste,
                        format!("Piste {n}"),
                        i as i64,
                        artiste,
                        n,
                        format!("/Musique/{i}/{n:02}.flac")
                    ])?;
                }
            }
        }
        c.execute_batch("COMMIT; ANALYZE;")
    })
    .unwrap();
    db
}

pub(super) fn page_par_titre(repo: &AlbumRepo, offset: i64) -> PageAlbumsAvecReperes {
    repo.list_filtered_seeded_avec_reperes(
        100, offset, "title", "asc", None, None, None, false, None, None, true,
    )
    .unwrap()
}

fn mediane(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

#[test]
#[ignore = "banc de mesure, lancé à la main (fil 3013)"]
fn banc_3013_pages_par_titre_sur_grande_bibliotheque() {
    let nb: usize = std::env::var("TUNE_BANC_ALBUMS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000);
    let dossier = std::env::var("TUNE_BANC_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let t0 = Instant::now();
    let db = base_synthetique(&dossier, nb);
    println!(
        "[banc 3013] base de {nb} albums écrite en {:?}",
        t0.elapsed()
    );
    let repo = AlbumRepo::new(db.clone());
    let premiere = page_par_titre(&repo, 0);
    let reperes = premiere.reperes_alphabetiques.clone().unwrap();
    let r = reperes.get("R").copied().unwrap() as i64;
    println!("[banc 3013] total {:?}, repère R = {r}", premiere.total);

    // FROID = le coût d'une page AVANT le correctif : une écriture juste
    // avant périme l'ordre gardé, la page relit et trie tout l'ensemble.
    // CHAUD = une page suivante, sans écriture entre les deux.
    let toucher = || {
        db.execute_batch("UPDATE albums SET track_count = track_count WHERE id = 1;")
            .unwrap()
    };
    for (nom, offset) in [
        ("début", 0i64),
        ("R", r),
        ("R+400", r + 400),
        ("fin", nb as i64 - 100),
    ] {
        let (mut froid, mut chaud) = (Vec::new(), Vec::new());
        for _ in 0..5 {
            toucher();
            let t = Instant::now();
            let p = page_par_titre(&repo, offset);
            froid.push(t.elapsed());
            assert_eq!(p.albums.len().min(1), 1);
            let t = Instant::now();
            let _ = page_par_titre(&repo, offset);
            chaud.push(t.elapsed());
        }
        println!(
            "[banc 3013] titre, offset {nom} ({offset}) : froid {:?}, chaud {:?}",
            mediane(froid),
            mediane(chaud)
        );
    }
    for tri in ["added", "artist"] {
        let t = Instant::now();
        let _ = repo
            .list_filtered_seeded_avec_reperes(
                100, r, tri, "asc", None, None, None, false, None, None, true,
            )
            .unwrap();
        println!("[banc 3013] tri {tri}, offset {r} : {:?}", t.elapsed());
    }

    // La liste ENTIÈRE comme la charge le client web (lots de 2 000, sans
    // tri demandé, donc `added_at`) quand il quitte les pages.
    let t = Instant::now();
    let mut lus = 0usize;
    let mut offset = 0i64;
    loop {
        let lot = repo
            .list_filtered_seeded_avec_reperes(
                2000, offset, "added_at", "asc", None, None, None, false, None, None, true,
            )
            .unwrap();
        lus += lot.albums.len();
        if lot.albums.len() < 2000 {
            break;
        }
        offset += 2000;
    }
    println!(
        "[banc 3013] liste entière par lots de 2000 (added_at) : {lus} albums en {:?}",
        t.elapsed()
    );

    // Rafale : la grille demande 4 pages d'un coup, une petite lecture
    // (une fiche d'album) part au même moment.
    let rafale = |n: usize, froid: bool| {
        if froid {
            toucher();
        }
        let t = Instant::now();
        let fils: Vec<_> = (0..n)
            .map(|k| {
                let repo = AlbumRepo::new(db.clone());
                std::thread::spawn(move || page_par_titre(&repo, r + 100 * k as i64))
            })
            .collect();
        std::thread::sleep(Duration::from_millis(5));
        let tf = Instant::now();
        let _ = repo.get(1).unwrap();
        let fiche = tf.elapsed();
        for f in fils {
            f.join().unwrap();
        }
        (t.elapsed(), fiche)
    };
    for n in [1usize, 4, 8] {
        for froid in [true, false] {
            let (tout, fiche) = rafale(n, froid);
            let etat = if froid { "froide" } else { "chaude" };
            println!(
                "[banc 3013] rafale {etat} de {n} pages : {tout:?} ; fiche d'album lue pendant : {fiche:?}"
            );
        }
    }

    // Écriture concurrente : un « scan » qui met à jour des albums en
    // transactions courtes, pendant qu'on lit des pages.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ecrivain = {
        let db = db.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut k = 0i64;
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let mut sql = String::from("BEGIN;");
                for j in 0..200 {
                    let id = (k * 200 + j) % nb as i64 + 1;
                    sql.push_str(&format!(
                        "UPDATE albums SET track_count = track_count WHERE id = {id};"
                    ));
                }
                sql.push_str("COMMIT;");
                db.execute_batch(&sql).unwrap();
                k += 1;
                n += 1;
            }
            n
        })
    };
    let mut d = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let _ = page_par_titre(&repo, r);
        d.push(t.elapsed());
    }
    let (tout, fiche) = rafale(4, false);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let lots = ecrivain.join().unwrap();
    println!(
        "[banc 3013] SOUS ÉCRITURE ({lots} lots) : page R médiane {:?} ; rafale de 4 : {tout:?}, fiche : {fiche:?}",
        mediane(d)
    );
}

/// Fil du forum 3013 — défiler sous « R » ne relit plus la bibliothèque
/// entière à chaque page : l'ordre est lu UNE fois entre deux écritures, les
/// pages sont celles de l'ordre complet, et une écriture le fait relire.
#[test]
fn les_pages_par_titre_ne_relisent_pas_toute_la_bibliotheque_3013() {
    let dossier = tempfile::tempdir().unwrap();
    let db = base_synthetique(dossier.path(), 2_000);
    let repo = AlbumRepo::new(db.clone());
    let lectures = || LECTURES_D_ORDRE_ALPHABETIQUE.with(|n| n.get());

    let avant = lectures();
    let premiere = page_par_titre(&repo, 0);
    let r = premiere.reperes_alphabetiques.unwrap()["R"] as i64;
    let mut vus: Vec<i64> = Vec::new();
    for k in 0..5 {
        vus.extend(
            page_par_titre(&repo, r + 100 * k)
                .albums
                .iter()
                .filter_map(|a| a.id),
        );
    }
    assert_eq!(
        lectures() - avant,
        1,
        "six pages (la première, puis cinq sous « R ») : l'ensemble ne doit être lu et trié qu'une fois"
    );

    // Les pages sont exactement celles de l'ordre complet.
    let sql = "SELECT a.id, a.title, ar.name, a.year FROM albums a \
               LEFT JOIN artists ar ON a.artist_id = ar.id";
    let (tous, _) = repo
        .ids_en_ordre_alphabetique(sql, &[], false, false)
        .unwrap();
    let debut = r as usize;
    assert_eq!(vus, tous[debut..debut + 500].to_vec());

    // Une écriture périme l'ordre gardé : l'album ajouté est vu tout de suite.
    db.execute_batch(
        "INSERT INTO albums (id, title, track_count, format, folder_path) \
         VALUES (900001, 'R 0', 0, 'flac', '/Musique/neuf');",
    )
    .unwrap();
    let avant = lectures();
    let page = page_par_titre(&repo, r);
    assert_eq!(
        lectures() - avant,
        1,
        "après une écriture, l'ordre est relu"
    );
    assert_eq!(page.albums.first().and_then(|a| a.id), Some(900_001));
    assert_eq!(page.total, Some(2_001));
}
