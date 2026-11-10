//! #7767 — le banc de la page artiste, sur le modèle de
//! `db/lenteur_albums_4800_tests.rs` : 600 000 pistes et 3 millions de
//! crédits dans une base SQLite en mémoire, et les deux lectures de la page
//! (`credits_hors_discographie` + `classer`, `compteurs_de_credits`) chacune
//! sous 300 ms. En `#[ignore]` : il remplit la base en quelques minutes.
//!
//!     cargo test -p tune-core --lib banc_7767 -- --ignored --nocapture
//!
//! Il mesure aussi la même base APRÈS les index `(artist_id, role)` et
//! `(artist_name, role)` que l'issue tient en réserve « si la mesure le
//! demande » : les deux temps sont imprimés, la décision se lit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::backend::DbBackend;
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::metadata::credits_release::{classer, compteurs_de_credits, credits_hors_discographie};

const ARTISTES: i64 = 20_000;
const ALBUMS: i64 = 60_000;
const PISTES_PAR_ALBUM: i64 = 10;
const CREDITS_PAR_PISTE: i64 = 5;
/// L'artiste mesuré : la fiche 1, « Artiste 1 », présente partout où le
/// modulo la met.
const CIBLE: i64 = 1;

const ROLES: [&str; 8] = [
    "performer",
    "vocal",
    "composer",
    "producer",
    "engineer",
    "arranger",
    "remixer",
    "mastering",
];

/// 600 000 pistes réparties sur 60 000 albums de 20 000 artistes, 3 millions
/// de crédits : chaque piste porte cinq crédits dont les noms tournent sur
/// les 20 000 artistes, un crédit sur cinq avec sa fiche, les autres par le
/// nom seul. L'artiste 1 y est l'artiste de 30 pistes (3 albums) et crédité
/// sur 150 pistes (le modulo tourne : 30 cycles de 5).
fn banc() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let debut = Instant::now();
    // Des `INSERT` à plusieurs lignes, par lots d'une transaction : une ligne
    // par instruction prenait une heure pour 3 millions de crédits.
    const LIGNES_PAR_INSERT: usize = 500;
    let inserer = |db: &SqliteDb, tete: &str, lignes: &[String]| {
        let mut sql = String::from("BEGIN;\n");
        for morceau in lignes.chunks(LIGNES_PAR_INSERT) {
            sql.push_str(tete);
            sql.push_str(&morceau.join(",\n"));
            sql.push_str(";\n");
        }
        sql.push_str("COMMIT;");
        db.execute_batch(&sql).unwrap();
    };
    let artistes: Vec<String> = (1..=ARTISTES)
        .map(|a| format!("({a}, 'Artiste {a}')"))
        .collect();
    inserer(&db, "INSERT INTO artists (id, name) VALUES\n", &artistes);
    let albums: Vec<String> = (1..=ALBUMS)
        .map(|al| {
            format!(
                "({al}, 'Album {al}', {}, {}, 'local', 'album')",
                al % ARTISTES + 1,
                1960 + al % 60
            )
        })
        .collect();
    inserer(
        &db,
        "INSERT INTO albums (id, title, artist_id, year, source, release_type) VALUES\n",
        &albums,
    );
    let mut credit_id = 0i64;
    let mut pistes: Vec<String> = Vec::with_capacity(20_000);
    let mut credits: Vec<String> = Vec::with_capacity(100_000);
    for al in 1..=ALBUMS {
        let artiste = al % ARTISTES + 1;
        for n in 1..=PISTES_PAR_ALBUM {
            let t = al * 10 + n;
            pistes.push(format!(
                "({t}, 'Piste {t}', {al}, {artiste}, '/m/{t}.flac', 200000, 'flac', 1, {n})"
            ));
            for c in 0..CREDITS_PAR_PISTE {
                credit_id += 1;
                let nom = (t * 7 + c * 13) % ARTISTES + 1;
                let role = ROLES[((t + c) % ROLES.len() as i64) as usize];
                let fiche = if c == 0 {
                    nom.to_string()
                } else {
                    "NULL".into()
                };
                credits.push(format!(
                    "({credit_id}, {t}, {fiche}, 'Artiste {nom}', '{role}', NULL, {c}, 'musicbrainz')"
                ));
            }
        }
        if al % 2_000 == 0 || al == ALBUMS {
            inserer(
                &db,
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) VALUES\n",
                &pistes,
            );
            inserer(
                &db,
                "INSERT INTO track_credits (id, track_id, artist_id, artist_name, role, instrument, position, source) VALUES\n",
                &credits,
            );
            pistes.clear();
            credits.clear();
        }
    }
    db.execute_batch("ANALYZE;").unwrap();
    eprintln!(
        "banc rempli en {:?} : {ALBUMS} albums, {} pistes, {credit_id} crédits",
        debut.elapsed(),
        ALBUMS * PISTES_PAR_ALBUM
    );
    Arc::new(db)
}

fn mesurer(b: &Arc<dyn DbBackend>, etiquette: &str) -> (Duration, Duration) {
    // Une première passe chauffe le cache de pages, la seconde est mesurée :
    // c'est la page artiste rouverte, le cas courant.
    let _ = credits_hors_discographie(b, CIBLE);
    let debut = Instant::now();
    let (credits, disques) = credits_hors_discographie(b, CIBLE);
    let classement = classer(&credits, &disques);
    let sections = debut.elapsed();
    let _ = compteurs_de_credits(b, CIBLE);
    let debut = Instant::now();
    let compteurs = compteurs_de_credits(b, CIBLE);
    let comptes = debut.elapsed();
    eprintln!(
        "{etiquette} : sections {sections:?} ({} crédits, {} apparaît sur, {} reprises, {} écriture) ; compteurs {comptes:?} ({compteurs:?})",
        credits.len(),
        classement.collaborations.len(),
        classement.reprises.len(),
        classement.ecriture.len()
    );
    assert!(
        credits.len() > 100,
        "le banc ne crédite pas l'artiste mesuré"
    );
    assert!(compteurs.performing.tracks > 30 && compteurs.writing.tracks > 0);
    (sections, comptes)
}

#[test]
#[ignore]
fn banc_7767_page_artiste_sous_300_ms_sur_3_millions_de_credits() {
    let b = banc();
    let (sections, comptes) = mesurer(&b, "sans index de rôle");
    b.execute(
        "CREATE INDEX IF NOT EXISTS idx_track_credits_artist_id_role ON track_credits(artist_id, role)",
        &[],
    )
    .unwrap();
    b.execute(
        "CREATE INDEX IF NOT EXISTS idx_track_credits_artist_name_role ON track_credits(artist_name, role)",
        &[],
    )
    .unwrap();
    b.execute("ANALYZE;", &[]).unwrap();
    let (sections_idx, comptes_idx) =
        mesurer(&b, "avec index (artist_id, role) et (artist_name, role)");
    let limite = Duration::from_millis(300);
    assert!(
        sections.min(sections_idx) < limite && comptes.min(comptes_idx) < limite,
        "page artiste trop lente : sections {sections:?} / {sections_idx:?}, compteurs {comptes:?} / {comptes_idx:?}"
    );
}
