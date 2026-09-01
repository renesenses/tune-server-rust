//! Le frère de `pg_schema_parity` : lui vérifie qu'une colonne EXISTE des deux
//! côtés, celui-ci qu'elle a le même **type**.
//!
//! `pg_schema_parity` compare `PG_FULL_SCHEMA` aux migrations PostgreSQL. Les
//! deux peuvent s'accorder parfaitement et laisser une colonne en `TEXT` là où
//! SQLite la porte en `INTEGER` — c'est précisément ce qui a produit #2860 :
//! `listen_history.album_id` en TEXT contre `albums.id` en BIGINT, donc
//!
//! ```text
//! operator does not exist: text = bigint
//! ```
//!
//! avalé par un `unwrap_or_default()`, et deux sections d'accueil vides
//! pendant des mois. SQLite est typé dynamiquement et ne bronche jamais ;
//! PostgreSQL refuse, et il a raison.
//!
//! ## Les deux PostgreSQL qui existent réellement
//!
//! Un serveur PostgreSQL en service est né de l'une de deux façons, et **les
//! deux donnent des types différents** :
//!
//! | naissance | ce qui monte le schéma |
//! |---|---|
//! | installation PostgreSQL native | les scripts numérotés, puis `ensure_schema` |
//! | bascule depuis SQLite | `PG_FULL_SCHEMA` (tout en TEXT), puis les scripts, puis `ensure_schema` |
//!
//! Le second chemin part d'un schéma **délibérément tout-TEXT** — la copie de
//! données lie chaque valeur SQLite en paramètre texte, un type numérique y
//! ferait échouer l'INSERT et la bibliothèque arriverait vide. Les types
//! voulus sont rétablis **après** la copie par les migrations de rattrapage
//! (010, 011, 012, 013…). Une migration qui ajoute la colonne par
//! `ADD COLUMN IF NOT EXISTS … INTEGER` au lieu de la **convertir** est un
//! no-op sur ce chemin-là : la colonne reste TEXT pour toujours (migration 032
//! et les douze réglages de zone).
//!
//! On monte donc les DEUX, et on les compare tous les deux à SQLite. Une porte
//! qui n'en regarderait qu'un serait verte sur la moitié du parc.
//!
//! ## Pourquoi de vraies bases, et pas un parseur
//!
//! Même raison que pour le frère : un parseur qui ne reconnaît plus une forme
//! SQL rend moins de colonnes, donc moins d'écarts, donc un test **vert par
//! ignorance**. Ici c'est PostgreSQL et SQLite eux-mêmes qui répondent sur ce
//! qu'ils viennent de créer.
//!
//! ## Les écarts tolérés
//!
//! `ECARTS_TOLERES` liste les divergences **connues au 31/08/2026**, datées et
//! motivées. Elles ne sont pas approuvées : elles sont inventoriées, pour que
//! la porte morde sur les écarts NOUVEAUX sans bloquer le lot en cours. Une
//! conversion de type sur une base vivante est irréversible et se traite une
//! par une, avec sa contre-épreuve.
//!
//! Le test refuse aussi une exception **périmée** : le jour où une colonne est
//! convertie, sa ligne doit disparaître d'ici. Sans quoi la liste grossit et
//! finit par tout couvrir — la façon habituelle dont un garde-fou meurt.
//!
//! ## Comment il tourne
//!
//! Comme les autres tests PostgreSQL : sauté sans `TUNE_TEST_PG_URL`, donc
//! absent d'un `cargo test` par défaut. La CI le lance sur son PostgreSQL 16
//! (job `Test (PostgreSQL)`), **sauté par défaut vers `batch/*` et `rc/*` :
//! une PR qui touche au schéma doit porter `ci:full`**.

#![cfg(all(test, feature = "postgres"))]

use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Connection, PgConnection, Row};

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;

/// Les deux bases jetables, recréées à chaque exécution.
const BASE_NATIVE: &str = "tune_types_native";
const BASE_MIGREE: &str = "tune_types_migree";

/// Famille de types : ce que la comparaison considère comme « le même type ».
///
/// SQLite n'a que cinq classes d'affinité et PostgreSQL une trentaine de
/// types ; exiger l'égalité littérale n'aurait aucun sens (`INTEGER` SQLite
/// est un entier 64 bits, il s'écrit `BIGINT`, `INTEGER` ou `SMALLINT` selon
/// la colonne côté PG, et les trois sont corrects). Ce qui casse une requête
/// est le passage d'une famille à l'autre — c'est donc la famille qu'on
/// compare.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Famille {
    Entier,
    Flottant,
    Texte,
    Binaire,
    Booleen,
    Temps,
    Autre,
}

/// Type SQLite déclaré → famille. On lit le type tel qu'il est écrit dans le
/// `CREATE TABLE` (c'est ce que rend `pragma_table_info`), pas l'affinité
/// calculée : la déclaration est l'intention de l'auteur, et c'est elle qu'on
/// veut voir respectée côté PostgreSQL.
fn famille_sqlite(t: &str) -> Famille {
    let t = t.trim().to_ascii_uppercase();
    if t.starts_with("INT") || t.starts_with("BIGINT") || t.starts_with("SMALLINT") {
        Famille::Entier
    } else if t.starts_with("REAL") || t.starts_with("DOUBLE") || t.starts_with("FLOAT") {
        Famille::Flottant
    } else if t.starts_with("BOOL") {
        Famille::Booleen
    } else if t.starts_with("BLOB") {
        Famille::Binaire
    } else if t.is_empty() {
        // Colonne sans type déclaré : affinité BLOB en SQLite, aucune
        // intention exprimée. Rien à exiger côté PostgreSQL.
        Famille::Autre
    } else {
        // TEXT, VARCHAR, NUMERIC, DATETIME… tout le reste est stocké et lu en
        // texte par ce code.
        Famille::Texte
    }
}

/// `information_schema.data_type` PostgreSQL → famille.
fn famille_pg(t: &str) -> Famille {
    match t {
        "smallint" | "integer" | "bigint" => Famille::Entier,
        "real" | "double precision" | "numeric" => Famille::Flottant,
        "text" | "character varying" | "character" => Famille::Texte,
        "bytea" => Famille::Binaire,
        "boolean" => Famille::Booleen,
        t if t.starts_with("timestamp") || t == "date" || t.starts_with("time") => Famille::Temps,
        _ => Famille::Autre,
    }
}

/// Les écarts de type PG/SQLite constatés le **31/08/2026** sur le lot
/// `batch/p2-recentes-1`, mesurés par ce test lui-même.
///
/// `(base, table, colonne, motif)` où `base` vaut `"native"` (installation
/// PostgreSQL directe) ou `"migree"` (bascule SQLite→PG).
///
/// ⚠️ Ces lignes ne disent pas « c'est bien ». Elles disent « c'est connu, daté,
/// et ça se traite une par une ». Convertir un type sur une base vivante est
/// irréversible : chaque conversion demande sa migration numérotée idempotente,
/// sa garde `IF NOT EXISTS`, et sa contre-épreuve (la requête qui échouait doit
/// échouer AVANT et réussir APRÈS).
///
/// Le classement par danger est dans #2995.
const ECARTS_TOLERES: &[(&str, &str, &str, &str)] = &[
    // ── Base NATIVE ────────────────────────────────────────────────────────
    // `listen_history.profile_id` et `playlists.profile_id` étaient ici : elles
    // sont CONVERTIES par cette PR (migration 049 + `ENSURE_COLUMNS` en
    // BIGINT), parce qu'elles sont les deux seules du lot dont la comparaison
    // à un entier est prouvée dans le code. Leur absence de cette liste est
    // volontaire.
    //
    // `zones.is_hidden` n'est visée par aucune migration de rattrapage, sur
    // aucun des deux chemins : elle n'existe que dans `ENSURE_COLUMNS`, en
    // TEXT DEFAULT '0'.
    (
        "native",
        "zones",
        "is_hidden",
        "TEXT vs INTEGER — posée seulement par ENSURE_COLUMNS, aucune conversion (#2995)",
    ),
    // ── Base MIGRÉE (bascule SQLite→PG) ────────────────────────────────────
    // La migration 032 AJOUTE ces réglages en INTEGER au lieu de les
    // CONVERTIR : sur une base migrée `PG_FULL_SCHEMA` les a déjà posés en
    // TEXT, l'`ADD COLUMN IF NOT EXISTS` est un no-op, et elles restent TEXT.
    // `COALESCE(<col>, 0)` y rend « COALESCE types text and integer cannot be
    // matched ». #2995.
    (
        "migree",
        "zones",
        "aac_passthrough",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "alac_passthrough",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "autoplay_enabled",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "dlna_native_flac",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "dlna_lpcm",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "dlna_cap_16bit",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "dlna_wav24",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "dlna_play_delay_ms",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "fixed_volume",
        "TEXT vs INTEGER — 032 ajoute au lieu de convertir (#2995)",
    ),
    (
        "migree",
        "zones",
        "gapless_enabled",
        "TEXT vs INTEGER — jamais convertie sur le chemin migré (#2995)",
    ),
    (
        "migree",
        "zones",
        "muted",
        "TEXT vs INTEGER — jamais convertie sur le chemin migré (#2995)",
    ),
    (
        "migree",
        "zones",
        "online",
        "TEXT vs INTEGER — jamais convertie sur le chemin migré (#2995)",
    ),
    (
        "migree",
        "zones",
        "is_hidden",
        "TEXT vs INTEGER — posée seulement par ENSURE_COLUMNS, aucune conversion (#2995)",
    ),
    (
        "migree",
        "zones",
        "lyrics_offset_ms",
        "TEXT vs INTEGER — jamais convertie sur le chemin migré (#2995)",
    ),
    (
        "migree",
        "queue_items",
        "is_current",
        "TEXT vs INTEGER — jamais convertie sur le chemin migré (#2995)",
    ),
];

fn url_vers_base(url: &str, base: &str) -> String {
    let (avant, apres) = match url.split_once('?') {
        Some((a, q)) => (a, Some(q)),
        None => (url, None),
    };
    let racine = avant.rsplit_once('/').map(|(r, _)| r).unwrap_or(avant);
    match apres {
        Some(q) => format!("{racine}/{base}?{q}"),
        None => format!("{racine}/{base}"),
    }
}

async fn connexion(url: &str) -> PgConnection {
    PgConnection::connect(url)
        .await
        .unwrap_or_else(|e| panic!("connexion à {url} impossible : {e}"))
}

async fn base_vierge(maintenance: &mut PgConnection, nom: &str, url_racine: &str) -> PgConnection {
    // `AssertSqlSafe` : `nom` est l'une des deux constantes de ce fichier, et
    // PostgreSQL n'accepte de toute façon pas de paramètre lié en DDL.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {nom}"
    )))
    .execute(&mut *maintenance)
    .await
    .unwrap_or_else(|e| panic!("suppression de {nom} : {e}"));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {nom}")))
        .execute(&mut *maintenance)
        .await
        .unwrap_or_else(|e| panic!("création de {nom} : {e}"));

    let mut c = connexion(&url_vers_base(url_racine, nom)).await;
    sqlx::raw_sql("CREATE EXTENSION IF NOT EXISTS unaccent")
        .execute(&mut c)
        .await
        .unwrap_or_else(|e| panic!("extension unaccent sur {nom} : {e}"));
    c
}

/// Les scripts numérotés, dans l'ordre, puis le rattrapage d'`ensure_schema`.
///
/// C'est l'ordre de `connect()` vu depuis la SECONDE ouverture : au tout
/// premier démarrage `ensure_schema` passe avant, mais échoue sur les tables
/// qui n'existent pas encore. Ce qu'on modélise ici est l'état stable.
async fn scripts_puis_rattrapage(c: &mut PgConnection) {
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TIMESTAMPTZ DEFAULT now(),
            name TEXT NOT NULL
        )",
    )
    .execute(&mut *c)
    .await
    .expect("schema_version");
    for (version, nom, sql) in crate::db::migrations::PG_MIGRATIONS {
        sqlx::raw_sql(*sql)
            .execute(&mut *c)
            .await
            .unwrap_or_else(|e| panic!("migration {version:03}_{nom} : {e}"));
    }
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        // Best-effort, exactement comme `run_each` : un ALTER sur une table
        // absente se journalise et n'arrête pas les suivants.
        let _ = sqlx::raw_sql(*sql).execute(&mut *c).await;
    }
}

/// `data_type` de chaque colonne du schéma `public`, groupé par table.
async fn types_pg(c: &mut PgConnection) -> BTreeMap<String, BTreeMap<String, String>> {
    let lignes = sqlx::query(
        "SELECT table_name, column_name, data_type \
         FROM information_schema.columns \
         WHERE table_schema = 'public'",
    )
    .fetch_all(c)
    .await
    .expect("lecture d'information_schema");

    let mut par_table: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for l in lignes {
        let table: String = l.get("table_name");
        let colonne: String = l.get("column_name");
        let t: String = l.get("data_type");
        par_table.entry(table).or_default().insert(colonne, t);
    }
    par_table
}

/// Le schéma SQLite complet : `CORE_SCHEMA` **puis** toutes les migrations.
///
/// `CORE_SCHEMA` seul ne porte que treize tables ; le comparer tel quel
/// laisserait hors de portée tout ce que les migrations ajoutent — dont
/// `listen_history`, la table de #2860.
fn types_sqlite() -> BTreeMap<String, BTreeMap<String, String>> {
    let db = SqliteDb::open_in_memory().expect("sqlite en mémoire");
    db.init_schema().expect("CORE_SCHEMA");
    run_migrations(&db).expect("migrations SQLite");

    let conn = db.connection().lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT m.name, p.name, p.type \
             FROM sqlite_master m JOIN pragma_table_info(m.name) p \
             WHERE m.type = 'table'",
        )
        .expect("lecture de pragma_table_info");
    let mut par_table: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let lignes = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .expect("query_map");
    for l in lignes {
        let (table, colonne, t) = l.expect("ligne");
        par_table.entry(table).or_default().insert(colonne, t);
    }
    par_table
}

/// Compare une base PostgreSQL montée à SQLite, et rend les écarts de famille.
///
/// Ne compare que les tables ET colonnes présentes des deux côtés :
/// l'existence est le sujet de `pg_schema_parity` et du garde-fou
/// `toute_colonne_sqlite_a_sa_migration_postgres`, pas celui-ci.
fn ecarts(
    quelle_base: &str,
    pg: &BTreeMap<String, BTreeMap<String, String>>,
    sqlite: &BTreeMap<String, BTreeMap<String, String>>,
) -> Vec<String> {
    let mut v = Vec::new();
    for (table, colonnes_sqlite) in sqlite {
        let Some(colonnes_pg) = pg.get(table) else {
            continue;
        };
        for (colonne, type_sqlite) in colonnes_sqlite {
            let Some(type_pg) = colonnes_pg.get(colonne) else {
                continue;
            };
            let (fs, fp) = (famille_sqlite(type_sqlite), famille_pg(type_pg));
            // `Autre` des deux côtés = aucune intention exprimée, rien à
            // exiger. Un `Temps` PG contre un `Texte` SQLite est légitime :
            // plusieurs colonnes d'horodatage sont TIMESTAMPTZ côté PG et
            // ISO-8601 texte côté SQLite, et le code lit les deux en chaîne.
            if fs == fp
                || fs == Famille::Autre
                || fp == Famille::Autre
                || (fs == Famille::Texte && fp == Famille::Temps)
                || (fs == Famille::Entier && fp == Famille::Booleen)
            {
                continue;
            }
            if ECARTS_TOLERES
                .iter()
                .any(|(b, t, c, _)| *b == quelle_base && t == table && c == colonne)
            {
                continue;
            }
            v.push(format!(
                "  [{quelle_base}] {table}.{colonne} : SQLite {type_sqlite} ({fs:?}) vs PostgreSQL {type_pg} ({fp:?})"
            ));
        }
    }
    v
}

/// Un type qui diverge entre PostgreSQL et SQLite est une requête qui marche
/// en développement et échoue en production, sans une ligne de journal.
#[tokio::test]
async fn parite_des_types_pg_sqlite() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absent — parite_des_types_pg_sqlite sautée");
        return;
    };

    let sqlite = types_sqlite();

    let mut maintenance = connexion(&url).await;

    // Base 1 — installation PostgreSQL native : les scripts, puis le
    // rattrapage.
    let mut native = base_vierge(&mut maintenance, BASE_NATIVE, &url).await;
    scripts_puis_rattrapage(&mut native).await;

    // Base 2 — bascule SQLite→PG : le schéma tout-TEXT de la copie, puis les
    // mêmes scripts et le même rattrapage. Sans elle, la porte serait aveugle
    // à la moitié du parc (c'est ce chemin qui laisse les réglages de zone en
    // TEXT).
    let mut migree = base_vierge(&mut maintenance, BASE_MIGREE, &url).await;
    sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
        .execute(&mut migree)
        .await
        .expect("PG_FULL_SCHEMA");
    scripts_puis_rattrapage(&mut migree).await;

    let types_native = types_pg(&mut native).await;
    let types_migree = types_pg(&mut migree).await;

    // Garde-fou du garde-fou : si l'une des lectures rendait peu de choses —
    // mauvaise base, schéma vide, requête muette — la comparaison serait vide
    // et le test vert sans avoir rien vérifié.
    for (nom, m) in [
        ("SQLite", &sqlite),
        ("PG natif", &types_native),
        ("PG migré", &types_migree),
    ] {
        assert!(
            m.get("tracks").is_some_and(|c| c.len() > 20),
            "{nom} n'a pas monté `tracks` — le test ne vérifiait rien"
        );
    }

    let mut tous = ecarts("native", &types_native, &sqlite);
    tous.extend(ecarts("migree", &types_migree, &sqlite));

    assert!(
        tous.is_empty(),
        "Colonnes dont le type DIVERGE entre PostgreSQL et SQLite :\n{}\n\n\
         SQLite est typé dynamiquement et ne bronchera jamais ; PostgreSQL rend \
         `operator does not exist: text = bigint` ou `COALESCE types text and \
         integer cannot be matched`, souvent avalé par un `unwrap_or_default()` \
         (#2860, #2995).\n\n\
         Une colonne s'ajoute — et se TYPE — aux QUATRE endroits : `CORE_SCHEMA` \
         (db/sqlite.rs), migration SQLite (db/migrations.rs), `PG_FULL_SCHEMA` \
         (db/pg_migrate.rs) et migration PG (migrations/postgres/NNN_….sql + \
         `PG_MIGRATIONS`).\n\
         ⚠️ Sur le chemin MIGRÉ, `PG_FULL_SCHEMA` pose tout en TEXT : un \
         `ADD COLUMN IF NOT EXISTS … INTEGER` y est un NO-OP. Il faut une \
         conversion gardée (`ALTER … TYPE … USING`), sur le modèle de la \
         migration 013.\n\
         Si l'écart est délibéré et doit attendre, l'inscrire dans \
         `ECARTS_TOLERES` avec sa date et son motif — jamais en silence.",
        tous.join("\n")
    );
}

/// Une exception qui ne correspond plus à rien doit disparaître.
///
/// C'est la façon habituelle dont un garde-fou meurt : la liste d'exceptions
/// grossit, personne ne l'élague, et elle finit par couvrir tout ce qu'elle
/// devait surveiller. Le jour où une colonne est convertie, sa ligne s'en va.
#[tokio::test]
async fn aucune_exception_perimee() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absent — aucune_exception_perimee sautée");
        return;
    };

    let sqlite = types_sqlite();
    let mut maintenance = connexion(&url).await;

    // Bases distinctes de celles du test précédent : les deux tournent en
    // parallèle dans le même binaire de test, et se partager une base ferait
    // dépendre le résultat de l'ordonnancement.
    let mut native = base_vierge(&mut maintenance, "tune_types_native_bis", &url).await;
    scripts_puis_rattrapage(&mut native).await;
    let mut migree = base_vierge(&mut maintenance, "tune_types_migree_bis", &url).await;
    sqlx::raw_sql(crate::db::pg_migrate::PG_FULL_SCHEMA)
        .execute(&mut migree)
        .await
        .expect("PG_FULL_SCHEMA");
    scripts_puis_rattrapage(&mut migree).await;

    let types_native = types_pg(&mut native).await;
    let types_migree = types_pg(&mut migree).await;

    let mut perimees: Vec<String> = Vec::new();
    for (base, table, colonne, motif) in ECARTS_TOLERES {
        let pg = match *base {
            "native" => &types_native,
            "migree" => &types_migree,
            autre => panic!("ECARTS_TOLERES : base inconnue `{autre}` pour {table}.{colonne}"),
        };
        let type_pg = pg.get(*table).and_then(|c| c.get(*colonne));
        let type_sqlite = sqlite.get(*table).and_then(|c| c.get(*colonne));
        let diverge = match (type_sqlite, type_pg) {
            (Some(s), Some(p)) => famille_sqlite(s) != famille_pg(p),
            _ => false,
        };
        if !diverge {
            perimees.push(format!("  [{base}] {table}.{colonne} — {motif}"));
        }
    }

    assert!(
        perimees.is_empty(),
        "Exceptions périmées dans `ECARTS_TOLERES` — la colonne ne diverge plus \
         (ou n'existe plus des deux côtés). Les retirer :\n{}",
        perimees.join("\n")
    );
}

/// Les familles sont le cœur de la comparaison : une erreur ici rendrait le
/// test vert sur un écart réel.
#[test]
fn les_familles_de_types_sont_bien_classees() {
    assert_eq!(famille_sqlite("INTEGER"), Famille::Entier);
    assert_eq!(famille_sqlite("integer"), Famille::Entier);
    assert_eq!(famille_sqlite("BIGINT"), Famille::Entier);
    assert_eq!(famille_sqlite("REAL"), Famille::Flottant);
    assert_eq!(famille_sqlite("TEXT"), Famille::Texte);
    assert_eq!(famille_sqlite("BLOB"), Famille::Binaire);
    assert_eq!(famille_sqlite(""), Famille::Autre);

    assert_eq!(famille_pg("bigint"), Famille::Entier);
    assert_eq!(famille_pg("smallint"), Famille::Entier);
    assert_eq!(famille_pg("double precision"), Famille::Flottant);
    assert_eq!(famille_pg("text"), Famille::Texte);
    assert_eq!(famille_pg("character varying"), Famille::Texte);
    assert_eq!(famille_pg("timestamp with time zone"), Famille::Temps);

    // Le couple exact de #2860 : SQLite INTEGER contre PostgreSQL TEXT.
    assert_ne!(famille_sqlite("INTEGER"), famille_pg("text"));
}

/// L'inventaire doit rester lisible : ni doublon, ni base inconnue, ni motif
/// vide. Une exception sans motif est une exception que personne ne pourra
/// retirer.
#[test]
fn l_inventaire_des_ecarts_toleres_est_propre() {
    let mut vus: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
    for (base, table, colonne, motif) in ECARTS_TOLERES {
        assert!(
            matches!(*base, "native" | "migree"),
            "base inconnue `{base}` pour {table}.{colonne}"
        );
        assert!(
            !motif.trim().is_empty(),
            "{base}/{table}.{colonne} n'a pas de motif"
        );
        assert!(
            vus.insert((base, table, colonne)),
            "doublon dans ECARTS_TOLERES : [{base}] {table}.{colonne}"
        );
    }
    // Le compte relevé le 31/08/2026. Le faire bouger sans le dire ici serait
    // exactement l'affaissement silencieux que ce garde-fou combat.
    assert_eq!(
        ECARTS_TOLERES.len(),
        16,
        // 1 côté natif (`zones.is_hidden`), 15 côté migré. Compte MESURÉ par
        // `parite_des_types_pg_sqlite` et `aucune_exception_perimee` sur le
        // PostgreSQL 16 de la CI, pas estimé à la lecture des sources.
        "le nombre d'écarts tolérés a changé — mettre à jour ce compte ET #2995"
    );
}

#[test]
fn le_nom_de_base_est_remplace_dans_l_url() {
    assert_eq!(
        url_vers_base("postgresql://tune:tune@localhost:5432/tune_test", "essai"),
        "postgresql://tune:tune@localhost:5432/essai"
    );
    assert_eq!(
        url_vers_base(
            "postgresql://tune:tune@localhost:5432/tune_test?sslmode=disable",
            "essai"
        ),
        "postgresql://tune:tune@localhost:5432/essai?sslmode=disable"
    );
}
