//! #7619 (chantier « accélération de la Bibliothèque », volets L4 et L5) — la
//! RÉVISION de la bibliothèque : un compteur tenu par la base elle-même, qui
//! change à chaque écriture dans l'une des tables dont dépend ce que la
//! Bibliothèque affiche (pistes, albums, artistes, métadonnées, crédits,
//! masquages, favoris, notes, listes de lecture, collections, réglages).
//!
//! Un résultat calculé sous la révision `r` est donc exactement celui que la
//! base rendrait tant qu'elle lit encore `r` : c'est la clé des caches de
//! lecture de la Bibliothèque (les compteurs des facettes, L4 ; la liste
//! triée, L5). UN seul compteur, ce nom-là, pour tous les volets.
//!
//! # Pourquoi des déclencheurs, et non un compteur en mémoire
//!
//! Les écritures viennent de partout : le scan, l'édition d'un album, la
//! synchronisation UPnP, la réconciliation des favoris, un second processus
//! (outil, migration). Un compteur incrémenté « par le code qui écrit »
//! raterait le premier chemin oublié — et un cache servirait alors un état
//! périmé sans que rien ne le signale. Le déclencheur, lui, voit toutes les
//! écritures, quelle que soit la connexion.
//!
//! Le jeton de SQLite (`PRAGMA data_version`, voir
//! [`super::backend::DbBackend::jeton_des_donnees`]) change aussi à chaque
//! écriture, mais à TOUTE écriture : l'historique d'écoute, la reprise de
//! lecture, l'état des zones l'invalident plusieurs fois par minute pendant
//! que la musique joue. Il n'existe pas non plus sous PostgreSQL.
//!
//! # Fiabilité
//!
//! La table, ses déclencheurs et sa ligne sont posés en UNE transaction, par
//! la passe rejouée à chaque démarrage (aucun numéro de migration). Si un seul
//! déclencheur manque, rien n'est posé — la ligne n'existe pas, [`lire`] rend
//! `None`, et les caches se taisent : plus lent, jamais faux.
//!
//! `instance` est tiré au hasard à la création de la ligne : deux bases (une
//! base recréée, deux bases de test dans un même processus) ne confondent
//! jamais leurs révisions.
//!
//! ⚠️ Une table nouvelle lue par la Bibliothèque doit entrer dans
//! [`TABLES_SUIVIES`], sans quoi ses écritures ne changent pas la révision.

use super::backend::DbBackend;
use super::engine::Engine;

/// Les tables dont une écriture change la révision.
///
/// Relevé le 01/11/2026 sur les lectures des facettes (`routes::library::
/// facets`, `db::facet_filter`) et de la liste des pistes : `tracks`,
/// `albums`, `artists`, `track_metadata`, `album_metadata`, `track_credits`,
/// les masquages, les favoris, les notes d'album, les listes de lecture, les
/// collections intelligentes, et `settings` (les collections manuelles y
/// vivent, en JSON).
pub const TABLES_SUIVIES: [&str; 14] = [
    "tracks",
    "albums",
    "artists",
    "track_metadata",
    "album_metadata",
    "track_credits",
    "hidden_items",
    "streaming_hidden_items",
    "favorites",
    "album_ratings",
    "playlists",
    "playlist_tracks",
    "smart_collections",
    "settings",
];

/// La table, ses déclencheurs et sa ligne, pour SQLite — en une transaction
/// (voir la doc du module). Idempotent : `IF NOT EXISTS` partout, et la ligne
/// n'est créée qu'une fois (`INSERT OR IGNORE`), donc la révision ne repart
/// pas de zéro au redémarrage.
pub fn sql_sqlite() -> String {
    let mut sql = String::from(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS library_revision (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             revision INTEGER NOT NULL,
             instance INTEGER NOT NULL
         );\n",
    );
    for table in TABLES_SUIVIES {
        for (suffixe, evenement) in [("ins", "INSERT"), ("upd", "UPDATE"), ("del", "DELETE")] {
            sql.push_str(&format!(
                "CREATE TRIGGER IF NOT EXISTS library_revision_{table}_{suffixe} \
                 AFTER {evenement} ON {table} BEGIN \
                 UPDATE library_revision SET revision = revision + 1 WHERE id = 1; END;\n"
            ));
        }
    }
    sql.push_str(
        "INSERT OR IGNORE INTO library_revision (id, revision, instance) \
         VALUES (1, 0, abs(random()));
         COMMIT;",
    );
    sql
}

/// La même chose pour PostgreSQL : une fonction, et un déclencheur PAR
/// INSTRUCTION sur chaque table (un `INSERT` de mille lignes ne compte qu'une
/// fois). `CREATE TRIGGER IF NOT EXISTS` n'existe pas avant PostgreSQL 17 :
/// chaque déclencheur est posé s'il manque, d'après `pg_trigger`. Le tout est
/// UN bloc `DO`, donc une transaction.
pub fn sql_postgres() -> String {
    let mut sql = String::from(
        "DO $revision$ BEGIN
         CREATE TABLE IF NOT EXISTS library_revision (
             id BIGINT PRIMARY KEY CHECK (id = 1),
             revision BIGINT NOT NULL,
             instance BIGINT NOT NULL
         );
         CREATE OR REPLACE FUNCTION library_revision_bump() RETURNS trigger AS $f$
         BEGIN
             UPDATE library_revision SET revision = revision + 1 WHERE id = 1;
             RETURN NULL;
         END $f$ LANGUAGE plpgsql;\n",
    );
    for table in TABLES_SUIVIES {
        sql.push_str(&format!(
            "IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname = 'library_revision_{table}' \
             AND tgrelid = '{table}'::regclass) THEN \
             CREATE TRIGGER library_revision_{table} \
             AFTER INSERT OR UPDATE OR DELETE ON {table} \
             FOR EACH STATEMENT EXECUTE FUNCTION library_revision_bump(); \
             END IF;\n"
        ));
    }
    sql.push_str(
        "INSERT INTO library_revision (id, revision, instance) \
         VALUES (1, 0, floor(random() * 9007199254740991)::bigint) \
         ON CONFLICT (id) DO NOTHING;
         END $revision$;",
    );
    sql
}

/// Le SQL de la passe de démarrage pour `engine`.
pub fn sql_pour(engine: Engine) -> String {
    match engine {
        Engine::Sqlite => sql_sqlite(),
        Engine::Postgres => sql_postgres(),
    }
}

/// La révision courante : `(instance, revision)`, ou `None` si le compteur
/// n'est pas posé sur cette base (voir « Fiabilité ») ou illisible. Un
/// appelant qui reçoit `None` ne met rien en cache.
pub fn lire(db: &dyn DbBackend) -> Option<(i64, i64)> {
    let ligne = db
        .query_one(
            "SELECT instance, revision FROM library_revision WHERE id = 1",
            &[],
        )
        .ok()??;
    Some((ligne.first()?.as_i64()?, ligne.get(1)?.as_i64()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use crate::db::sqlite::SqliteDb;

    fn base() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    /// Chaque table suivie, écrite, change la révision ; une table hors de la
    /// liste (l'historique d'écoute) ne la change pas.
    #[test]
    fn chaque_table_suivie_change_la_revision_et_l_historique_non() {
        let db = base();
        let r0 = lire(&db).expect("compteur posé par la passe de démarrage");
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'A');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1);
             INSERT INTO tracks (id, title, album_id, artist_id) VALUES (1, 'C', 1, 1);",
        )
        .unwrap();
        let r1 = lire(&db).unwrap();
        assert_eq!(r1.0, r0.0, "l'instance ne bouge pas");
        assert!(r1.1 > r0.1, "une écriture de pistes change la révision");

        DbBackend::execute(&db, "UPDATE tracks SET genre = 'Jazz' WHERE id = 1", &[]).unwrap();
        let r2 = lire(&db).unwrap();
        assert!(r2.1 > r1.1, "une modification change la révision");

        DbBackend::execute(
            &db,
            "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 1)",
            &[],
        )
        .unwrap();
        let r3 = lire(&db).unwrap();
        assert!(r3.1 > r2.1, "un masquage change la révision");

        DbBackend::execute(&db, "DELETE FROM tracks WHERE id = 1", &[]).unwrap();
        let r4 = lire(&db).unwrap();
        assert!(r4.1 > r3.1, "une suppression change la révision");

        // Contre-épreuve : une table que la Bibliothèque ne lit pas.
        let avant = lire(&db).unwrap();
        db.execute_batch("INSERT INTO listen_history (title) VALUES ('x');")
            .unwrap();
        assert_eq!(
            lire(&db).unwrap(),
            avant,
            "l'historique ne change pas la révision"
        );
    }

    /// Toutes les tables suivies ont leurs trois déclencheurs ; la passe
    /// rejouée ne double rien et ne remet pas la révision à zéro.
    #[test]
    fn passe_rejouee_idempotente() {
        let db = base();
        db.execute_batch("INSERT INTO artists (id, name) VALUES (1, 'A');")
            .unwrap();
        let avant = lire(&db).unwrap();
        db.execute_batch(&sql_sqlite()).unwrap();
        assert_eq!(lire(&db).unwrap(), avant);
        let n = db
            .query_one(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' \
                 AND name LIKE 'library_revision_%'",
                &[],
            )
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap();
        assert_eq!(n, (TABLES_SUIVIES.len() * 3) as i64);
    }

    /// Une table suivie absente : RIEN n'est posé (transaction), `lire` rend
    /// `None`, et aucun cache ne s'appuie sur un compteur incomplet.
    #[test]
    fn compteur_incomplet_jamais_pose() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE tracks (id INTEGER PRIMARY KEY, title TEXT);")
            .unwrap();
        assert!(db.execute_batch(&sql_sqlite()).is_err());
        db.execute_batch("ROLLBACK").ok();
        assert_eq!(lire(&db), None);
    }
}
