//! #7849 — notes personnelles et répartitions de la bibliothèque, en SQLite
//! puis sur une vraie base PostgreSQL (`TUNE_TEST_PG_URL`, SAUT sans elle).
//!
//! * `notes_creation_lecture_mise_a_jour_7849` : une note écrite se relit,
//!   se remplace, se supprime ; un texte vide la supprime ;
//! * `notes_separees_par_profil_7849` : un profil ne voit, ne modifie ni ne
//!   supprime la note d'un autre — le témoin de la contre-épreuve ;
//! * `notes_recherche_q_7849` : filtre `q` sans casse, `%` et `_` littéraux ;
//! * `migration_reprend_les_notes_d_album_7849` : la migration 138 recopie
//!   `album_ratings.note` sans rien perdre ni écraser ;
//! * `note_d_album_tenue_des_deux_cotes_7849` : l'ancienne colonne suit ;
//! * `repartitions_sur_un_jeu_connu_7849` : les quatre répartitions ;
//! * `pg_7849_*` : les mêmes propriétés, script PG 103 compris.

use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::migrations::{current_version, latest_version, run_migrations};
use super::notes_personnelles::{self as notes, TypeDObjet};
use super::rating_repo::RatingRepo;
use super::repartitions_bibliotheque::{self as repartitions, Case};
use super::sqlite::SqliteDb;

fn base() -> (SqliteDb, Arc<dyn DbBackend>) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db.clone());
    (db, b)
}

fn executer(b: &dyn DbBackend, sql: &str) {
    b.execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

fn textes(liste: &[notes::NotePersonnelle]) -> Vec<(TypeDObjet, i64, String)> {
    liste
        .iter()
        .map(|n| (n.item_type, n.item_id, n.texte_md.clone()))
        .collect()
}

// ── Propriétés communes aux deux moteurs ────────────────────────────────────

fn verifier_creation_lecture_mise_a_jour(b: &dyn DbBackend) {
    assert_eq!(notes::lire(b, 1, TypeDObjet::Track, 7).unwrap(), None);
    let ecrite = notes::ecrire(b, 1, TypeDObjet::Track, 7, "# Prise\n*live* <b>brut</b>")
        .unwrap()
        .expect("note écrite");
    let lue = notes::lire(b, 1, TypeDObjet::Track, 7).unwrap().unwrap();
    assert_eq!(lue, ecrite);
    assert_eq!(
        lue.texte_md, "# Prise\n*live* <b>brut</b>",
        "le texte est gardé brut, sans assainissement"
    );
    notes::ecrire(b, 1, TypeDObjet::Track, 7, "Remplacée").unwrap();
    assert_eq!(
        notes::lire(b, 1, TypeDObjet::Track, 7)
            .unwrap()
            .unwrap()
            .texte_md,
        "Remplacée"
    );
    // Même identifiant, autre type : une autre note.
    notes::ecrire(b, 1, TypeDObjet::Artist, 7, "L'artiste 7").unwrap();
    assert_eq!(
        notes::lire(b, 1, TypeDObjet::Track, 7)
            .unwrap()
            .unwrap()
            .texte_md,
        "Remplacée"
    );
    assert_eq!(
        notes::ecrire(b, 1, TypeDObjet::Track, 7, "  \n").unwrap(),
        None
    );
    assert_eq!(notes::lire(b, 1, TypeDObjet::Track, 7).unwrap(), None);
    assert!(notes::supprimer(b, 1, TypeDObjet::Artist, 7).unwrap());
    assert!(!notes::supprimer(b, 1, TypeDObjet::Artist, 7).unwrap());
}

fn verifier_separation_par_profil(b: &dyn DbBackend) {
    notes::ecrire(b, 21, TypeDObjet::Album, 40, "Note du profil 21").unwrap();
    assert_eq!(
        notes::lire(b, 22, TypeDObjet::Album, 40).unwrap(),
        None,
        "#7849 : le profil 22 lit la note du profil 21"
    );
    assert!(
        notes::lister(b, 22, None, None, 100).unwrap().is_empty(),
        "#7849 : la liste du profil 22 montre la note du profil 21"
    );
    assert!(
        notes::lister(b, 22, None, Some("profil"), 100)
            .unwrap()
            .is_empty(),
        "#7849 : la recherche du profil 22 trouve la note du profil 21"
    );
    notes::ecrire(b, 22, TypeDObjet::Album, 40, "Note du profil 22").unwrap();
    assert!(
        !notes::supprimer(b, 22, TypeDObjet::Artist, 40).unwrap(),
        "rien à supprimer pour le profil 22 sur l'artiste 40"
    );
    assert_eq!(
        notes::lire(b, 21, TypeDObjet::Album, 40)
            .unwrap()
            .unwrap()
            .texte_md,
        "Note du profil 21",
        "#7849 : l'écriture du profil 22 a modifié la note du profil 21"
    );
    assert!(notes::supprimer(b, 22, TypeDObjet::Album, 40).unwrap());
    assert_eq!(
        notes::lire(b, 21, TypeDObjet::Album, 40)
            .unwrap()
            .unwrap()
            .texte_md,
        "Note du profil 21",
        "#7849 : la suppression du profil 22 a emporté la note du profil 21"
    );
}

fn verifier_recherche(b: &dyn DbBackend) {
    notes::ecrire(b, 23, TypeDObjet::Album, 1, "Pressage JAPONAIS, superbe").unwrap();
    notes::ecrire(b, 23, TypeDObjet::Track, 2, "Solo japonais à 3:12").unwrap();
    notes::ecrire(b, 23, TypeDObjet::Artist, 3, "Vu en concert").unwrap();
    notes::ecrire(b, 23, TypeDObjet::Track, 4, "Volume à 100% conseillé").unwrap();
    notes::ecrire(b, 23, TypeDObjet::Track, 5, "Volume à 1000 conseillé").unwrap();
    let mut trouvees = textes(&notes::lister(b, 23, None, Some("japonais"), 100).unwrap());
    trouvees.sort_by_key(|t| t.1);
    assert_eq!(
        trouvees,
        vec![
            (
                TypeDObjet::Album,
                1,
                "Pressage JAPONAIS, superbe".to_string()
            ),
            (TypeDObjet::Track, 2, "Solo japonais à 3:12".to_string()),
        ]
    );
    let pistes =
        textes(&notes::lister(b, 23, Some(TypeDObjet::Track), Some("japonais"), 100).unwrap());
    assert_eq!(pistes.len(), 1);
    assert_eq!(pistes[0].1, 2);
    let pour_cent = textes(&notes::lister(b, 23, None, Some("100%"), 100).unwrap());
    assert_eq!(
        pour_cent.iter().map(|t| t.1).collect::<Vec<_>>(),
        vec![4],
        "`%` de la saisie est un caractère, pas un joker"
    );
    assert_eq!(notes::lister(b, 23, None, None, 100).unwrap().len(), 5);
    assert_eq!(notes::lister(b, 23, None, Some("  "), 2).unwrap().len(), 2);
}

/// Albums 1 à 3, notes sur cinq : 1 (profil 1, note), 2 (profil 1, note
/// vide), 3 (profil 1 sans note), 1 (profil 2, note), 3 (profil 4, note).
fn semer_notes_d_album(b: &dyn DbBackend) {
    for (id, titre) in [(1, "Un"), (2, "Deux"), (3, "Trois")] {
        b.execute(
            "INSERT INTO albums (id, title) VALUES (?, ?)",
            &[&(id as i64) as &dyn ToSqlValue, &titre],
        )
        .unwrap();
    }
}

#[cfg(feature = "postgres")]
fn semer_notes_d_album_pg(b: &dyn DbBackend) {
    for (id, titre) in [(1i64, "Un"), (2, "Deux"), (3, "Trois")] {
        b.execute(
            "INSERT INTO albums (id, title) VALUES ($1, $2)",
            &[&id as &dyn ToSqlValue, &titre],
        )
        .unwrap();
    }
}

const NOTES_SUR_CINQ: &str = "INSERT INTO album_ratings (album_id, profile_id, rating, note, created_at) VALUES \
    (1, 1, 5, 'Mon **préféré**', '2024-01-02T03:04:05Z'), \
    (2, 1, 3, '   ', '2024-01-02T03:04:05Z'), \
    (3, 1, 4, NULL, '2024-01-02T03:04:05Z'), \
    (1, 2, 2, 'Pas pour moi', '2024-02-03T04:05:06Z'), \
    (3, 4, 1, 'Rayé', '2024-03-04T05:06:07Z')";

fn verifier_reprise(b: &dyn DbBackend) {
    let mut toutes = Vec::new();
    for profil in [1, 2, 3, 4] {
        for n in notes::lister(b, profil, None, None, 100).unwrap() {
            toutes.push((profil, n.item_type, n.item_id, n.texte_md, n.maj_le));
        }
    }
    toutes.sort_by_key(|t| (t.0, t.2));
    assert_eq!(
        toutes,
        vec![
            (
                1,
                TypeDObjet::Album,
                1,
                "Mon **préféré**".to_string(),
                "2024-01-02T03:04:05Z".to_string()
            ),
            (
                2,
                TypeDObjet::Album,
                1,
                "Pas pour moi".to_string(),
                "2024-02-03T04:05:06Z".to_string()
            ),
            (
                4,
                TypeDObjet::Album,
                3,
                "Rayé".to_string(),
                "2024-03-04T05:06:07Z".to_string()
            ),
        ],
        "#7849 : chaque note d'album non vide, et elle seule, est reprise avec son profil"
    );
}

fn verifier_tenue_des_deux_cotes(base: &Arc<dyn DbBackend>) {
    let b = base.as_ref();
    let ratings = RatingRepo::with_backend(base.clone());
    // Nouvelle route → ancienne colonne (la ligne existe : album 1, profil 1).
    notes::ecrire(b, 1, TypeDObjet::Album, 1, "Réécrite").unwrap();
    assert_eq!(
        ratings.get_rating(1, 1).unwrap().unwrap().note.as_deref(),
        Some("Réécrite")
    );
    notes::supprimer(b, 1, TypeDObjet::Album, 1).unwrap();
    assert_eq!(ratings.get_rating(1, 1).unwrap().unwrap().note, None);
    assert_eq!(ratings.get_rating(1, 1).unwrap().unwrap().rating, 5);
    // Ancienne route → nouvelle table.
    ratings
        .rate_album(2, 1, 4, Some("Par l'ancienne route"))
        .unwrap();
    assert_eq!(
        notes::lire(b, 1, TypeDObjet::Album, 2)
            .unwrap()
            .unwrap()
            .texte_md,
        "Par l'ancienne route"
    );
    // Une note sur cinq seule n'efface pas la note personnelle.
    ratings.rate_album(2, 1, 5, None).unwrap();
    assert!(notes::lire(b, 1, TypeDObjet::Album, 2).unwrap().is_some());
}

fn jeu_connu() -> Repartitions7849 {
    Repartitions7849 {
        codec: vec![
            Case {
                value: Some("flac".into()),
                tracks: 3,
                people: None,
            },
            Case {
                value: Some("mp3".into()),
                tracks: 1,
                people: None,
            },
            Case {
                value: None,
                tracks: 2,
                people: None,
            },
        ],
        frequence: vec![
            Case {
                value: Some(44_100),
                tracks: 3,
                people: None,
            },
            Case {
                value: Some(96_000),
                tracks: 2,
                people: None,
            },
            Case {
                value: None,
                tracks: 1,
                people: None,
            },
        ],
        decennie: vec![
            Case {
                value: Some(1950),
                tracks: 1,
                people: None,
            },
            Case {
                value: Some(1970),
                tracks: 2,
                people: None,
            },
            Case {
                value: Some(2020),
                tracks: 1,
                people: None,
            },
            Case {
                value: None,
                tracks: 2,
                people: None,
            },
        ],
        role: vec![
            Case {
                value: Some("performer".into()),
                tracks: 3,
                people: Some(2),
            },
            Case {
                value: Some("composer".into()),
                tracks: 2,
                people: Some(2),
            },
            Case {
                value: None,
                tracks: 1,
                people: Some(1),
            },
        ],
    }
}

struct Repartitions7849 {
    codec: Vec<Case<String>>,
    frequence: Vec<Case<i64>>,
    decennie: Vec<Case<i64>>,
    role: Vec<Case<String>>,
}

/// Six pistes : codecs `FLAC`, `flac`, `flac`, `mp3`, vide, absent ;
/// fréquences 44100 ×3, 96000 ×2, 0 ; années 1959, 1971, album 1979 (piste
/// sans année), 2024, 0 (album sans année), absente.
/// Crédits : piste 1 « Miles » (fiche 10) performer et « miles » sans fiche
/// performer (deux personnes) ; piste 2 Miles (10) performer ; piste 3
/// performer Miles (10) ; compositeurs Gil (fiche 11) piste 1 et « gil »
/// sans fiche piste 4 ; un rôle vide sur la piste 5.
const JEU_DE_PISTES: &[&str] = &[
    "INSERT INTO artists (id, name) VALUES (10, 'Miles Davis'), (11, 'Gil Evans')",
    "INSERT INTO albums (id, title, year) VALUES (100, 'Avec année', 1979), (101, 'Sans année', NULL)",
    "INSERT INTO tracks (id, album_id, title, file_path, format, sample_rate, year) VALUES \
        (1001, 100, 'a', '/m/a', 'FLAC', 44100, 1959), \
        (1002, 100, 'b', '/m/b', 'flac', 44100, 1971), \
        (1003, 100, 'c', '/m/c', 'flac', 96000, NULL), \
        (1004, 100, 'd', '/m/d', 'mp3', 44100, 2024), \
        (1005, 101, 'e', '/m/e', '', 96000, 0), \
        (1006, NULL, 'f', '/m/f', NULL, 0, NULL)",
    "INSERT INTO track_credits (track_id, artist_id, artist_name, role) VALUES \
        (1001, 10, 'Miles', 'performer'), \
        (1001, NULL, 'Miles ', 'Performer'), \
        (1002, 10, 'Miles', 'performer'), \
        (1003, 10, 'Miles Davis', 'performer'), \
        (1001, 11, 'Gil', 'composer'), \
        (1004, NULL, 'gil', 'composer'), \
        (1005, NULL, 'Quelqu''un', '')",
];

fn verifier_repartitions(b: &dyn DbBackend) {
    for sql in JEU_DE_PISTES {
        executer(b, sql);
    }
    let r = repartitions::lire(b).unwrap();
    let attendu = jeu_connu();
    assert_eq!(r.by_codec, attendu.codec, "par codec");
    assert_eq!(r.by_sample_rate, attendu.frequence, "par fréquence");
    assert_eq!(r.by_decade, attendu.decennie, "par décennie");
    assert_eq!(r.by_credit_role, attendu.role, "par rôle de crédit");
}

// ── SQLite ──────────────────────────────────────────────────────────────────

#[test]
fn notes_creation_lecture_mise_a_jour_7849() {
    let (_db, b) = base();
    verifier_creation_lecture_mise_a_jour(b.as_ref());
}

#[test]
fn notes_separees_par_profil_7849() {
    let (_db, b) = base();
    verifier_separation_par_profil(b.as_ref());
}

#[test]
fn notes_recherche_q_7849() {
    let (_db, b) = base();
    verifier_recherche(b.as_ref());
}

#[test]
fn migration_reprend_les_notes_d_album_7849() {
    let (db, b) = base();
    // Base « d'avant » : la table n'existe pas, la 138 n'est pas jouée.
    db.execute_batch(
        "DROP TABLE notes_personnelles;
         DELETE FROM _migrations WHERE version >= 138;",
    )
    .unwrap();
    semer_notes_d_album(b.as_ref());
    executer(b.as_ref(), NOTES_SUR_CINQ);
    assert_eq!(current_version(&db).unwrap(), 135);
    run_migrations(&db).unwrap();
    assert_eq!(current_version(&db).unwrap(), latest_version());
    verifier_reprise(b.as_ref());
    // L'ancienne colonne est intacte.
    let (lignes, notees) = {
        let r = b
            .query_one("SELECT COUNT(*), COUNT(note) FROM album_ratings", &[])
            .unwrap()
            .unwrap();
        (r[0].as_i64().unwrap(), r[1].as_i64().unwrap())
    };
    assert_eq!((lignes, notees), (5, 4), "album_ratings n'a rien perdu");
    // Rejouée, la reprise n'écrase pas une note modifiée depuis.
    notes::ecrire(b.as_ref(), 2, TypeDObjet::Album, 1, "Changé d'avis").unwrap();
    executer(
        b.as_ref(),
        "UPDATE album_ratings SET note = 'Pas pour moi' WHERE album_id = 1 AND profile_id = 2",
    );
    db.execute_batch(include_str!("../../migrations/notes_personnelles.sql"))
        .unwrap();
    assert_eq!(
        notes::lire(b.as_ref(), 2, TypeDObjet::Album, 1)
            .unwrap()
            .unwrap()
            .texte_md,
        "Changé d'avis"
    );
}

#[test]
fn note_d_album_tenue_des_deux_cotes_7849() {
    let (_db, b) = base();
    semer_notes_d_album(b.as_ref());
    executer(b.as_ref(), NOTES_SUR_CINQ);
    verifier_tenue_des_deux_cotes(&b);
}

#[test]
fn repartitions_sur_un_jeu_connu_7849() {
    let (_db, b) = base();
    verifier_repartitions(b.as_ref());
}

// ── PostgreSQL ──────────────────────────────────────────────────────────────

#[cfg(feature = "postgres")]
async fn pg_7849_base() -> Option<(sqlx::PgPool, Arc<dyn DbBackend>)> {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return None;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    let b: Arc<dyn DbBackend> = Arc::new(super::backend::PostgresBackend::new(pool.clone()));
    // Base d'avant la 103 : la table retirée, sa marque aussi ; les tables
    // lues vidées.
    sqlx::raw_sql(
        "DROP TABLE IF EXISTS notes_personnelles;
         DELETE FROM schema_version WHERE version = 103;
         TRUNCATE TABLE track_credits, album_ratings, tracks, albums RESTART IDENTITY CASCADE;",
    )
    .execute(&pool)
    .await
    .unwrap();
    Some((pool, b))
}

#[cfg(feature = "postgres")]
async fn pg_7849_script_101(pool: &sqlx::PgPool) {
    sqlx::raw_sql(include_str!(
        "../../migrations/postgres/103_notes_personnelles.sql"
    ))
    .execute(pool)
    .await
    .unwrap_or_else(|e| panic!("script PG 103 : {e}"));
}

/// Joué contre une vraie base (`TUNE_TEST_PG_URL`, schéma des scripts
/// numérotés). Une seule épreuve, en série : elle vide les tables qu'elle lit.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7849_notes_et_repartitions() {
    let Some((pool, b)) = pg_7849_base().await else {
        return;
    };
    semer_notes_d_album_pg(b.as_ref());
    executer(b.as_ref(), NOTES_SUR_CINQ);
    pg_7849_script_101(&pool).await;
    verifier_reprise(b.as_ref());
    // Rejouable sans effet.
    pg_7849_script_101(&pool).await;
    verifier_reprise(b.as_ref());

    verifier_tenue_des_deux_cotes(&b);
    verifier_creation_lecture_mise_a_jour(b.as_ref());
    verifier_separation_par_profil(b.as_ref());
    verifier_recherche(b.as_ref());
    verifier_repartitions(b.as_ref());
}
