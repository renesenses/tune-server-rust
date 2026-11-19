//! #9116 — témoins de l'arbre des dossiers : équivalence avec les requêtes
//! SQL qu'il remplace (≥ 200 000 chemins), temps par niveau, mémoire,
//! invalidation, Unicode, séparateurs Windows, casse, tranches CUE.

use std::time::{Duration, Instant};

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::track_repo as tr;

fn base() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db
}

/// 20 genres × 50 artistes × 20 albums × 10 pistes = 200 000 pistes sous
/// `/srv/musique`, plus, par artiste, une piste posée à même son dossier.
const GENRES: i64 = 20;
const ARTISTES: i64 = 50;
const ALBUMS: i64 = 20;
const PISTES: i64 = 10;

fn peupler(db: &SqliteDb) -> i64 {
    let n = GENRES * ARTISTES * ALBUMS * PISTES;
    db.execute_batch(&format!(
        "BEGIN;
         WITH RECURSIVE g(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM g WHERE i < {n} - 1)
         INSERT INTO tracks (title, file_path, format, source, track_number)
         SELECT 'Piste ' || i,
                '/srv/musique/Genre ' || (i / {apg}) || '/Artiste ' || ((i / {apa}) % {ARTISTES})
                || '/Album ' || ((i / {PISTES}) % {ALBUMS}) || '/' || (i % {PISTES}) || '.flac',
                'flac', 'local', i % {PISTES} FROM g;
         WITH RECURSIVE g(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM g WHERE i < {na} - 1)
         INSERT INTO tracks (title, file_path, format, source)
         SELECT 'Inédit ' || i, '/srv/musique/Genre ' || (i / {ARTISTES}) || '/Artiste '
                || (i % {ARTISTES}) || '/inedit.flac', 'flac', 'local' FROM g;
         COMMIT; ANALYZE;",
        apg = ARTISTES * ALBUMS * PISTES,
        apa = ALBUMS * PISTES,
        na = GENRES * ARTISTES,
    ))
    .unwrap();
    n + GENRES * ARTISTES
}

fn temps<T>(f: impl FnOnce() -> T) -> (T, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}

/// Les pistes directes de `dossier` selon la route d'avant (plage récursive
/// puis filtre « enfant direct »), en identifiants triés.
fn directes_sql(db: &SqliteDb, dossier: &str) -> Vec<i64> {
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let motif = tr::folder_like_pattern(dossier);
    let (bas, haut) = tr::bornes_du_dossier(dossier);
    let depart = dossier.chars().count() + 2;
    let sql = tr::sql_pistes_du_dossier(Engine::Sqlite, depart);
    let mut v: Vec<i64> = db
        .query_many(&sql, &[&motif as &dyn ToSqlValue, &sep, &bas, &haut])
        .unwrap()
        .iter()
        .filter(|l| {
            l.get(16)
                .and_then(|v| v.as_string())
                .and_then(|c| {
                    std::path::Path::new(&c)
                        .parent()
                        .map(|p| p.to_str() == Some(dossier))
                })
                .unwrap_or(false)
        })
        .map(|l| l[0].as_i64().unwrap())
        .collect();
    v.sort();
    v
}

/// Le banc : ≥ 200 000 chemins, la descente « racine › genre › artiste ›
/// album » mesurée niveau par niveau, arbre contre requêtes SQL (la
/// contre-épreuve sans cache), et chaque chiffre de l'arbre confronté à
/// celui de la requête qu'il remplace.
#[test]
fn banc_200k_descente_par_niveau_arbre_contre_sql() {
    let db = base();
    let total = peupler(&db);
    assert!(total >= 200_000);
    let (arbre, t_construction) = temps(|| construire(&db).unwrap());
    assert_eq!(arbre.nb_chemins() as i64, total);
    let octets = arbre.octets();
    eprintln!(
        "#9116 arbre : {} chemins, {} dossiers, construit en {} ms, {} Kio ({:.1} octets par chemin)",
        arbre.nb_chemins(),
        arbre.nb_dossiers(),
        t_construction.as_millis(),
        octets / 1024,
        octets as f64 / total as f64
    );
    // Mémoire bornée : l'identifiant (8 octets) par piste, plus un nœud par
    // dossier (≈ 21 000 ici). Plafond large, mais qui rougit si l'arbre se
    // mettait à garder les chemins entiers (~50 octets chacun).
    assert!(
        octets < 40 * total as usize,
        "{octets} octets pour {total} chemins : l'arbre ne doit pas garder les chemins"
    );

    let niveaux = [
        "/srv/musique".to_string(),
        "/srv/musique/Genre 7".to_string(),
        "/srv/musique/Genre 7/Artiste 13".to_string(),
        "/srv/musique/Genre 7/Artiste 13/Album 4".to_string(),
    ];
    let mut pire_arbre = Duration::ZERO;
    for dossier in &niveaux {
        let (comptes_sql, t_sql) =
            temps(|| tr::compter_pistes_par_sous_dossier(&db, dossier).unwrap());
        let (directes_attendues, t_sql_pistes) = temps(|| directes_sql(&db, dossier));
        let (vue, t_arbre) = temps(|| {
            let v = arbre.dossier(dossier);
            // Ce que la route lit pour un niveau : chaque sous-dossier et
            // les pistes directes.
            let noms: Vec<String> = (0..ARTISTES.max(GENRES).max(ALBUMS))
                .flat_map(|i| {
                    [
                        format!("Genre {i}"),
                        format!("Artiste {i}"),
                        format!("Album {i}"),
                    ]
                })
                .collect();
            let comptes: Vec<(String, i64)> = noms
                .into_iter()
                .map(|n| {
                    let c = v.pistes_du_sous_dossier(&n);
                    (n, c)
                })
                .collect();
            (comptes, v.pistes_directes().to_vec())
        });
        pire_arbre = pire_arbre.max(t_arbre);
        eprintln!(
            "#9116 niveau {dossier} : SQL {} ms (comptes) + {} ms (pistes directes), arbre {} µs",
            t_sql.as_millis(),
            t_sql_pistes.as_millis(),
            t_arbre.as_micros()
        );
        let (comptes, mut directes) = vue;
        for (nom, c) in &comptes {
            assert_eq!(*c, comptes_sql.get(nom), "{dossier} › {nom}");
        }
        assert_eq!(
            arbre.dossier(dossier).nb_sous_dossiers(),
            comptes_sql.nb_dossiers_peuples(),
            "{dossier}"
        );
        directes.sort();
        assert_eq!(directes, directes_attendues, "pistes directes de {dossier}");
        assert_eq!(
            arbre.fichiers_sous(dossier),
            tr::compter_pistes_sous_dossier(&db, dossier).unwrap(),
            "compte de racine {dossier}"
        );
    }
    assert!(
        pire_arbre < Duration::from_millis(100),
        "un niveau lu dans l'arbre a pris {} ms (seuil 100 ms)",
        pire_arbre.as_millis()
    );
}

/// `lire` garde l'arbre tant que la révision des chemins ne bouge pas, et en
/// reconstruit un dès qu'elle bouge — mais pas pour une écriture hors chemins.
#[test]
fn lire_garde_l_arbre_et_l_invalide_sur_les_seuls_chemins() {
    let db = base();
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path, format, source) \
         VALUES (1, 'a', '/m/A/1.flac', 'flac', 'local');",
    )
    .unwrap();
    let a1 = lire(&db).expect("révision des chemins posée par la passe de démarrage");
    let a2 = lire(&db).unwrap();
    assert!(Arc::ptr_eq(&a1, &a2), "même révision : même arbre");
    db.execute_batch("UPDATE tracks SET genre = 'Jazz', title = 'x' WHERE id = 1;")
        .unwrap();
    db.execute_batch(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (1, 'dr_track', '12');",
    )
    .unwrap();
    assert!(
        Arc::ptr_eq(&a1, &lire(&db).unwrap()),
        "une écriture hors chemins ne reconstruit rien"
    );
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path, format, source) \
         VALUES (2, 'b', '/m/A/2.flac', 'flac', 'local');",
    )
    .unwrap();
    let a3 = lire(&db).unwrap();
    assert!(!Arc::ptr_eq(&a1, &a3), "un ajout reconstruit");
    assert_eq!(a3.pistes_sous("/m/A"), 2);
    db.execute_batch("UPDATE tracks SET file_path = '/m/B/2.flac' WHERE id = 2;")
        .unwrap();
    let a4 = lire(&db).unwrap();
    assert_eq!((a4.pistes_sous("/m/A"), a4.pistes_sous("/m/B")), (1, 1));
    db.execute_batch("DELETE FROM tracks WHERE id = 1;")
        .unwrap();
    assert_eq!(lire(&db).unwrap().pistes_sous("/m"), 1);
}

/// Sans aucune révision lisible, rien n'est gardé : `lire` rend `None` et la
/// route retombe sur ses requêtes.
#[test]
fn sans_revision_aucun_arbre() {
    let db = base();
    db.execute_batch("DROP TABLE library_paths_revision; DROP TABLE library_revision;")
        .unwrap();
    assert!(lire(&db).is_none());
}

/// Le disque peut rendre du NFD, la base peut garder une ligne ancienne en
/// NFD : les deux se rejoignent en NFC.
#[test]
fn unicode_nfc_et_nfd_se_rejoignent() {
    use unicode_normalization::UnicodeNormalization as _;
    let nfc: String = "/m/Dvořák/Symphonie é".nfc().collect();
    let nfd: String = "/m/Dvořák/Symphonie é".nfd().collect();
    assert_ne!(nfc, nfd);
    let mut a = ArbreDesDossiers::nouveau('/', true);
    a.ajouter(1, &format!("{nfc}/1.flac"), true);
    a.ajouter(2, &format!("{nfd}/2.flac"), true);
    assert_eq!(a.pistes_sous(&nfc), 2);
    assert_eq!(a.pistes_sous(&nfd), 2);
    assert_eq!(a.dossier(&nfd).pistes_directes(), &[1, 2]);
    let parent_nfd: String = "/m/Dvořák".nfd().collect();
    let enfant_nfd: String = "Symphonie é".nfd().collect();
    assert_eq!(
        a.dossier(&parent_nfd).pistes_du_sous_dossier(&enfant_nfd),
        2
    );
}

/// Séparateur Windows : lettre de lecteur, racine de lecteur (`D:\`), chemin
/// UNC (`\\NAS\partage`), séparateur final, et `/` qui n'est PAS un
/// séparateur dans `file_path` sous Windows (le `LIKE` non plus ne le
/// coupait pas).
#[test]
fn separateurs_windows() {
    let mut a = ArbreDesDossiers::nouveau('\\', true);
    a.ajouter(1, r"D:\Musique\Bach\01.flac", true);
    a.ajouter(2, r"D:\Musique\Bach\Cantates\01.flac", true);
    a.ajouter(3, r"D:\Musique\AC/DC\01.flac", true);
    a.ajouter(4, r"\\NAS\partage\Jazz\01.flac", true);
    assert_eq!(a.fichiers_sous(r"D:\"), 3);
    assert_eq!(a.fichiers_sous(r"D:\Musique"), 3);
    assert_eq!(a.fichiers_sous(r"D:\Musique\"), 3);
    assert_eq!(a.dossier(r"D:\Musique").pistes_du_sous_dossier("Bach"), 2);
    assert_eq!(a.dossier(r"D:\Musique").pistes_du_sous_dossier("AC/DC"), 1);
    assert_eq!(a.dossier(r"D:\Musique\Bach").pistes_directes(), &[1]);
    assert_eq!(a.fichiers_sous(r"\\NAS\partage"), 1);
    assert_eq!(a.fichiers_sous(r"\\NAS\"), 1);
    assert_eq!(a.fichiers_sous(r"E:\"), 0);
}

/// SQLite : `LIKE` ignore la casse ASCII, l'arbre aussi ; PostgreSQL : non.
#[test]
fn la_casse_suit_le_moteur() {
    for (replier, attendu) in [(true, 2), (false, 1)] {
        let mut a = ArbreDesDossiers::nouveau('/', replier);
        a.ajouter(1, "/m/Sub/1.flac", true);
        a.ajouter(2, "/m/sub/2.flac", true);
        assert_eq!(a.dossier("/m").pistes_du_sous_dossier("Sub"), attendu);
        assert_eq!(a.fichiers_sous("/M/Sub"), if replier { 2 } else { 0 });
    }
}

/// Une tranche CUE (`file_path` NULL, chemin dans `cue_media_path`) compte
/// dans un sous-dossier (#4625) mais pas dans le compte d'une racine (la
/// requête d'avant ne voyait que `file_path`) — en base comme dans l'arbre.
#[test]
fn tranches_cue() {
    let db = base();
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path, format, source) \
             VALUES (1, 'a', '/m/Album/1.flac', 'flac', 'local');
         INSERT INTO tracks (id, title, file_path, cue_media_path, format, source) \
             VALUES (2, 'b', NULL, '/m/Cue/disque.flac', 'flac', 'local');
         INSERT INTO tracks (id, title, file_path, cue_media_path, format, source) \
             VALUES (3, 'c', NULL, '/m/Cue/disque.flac', 'flac', 'local');",
    )
    .unwrap();
    let a = construire(&db).unwrap();
    assert_eq!(
        a.fichiers_sous("/m"),
        tr::compter_pistes_sous_dossier(&db, "/m").unwrap()
    );
    assert_eq!(a.fichiers_sous("/m"), 1);
    let sql = tr::compter_pistes_par_sous_dossier(&db, "/m").unwrap();
    assert_eq!(
        a.dossier("/m").pistes_du_sous_dossier("Cue"),
        sql.get("Cue")
    );
    assert_eq!(a.dossier("/m").pistes_du_sous_dossier("Cue"), 2);
    assert_eq!(a.dossier("/m/Cue").pistes_directes(), &[2, 3]);
}

/// Pages de lecture : une bibliothèque plus grande qu'une page est lue en
/// entier (la reprise `file_path > dernier` ne perd ni ne double rien).
#[test]
fn la_lecture_par_pages_ne_perd_rien() {
    let db = base();
    let n = PAGE + 7;
    db.execute_batch(&format!(
        "WITH RECURSIVE g(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM g WHERE i < {n} - 1)
         INSERT INTO tracks (title, file_path, format, source)
         SELECT 't', '/m/' || (i % 97) || '/' || i || '.flac', 'flac', 'local' FROM g;"
    ))
    .unwrap();
    let a = construire(&db).unwrap();
    assert_eq!(a.nb_chemins() as i64, n);
    assert_eq!(a.fichiers_sous("/m"), n);
}
