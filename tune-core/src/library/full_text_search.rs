//! Full-text search — SQLite-only implementation using FTS5 virtual tables.
//!
//! The FTS5 tables are **contentless** (`content=''`) with triggers that
//! keep them in sync with the source tables. This works perfectly when
//! all writes go through the Tune server (the triggers fire on every
//! INSERT/UPDATE/DELETE). However, if a user edits the SQLite database
//! directly (e.g. via `sqlite3` CLI or DB Browser), the triggers may not
//! fire or the FTS content may drift. The `rebuild_fts_contentless`
//! function handles this by deleting all FTS rows and re-inserting from
//! the source tables.
//!
//! Phase 4 of the PostgreSQL support roadmap will introduce a parallel
//! module (or trait split) that targets PostgreSQL: tsvector columns
//! materialised on the source tables, GIN indexes, and `@@ to_tsquery`
//! search predicates. The repos' search() methods will then call
//! `dialect.fts_match(column, placeholder)` to emit the right clause
//! for whichever engine is active.
//!
//! See docs/POSTGRES-PLAN.md.

use rusqlite::Connection;
use tracing::{info, warn};

const FTS_TABLES: &[(&str, &[&str])] = &[
    (
        "tracks",
        &[
            "title",
            "artist_name",
            "album_title",
            "genre",
            "composer",
            COLONNE_TERMES_DE_CHEMIN,
        ],
    ),
    ("albums", &["title", "artist_name", "genre"]),
    ("artists", &["name", "sort_name"]),
];

// ─── Termes de chemin (#5192) ────────────────────────────────────────────
//
// « Tout le monde n'a pas taggé sa bibliothèque » (fil 1966) : un dossier
// « Mahler Kondrashin » restait introuvable, par la recherche comme par le
// texte libre d'Oxygen, parce que le chemin n'était dans aucun champ indexé.
//
// Deux termes par piste, et deux seulement : le nom du DERNIER dossier et le
// nom du FICHIER sans son extension. Pas le chemin entier — `/Classique/
// Mahler/…` ferait remonter toute une arborescence sur « Mahler ». Les
// séparateurs `_`, `-` et `.` y deviennent des espaces, les suites d'espaces
// sont resserrées.
//
// UNE définition, écrite deux fois dans deux langues et tenue égale par
// l'épreuve `termes_de_chemin_rust_et_sql_disent_la_meme_chose` :
//  - [`sql_termes_de_chemin`], une expression SQL qui n'emploie que `REPLACE`,
//    `RTRIM`, `SUBSTR`, `LENGTH`, `TRIM`, `COALESCE` et `LIKE` — le MÊME texte
//    passe sous SQLite et sous PostgreSQL. Aucune fonction enregistrée depuis
//    Rust : les déclencheurs de `tracks_fts` s'en servent, et une base éditée
//    au `sqlite3` en ligne de commande (voir l'en-tête de ce module) doit
//    continuer d'accepter une écriture sur `tracks` ;
//  - [`termes_de_chemin`], la même chose en Rust, pour l'API : c'est la valeur
//    `path_terms` que rend `/library/tracks` et que le client web compare, au
//    lieu de refaire le découpage de son côté.

/// Colonne de `tracks_fts` qui porte les termes de chemin.
pub const COLONNE_TERMES_DE_CHEMIN: &str = "path_terms";

/// Nombre de passes `REPLACE('  ', ' ')` : une suite de `n` espaces en sort
/// longue de `ceil(n / 2^passes)`. Trois passes resserrent jusqu'à 8 espaces
/// d'affilée — « Mahler - Kondrashin » en fait 3. La version Rust fait les
/// MÊMES passes, pas un resserrement complet, pour rendre le même texte.
const PASSES_DE_RESSERREMENT: usize = 3;

/// Les termes de chemin d'une piste, calculés en Rust — le jumeau exact de
/// [`sql_termes_de_chemin`].
///
/// `chemin` est `file_path`, ou `cue_media_path` pour une piste virtuelle de
/// feuille CUE. Une adresse (`http://…`, `smb://…`) ne donne rien : ses
/// segments ne sont pas des noms que quelqu'un a choisis.
pub fn termes_de_chemin(chemin: Option<&str>) -> String {
    let p = chemin.unwrap_or("").replace('\\', "/");
    if p.contains("://") {
        return String::new();
    }
    let (repertoire, fichier) = match p.rfind('/') {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => ("", p.as_str()),
    };
    let parent = repertoire.trim_end_matches('/');
    let dernier_dossier = match parent.rfind('/') {
        Some(i) => &parent[i + 1..],
        None => parent,
    };
    let radical = match fichier.rfind('.') {
        Some(i) => &fichier[..i],
        None => fichier,
    };
    let mut s = format!("{dernier_dossier} {radical}").replace(['_', '-', '.'], " ");
    for _ in 0..PASSES_DE_RESSERREMENT {
        s = s.replace("  ", " ");
    }
    s.trim_matches(' ').to_string()
}

/// Tout ce qui précède le dernier `/` de `x`, `/` compris — `''` sans `/`.
///
/// `RTRIM(x, y)` retire à droite tout caractère présent dans `y` ; avec pour
/// `y` les caractères de `x` hors `/`, il retire exactement le dernier
/// segment.
fn sql_jusqu_au_dernier_slash(x: &str) -> String {
    format!("RTRIM({x}, REPLACE({x}, '/', ''))")
}

/// Le dernier segment de `x` (ce qui suit son dernier `/`).
fn sql_dernier_segment(x: &str) -> String {
    format!("SUBSTR({x}, LENGTH({}) + 1)", sql_jusqu_au_dernier_slash(x))
}

/// L'expression SQL des termes de chemin, pour une colonne (ou une
/// expression) `chemin` — voir [`termes_de_chemin`], qu'elle égale.
///
/// Même texte pour SQLite et PostgreSQL. Sans `INSTR` (absent de PG) ni
/// expression régulière (absente de SQLite).
pub fn sql_termes_de_chemin(chemin: &str) -> String {
    let p = format!("REPLACE(COALESCE({chemin}, ''), '\\', '/')");
    let fichier = sql_dernier_segment(&p);
    let parent = format!("RTRIM({}, '/')", sql_jusqu_au_dernier_slash(&p));
    let dernier_dossier = sql_dernier_segment(&parent);
    // Tout jusqu'au dernier `.` du fichier, point compris ; `''` sans point.
    let jusqu_au_point = format!("RTRIM({fichier}, REPLACE({fichier}, '.', ''))");
    let radical = format!(
        "(CASE WHEN {jusqu_au_point} = '' THEN {fichier} \
         ELSE SUBSTR({fichier}, 1, LENGTH({jusqu_au_point}) - 1) END)"
    );
    let mut s = format!(
        "REPLACE(REPLACE(REPLACE({dernier_dossier} || ' ' || {radical}, '_', ' '), '-', ' '), '.', ' ')"
    );
    for _ in 0..PASSES_DE_RESSERREMENT {
        s = format!("REPLACE({s}, '  ', ' ')");
    }
    format!("(CASE WHEN {p} LIKE '%://%' THEN '' ELSE TRIM({s}) END)")
}

/// Les termes de chemin d'une ligne `tracks` d'alias `alias` (`new`, `old`,
/// `t`…) : `file_path`, sinon le fichier image d'une piste CUE.
pub fn sql_termes_de_chemin_de_piste(alias: &str) -> String {
    sql_termes_de_chemin(&format!(
        "COALESCE({alias}.file_path, {alias}.cue_media_path)"
    ))
}

/// Nom de la fonction SQLite qui porte [`termes_de_chemin`], enregistrée sur
/// chaque connexion de Tune par `SqliteDb` (écriture ET lecture).
pub const FONCTION_TERMES_DE_CHEMIN: &str = "tune_termes_de_chemin";

/// Enregistre [`FONCTION_TERMES_DE_CHEMIN`] sur `conn`.
///
/// Pourquoi une fonction EN PLUS de l'expression : mesuré (#5192, binaire
/// optimisé, 100 000 pistes), l'expression SQL coûte ~60 µs par ligne —
/// ses `RTRIM` à jeu de caractères sont quadratiques dans la longueur du
/// chemin. Sur les 542 423 pistes du demandeur, ~30 s de plus à chaque
/// reconstruction d'après scan, et autant pour CHAQUE frappe du texte libre
/// d'Oxygen. La fonction Rust, elle, est linéaire. Les deux rendent le même
/// texte : `termes_de_chemin_rust_et_sql_disent_la_meme_chose`.
pub fn enregistrer_termes_de_chemin(conn: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    conn.create_scalar_function(
        FONCTION_TERMES_DE_CHEMIN,
        1,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let chemin: Option<String> = ctx.get(0)?;
            Ok(termes_de_chemin(chemin.as_deref()))
        },
    )
}

/// Les termes de chemin de la piste `alias` pour une requête EN MASSE sous
/// SQLite : la fonction Rust enregistrée (voir
/// [`enregistrer_termes_de_chemin`]).
pub fn sql_sqlite_termes_de_chemin_de_piste(alias: &str) -> String {
    format!("{FONCTION_TERMES_DE_CHEMIN}(COALESCE({alias}.file_path, {alias}.cue_media_path))")
}

/// Les valeurs d'une ligne de `tracks_fts` pour la piste `alias` des
/// déclencheurs (`new` ou `old`), dans l'ordre de [`COLONNES_TRACKS_FTS`].
fn valeurs_tracks_fts_declencheur(alias: &str) -> String {
    format!(
        "{alias}.title, \
         (SELECT name FROM artists WHERE id = {alias}.artist_id), \
         (SELECT title FROM albums WHERE id = {alias}.album_id), \
         {alias}.genre, {alias}.composer, {}",
        sql_termes_de_chemin_de_piste(alias)
    )
}

/// Les colonnes de `tracks_fts`, dans l'ordre de la table.
const COLONNES_TRACKS_FTS: &str = "title, artist_name, album_title, genre, composer, path_terms";

/// La table `tracks_fts` et ses trois déclencheurs, termes de chemin compris.
///
/// Les NOMS des déclencheurs sont ceux de la migration 12
/// (`upgrade_fts5_tables`) et du schéma de base : `CORE_SCHEMA` les pose en
/// `IF NOT EXISTS` à chaque démarrage, un autre nom en aurait ajouté une
/// seconde série.
pub fn sql_tracks_fts_avec_termes_de_chemin() -> String {
    let nouvelles = valeurs_tracks_fts_declencheur("new");
    let anciennes = valeurs_tracks_fts_declencheur("old");
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(\
             {COLONNES_TRACKS_FTS}, \
             tokenize='unicode61 remove_diacritics 2', \
             content='', content_rowid='id');\n\
         CREATE TRIGGER IF NOT EXISTS tracks_fts_insert AFTER INSERT ON tracks BEGIN \
             INSERT INTO tracks_fts(rowid, {COLONNES_TRACKS_FTS}) VALUES (new.id, {nouvelles}); \
         END;\n\
         CREATE TRIGGER IF NOT EXISTS tracks_fts_update AFTER UPDATE ON tracks BEGIN \
             INSERT INTO tracks_fts(tracks_fts, rowid, {COLONNES_TRACKS_FTS}) \
                 VALUES ('delete', old.id, {anciennes}); \
             INSERT INTO tracks_fts(rowid, {COLONNES_TRACKS_FTS}) VALUES (new.id, {nouvelles}); \
         END;\n\
         CREATE TRIGGER IF NOT EXISTS tracks_fts_delete AFTER DELETE ON tracks BEGIN \
             INSERT INTO tracks_fts(tracks_fts, rowid, {COLONNES_TRACKS_FTS}) \
                 VALUES ('delete', old.id, {anciennes}); \
         END;"
    )
}

/// Remplir `tracks_fts` depuis `tracks` (la table doit être vide : après un
/// `delete-all`, ou juste créée). Partagé par la reconstruction manuelle, la
/// reconstruction d'après scan et la mise à niveau du démarrage — une seule
/// liste de colonnes, qu'aucun des trois ne peut oublier de suivre.
///
/// La connexion doit porter [`FONCTION_TERMES_DE_CHEMIN`] — c'est le cas de
/// toutes celles de `SqliteDb`.
pub fn sql_remplir_tracks_fts() -> String {
    format!(
        "INSERT INTO tracks_fts(rowid, {COLONNES_TRACKS_FTS}) \
         SELECT t.id, t.title, ar.name, al.title, t.genre, t.composer, {} \
         FROM tracks t \
         LEFT JOIN artists ar ON ar.id = t.artist_id \
         LEFT JOIN albums al ON al.id = t.album_id",
        sql_sqlite_termes_de_chemin_de_piste("t")
    )
}

/// `tracks_fts` porte-t-elle déjà les termes de chemin, table ET
/// déclencheurs ?
fn tracks_fts_a_les_termes_de_chemin(conn: &Connection) -> bool {
    let sql_de = |kind: &str, name: &str| -> String {
        conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = ?1 AND name = ?2",
            rusqlite::params![kind, name],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_default()
    };
    let table = sql_de("table", "tracks_fts");
    // `content=''` : une table héritée du schéma de base (`content='tracks'`)
    // n'est pas celle que les déclencheurs alimentent.
    table.contains(COLONNE_TERMES_DE_CHEMIN)
        && table.contains("content=''")
        && [
            "tracks_fts_insert",
            "tracks_fts_update",
            "tracks_fts_delete",
        ]
        .iter()
        .all(|t| sql_de("trigger", t).contains("cue_media_path"))
}

/// Mise à niveau IDEMPOTENTE de `tracks_fts` vers les termes de chemin
/// (#5192), rejouée à chaque démarrage par la passe finale de
/// `run_migrations` — sans numéro de migration : une table FTS5 ne s'altère
/// pas, elle se recrée, et la question « est-ce déjà fait ? » se lit dans
/// `sqlite_master`.
///
/// Rend `Ok(None)` quand il n'y avait rien à faire, `Ok(Some(n))` avec le
/// nombre de pistes réindexées sinon. Tout se fait dans UNE transaction : un
/// échec laisse l'ancien index en place, jamais une table vide.
pub fn assurer_termes_de_chemin(conn: &Connection) -> Result<Option<usize>, String> {
    if tracks_fts_a_les_termes_de_chemin(conn) {
        return Ok(None);
    }
    let debut = std::time::Instant::now();
    let tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("transaction : {e}"))?;
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS tracks_fts_insert;\
         DROP TRIGGER IF EXISTS tracks_fts_update;\
         DROP TRIGGER IF EXISTS tracks_fts_delete;\
         DROP TABLE IF EXISTS tracks_fts;",
    )
    .map_err(|e| format!("suppression de l'ancien index : {e}"))?;
    tx.execute_batch(&sql_tracks_fts_avec_termes_de_chemin())
        .map_err(|e| format!("création de l'index : {e}"))?;
    let n = tx
        .execute(&sql_remplir_tracks_fts(), [])
        .map_err(|e| format!("remplissage de l'index : {e}"))?;
    tx.commit().map_err(|e| format!("validation : {e}"))?;
    info!(
        pistes = n,
        ms = debut.elapsed().as_millis() as u64,
        "tracks_fts_termes_de_chemin_poses"
    );
    Ok(Some(n))
}

/// PostgreSQL : le vecteur `search_tsv` des pistes avec les termes de chemin.
///
/// Le corps est celui de `002_fts_tsvector.sql` plus UNE ligne, la dernière.
/// Le marqueur `tune:termes_de_chemin v1` rend la passe idempotente : elle lit
/// `pg_proc.prosrc` et ne réécrit ni la fonction ni les vecteurs quand il y
/// est déjà. Le déclencheur écoute en plus `file_path` et `cue_media_path` —
/// sans eux, un fichier renommé garderait les termes de son ancien nom.
pub fn sql_pg_fonction_tsv_des_pistes() -> String {
    format!(
        "CREATE OR REPLACE FUNCTION tracks_search_tsv_refresh()\n\
         RETURNS trigger AS $$\n\
         DECLARE\n    artist_name TEXT;\n    album_title TEXT;\n\
         BEGIN\n\
         -- {MARQUEUR_PG_TERMES_DE_CHEMIN} (#5192)\n\
         SELECT name INTO artist_name FROM artists WHERE id = NEW.artist_id;\n\
         SELECT title INTO album_title FROM albums WHERE id = NEW.album_id;\n\
         NEW.search_tsv :=\n\
             to_tsvector('simple', unaccent(COALESCE(NEW.title, '')))\n\
             || to_tsvector('simple', unaccent(COALESCE(artist_name, '')))\n\
             || to_tsvector('simple', unaccent(COALESCE(album_title, '')))\n\
             || to_tsvector('simple', unaccent(COALESCE(NEW.genre, '')))\n\
             || to_tsvector('simple', unaccent(COALESCE(NEW.composer, '')))\n\
             || to_tsvector('simple', unaccent({}));\n\
         RETURN NEW;\n\
         END;\n\
         $$ LANGUAGE plpgsql;\n\
         DROP TRIGGER IF EXISTS tracks_search_tsv_trg ON tracks;\n\
         CREATE TRIGGER tracks_search_tsv_trg\n\
             BEFORE INSERT OR UPDATE OF title, album_id, artist_id, genre, composer, \
             file_path, cue_media_path ON tracks\n\
             FOR EACH ROW EXECUTE FUNCTION tracks_search_tsv_refresh();",
        sql_termes_de_chemin_de_piste("NEW")
    )
}

/// Le marqueur que [`sql_pg_fonction_tsv_des_pistes`] écrit dans le corps de
/// la fonction PG, et que la passe de démarrage y cherche.
pub const MARQUEUR_PG_TERMES_DE_CHEMIN: &str = "tune:termes_de_chemin v1";

/// PostgreSQL : la fonction porte-t-elle déjà les termes de chemin ?
pub const SQL_PG_A_LES_TERMES_DE_CHEMIN: &str = "SELECT COUNT(*) FROM pg_proc p \
     JOIN pg_namespace n ON n.oid = p.pronamespace \
     WHERE p.proname = 'tracks_search_tsv_refresh' \
       AND n.nspname = current_schema() \
       AND p.prosrc LIKE '%tune:termes_de_chemin v1%'";

/// PostgreSQL : recalculer le vecteur de TOUTES les pistes. `title = title`
/// réveille le déclencheur `BEFORE UPDATE OF title` — le procédé même de
/// `002_fts_tsvector.sql` ; le déclencheur de révision UPnP, gardé par
/// `IS DISTINCT FROM`, ne s'en émeut pas.
pub const SQL_PG_RECALCULER_TSV_DES_PISTES: &str = "UPDATE tracks SET title = title";

pub fn setup_fts(conn: &Connection) {
    for &(table, columns) in FTS_TABLES {
        let fts_name = format!("{table}_fts");
        let cols_csv = columns.join(", ");

        // Check if the FTS table exists with the wrong number of columns
        // and drop it so we can recreate with the correct schema.
        let needs_recreate = conn
            .query_row(
                &format!("SELECT sql FROM sqlite_master WHERE type='table' AND name='{fts_name}'"),
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .map(|sql| columns.iter().skip(1).any(|c| !sql.contains(c)))
            .unwrap_or(false);

        if needs_recreate {
            info!(table = %fts_name, "fts_recreating_multi_column");
            let _ = conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {table}_ai"));
            let _ = conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {table}_ad"));
            let _ = conn.execute_batch(&format!("DROP TRIGGER IF EXISTS {table}_au"));
            let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {fts_name}"));
        }

        let create = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {fts_name} USING fts5(\
             {cols_csv}, content='', \
             tokenize='unicode61 remove_diacritics 2')"
        );
        if let Err(e) = conn.execute_batch(&create) {
            warn!(table = fts_name, error = %e, "fts_create_error");
            continue;
        }

        // Rebuild FTS if content table has rows but FTS is empty
        let fts_count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {fts_name}"), [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let source_count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(0);

        if source_count > 0 && fts_count == 0 {
            info!(
                table = fts_name,
                rows = source_count,
                "fts_rebuild_on_setup"
            );
        }
    }

    info!("fts_initialized");
}

pub fn rebuild_fts(conn: &Connection) {
    // For contentless FTS5 tables, 'rebuild' command doesn't work.
    // Delegate to rebuild_fts_contentless which does delete-all + re-insert.
    match rebuild_fts_contentless(conn) {
        Ok(n) => info!(rows = n, "fts_rebuilt_all"),
        Err(e) => warn!(error = %e, "fts_rebuild_error"),
    }
}

/// Rebuild FTS5 contentless tables by deleting all FTS rows and
/// re-inserting from the source tables. This is the correct approach
/// for `content=''` FTS tables (the standard `rebuild` command only
/// works when `content=<table>` points to an actual table whose
/// columns match the FTS columns).
///
/// The multi-column FTS tables (tracks_fts, albums_fts, artists_fts)
/// use triggers to stay in sync, but manual DB edits bypass triggers.
/// Call this after manual DB corrections, backup restores, or whenever
/// search results seem out of sync with the actual library.
pub fn rebuild_fts_contentless(conn: &Connection) -> Result<i64, String> {
    let start = std::time::Instant::now();
    let mut total_rows = 0i64;

    // For contentless FTS5 tables (content=''), we cannot use
    // `DELETE FROM fts_name` — instead we use the special FTS5
    // `delete-all` command to clear all rows, then re-insert.
    //
    // For content-backed FTS5 tables (content='<table>'), `rebuild`
    // would work, but `delete-all` + re-insert is correct for both.

    // --- tracks_fts: title, artist_name, album_title, genre, composer, path_terms ---
    match conn.execute_batch("INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all')") {
        Ok(_) => {}
        Err(e) => {
            // Table might not exist yet (fresh DB before migration 12)
            warn!(error = %e, "fts_rebuild_delete_tracks_fts");
        }
    }
    match conn.execute(&sql_remplir_tracks_fts(), []) {
        Ok(n) => {
            total_rows += n as i64;
            info!(rows = n, "fts_rebuild_tracks_fts");
        }
        Err(e) => warn!(error = %e, "fts_rebuild_insert_tracks_fts"),
    }

    // --- albums_fts: title, artist_name, genre ---
    match conn.execute_batch("INSERT INTO albums_fts(albums_fts) VALUES('delete-all')") {
        Ok(_) => {}
        Err(e) => warn!(error = %e, "fts_rebuild_delete_albums_fts"),
    }
    match conn.execute(
        "INSERT INTO albums_fts(rowid, title, artist_name, genre) \
         SELECT a.id, a.title, \
                (SELECT name FROM artists WHERE id = a.artist_id), \
                a.genre \
         FROM albums a",
        [],
    ) {
        Ok(n) => {
            total_rows += n as i64;
            info!(rows = n, "fts_rebuild_albums_fts");
        }
        Err(e) => warn!(error = %e, "fts_rebuild_insert_albums_fts"),
    }

    // --- artists_fts: name, sort_name ---
    match conn.execute_batch("INSERT INTO artists_fts(artists_fts) VALUES('delete-all')") {
        Ok(_) => {}
        Err(e) => warn!(error = %e, "fts_rebuild_delete_artists_fts"),
    }
    match conn.execute(
        "INSERT INTO artists_fts(rowid, name, sort_name) \
         SELECT id, name, sort_name FROM artists",
        [],
    ) {
        Ok(n) => {
            total_rows += n as i64;
            info!(rows = n, "fts_rebuild_artists_fts");
        }
        Err(e) => warn!(error = %e, "fts_rebuild_insert_artists_fts"),
    }

    let elapsed_ms = start.elapsed().as_millis();
    info!(total_rows, elapsed_ms, "fts_rebuild_contentless_complete");
    Ok(total_rows)
}

pub fn search_where(table_name: &str) -> String {
    let fts_name = format!("{table_name}_fts");
    format!("{table_name}.id IN (SELECT rowid FROM {fts_name} WHERE {fts_name} MATCH ?)")
}

pub fn search_with_rank(table_name: &str) -> String {
    let fts_name = format!("{table_name}_fts");
    format!("SELECT rowid, rank FROM {fts_name} WHERE {fts_name} MATCH ? ORDER BY rank")
}

pub fn fts_search(conn: &Connection, table_name: &str, query: &str, limit: i64) -> Vec<i64> {
    let fts_name = format!("{table_name}_fts");
    let sql =
        format!("SELECT rowid FROM {fts_name} WHERE {fts_name} MATCH ? ORDER BY rank LIMIT ?");

    let escaped = escape_fts_query(query);

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "fts_search_error");
            return Vec::new();
        }
    };

    stmt.query_map(rusqlite::params![escaped, limit], |row| row.get(0))
        .unwrap_or_else(|_| panic!("fts query_map failed"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default()
}

fn escape_fts_query(query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| {
            let clean: String = t
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == '*')
                .collect();
            if clean.is_empty() {
                String::new()
            } else if clean.ends_with('*') {
                clean
            } else {
                format!("{clean}*")
            }
        })
        .filter(|t| !t.is_empty())
        .collect();

    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn setup_fts_succeeds() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks_fts", [], |r| r.get(0))
            .unwrap_or(-1);
        assert!(count >= 0);
    }

    #[test]
    fn escape_fts_query_basic() {
        assert_eq!(escape_fts_query("hello world"), "hello* world*");
    }

    #[test]
    fn escape_fts_query_special_chars() {
        assert_eq!(escape_fts_query("rock & roll"), "rock* roll*");
    }

    #[test]
    fn escape_fts_query_wildcard_preserved() {
        assert_eq!(escape_fts_query("pink*"), "pink*");
    }

    #[test]
    fn search_where_format() {
        let clause = search_where("tracks");
        assert!(clause.contains("tracks_fts"));
        assert!(clause.contains("MATCH"));
    }

    #[test]
    fn fts_insert_and_search() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);

        conn.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd')",
            [],
        )
        .unwrap();

        let results = fts_search(&conn, "artists", "pink", 10);
        assert_eq!(results, vec![1]);
    }

    #[test]
    fn fts_accent_insensitive() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);

        conn.execute("INSERT INTO artists (id, name) VALUES (1, 'Stromae')", [])
            .unwrap();

        let results = fts_search(&conn, "artists", "stromae", 10);
        assert_eq!(results, vec![1]);
    }

    #[test]
    fn rebuild_fts_works() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        setup_fts(&conn);
        rebuild_fts(&conn);
    }

    #[test]
    fn rebuild_fts_contentless_repopulates_after_manual_edit() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();

        // Insert data via normal path (triggers fire)
        conn.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Pink Floyd')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'The Wall', 1, 1979)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, genre) VALUES (1, 'Comfortably Numb', 1, 1, 'Rock')",
            [],
        ).unwrap();

        // Verify FTS has data
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists_fts", [], |r| r.get(0))
            .unwrap();
        assert!(
            fts_count > 0,
            "FTS should have data after trigger-based inserts"
        );

        // Simulate FTS corruption: clear FTS content using the contentless delete-all command
        conn.execute_batch(
            "INSERT INTO artists_fts(artists_fts) VALUES('delete-all'); \
             INSERT INTO albums_fts(albums_fts) VALUES('delete-all'); \
             INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all');",
        )
        .unwrap();

        // Verify FTS is empty
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 0, "FTS should be empty after manual delete");

        // Rebuild
        let rows = rebuild_fts_contentless(&conn).unwrap();
        assert!(
            rows >= 3,
            "Should have rebuilt at least 3 rows (1 artist + 1 album + 1 track)"
        );

        // Verify FTS is repopulated
        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "artists_fts should have 1 row after rebuild");

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM albums_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "albums_fts should have 1 row after rebuild");

        let fts_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks_fts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fts_count, 1, "tracks_fts should have 1 row after rebuild");
    }

    // ─── #5192 : termes de chemin ─────────────────────────────────────────

    /// Des chemins qui couvrent les pièges du découpage : séparateurs, point
    /// absent ou multiple, antislash Windows, pas de dossier, dossier final
    /// vide, adresse réseau, accents, suites d'espaces.
    pub(crate) const CORPUS_DE_CHEMINS: &[&str] = &[
        "/music/Classique/Mahler Kondrashin/01 - Symphonie n°1.flac",
        "/music/Classique/Mahler_Kondrashin/02_Symphonie-n°1.mvt.2.flac",
        "D:\\Musique\\Mahler Kondrashin\\03 Titan.flac",
        "/music/Mahler - Kondrashin (1961)/04.   Adagio.flac",
        "sans-dossier.flac",
        "/fichier_sans_extension",
        "/a/b/",
        "/a//double//slash.mp3",
        "http://192.168.1.2:8200/MediaItems/123.flac",
        "smb://nas/partage/Album/piste.flac",
        "/music/Beyoncé/Déjà_Vu.m4a",
        "/music/.cache/.hidden",
        "/music/a.b.c/d.e.f.g",
        "",
        "/music/Pink Floyd - Wish You Were Here/03 - Have A Cigar.flac",
    ];

    #[test]
    fn termes_de_chemin_dernier_dossier_et_fichier_sans_extension() {
        let t = |p: &str| termes_de_chemin(Some(p));
        assert_eq!(
            t("/music/Classique/Mahler Kondrashin/01 - Symphonie n°1.flac"),
            "Mahler Kondrashin 01 Symphonie n°1",
            "le dernier dossier seulement, pas « Classique »"
        );
        assert_eq!(
            t("/music/Classique/Mahler_Kondrashin/02_Symphonie-n°1.mvt.2.flac"),
            "Mahler Kondrashin 02 Symphonie n°1 mvt 2",
            "_ - . deviennent des espaces, seule la DERNIÈRE extension tombe"
        );
        assert_eq!(
            t("D:\\Musique\\Mahler Kondrashin\\03 Titan.flac"),
            "Mahler Kondrashin 03 Titan"
        );
        assert_eq!(t("sans-dossier.flac"), "sans dossier");
        assert_eq!(t("http://192.168.1.2:8200/MediaItems/123.flac"), "");
        assert_eq!(termes_de_chemin(None), "");
    }

    /// LA garde de « une seule définition » : la fonction Rust (ce que rend
    /// l'API) et l'expression SQL (ce qu'indexe `tracks_fts` et ce que filtre
    /// le texte libre) rendent le MÊME texte, chemin par chemin.
    #[test]
    fn termes_de_chemin_rust_et_sql_disent_la_meme_chose() {
        let db = SqliteDb::open_in_memory().unwrap();
        let conn = db.connection().lock().unwrap();
        let sql = format!("SELECT {}", sql_termes_de_chemin("?1"));
        for chemin in CORPUS_DE_CHEMINS {
            let par_sql: String = conn
                .query_row(&sql, [chemin], |r| r.get(0))
                .unwrap_or_else(|e| panic!("{chemin:?} : {e}"));
            assert_eq!(
                par_sql,
                termes_de_chemin(Some(chemin)),
                "Rust et SQL divergent sur {chemin:?}"
            );
        }
        let nul: String = conn
            .query_row(
                &format!("SELECT {}", sql_termes_de_chemin("NULL")),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nul, termes_de_chemin(None));
    }

    /// Les deux mots d'un DOSSIER, sur une piste dont aucune balise ne les
    /// porte, par l'index plein texte — y compris quand le fichier est
    /// renommé (déclencheur de mise à jour) et après la reconstruction
    /// manuelle.
    #[test]
    fn tracks_fts_trouve_une_piste_par_son_dossier_et_son_fichier() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        conn.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Gustav Mahler');\
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Symphonie n°1', 1);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path) VALUES \
               (1, 'Piste 1', 1, 1, '/music/Mahler_Kondrashin/01-Titan.flac');",
        )
        .unwrap();
        let cherche = |q: &str| fts_search(&conn, "tracks", q, 10);
        assert_eq!(cherche("Kondrashin"), vec![1], "le nom du dossier");
        assert_eq!(cherche("Titan"), vec![1], "le nom du fichier");
        assert_eq!(cherche("flac"), Vec::<i64>::new(), "pas l'extension");
        assert_eq!(cherche("music"), Vec::<i64>::new(), "pas le chemin entier");

        conn.execute(
            "UPDATE tracks SET file_path = '/music/Solti/01-Resurrection.flac' WHERE id = 1",
            [],
        )
        .unwrap();
        assert_eq!(
            cherche("Kondrashin"),
            Vec::<i64>::new(),
            "l'ancien nom sort"
        );
        assert_eq!(cherche("Solti"), vec![1], "le nouveau entre");

        rebuild_fts_contentless(&conn).unwrap();
        assert_eq!(
            cherche("Resurrection"),
            vec![1],
            "la reconstruction le garde"
        );
    }

    /// La mise à niveau du démarrage : une base à l'ANCIEN index (cinq
    /// colonnes, sans chemin) passe au nouveau, pistes existantes réindexées,
    /// une fois — le second passage ne fait rien.
    #[test]
    fn assurer_termes_de_chemin_reindexe_l_existant_une_seule_fois() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        // L'index tel que la migration 12 le laissait.
        conn.execute_batch(
            "DROP TRIGGER IF EXISTS tracks_fts_insert;\
             DROP TRIGGER IF EXISTS tracks_fts_update;\
             DROP TRIGGER IF EXISTS tracks_fts_delete;\
             DROP TABLE IF EXISTS tracks_fts;\
             CREATE VIRTUAL TABLE tracks_fts USING fts5(\
               title, artist_name, album_title, genre, composer,\
               tokenize='unicode61 remove_diacritics 2', content='', content_rowid='id');\
             CREATE TRIGGER tracks_fts_insert AFTER INSERT ON tracks BEGIN \
               INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer) \
               VALUES (new.id, new.title, NULL, NULL, new.genre, new.composer); \
             END;\
             INSERT INTO tracks (id, title, file_path) VALUES \
               (7, 'Piste', '/music/Mahler Kondrashin/01.flac');",
        )
        .unwrap();
        assert!(!tracks_fts_a_les_termes_de_chemin(&conn));

        assert_eq!(assurer_termes_de_chemin(&conn).unwrap(), Some(1));
        assert_eq!(fts_search(&conn, "tracks", "Kondrashin", 10), vec![7]);
        assert_eq!(fts_search(&conn, "tracks", "Piste", 10), vec![7]);
        assert_eq!(
            assurer_termes_de_chemin(&conn).unwrap(),
            None,
            "déjà fait : ni recréation ni réindexation"
        );
        // Et les déclencheurs posés suivent les écritures suivantes.
        conn.execute(
            "INSERT INTO tracks (id, title, file_path) VALUES (8, 'Autre', '/x/Karajan/02.flac')",
            [],
        )
        .unwrap();
        assert_eq!(fts_search(&conn, "tracks", "Karajan", 10), vec![8]);
    }

    /// Mesure (#5192) : taille de l'index et temps de reconstruction, sur une
    /// bibliothèque de `TUNE_MESURE_5192_PISTES` pistes (20 000 par défaut).
    /// Ignorée par défaut : `cargo test -p tune-core --lib mesure_5192 --
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn mesure_5192_taille_et_temps_de_reconstruction() {
        let n: i64 = std::env::var("TUNE_MESURE_5192_PISTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20_000);
        let dir = crate::test_scratch::scratch_dir("mesure5192");
        let chemin_base = dir.path().join("mesure.db");
        let conn = Connection::open(&chemin_base).unwrap();
        enregistrer_termes_de_chemin(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, sort_name TEXT);\
             CREATE TABLE albums (id INTEGER PRIMARY KEY, title TEXT, artist_id INTEGER, genre TEXT);\
             CREATE TABLE tracks (id INTEGER PRIMARY KEY, title TEXT, album_id INTEGER, \
               artist_id INTEGER, genre TEXT, composer TEXT, file_path TEXT, cue_media_path TEXT);",
        )
        .unwrap();
        {
            let tx = conn.unchecked_transaction().unwrap();
            for a in 0..(n / 12).max(1) {
                tx.execute(
                    "INSERT INTO artists (id, name) VALUES (?1, ?2)",
                    rusqlite::params![a, format!("Artiste {a}")],
                )
                .unwrap();
                tx.execute(
                    "INSERT INTO albums (id, title, artist_id, genre) VALUES (?1, ?2, ?1, 'Classique')",
                    rusqlite::params![a, format!("Album numéro {a}")],
                )
                .unwrap();
            }
            for i in 0..n {
                let a = i / 12;
                tx.execute(
                    "INSERT INTO tracks (id, title, album_id, artist_id, genre, composer, file_path) \
                     VALUES (?1, ?2, ?3, ?3, 'Classique', 'Compositeur', ?4)",
                    rusqlite::params![
                        i,
                        format!("Mouvement {i}"),
                        a,
                        format!(
                            "/srv/musique/Classique/Artiste {a}/Artiste_{a} - Album numéro {a} (1961)/{:02} - Mouvement_{i}.flac",
                            i % 12 + 1
                        )
                    ],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let taille = |conn: &Connection| -> i64 {
            conn.execute_batch("VACUUM").unwrap();
            let pages: i64 = conn
                .query_row("PRAGMA page_count", [], |r| r.get(0))
                .unwrap();
            let page: i64 = conn
                .query_row("PRAGMA page_size", [], |r| r.get(0))
                .unwrap();
            pages * page
        };
        let sans_index = taille(&conn);

        // L'ANCIEN index : cinq colonnes.
        let debut = std::time::Instant::now();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE tracks_fts USING fts5(\
               title, artist_name, album_title, genre, composer,\
               tokenize='unicode61 remove_diacritics 2', content='', content_rowid='id');\
             INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer) \
             SELECT t.id, t.title, ar.name, al.title, t.genre, t.composer FROM tracks t \
             LEFT JOIN artists ar ON ar.id = t.artist_id LEFT JOIN albums al ON al.id = t.album_id;",
        )
        .unwrap();
        let ms_ancien = debut.elapsed().as_millis();
        let avec_ancien = taille(&conn);

        // Le NOUVEAU, par la passe du démarrage.
        let debut = std::time::Instant::now();
        let reindexees = assurer_termes_de_chemin(&conn).unwrap();
        let ms_nouveau = debut.elapsed().as_millis();
        let avec_nouveau = taille(&conn);

        // Le seul calcul des termes, sans index.
        let debut = std::time::Instant::now();
        let _: i64 = conn
            .query_row(
                &format!(
                    "SELECT SUM(LENGTH({})) FROM tracks t",
                    sql_termes_de_chemin_de_piste("t")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        let ms_expression = debut.elapsed().as_millis();
        let debut = std::time::Instant::now();
        let _: i64 = conn
            .query_row(
                &format!(
                    "SELECT SUM(LENGTH({})) FROM tracks t",
                    sql_sqlite_termes_de_chemin_de_piste("t")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        let ms_fonction = debut.elapsed().as_millis();
        // Le texte libre d'Oxygen sur toute la bibliothèque.
        let debut = std::time::Instant::now();
        let _: i64 = conn
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM tracks t WHERE LOWER(t.title) LIKE '%kondrashin%' \
                     OR LOWER({}) LIKE '%kondrashin%'",
                    sql_sqlite_termes_de_chemin_de_piste("t")
                ),
                [],
                |r| r.get(0),
            )
            .unwrap();
        let ms_texte_libre = debut.elapsed().as_millis();

        // Une recherche, pour l'ordre de grandeur.
        let debut = std::time::Instant::now();
        let trouves = fts_search(&conn, "tracks", "Mouvement 1961", 50);
        let ms_recherche = debut.elapsed().as_micros();

        let ko = |o: i64| o / 1024;
        eprintln!(
            "MESURE_5192 pistes={n} reindexees={reindexees:?}\n\
             MESURE_5192 base_sans_index={} Kio\n\
             MESURE_5192 index_ancien={} Kio (construit en {ms_ancien} ms)\n\
             MESURE_5192 index_nouveau={} Kio (passe du démarrage : {ms_nouveau} ms)\n\
             MESURE_5192 surcout_index={} Kio (+{:.1} %)\n\
             MESURE_5192 expression_sql_seule={ms_expression} ms, fonction_rust_seule={ms_fonction} ms\n\
             MESURE_5192 texte_libre_bibliotheque_entiere={ms_texte_libre} ms\n\
             MESURE_5192 recherche « Mouvement 1961 » : {} résultats en {ms_recherche} µs",
            ko(sans_index),
            ko(avec_ancien - sans_index),
            ko(avec_nouveau - sans_index),
            ko(avec_nouveau - avec_ancien),
            100.0 * (avec_nouveau - avec_ancien) as f64 / (avec_ancien - sans_index).max(1) as f64,
            trouves.len(),
        );
        assert_eq!(reindexees, Some(n as usize));
        drop(conn);
    }

    /// #5192 sur PostgreSQL, par le VRAI démarrage (`ensure_schema` puis
    /// `run_pg_migrations`) : l'expression SQL égale la fonction Rust, la
    /// recherche trouve par le dossier et classe après, le texte libre
    /// d'Oxygen compare les termes de chemin — et la passe du démarrage
    /// réindexe une base restée à l'ancienne fonction, une seule fois.
    ///
    /// Contre-épreuve DANS l'épreuve : `002_fts_tsvector.sql` rejoué remet
    /// l'ancienne fonction et les anciens vecteurs ; le dossier n'est alors
    /// plus trouvé, jusqu'à la passe.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    async fn pg_5192_termes_de_chemin_sur_postgres() {
        use crate::db::backend::DbBackend;
        use crate::db::facet_filter::TrackFilter;
        use crate::db::track_repo::TrackRepo;
        use std::sync::Arc;

        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        const SCHEMA: &str = "termes_de_chemin_5192";
        let maintenance = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA}"
        )))
        .execute(&maintenance)
        .await
        .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|c, _| {
                Box::pin(async move {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                        "SET search_path TO {SCHEMA}, public"
                    )))
                    .execute(c)
                    .await
                    .map(|_| ())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        for sql in crate::db::postgres::ENSURE_TABLES
            .iter()
            .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
        {
            let _ = sqlx::raw_sql(*sql).execute(&pool).await;
        }
        crate::db::migrations::run_pg_migrations(&pool)
            .await
            .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));

        // 1. Une seule définition : Rust = SQL, sous PostgreSQL aussi.
        let sql = format!("SELECT {}", sql_termes_de_chemin("$1::text"));
        for chemin in CORPUS_DE_CHEMINS {
            let par_sql: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.clone()))
                .bind(*chemin)
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("{chemin:?} : {e}"));
            assert_eq!(par_sql, termes_de_chemin(Some(chemin)), "{chemin:?}");
        }

        // 2. Le jeu de `track_repo::jeu_5192`, en SQL.
        sqlx::raw_sql(
            "INSERT INTO artists (id, name) VALUES (1, 'Gustav Mahler'), (2, 'Pink Floyd');\
             INSERT INTO albums (id, title, artist_id) VALUES \
               (1, 'Symphonie n°1', 1), (2, 'Wish You Were Here', 2);\
             INSERT INTO tracks (id, title, album_id, artist_id, file_path, source) VALUES \
               (1, 'Langsam, schleppend', 1, 1, '/music/Classique/Mahler_Kondrashin/01-Langsam.flac', 'local'),\
               (2, 'Mahler Kondrashin, l''entretien', NULL, NULL, '/music/Radio/entretien.flac', 'local'),\
               (3, 'Have A Cigar', 2, 2, '/music/Pink Floyd - Wish You Were Here/03 - Have A Cigar.flac', 'local');",
        )
        .execute(&pool)
        .await
        .unwrap();
        let backend: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        let repo = TrackRepo::with_backend(backend);
        let ids = |q: &str| -> Vec<i64> {
            repo.search(q, 50)
                .unwrap_or_else(|e| panic!("recherche {q:?} refusée par PostgreSQL : {e}"))
                .into_iter()
                .filter_map(|t| t.id)
                .collect()
        };
        assert_eq!(
            ids("Mahler Kondrashin"),
            vec![2, 1],
            "titre d'abord, dossier ensuite"
        );
        assert_eq!(repo.search_count("Mahler Kondrashin", 1_000).unwrap(), 2);
        assert_eq!(ids("Kondrashin Langsam"), vec![1]);
        assert!(
            !ids("Wish You Were Here").contains(&3),
            "#4367 par le chemin"
        );
        assert_eq!(ids("03"), vec![3]);

        let liste = |q: &str| -> i64 {
            let f = TrackFilter {
                q: Some(q.into()),
                ..Default::default()
            };
            repo.list_filtered(&f, 50, 0)
                .unwrap_or_else(|e| panic!("texte libre {q:?} refusé : {e}"))
                .1
        };
        assert_eq!(liste("mahler kondrashin"), 2);
        assert_eq!(liste("Langsam"), 1);
        assert_eq!(liste("mahler_kondrashin"), 0, "`_` littéral");
        let (tires, total) = repo
            .random_ids_in_folder(1, "/music/Classique", Some("kondrashin"), 10)
            .unwrap();
        assert_eq!((tires, total), (vec![1], 1));

        // 3. Idempotence : déjà posé par le démarrage.
        assert_eq!(
            crate::db::migrations::assurer_termes_de_chemin_pg(&pool)
                .await
                .unwrap(),
            None
        );
        // 4. Contre-épreuve : l'ancienne fonction et ses vecteurs.
        sqlx::raw_sql(include_str!(
            "../../migrations/postgres/002_fts_tsvector.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            ids("Kondrashin Langsam"),
            Vec::<i64>::new(),
            "l'ancien vecteur ne porte pas le dossier : l'épreuve mord"
        );
        // 5. La passe réindexe l'existant, une fois.
        assert_eq!(
            crate::db::migrations::assurer_termes_de_chemin_pg(&pool)
                .await
                .unwrap(),
            Some(3)
        );
        assert_eq!(ids("Kondrashin Langsam"), vec![1]);
        assert_eq!(
            crate::db::migrations::assurer_termes_de_chemin_pg(&pool)
                .await
                .unwrap(),
            None
        );
        // 6. Un renommage de fichier suit (déclencheur sur `file_path`).
        sqlx::raw_sql("UPDATE tracks SET file_path = '/music/Solti/01-Langsam.flac' WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(ids("Kondrashin Langsam"), Vec::<i64>::new());
        assert_eq!(ids("Solti"), vec![1]);

        pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
        )))
        .execute(&maintenance)
        .await
        .unwrap();
    }

    /// Mesure PostgreSQL (#5192) : recalcul des vecteurs par la passe du
    /// démarrage, texte libre d'Oxygen et recherche, sur
    /// `TUNE_MESURE_5192_PISTES` pistes (100 000 par défaut). Ignorée par
    /// défaut ; `TUNE_TEST_PG_URL` requis.
    #[cfg(feature = "postgres")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn mesure_pg_5192_recalcul_et_texte_libre() {
        use crate::db::backend::DbBackend;
        use crate::db::facet_filter::TrackFilter;
        use crate::db::track_repo::TrackRepo;
        use std::sync::Arc;
        use std::time::Instant;

        let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
            eprintln!("SAUT: TUNE_TEST_PG_URL absent");
            return;
        };
        let n: i64 = std::env::var("TUNE_MESURE_5192_PISTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100_000);
        const SCHEMA: &str = "mesure_5192";
        let maintenance = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE; CREATE SCHEMA {SCHEMA}"
        )))
        .execute(&maintenance)
        .await
        .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .after_connect(|c, _| {
                Box::pin(async move {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                        "SET search_path TO {SCHEMA}, public"
                    )))
                    .execute(c)
                    .await
                    .map(|_| ())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        for sql in crate::db::postgres::ENSURE_TABLES
            .iter()
            .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
        {
            let _ = sqlx::raw_sql(*sql).execute(&pool).await;
        }
        crate::db::migrations::run_pg_migrations(&pool)
            .await
            .unwrap();
        // L'état d'AVANT : l'ancienne fonction, puis les pistes.
        sqlx::raw_sql(include_str!(
            "../../migrations/postgres/002_fts_tsvector.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let debut = Instant::now();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO artists (id, name) SELECT a, 'Artiste ' || a FROM generate_series(0, {a}) a;\
             INSERT INTO albums (id, title, artist_id, genre) \
               SELECT a, 'Album numéro ' || a, a, 'Classique' FROM generate_series(0, {a}) a;\
             INSERT INTO tracks (id, title, album_id, artist_id, genre, composer, file_path, source) \
               SELECT i, 'Mouvement ' || i, i / 12, i / 12, 'Classique', 'Compositeur', \
                 '/srv/musique/Classique/Artiste ' || (i / 12) || '/Artiste_' || (i / 12) \
                 || ' - Album numéro ' || (i / 12) || ' (1961)/' || lpad((i % 12 + 1)::text, 2, '0') \
                 || ' - Mouvement_' || i || '.flac', 'local' \
               FROM generate_series(0, {m}) i;",
            a = n / 12,
            m = n - 1
        )))
        .execute(&pool)
        .await
        .unwrap();
        let ms_insertion_ancienne = debut.elapsed().as_millis();
        let taille = |pool: sqlx::PgPool| async move {
            sqlx::query_scalar::<_, i64>("SELECT pg_total_relation_size('tracks')")
                .fetch_one(&pool)
                .await
                .unwrap()
        };
        sqlx::raw_sql("VACUUM FULL tracks")
            .execute(&pool)
            .await
            .unwrap();
        let avant = taille(pool.clone()).await;

        let debut = Instant::now();
        let recalculees = crate::db::migrations::assurer_termes_de_chemin_pg(&pool)
            .await
            .unwrap();
        let ms_passe = debut.elapsed().as_millis();
        sqlx::raw_sql("VACUUM FULL tracks")
            .execute(&pool)
            .await
            .unwrap();
        let apres = taille(pool.clone()).await;

        let debut = Instant::now();
        let _: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT SUM(LENGTH({})) FROM tracks t",
            sql_termes_de_chemin_de_piste("t")
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        let ms_expression = debut.elapsed().as_millis();

        let backend: Arc<dyn DbBackend> =
            Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
        let repo = TrackRepo::with_backend(backend);
        let debut = Instant::now();
        let f = TrackFilter {
            q: Some("kondrashin".into()),
            ..Default::default()
        };
        let (_, total_q) = repo.list_filtered(&f, 3000, 0).unwrap();
        let ms_texte_libre = debut.elapsed().as_millis();
        let debut = Instant::now();
        let trouves = repo.search("Album numéro 42", 50).unwrap().len();
        let ms_recherche = debut.elapsed().as_millis();

        eprintln!(
            "MESURE_PG_5192 pistes={n} insertion(ancienne fonction)={ms_insertion_ancienne} ms\n\
             MESURE_PG_5192 passe du démarrage : {recalculees:?} pistes en {ms_passe} ms\n\
             MESURE_PG_5192 tracks (table+index) avant={} Kio après={} Kio (+{} Kio)\n\
             MESURE_PG_5192 expression_seule={ms_expression} ms\n\
             MESURE_PG_5192 texte libre « kondrashin » : {total_q} pistes en {ms_texte_libre} ms\n\
             MESURE_PG_5192 recherche « Album numéro 42 » : {trouves} en {ms_recherche} ms",
            avant / 1024,
            apres / 1024,
            (apres - avant) / 1024
        );
        pool.close().await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
        )))
        .execute(&maintenance)
        .await
        .unwrap();
    }
}
