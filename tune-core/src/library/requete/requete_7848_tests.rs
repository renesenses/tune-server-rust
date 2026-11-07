//! #7848 — la grammaire de requêtes : l'analyseur (arbres et erreurs avec
//! position), le compilateur joué sur SQLite ET PostgreSQL avec le même jeu
//! de pistes et de crédits, l'injection, et le banc sur 100 000 pistes.

use std::sync::Arc;
use std::time::Instant;

use super::analyse::{Champ, ChampNombre, ChampTexte, Comparaison, Expr, Role, Test, analyser};
use super::compile::{
    Contexte, FROM_DES_PISTES, LIMITE_MAX, compiler, nombre_de_pistes, pistes_correspondantes,
};
use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::engine::Engine;

/// Le profil des notes et des favoris du jeu d'essai (isolé sur une base PG
/// partagée).
const PROFIL: i64 = 7848;

fn terme(t: &str) -> Expr {
    Expr::Terme(t.to_string())
}

fn texte(champ: ChampTexte, v: &str) -> Expr {
    Expr::Condition {
        champ: Champ::Texte(champ),
        test: Test::Contient(v.to_string()),
    }
}

// ---------------------------------------------------------------- analyseur

#[test]
fn l_analyseur_rend_l_arbre_attendu_7848() {
    assert_eq!(
        analyser("miles davis").unwrap(),
        Expr::Et(vec![terme("miles"), terme("davis")])
    );
    // Le ET implicite lie plus fort que OR.
    assert_eq!(
        analyser("a OR b c").unwrap(),
        Expr::Ou(vec![terme("a"), Expr::Et(vec![terme("b"), terme("c")])])
    );
    assert_eq!(
        analyser("a AND b").unwrap(),
        Expr::Et(vec![terme("a"), terme("b")])
    );
    // En minuscules, `or` est un mot cherché.
    assert_eq!(
        analyser("rock or roll").unwrap(),
        Expr::Et(vec![terme("rock"), terme("or"), terme("roll")])
    );
    assert_eq!(
        analyser("-genre:jazz").unwrap(),
        Expr::Non(Box::new(texte(ChampTexte::Genre, "jazz")))
    );
    assert_eq!(
        analyser("NOT (a OR b)").unwrap(),
        Expr::Non(Box::new(Expr::Ou(vec![terme("a"), terme("b")])))
    );
    assert_eq!(
        analyser("-\"live at\"").unwrap(),
        Expr::Non(Box::new(terme("live at")))
    );
    assert_eq!(
        analyser("artist:\"Miles Davis\"").unwrap(),
        texte(ChampTexte::Artiste, "Miles Davis")
    );
    assert_eq!(
        analyser("title:=\"So What\"").unwrap(),
        Expr::Condition {
            champ: Champ::Texte(ChampTexte::Titre),
            test: Test::Est("So What".into()),
        }
    );
    let nombre = |c, t| Expr::Condition {
        champ: Champ::Nombre(c),
        test: t,
    };
    assert_eq!(
        analyser("year:1960..1969").unwrap(),
        nombre(ChampNombre::Annee, Test::Plage(Some(1960), Some(1969)))
    );
    assert_eq!(
        analyser("year:1960..").unwrap(),
        nombre(ChampNombre::Annee, Test::Plage(Some(1960), None))
    );
    assert_eq!(
        analyser("rating:>=4").unwrap(),
        nombre(ChampNombre::Note, Test::Compare(Comparaison::PlusOuEgal, 4))
    );
    assert_eq!(
        analyser("sample_rate:44.1k").unwrap(),
        nombre(
            ChampNombre::Frequence,
            Test::Compare(Comparaison::Egal, 44_100)
        )
    );
    assert_eq!(
        analyser("composer:bach").unwrap(),
        Expr::Condition {
            champ: Champ::Role(Role::Compositeur),
            test: Test::Contient("bach".into()),
        }
    );
    assert_eq!(
        analyser("loved").unwrap(),
        Expr::Condition {
            champ: Champ::Aime,
            test: Test::Booleen(true),
        }
    );
    assert_eq!(
        analyser("loved:=false").unwrap(),
        Expr::Condition {
            champ: Champ::Aime,
            test: Test::Booleen(false),
        }
    );
    // Un nom de champ ne commence pas par un chiffre : `12:30` est un terme.
    assert_eq!(analyser("12:30").unwrap(), terme("12:30"));
}

/// (requête, position attendue, morceau du « attendu »).
#[test]
fn une_requete_mal_formee_dit_ou_et_quoi_7848() {
    let cas: &[(&str, usize, &str)] = &[
        ("", 0, "une condition"),
        ("   ", 0, "une condition"),
        ("artist:", 7, "une valeur"),
        ("(a OR b", 7, ")"),
        ("a )", 2, "la fin"),
        ("ratng:5", 0, "rating"),
        ("year:19x0", 5, "nombre"),
        ("year:1960..19x", 11, "nombre"),
        ("year:..", 5, "borne"),
        ("\"abc", 0, "\""),
        ("a OR", 4, "une condition"),
        ("OR a", 0, "une condition"),
        ("title:>abc", 6, ":="),
        ("loved:peut-etre", 6, "true"),
        ("loved:>true", 7, "true"),
        ("year:\"1960\"", 5, "nombre"),
    ];
    for (requete, position, attendu) in cas {
        let e = analyser(requete).expect_err(requete);
        assert_eq!(e.position, *position, "position pour {requete:?} : {e}");
        assert!(
            e.attendu.contains(attendu),
            "attendu pour {requete:?} : {e}"
        );
    }
    let ouvrantes = format!("{}a{}", "(".repeat(40), ")".repeat(40));
    assert!(
        analyser(&ouvrantes)
            .unwrap_err()
            .message
            .contains("imbrication")
    );
    assert!(
        analyser(&"a ".repeat(1_500))
            .unwrap_err()
            .message
            .contains("trop longue")
    );
    assert!(
        analyser(&"a ".repeat(150))
            .unwrap_err()
            .message
            .contains("conditions")
    );
}

// ------------------------------------------------------------- compilateur

/// Aucune valeur tapée n'entre dans le texte SQL : elle est dans les
/// paramètres, et seulement là.
#[test]
fn aucune_valeur_n_entre_dans_le_sql_7848() {
    let piege = "x'; DROP TABLE tracks; --";
    let requete = format!(
        "title:\"{piege}\" OR composer:\"{piege}\" OR \"{piege}\" OR genre:=\"{piege}\" loved"
    );
    for moteur in [Engine::Sqlite, Engine::Postgres] {
        let c = compiler(
            &analyser(&requete).unwrap(),
            Contexte {
                moteur,
                profil: PROFIL,
            },
        );
        assert!(!c.sql.contains("DROP"), "{}", c.sql);
        assert!(!c.sql.contains("x'"), "{}", c.sql);
        assert!(
            !c.sql.contains("7848"),
            "le profil aussi est lié : {}",
            c.sql
        );
        assert_eq!(c.sql.matches('?').count(), c.parametres.len(), "{}", c.sql);
        assert!(
            c.parametres
                .iter()
                .any(|p| p.as_str().is_some_and(|s| s.contains("DROP TABLE"))),
            "la valeur est liée"
        );
    }
}

/// Un rôle se compile en EXISTS sur `track_credits`, avec le rôle lié.
#[test]
fn un_role_devient_un_exists_sur_les_credits_7848() {
    let c = compiler(
        &analyser("conductor:karajan").unwrap(),
        Contexte {
            moteur: Engine::Sqlite,
            profil: 1,
        },
    );
    assert!(
        c.sql
            .starts_with("EXISTS (SELECT 1 FROM track_credits tc WHERE tc.track_id = t.id"),
        "{}",
        c.sql
    );
    assert_eq!(c.parametres[0].as_str(), Some("conductor"));
    assert_eq!(c.parametres[1].as_str(), Some("%karajan%"));
}

// --------------------------------------------- jeu d'essai, les deux bases

fn inserer(db: &dyn DbBackend, sql: &str, p: &[SqlValue]) -> i64 {
    let liens: Vec<&dyn ToSqlValue> = p.iter().map(|v| v as &dyn ToSqlValue).collect();
    db.execute_returning_id(sql, &liens)
        .unwrap_or_else(|e| panic!("{sql} : {e}"))
}

fn t(s: &str) -> SqlValue {
    SqlValue::Text(s.to_string())
}

fn i(n: i64) -> SqlValue {
    SqlValue::Int(n)
}

/// Le jeu de pistes et de crédits, identique sur les deux moteurs. Les
/// chemins commencent par `/q7848/` pour isoler le jeu sur une base partagée.
fn remplir(db: &dyn DbBackend) {
    let artiste = |nom: &str| inserer(db, "INSERT INTO artists (name) VALUES (?)", &[t(nom)]);
    let davis = artiste("Miles Davis (q7848)");
    let bjork = artiste("Björk (q7848)");
    let berlin = artiste("Berliner Philharmoniker (q7848)");
    let album = |titre: &str, artiste: i64, annee: i64| {
        inserer(
            db,
            "INSERT INTO albums (title, artist_id, year) VALUES (?, ?, ?)",
            &[t(titre), i(artiste), i(annee)],
        )
    };
    let kind = album("Kind of Blue (q7848)", davis, 1959);
    let homogenic = album("Homogenic (q7848)", bjork, 1997);
    let cinquieme = album("Symphonie n° 5 (q7848)", berlin, 1963);
    let mut n = 0;
    let mut piste = |titre: &str,
                     album: i64,
                     artiste: i64,
                     format: &str,
                     frequence: i64,
                     bits: i64,
                     annee: i64,
                     genre: Option<&str>,
                     duree_s: i64,
                     note: Option<i64>| {
        n += 1;
        inserer(
            db,
            "INSERT INTO tracks (title, album_id, artist_id, file_path, format, sample_rate, \
             bit_depth, year, genre, duration_ms, rating) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                t(titre),
                i(album),
                i(artiste),
                t(&format!("/q7848/{n}.flac")),
                t(format),
                i(frequence),
                i(bits),
                i(annee),
                genre.map(t).unwrap_or(SqlValue::NullText),
                i(duree_s * 1000),
                note.map(i).unwrap_or(SqlValue::NullInt),
            ],
        )
    };
    let so_what = piste(
        "So What",
        kind,
        davis,
        "FLAC",
        44_100,
        16,
        1959,
        Some("Jazz"),
        562,
        None,
    );
    let blue = piste(
        "Blue in Green",
        kind,
        davis,
        "FLAC",
        96_000,
        24,
        1959,
        Some("Jazz"),
        337,
        None,
    );
    let joga = piste(
        "Jóga",
        homogenic,
        bjork,
        "MP3",
        44_100,
        16,
        1997,
        Some("Electronic"),
        305,
        Some(3),
    );
    let allegro = piste(
        "Allegro con brio",
        cinquieme,
        berlin,
        "FLAC",
        96_000,
        24,
        1963,
        None,
        450,
        None,
    );
    let andante = piste(
        "Andante con moto",
        cinquieme,
        berlin,
        "FLAC",
        44_100,
        16,
        1963,
        Some("Classical"),
        600,
        None,
    );
    piste(
        "It's 100% ; DROP TABLE tracks; --",
        homogenic,
        bjork,
        "MP3",
        44_100,
        16,
        1997,
        Some("Electronic"),
        200,
        None,
    );
    let credit = |piste: i64, nom: &str, role: &str| {
        inserer(
            db,
            "INSERT INTO track_credits (track_id, artist_name, role) VALUES (?, ?, ?)",
            &[i(piste), t(nom), t(role)],
        )
    };
    credit(allegro, "Ludwig van Beethoven", "composer");
    credit(allegro, "Herbert von Karajan", "conductor");
    credit(andante, "Ludwig van Beethoven", "composer");
    credit(andante, "Karl Böhm", "conductor");
    credit(so_what, "John Coltrane", "performer");
    credit(so_what, "Miles Davis", "composer");
    credit(joga, "Sjón", "writer");
    credit(joga, "Mark Bell", "producer");
    inserer(
        db,
        "INSERT INTO album_ratings (album_id, profile_id, rating) VALUES (?, ?, 5)",
        &[i(kind), i(PROFIL)],
    );
    inserer(
        db,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (?, 'track', ?)",
        &[i(PROFIL), i(blue)],
    );
}

/// Les titres, triés en Rust (la collation des deux moteurs diffère).
fn titres(db: &dyn DbBackend, requete: &str) -> Vec<String> {
    let expr = analyser(requete).unwrap_or_else(|e| panic!("{requete:?} : {e}"));
    let c = compiler(
        &expr,
        Contexte {
            moteur: db.engine(),
            profil: PROFIL,
        },
    );
    let sql = format!(
        "SELECT t.title FROM {FROM_DES_PISTES} WHERE t.file_path LIKE '/q7848/%' AND {}",
        c.sql
    );
    let mut v: Vec<String> = db
        .query_many(&sql, &c.liens())
        .unwrap_or_else(|e| panic!("{requete:?} → {sql} : {e}"))
        .iter()
        .filter_map(|l| l[0].as_string())
        .collect();
    v.sort();
    v
}

/// (requête, titres attendus, triés).
const ATTENDUS: &[(&str, &[&str])] = &[
    ("blue", &["Blue in Green", "So What"]),
    ("\"blue in\"", &["Blue in Green"]),
    (
        "artist:bjork",
        &["It's 100% ; DROP TABLE tracks; --", "Jóga"],
    ),
    ("JOGA", &["Jóga"]),
    (
        "composer:beethoven conductor:karajan",
        &["Allegro con brio"],
    ),
    (
        "composer:beethoven -conductor:karajan",
        &["Andante con moto"],
    ),
    ("conductor:BOHM", &["Andante con moto"]),
    ("composer:karajan", &[]),
    ("performer:coltrane", &["So What"]),
    ("lyricist:sjon", &["Jóga"]),
    ("producer:=\"mark bell\"", &["Jóga"]),
    ("producer:=mark", &[]),
    ("year:1960..1969", &["Allegro con brio", "Andante con moto"]),
    ("year:<1960", &["Blue in Green", "So What"]),
    (
        "sample_rate:>=96k bit_depth:24",
        &["Allegro con brio", "Blue in Green"],
    ),
    ("rating:>=4", &["Blue in Green", "So What"]),
    ("rating:3", &["Jóga"]),
    ("loved", &["Blue in Green"]),
    ("loved:false artist:davis", &["So What"]),
    (
        "-genre:jazz",
        &[
            "Allegro con brio",
            "Andante con moto",
            "It's 100% ; DROP TABLE tracks; --",
            "Jóga",
        ],
    ),
    (
        "(artist:davis OR artist:bjork) format:flac",
        &["Blue in Green", "So What"],
    ),
    (
        "NOT (artist:davis OR artist:bjork)",
        &["Allegro con brio", "Andante con moto"],
    ),
    ("title:=\"so what\"", &["So What"]),
    ("duration:>500", &["Andante con moto", "So What"]),
    (
        "album:\"symphonie n° 5\" duration:400..500",
        &["Allegro con brio"],
    ),
    // Injection et jokers : des valeurs, rien d'autre.
    ("title:\"it's\"", &["It's 100% ; DROP TABLE tracks; --"]),
    ("title:\"'; DROP TABLE tracks; --\"", &[]),
    (
        "title:\"; DROP TABLE tracks; --\"",
        &["It's 100% ; DROP TABLE tracks; --"],
    ),
    ("title:100%", &["It's 100% ; DROP TABLE tracks; --"]),
    ("genre:%", &[]),
    ("title:_", &[]),
];

fn scenario(db: &dyn DbBackend) {
    remplir(db);
    let mut ecarts = Vec::new();
    for (requete, attendu) in ATTENDUS {
        let obtenu = titres(db, requete);
        if obtenu != *attendu {
            ecarts.push(format!(
                "{requete:?} : attendu {attendu:?}, obtenu {obtenu:?}"
            ));
        }
    }
    assert!(
        ecarts.is_empty(),
        "{:?} :\n{}",
        db.engine(),
        ecarts.join("\n")
    );
    // La table a survécu aux valeurs piégées.
    assert_eq!(titres(db, "artist:davis").len(), 2);
}

fn sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn le_compilateur_rend_les_pistes_attendues_sqlite_7848() {
    scenario(sqlite().as_ref());
}

/// Le même jeu, les mêmes réponses, sur PostgreSQL (`TUNE_TEST_PG_URL`,
/// schéma des scripts numérotés). SAUT sans URL.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7848_le_compilateur_rend_les_pistes_attendues() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pool = sqlx::PgPool::connect(&url)
        .await
        .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée ({url}) mais injoignable : {e}"));
    let b: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool));
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = b.execute(sql, &[]);
    }
    // Le banc est partagé : seul le jeu de ce test est effacé.
    for sql in [
        "DELETE FROM track_credits WHERE track_id IN (SELECT id FROM tracks WHERE file_path LIKE '/q7848/%')",
        "DELETE FROM favorites WHERE profile_id = 7848",
        "DELETE FROM album_ratings WHERE profile_id = 7848",
        "DELETE FROM tracks WHERE file_path LIKE '/q7848/%'",
        "DELETE FROM albums WHERE title LIKE '%(q7848)'",
        "DELETE FROM artists WHERE name LIKE '%(q7848)'",
    ] {
        b.execute(sql, &[]).unwrap_or_else(|e| {
            panic!(
                "{sql} : {e} (le schéma vient des scripts numérotés : appliquer \
                 tune-core/migrations/postgres/*.sql, comme l'étape « Apply PG \
                 migrations » de test-postgres.yml)"
            )
        });
    }
    scenario(b.as_ref());
}

/// Une limite négative veut dire « sans limite » en SQLite : elle est
/// ramenée à 0, et une limite trop grande à `LIMITE_MAX`.
#[test]
fn la_limite_de_page_est_bornee_7848() {
    let db = sqlite();
    db.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1500)
         INSERT INTO tracks (title, file_path, format, year)
         SELECT 'Borne ' || i, '/borne/' || i || '.flac', 'FLAC', 1970 FROM n;",
    )
    .unwrap();
    let expr = analyser("title:borne").unwrap();
    assert_eq!(nombre_de_pistes(db.as_ref(), &expr, 1).unwrap(), 1500);
    let page = |limite| {
        pistes_correspondantes(db.as_ref(), &expr, 1, limite, 0)
            .unwrap()
            .len() as i64
    };
    assert_eq!(page(-1), 0, "une limite négative ne rend pas tout");
    assert_eq!(page(0), 0);
    assert_eq!(page(10), 10);
    assert_eq!(page(i64::MAX), LIMITE_MAX);
}

// --------------------------------------------------------------------- banc

/// Banc : 100 000 pistes, 200 000 crédits, une requête à deux EXISTS dont
/// un sous une négation imbriquée dans un OU. Le temps est imprimé
/// (`--nocapture`) ; le plafond n'est qu'un garde-fou contre un parcours
/// quadratique, pas une mesure.
#[test]
fn banc_deux_exists_sur_100_000_pistes_7848() {
    let db = sqlite();
    db.execute_batch(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 100000)
         INSERT INTO tracks (title, file_path, format, sample_rate, bit_depth, year)
         SELECT 'Piste ' || i, '/banc/' || i || '.flac', 'FLAC', 44100, 16, 1950 + (i % 70) FROM n;
         INSERT INTO track_credits (track_id, artist_name, role)
         SELECT id, 'Compositeur ' || (id % 500), 'composer' FROM tracks;
         INSERT INTO track_credits (track_id, artist_name, role)
         SELECT id, 'Chef ' || (id % 300), 'conductor' FROM tracks;",
    )
    .unwrap();
    let expr = analyser(
        "composer:=\"compositeur 42\" (conductor:=\"chef 42\" OR -conductor:\"chef 1\") year:1960..1999",
    )
    .unwrap();
    let debut = Instant::now();
    let total = nombre_de_pistes(db.as_ref(), &expr, 1).unwrap();
    let page = pistes_correspondantes(db.as_ref(), &expr, 1, 50, 0).unwrap();
    let duree = debut.elapsed();
    eprintln!(
        "BANC #7848 : 100 000 pistes, deux EXISTS : {total} pistes, compte + page en {duree:?}"
    );
    // id % 500 = 42 → 200 pistes ; dont id % 300 = 42 (même reste modulo 1500)
    // ou un chef dont le nom ne contient pas « chef 1 ».
    assert!(total > 0 && total < 200, "{total}");
    assert_eq!(page.len().min(50) as i64, total.min(50));
    assert!(duree.as_secs() < 30, "banc trop lent : {duree:?}");
}
