use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for the unified `queue_items` table (v0.9 rc.2).
///
/// A single table holds both local tracks (`track_id` set, `source='local'`)
/// and streaming tracks (`track_id` NULL, inline metadata). The two subsets are
/// discriminated by `track_id IS [NOT] NULL` and keep independent position
/// spaces, preserving the exact behaviour of the former split
/// `play_queue` / `streaming_queue` tables.
pub mod sql {
    use super::SqlDialect;

    pub fn queue_select_base() -> &'static str {
        "SELECT q.id, q.zone_id, q.track_id, q.position, q.is_current, t.title, ar.name, al.title, t.duration_ms, t.file_path, COALESCE(t.cover_path, al.cover_path), t.format, t.sample_rate, t.bit_depth FROM queue_items q LEFT JOIN tracks t ON q.track_id = t.id LEFT JOIN albums al ON t.album_id = al.id LEFT JOIN artists ar ON t.artist_id = ar.id"
    }

    pub fn get_queue<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} AND q.track_id IS NOT NULL ORDER BY q.position",
            queue_select_base(),
            d.placeholder(1)
        )
    }

    pub fn get_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} AND q.track_id IS NOT NULL AND q.is_current = '1'",
            queue_select_base(),
            d.placeholder(1)
        )
    }

    pub fn delete_for_zone<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn insert_queue_row<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) VALUES ({}, {}, {}, {}, 'local')",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4)
        )
    }

    pub fn max_position<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM queue_items WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn insert_queue_row_no_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) VALUES ({}, {}, {}, 0, 'local')",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn unset_current<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 0 WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn set_current_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 1 WHERE zone_id = {} AND position = {} AND track_id IS NOT NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND position = {} AND track_id IS NOT NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_after_delete<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position - 1 WHERE zone_id = {} AND position > {} AND track_id IS NOT NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// Make room for a mid-queue insert: shift every existing row at
    /// `position >= start` up by `count`, so the inserted rows land on
    /// now-free positions instead of colliding. `{1}=count, {2}=zone, {3}=start`.
    pub fn shift_positions_up<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position + {} WHERE zone_id = {} AND position >= {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND track_id IS NULL",
            d.placeholder(1)
        )
    }

    pub fn insert_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, position, source_id, title, artist, album, cover_url, duration_ms, source, track_number, disc_number) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10),
            d.placeholder(11)
        )
    }

    pub fn select_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT source_id, title, artist, album, cover_url, duration_ms, position, source, track_number, disc_number FROM queue_items WHERE zone_id = {} AND track_id IS NULL ORDER BY position",
            d.placeholder(1)
        )
    }

    pub fn count_queue<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM queue_items WHERE zone_id = {} AND track_id IS NOT NULL",
            d.placeholder(1)
        )
    }

    pub fn delete_streaming_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND position = {} AND track_id IS NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_streaming_after_delete<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position - 1 WHERE zone_id = {} AND position > {} AND track_id IS NULL",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_streaming<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM queue_items WHERE zone_id = {} AND track_id IS NULL",
            d.placeholder(1)
        )
    }

    // ─────────────────────────────────────────────────────────────────────
    // Unified single-position-space API (Lot 1 of the queue unification).
    // These builders treat the whole zone queue as ONE ordered sequence,
    // regardless of source (local `track_id` set, streaming `track_id` NULL).
    // Display fields COALESCE the joined track/album/artist (local) with the
    // inline columns (streaming). Added alongside the legacy split builders;
    // callers switch over in Lot 2.
    // ─────────────────────────────────────────────────────────────────────

    pub fn unified_select_base() -> &'static str {
        "SELECT q.id, q.zone_id, q.track_id, q.position, q.is_current, q.source, \
                COALESCE(t.title, q.title), COALESCE(ar.name, q.artist), \
                COALESCE(al.title, q.album), q.source_id, \
                COALESCE(t.duration_ms, q.duration_ms), t.file_path, \
                COALESCE(t.cover_path, al.cover_path, q.cover_url), t.format, t.sample_rate, t.bit_depth, \
                q.track_number, q.disc_number \
         FROM queue_items q \
         LEFT JOIN tracks t ON q.track_id = t.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id"
    }

    pub fn get_ordered<D: SqlDialect>(d: &D) -> String {
        format!(
            "{} WHERE q.zone_id = {} ORDER BY q.position",
            unified_select_base(),
            d.placeholder(1)
        )
    }

    pub fn get_at<D: SqlDialect>(d: &D) -> String {
        format!(
            // `ORDER BY q.position` ne DEPARTAGE RIEN quand deux lignes portent
            // la meme position — et une file heritee en porte, voir
            // `append_streaming_queue` (#2055). La ligne rendue etait alors
            // celle que le plan atteignait en premier : meme position, autre
            // piste, sans qu'aucun message ne le dise. C'est le mal de #3074,
            // transpose a la file. `q.id` rend l'ordre TOTAL : a defaut de
            // pouvoir designer LA bonne ligne d'une file deja abimee, on rend
            // toujours la MEME, et « suivant » redevient reproductible.
            "{} WHERE q.zone_id = {} AND q.position = {} ORDER BY q.position, q.id LIMIT 1",
            unified_select_base(),
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn count_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM queue_items WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    /// Lesquels de `n` identifiants possedent encore une ligne dans `tracks`.
    ///
    /// 🔴 #3231 — `set_queue` tranche l'existence AVANT d'inserer, pour que la
    /// position puisse compter les insertions reussies au lieu des tours de
    /// boucle. La liste `IN` est construite ici, avec les marqueurs du dialecte :
    /// `?` sur SQLite, `$1..$n` sur PostgreSQL, ou l'ORDRE et le NUMERO comptent.
    /// L'appelant borne `n` (paquets de 500) sous le plafond de 65535 parametres
    /// de PostgreSQL.
    pub fn tracks_existing_in<D: SqlDialect>(d: &D, n: usize) -> String {
        let list = (1..=n)
            .map(|i| d.placeholder(i))
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT id FROM tracks WHERE id IN ({list})")
    }

    pub fn max_position_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COALESCE(MAX(position), -1) FROM queue_items WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    /// Shift positions of every row at/after `from` up by `by`, to open a gap
    /// for an insertion. Placeholders: 1=by, 2=zone_id, 3=from.
    pub fn shift_positions<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position + {} WHERE zone_id = {} AND position >= {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn insert_local_at<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) VALUES ({}, {}, {}, 0, 'local')",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    // insert_streaming (position-explicit) is reused from the legacy builder.

    pub fn delete_at_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM queue_items WHERE zone_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn reindex_after_delete_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = position - 1 WHERE zone_id = {} AND position > {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn unset_current_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 0 WHERE zone_id = {}",
            d.placeholder(1)
        )
    }

    pub fn set_current_at_any<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET is_current = 1 WHERE zone_id = {} AND position = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn set_position_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE queue_items SET position = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_by_id<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM queue_items WHERE id = {}", d.placeholder(1))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: i64,
    pub zone_id: i64,
    pub track_id: i64,
    pub position: i64,
    pub is_current: bool,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub duration_ms: Option<i64>,
    pub file_path: Option<String>,
    pub cover_path: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
}

/// One streaming item for the legacy tuple-based enqueue API
/// (`set_streaming_queue` / `append_streaming_queue` / `persist_streaming_queue`):
/// `(source_id, title, artist, album, cover_url, duration_ms, source,
/// track_number, disc_number)`. The trailing `track_number`/`disc_number` carry
/// the album's own numbering so multi-disc streaming albums stay distinguishable
/// in the queue (#1062); pass `None` when the source has no numbering.
pub type StreamingQueueItem = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

/// A queue row in the unified single-position-space model. Unlike `QueueItem`
/// (local-only, `track_id: i64`), this represents BOTH local and streaming
/// items: `track_id`/`file_path` are set for local, `source_id` for streaming,
/// and the display fields are already COALESCE-d from the right origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    pub id: i64,
    pub zone_id: i64,
    pub track_id: Option<i64>,
    pub position: i64,
    pub is_current: bool,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub duration_ms: Option<i64>,
    pub file_path: Option<String>,
    pub cover_path: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    /// Album track number (streaming items only; local items read it from the
    /// joined `tracks` row). NULL for pre-existing rows and local items.
    pub track_number: Option<i64>,
    /// Album disc number (streaming items only). NULL for pre-existing rows and
    /// local items. Lets multi-disc streaming albums keep per-disc numbering.
    pub disc_number: Option<i64>,
}

impl QueueEntry {
    pub fn is_local(&self) -> bool {
        self.track_id.is_some()
    }
}

/// An item to enqueue, source-agnostic. Used by the unified `insert_at`/`append`.
#[derive(Debug, Clone)]
pub enum QueueInput {
    Local {
        track_id: i64,
    },
    Streaming {
        source: String,
        source_id: String,
        title: String,
        artist: String,
        album: Option<String>,
        cover_url: Option<String>,
        duration_ms: i64,
        track_number: Option<i64>,
        disc_number: Option<i64>,
    },
}

/// What `set_queue` actually managed to persist.
///
/// 🔴 #3231 — une file qui perd des lignes doit le DIRE. `set_queue` saute en
/// silence tout identifiant sans ligne dans `tracks` ; avant ce type, l'appelant
/// n'avait AUCUN moyen de l'apprendre, et le seul indice — un `count_all` plus
/// court que demandé — était indiscernable d'une file volontairement courte.
/// #2394 : un compteur qui ment est pire qu'un compteur absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetQueueOutcome {
    /// Nombre d'identifiants passés à `set_queue`.
    pub requested: usize,
    /// Nombre de lignes réellement écrites. C'est AUSSI la longueur de la file
    /// et le successeur de la dernière position, puisque les positions sont
    /// denses : `inserted == count_all == max(position) + 1`.
    pub inserted: usize,
    /// Les identifiants sans ligne dans `tracks`, dans l'ordre demandé.
    pub skipped: Vec<i64>,
}

impl SetQueueOutcome {
    /// Combien d'identifiants demandés ne sont jamais entrés dans la file.
    pub fn skipped_count(&self) -> usize {
        self.skipped.len()
    }

    /// Vrai dès qu'au moins une ligne est tombée.
    pub fn has_loss(&self) -> bool {
        !self.skipped.is_empty()
    }
}

pub struct PlayQueueRepo {
    db: Arc<dyn DbBackend>,
}

impl PlayQueueRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    fn dialect_sql<F1, F2>(&self, sqlite: F1, postgres: F2) -> String
    where
        F1: FnOnce(&SqliteDialect) -> String,
        F2: FnOnce(&PostgresDialect) -> String,
    {
        match self.db.engine() {
            Engine::Sqlite => sqlite(&SqliteDialect),
            Engine::Postgres => postgres(&PostgresDialect),
        }
    }

    pub fn get_queue(&self, zone_id: i64) -> Result<Vec<QueueItem>, String> {
        // WAL fallback pattern: read first, fall back to strong if 0.
        let sql = self.dialect_sql(sql::get_queue, sql::get_queue);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let rows = self.db.query_many(&sql, &params)?;
        if !rows.is_empty() {
            return Ok(rows.iter().map(row_to_queue_item).collect());
        }
        let strong = self.db.query_many_strong(&sql, &params)?;
        Ok(strong.iter().map(row_to_queue_item).collect())
    }

    pub fn get_current(&self, zone_id: i64) -> Result<Option<QueueItem>, String> {
        let sql = self.dialect_sql(sql::get_current, sql::get_current);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_queue_item))
    }

    /// Le sous-ensemble de `ids` qui possède encore une ligne dans `tracks`,
    /// interrogé DANS la transaction de l'appelant pour que la réponse ne puisse
    /// pas périmer avant l'insertion.
    fn existing_track_ids(
        &self,
        tx: &dyn DbTxHandle,
        ids: &[i64],
    ) -> Result<std::collections::HashSet<i64>, String> {
        let mut found: std::collections::HashSet<i64> = std::collections::HashSet::new();
        if ids.is_empty() {
            return Ok(found);
        }
        // Dédoublonner d'abord : une liste de lecture peut légitimement répéter
        // une piste, et la liste `IN` est bornée par le plafond de 65535
        // paramètres de PostgreSQL. Les paquets de 500 tiennent sur les deux
        // moteurs quelle que soit la taille de la file.
        let mut uniques: Vec<i64> = ids.to_vec();
        uniques.sort_unstable();
        uniques.dedup();
        for chunk in uniques.chunks(500) {
            let sql = self.dialect_sql(
                |d| sql::tracks_existing_in(d, chunk.len()),
                |d| sql::tracks_existing_in(d, chunk.len()),
            );
            let params: Vec<&dyn ToSqlValue> = chunk.iter().map(|v| v as &dyn ToSqlValue).collect();
            for row in tx.query_many(&sql, &params)? {
                if let Some(id) = row.first().and_then(|v| v.as_i64()) {
                    found.insert(id);
                }
            }
        }
        Ok(found)
    }

    /// Remplace la file de la zone par `track_ids`, et REND COMPTE de ce qui est
    /// tombé.
    ///
    /// 🔴 #3231 (Pierre M, fil forum 978) — « une compilation de 190 titres
    /// bascule en suggestions après 4 titres ».
    ///
    /// L'insertion est gardée : un identifiant sans ligne dans `tracks` est sauté
    /// au lieu de lever « FOREIGN KEY constraint failed » et d'annuler tout le
    /// remplacement (JP Borderies : suppression + ré-ingestion → lecture coupée).
    /// Mais `position` valait l'INDICE DE BOUCLE, donc chaque identifiant sauté
    /// laissait un TROU : 190 demandés dont 5 survivants donnaient des positions
    /// du genre 0, 37, 88, 120, 150 — pendant que `count_all`, un `SELECT
    /// COUNT(*)` nu, répondait 5.
    ///
    /// Rien en aval ne sait parcourir une telle file. `next_position_inner`
    /// (poller.rs) est de l'arithmétique pure sur les positions —
    /// `queue_position + 1`, `% queue_length`, et l'arrêt `next >= queue_length`
    /// — et `Orchestrator::play_from_queue` résout le résultat avec `get_at`, un
    /// `WHERE q.position = ?` littéral. Les deux ne sont justes que si les
    /// positions valent exactement `0..count_all-1`. Avec des trous, la marche
    /// réclame les positions 1 à 4, ne trouve aucune ligne, et la zone bascule
    /// dans l'autoplay : les suggestions après 4 titres de Pierre M.
    ///
    /// La position compte donc désormais les INSERTIONS RÉUSSIES, pas les tours
    /// de boucle, et le nombre d'identifiants tombés est rendu ET journalisé.
    /// Une file qui perd des lignes le dit (#2394 : un compteur qui ment est pire
    /// qu'un compteur absent).
    pub fn set_queue(&self, zone_id: i64, track_ids: &[i64]) -> Result<SetQueueOutcome, String> {
        let delete_sql = self.dialect_sql(sql::delete_for_zone, sql::delete_for_zone);
        // set_queue is a FULL replacement of the zone's queue: the streaming
        // subset must go too. It only deleted the local subset, so streaming
        // rows (autoplay leftovers, old streaming albums) were IMMORTAL —
        // every subsequent album/playlist play interleaved its tracks with
        // yesterday's ghosts by position (Villerio: 10 DSD tracks woven into
        // 77 stale Qobuz autoplay entries; likely forum #1202/#1049 too).
        let delete_streaming_sql = self.dialect_sql(sql::delete_streaming, sql::delete_streaming);
        let ph = |i: usize| match self.db.engine() {
            Engine::Sqlite => SqliteDialect.placeholder(i),
            Engine::Postgres => PostgresDialect.placeholder(i),
        };
        // Le `WHERE EXISTS` RESTE. L'existence est tranchée juste au-dessus, dans
        // la même transaction, donc en régime normal chaque insertion écrit
        // exactement une ligne ; la garde ne coûte rien et empêche encore un
        // rescan concurrent de transformer une ligne disparue en remplacement
        // annulé.
        //
        // ⚠️ Elle PEUT de nouveau rendre zéro ligne, et c'est désormais sans
        // danger. Historique, à ne pas reperdre : `PgTxHandle::execute` ajoute
        // « RETURNING id » à tout INSERT nu, puis appelait `fetch_one`
        // (backend.rs). Une insertion gardée qui n'écrit rien n'y rend aucune
        // ligne : `fetch_one` échouait et annulait TOUTE la transaction — la
        // garde censée sauver la file la vidait entièrement sur PG. #3231
        // (PR #3247) a retiré le DÉCLENCHEUR en pré-filtrant ici ; #3248 a
        // retiré le DÉFAUT en passant `execute` à `fetch_all`, qui rend le vrai
        // nombre de lignes. Zéro ligne est maintenant un `Ok(0)` ordinaire.
        //
        // C'est pourquoi le compteur ci-dessous suit le nombre de lignes RENDU
        // PAR `execute` et non l'indice de boucle : c'est la seule valeur qui
        // dit la vérité sur les DEUX moteurs. Un `+= 1` inconditionnel
        // compterait une ligne fantôme quand la garde mord (course avec un
        // rescan concurrent), creusant un trou dans `position` et pouvant
        // laisser la file sans aucune ligne courante.
        let insert_sql = format!(
            "INSERT INTO queue_items (zone_id, track_id, position, is_current, source) \
             SELECT {}, {}, {}, {}, 'local' WHERE EXISTS (SELECT 1 FROM tracks WHERE id = {})",
            ph(1),
            ph(2),
            ph(3),
            ph(4),
            ph(5)
        );
        let mut outcome = SetQueueOutcome {
            requested: track_ids.len(),
            ..Default::default()
        };
        self.db.write_tx(&mut |tx| {
            // `write_tx` prend un FnMut : repartir d'une ardoise propre pour qu'une
            // fermeture rejouée ne compte pas deux fois la même perte.
            outcome.inserted = 0;
            outcome.skipped.clear();
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&delete_sql, &p)?;
            tx.execute(&delete_streaming_sql, &p)?;
            let existing = self.existing_track_ids(tx, track_ids)?;
            for tid in track_ids {
                if !existing.contains(tid) {
                    outcome.skipped.push(*tid);
                    continue;
                }
                // `position` ET `is_current` suivent le nombre de lignes ÉCRITES.
                // Prendre l'indice de boucle laissait des trous — et laissait la
                // file SANS AUCUNE ligne courante dès que le tout premier
                // identifiant était celui qui manquait.
                let pos = outcome.inserted as i64;
                let is_current = if outcome.inserted == 0 { 1i64 } else { 0i64 };
                let p: [&dyn ToSqlValue; 5] = [&zone_id, tid, &pos, &is_current, tid];
                // #3248 — compter les lignes RÉELLEMENT écrites. Zéro ligne
                // signifie que la piste a disparu entre le pré-filtre et
                // l'INSERT : c'est une perte, elle se déclare dans `skipped`
                // pour que `has_loss()` la voie, et `position` reste dense.
                let ecrites = tx.execute(&insert_sql, &p)?;
                if ecrites == 0 {
                    outcome.skipped.push(*tid);
                    continue;
                }
                outcome.inserted += ecrites;
            }
            Ok(())
        })?;
        if outcome.has_loss() {
            let apercu: Vec<i64> = outcome.skipped.iter().take(10).copied().collect();
            warn!(
                zone_id,
                demandees = outcome.requested,
                inserees = outcome.inserted,
                absentes = outcome.skipped.len(),
                apercu_ids_absents = ?apercu,
                "set_queue_pistes_absentes"
            );
        }
        Ok(outcome)
    }

    pub fn add_tracks(
        &self,
        zone_id: i64,
        track_ids: &[i64],
        position: Option<i64>,
    ) -> Result<(), String> {
        let max_pos_sql = self.dialect_sql(sql::max_position, sql::max_position);
        let insert_sql = self.dialect_sql(
            sql::insert_queue_row_no_current,
            sql::insert_queue_row_no_current,
        );
        let shift_sql = self.dialect_sql(sql::shift_positions_up, sql::shift_positions_up);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &p)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let start = position.unwrap_or(max_pos + 1);
            // Mid-queue insert (explicit position, e.g. "Play next"): shift the
            // existing rows up first so the new rows don't collide on `position`.
            // Without this, inserting at an occupied position left two rows with
            // the same position — a corrupted queue where "play from here" and
            // gapless advance resolved to the wrong track (Bertrand: a "Play next"
            // on an album produced a duplicate entry + duplicate position). Append
            // (position = None → start = max+1) matches nothing, so this is a no-op.
            let count = track_ids.len() as i64;
            if position.is_some() && count > 0 {
                let sp: [&dyn ToSqlValue; 3] = [&count, &zone_id, &start];
                tx.execute(&shift_sql, &sp)?;
            }
            for (i, tid) in track_ids.iter().enumerate() {
                let pos = start + i as i64;
                let p: [&dyn ToSqlValue; 3] = [&zone_id, tid, &pos];
                tx.execute(&insert_sql, &p)?;
            }
            Ok(())
        })
    }

    /// Append tracks at the end of the local queue for a zone.
    /// Convenience wrapper over add_tracks(zone_id, track_ids, None).
    pub fn append_tracks(&self, zone_id: i64, track_ids: &[i64]) -> Result<(), String> {
        self.add_tracks(zone_id, track_ids, None)
    }

    pub fn set_current(&self, zone_id: i64, position: i64) -> Result<(), String> {
        // unset-all-then-set-one needs to be atomic — between the two
        // UPDATEs, the zone would have zero "current" entries, which a
        // concurrent read could mistake for an empty queue. write_tx
        // serializes the pair.
        let unset_sql = self.dialect_sql(sql::unset_current, sql::unset_current);
        let set_sql = self.dialect_sql(sql::set_current_at, sql::set_current_at);
        self.db.write_tx(&mut |tx| {
            let p1: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&unset_sql, &p1)?;
            let p2: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            tx.execute(&set_sql, &p2)?;
            Ok(())
        })
    }

    pub fn remove_at(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        let delete_sql = self.dialect_sql(sql::delete_at, sql::delete_at);
        let reindex_sql = self.dialect_sql(sql::reindex_after_delete, sql::reindex_after_delete);
        let mut deleted = 0usize;
        let deleted_ref = &mut deleted;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            *deleted_ref = tx.execute(&delete_sql, &p)?;
            if *deleted_ref > 0 {
                tx.execute(&reindex_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(deleted > 0)
    }

    /// Remove a streaming track (track_id NULL) at the given position.
    /// Returns true if a row was actually deleted.
    pub fn remove_streaming_at(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        let delete_sql = self.dialect_sql(sql::delete_streaming_at, sql::delete_streaming_at);
        let reindex_sql = self.dialect_sql(
            sql::reindex_streaming_after_delete,
            sql::reindex_streaming_after_delete,
        );
        let mut deleted = 0usize;
        let deleted_ref = &mut deleted;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            *deleted_ref = tx.execute(&delete_sql, &p)?;
            if *deleted_ref > 0 {
                tx.execute(&reindex_sql, &p)?;
            }
            Ok(())
        })?;
        Ok(deleted > 0)
    }

    /// Count streaming tracks (track_id NULL) for a zone.
    pub fn count_streaming(&self, zone_id: i64) -> Result<i64, String> {
        let count_sql = self.dialect_sql(sql::count_streaming, sql::count_streaming);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let n = self
            .db
            .query_one(&count_sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n)
    }

    pub fn clear(&self, zone_id: i64) -> Result<(), String> {
        // Delete both subsets (local + streaming) of the unified table.
        let delete_local = self.dialect_sql(sql::delete_for_zone, sql::delete_for_zone);
        let delete_streaming = self.dialect_sql(sql::delete_streaming, sql::delete_streaming);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        self.db.execute(&delete_local, &params)?;
        self.db.execute(&delete_streaming, &params)?;
        Ok(())
    }

    // ── Unified single-position-space API (Lot 1) ─────────────────────────
    // Added alongside the legacy split methods; route/orchestrator callers
    // switch to these in Lot 2 (with the position-renumbering migration).

    /// Total number of items in the zone queue (local + streaming).
    pub fn count_all(&self, zone_id: i64) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::count_all, sql::count_all);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let n = self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        Ok(n)
    }

    /// The whole zone queue as ONE ordered sequence (local + streaming).
    pub fn get_ordered(&self, zone_id: i64) -> Result<Vec<QueueEntry>, String> {
        let sql = self.dialect_sql(sql::get_ordered, sql::get_ordered);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let rows = self.db.query_many(&sql, &params)?;
        if !rows.is_empty() {
            return Ok(rows.iter().map(row_to_queue_entry).collect());
        }
        let strong = self.db.query_many_strong(&sql, &params)?;
        Ok(strong.iter().map(row_to_queue_entry).collect())
    }

    /// The single queue entry at `position` (source-agnostic), if any.
    pub fn get_at(&self, zone_id: i64, position: i64) -> Result<Option<QueueEntry>, String> {
        let sql = self.dialect_sql(sql::get_at, sql::get_at);
        let params: [&dyn ToSqlValue; 2] = [&zone_id, &position];
        Ok(self
            .db
            .query_one(&sql, &params)?
            .as_ref()
            .map(row_to_queue_entry))
    }

    /// Insert items at `position` (or append when None) in the unified space,
    /// shifting every existing row at/after the insert point up to make room.
    /// This is the basis of "Play Next" (position = current + 1): a streaming
    /// item added while a local album plays now lands right after the current
    /// track instead of at the end of the album (Sandro S1).
    ///
    /// Renvoie la position **réelle** de la première ligne insérée, `None`
    /// quand `items` est vide — aucune ligne, donc aucune position.
    ///
    /// Cette valeur n'est pas décorative : `position` est **ramenée** dans
    /// `0..=max_pos + 1`. Un client qui demande « juste après la piste en
    /// cours » sur une file plus courte que son calcul obtient un ajout en FIN
    /// de file, et l'écriture réussit dans les deux cas. Sans la position
    /// effective, un appelant ne peut pas distinguer les deux (#2079) : il ne
    /// lui reste qu'à relire toute la file pour savoir ce qu'il vient de faire.
    pub fn insert_at(
        &self,
        zone_id: i64,
        items: &[QueueInput],
        position: Option<i64>,
    ) -> Result<Option<i64>, String> {
        if items.is_empty() {
            return Ok(None);
        }
        let max_pos_sql = self.dialect_sql(sql::max_position_any, sql::max_position_any);
        let shift_sql = self.dialect_sql(sql::shift_positions, sql::shift_positions);
        let insert_local_sql = self.dialect_sql(sql::insert_local_at, sql::insert_local_at);
        let insert_streaming_sql = self.dialect_sql(sql::insert_streaming, sql::insert_streaming);
        let n = items.len() as i64;
        // Renseignée DANS la transaction, lue après elle : `write_tx` peut
        // rejouer la fermeture (base occupée), et c'est le dernier passage —
        // celui qui a réellement commité — qui doit gagner.
        let mut start_effectif: i64 = 0;
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            let max_pos: i64 = tx
                .query_one(&max_pos_sql, &p)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1);
            let start = position.unwrap_or(max_pos + 1).clamp(0, max_pos + 1);
            start_effectif = start;
            // Open a gap of `n` at `start` (no-op when appending at the end).
            let sp: [&dyn ToSqlValue; 3] = [&n, &zone_id, &start];
            tx.execute(&shift_sql, &sp)?;
            for (i, item) in items.iter().enumerate() {
                let pos = start + i as i64;
                match item {
                    QueueInput::Local { track_id } => {
                        let p: [&dyn ToSqlValue; 3] = [&zone_id, track_id, &pos];
                        tx.execute(&insert_local_sql, &p)?;
                    }
                    QueueInput::Streaming {
                        source,
                        source_id,
                        title,
                        artist,
                        album,
                        cover_url,
                        duration_ms,
                        track_number,
                        disc_number,
                    } => {
                        let p: [&dyn ToSqlValue; 11] = [
                            &zone_id,
                            &pos,
                            source_id,
                            title,
                            artist,
                            album,
                            cover_url,
                            duration_ms,
                            source,
                            track_number,
                            disc_number,
                        ];
                        tx.execute(&insert_streaming_sql, &p)?;
                    }
                }
            }
            Ok(())
        })?;
        Ok(Some(start_effectif))
    }

    /// Append items at the end of the unified queue.
    pub fn append(&self, zone_id: i64, items: &[QueueInput]) -> Result<(), String> {
        self.insert_at(zone_id, items, None).map(|_| ())
    }

    /// Remove the item at `position` (source-agnostic) and close the gap.
    pub fn remove_pos(&self, zone_id: i64, position: i64) -> Result<bool, String> {
        // `position` from the client is an ORDINAL into the displayed queue
        // (the get_ordered order), not necessarily the stored `position` column.
        // Those diverge when local and streaming rows were inserted through
        // different code paths, leaving gaps/overlaps in the position space —
        // a plain `DELETE WHERE position = ?` then matches the wrong row or none
        // at all, so the item (e.g. a stray Qobuz "Piste inconnue") can't be
        // removed. Resolve the Nth row, delete it by id, and renumber the rest
        // to a contiguous 0..N-1 space so the queue self-heals (same strategy
        // as move_pos).
        let entries = self.get_ordered(zone_id)?;
        if position < 0 || position as usize >= entries.len() {
            return Ok(false);
        }
        let target_id = entries[position as usize].id;
        let delete_sql = self.dialect_sql(sql::delete_by_id, sql::delete_by_id);
        let set_pos_sql = self.dialect_sql(sql::set_position_by_id, sql::set_position_by_id);
        self.db.write_tx(&mut |tx| {
            let dp: [&dyn ToSqlValue; 1] = [&target_id];
            tx.execute(&delete_sql, &dp)?;
            let mut new_pos = 0i64;
            for e in entries.iter() {
                if e.id == target_id {
                    continue;
                }
                let p: [&dyn ToSqlValue; 2] = [&new_pos, &e.id];
                tx.execute(&set_pos_sql, &p)?;
                new_pos += 1;
            }
            Ok(())
        })?;
        Ok(true)
    }

    /// Mark the item at `position` as current (source-agnostic). Unlike the
    /// legacy `set_current` (local rows only), a streaming item can be current.
    pub fn set_current_pos(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let unset_sql = self.dialect_sql(sql::unset_current_any, sql::unset_current_any);
        let set_sql = self.dialect_sql(sql::set_current_at_any, sql::set_current_at_any);
        self.db.write_tx(&mut |tx| {
            let p1: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&unset_sql, &p1)?;
            let p2: [&dyn ToSqlValue; 2] = [&zone_id, &position];
            tx.execute(&set_sql, &p2)?;
            Ok(())
        })
    }

    /// Move the item at `from` to `to` within the unified space, renumbering
    /// the affected rows so positions stay contiguous (0..N-1).
    pub fn move_pos(&self, zone_id: i64, from: i64, to: i64) -> Result<(), String> {
        if from == to {
            return Ok(());
        }
        let mut entries = self.get_ordered(zone_id)?;
        let len = entries.len() as i64;
        if from < 0 || from >= len || to < 0 || to >= len {
            return Ok(());
        }
        let item = entries.remove(from as usize);
        entries.insert(to as usize, item);
        let set_pos_sql = self.dialect_sql(sql::set_position_by_id, sql::set_position_by_id);
        self.db.write_tx(&mut |tx| {
            for (i, e) in entries.iter().enumerate() {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 2] = [&pos, &e.id];
                tx.execute(&set_pos_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn set_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[StreamingQueueItem],
    ) -> Result<(), String> {
        let delete_local_sql = self.dialect_sql(sql::delete_for_zone, sql::delete_for_zone);
        let delete_streaming_sql = self.dialect_sql(sql::delete_streaming, sql::delete_streaming);
        let insert_streaming_sql = self.dialect_sql(sql::insert_streaming, sql::insert_streaming);
        self.db.write_tx(&mut |tx| {
            let p: [&dyn ToSqlValue; 1] = [&zone_id];
            tx.execute(&delete_local_sql, &p)?;
            tx.execute(&delete_streaming_sql, &p)?;
            for (
                i,
                (
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                    source,
                    track_no,
                    disc_no,
                ),
            ) in tracks.iter().enumerate()
            {
                let pos = i as i64;
                let p: [&dyn ToSqlValue; 11] = [
                    &zone_id,
                    &pos,
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                    source,
                    track_no,
                    disc_no,
                ];
                tx.execute(&insert_streaming_sql, &p)?;
            }
            Ok(())
        })
    }

    /// Ajoute des pistes de service A LA FIN de la file — de la file ENTIERE.
    ///
    /// 🔴 #2055 — ce site etait le DERNIER a numeroter dans l'espace de
    /// positions par source que la migration 53 (`unify_queue_positions`) a
    /// justement aboli. Il partait de `count_streaming`, qui ne compte que les
    /// lignes de service : sur la file d'un album, purement locale, ce compte
    /// vaut 0. Les pistes ajoutees par l'autoplay du sondeur — seul appelant,
    /// `poller.rs`, `autoplay_streaming_*` — s'ecrivaient donc aux positions
    /// 0, 1, 2 PAR-DESSUS les pistes 0, 1, 2 de l'album.
    ///
    /// Mesure sur une file de 5 pistes locales suivie de 3 ajouts :
    ///  - `count_all` rend 8, alors que les positions s'arretent a 4. Les
    ///    index 5, 6 et 7 que `next_position` parcourt n'ont AUCUNE ligne :
    ///    `play_from_queue` echoue sur « no queue item at position », la
    ///    route « suivant » a deja repondu `"playing"`, et rien ne sort ;
    ///  - `get_at(0..2)` doit departager deux lignes de meme position, avec
    ///    un `ORDER BY` qui ne les departage pas — meme position, autre
    ///    piste, sans qu'aucun message ne le dise (le mal de #3074) ;
    ///  - les trois pistes ajoutees sont INJOIGNABLES, et `get_ordered` les
    ///    entrelace au milieu de l'album — « 10 pistes DSD tissees dans 77
    ///    entrees Qobuz » (Villerio), « la fleche suivant choisit une piste
    ///    aleatoire, heureusement cela reste dans le meme album » (Tades).
    ///
    /// On numerote donc a partir du `MAX(position)` de la file ENTIERE, et
    /// dans la transaction — comme `insert_at`, et pour la meme raison : lu
    /// avant elle, le maximum serait perime des qu'un second ecrivain passe,
    /// et `write_tx` peut rejouer sa fermeture sur base occupee.
    pub fn append_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[StreamingQueueItem],
    ) -> Result<(), String> {
        let insert_streaming_sql = self.dialect_sql(sql::insert_streaming, sql::insert_streaming);
        let max_pos_sql = self.dialect_sql(sql::max_position_any, sql::max_position_any);
        self.db.write_tx(&mut |tx| {
            let mp: [&dyn ToSqlValue; 1] = [&zone_id];
            let current_count: i64 = tx
                .query_one(&max_pos_sql, &mp)?
                .as_ref()
                .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
                .unwrap_or(-1)
                + 1;
            for (
                i,
                (
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                    source,
                    track_no,
                    disc_no,
                ),
            ) in tracks.iter().enumerate()
            {
                let pos = current_count + i as i64;
                let p: [&dyn ToSqlValue; 11] = [
                    &zone_id,
                    &pos,
                    source_id,
                    title,
                    artist,
                    album,
                    cover_url,
                    duration_ms,
                    source,
                    track_no,
                    disc_no,
                ];
                tx.execute(&insert_streaming_sql, &p)?;
            }
            Ok(())
        })
    }

    pub fn get_streaming_queue(&self, zone_id: i64) -> Result<Vec<serde_json::Value>, String> {
        let select_sql = self.dialect_sql(sql::select_streaming, sql::select_streaming);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let rows = self.db.query_many(&select_sql, &params)?;
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|cols| {
                serde_json::json!({
                    "source_id": cols.first().and_then(|v| v.as_string()),
                    "title": cols.get(1).and_then(|v| v.as_string()),
                    "artist_name": cols.get(2).and_then(|v| v.as_string()),
                    "album_title": cols.get(3).and_then(|v| v.as_string()),
                    "cover_path": cols.get(4).and_then(|v| v.as_string()),
                    "duration_ms": cols.get(5).and_then(|v| v.as_i64()).unwrap_or(0),
                    "position": cols.get(6).and_then(|v| v.as_i64()).unwrap_or(0),
                    "source": cols.get(7).and_then(|v| v.as_string()),
                    "track_number": cols.get(8).and_then(|v| v.as_i64()),
                    "disc_number": cols.get(9).and_then(|v| v.as_i64()),
                })
            })
            .collect();
        Ok(items)
    }

    pub fn count(&self, zone_id: i64) -> Result<i64, String> {
        let count_sql = self.dialect_sql(sql::count_queue, sql::count_queue);
        let params: [&dyn ToSqlValue; 1] = [&zone_id];
        let n = self
            .db
            .query_one(&count_sql, &params)?
            .as_ref()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if n > 0 {
            return Ok(n);
        }
        // WAL fallback: read connection may lag behind the writer.
        let strong = self.db.query_many_strong(&count_sql, &params)?;
        Ok(strong
            .first()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()))
            .unwrap_or(0))
    }
}

fn row_to_queue_item(cols: &Vec<SqlValue>) -> QueueItem {
    QueueItem {
        id: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        zone_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        track_id: cols.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
        position: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
        is_current: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        title: cols.get(5).and_then(|v| v.as_string()),
        artist_name: cols.get(6).and_then(|v| v.as_string()),
        album_title: cols.get(7).and_then(|v| v.as_string()),
        duration_ms: cols.get(8).and_then(|v| v.as_i64()),
        file_path: cols.get(9).and_then(|v| v.as_string()),
        cover_path: cols.get(10).and_then(|v| v.as_string()),
        format: cols.get(11).and_then(|v| v.as_string()),
        sample_rate: cols.get(12).and_then(|v| v.as_i64()),
        bit_depth: cols.get(13).and_then(|v| v.as_i64()),
    }
}

/// Maps a row from `sql::unified_select_base()` (18 columns) to a QueueEntry.
fn row_to_queue_entry(cols: &Vec<SqlValue>) -> QueueEntry {
    QueueEntry {
        id: cols.first().and_then(|v| v.as_i64()).unwrap_or(0),
        zone_id: cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        track_id: cols.get(2).and_then(|v| v.as_i64()),
        position: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
        is_current: cols.get(4).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        source: cols.get(5).and_then(|v| v.as_string()),
        title: cols.get(6).and_then(|v| v.as_string()),
        artist_name: cols.get(7).and_then(|v| v.as_string()),
        album_title: cols.get(8).and_then(|v| v.as_string()),
        source_id: cols.get(9).and_then(|v| v.as_string()),
        duration_ms: cols.get(10).and_then(|v| v.as_i64()),
        file_path: cols.get(11).and_then(|v| v.as_string()),
        cover_path: cols.get(12).and_then(|v| v.as_string()),
        format: cols.get(13).and_then(|v| v.as_string()),
        sample_rate: cols.get(14).and_then(|v| v.as_i64()),
        bit_depth: cols.get(15).and_then(|v| v.as_i64()),
        track_number: cols.get(16).and_then(|v| v.as_i64()),
        disc_number: cols.get(17).and_then(|v| v.as_i64()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Track;
    use crate::db::track_repo::TrackRepo;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db.execute(
            "INSERT INTO zones (name, output_type) VALUES ('Main', 'local')",
            &[],
        )
        .unwrap();
        db
    }

    #[test]
    fn queue_lifecycle() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("Song 1".into());
        t1.file_path = Some("/1.flac".into());
        let mut t2 = Track::new("Song 2".into());
        t2.file_path = Some("/2.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1, tid2]).unwrap();
        assert_eq!(repo.count(1).unwrap(), 2);

        let current = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current.track_id, tid1);
        assert!(current.is_current);

        repo.set_current(1, 1).unwrap();
        let current2 = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current2.track_id, tid2);

        repo.clear(1).unwrap();
        assert_eq!(repo.count(1).unwrap(), 0);
    }

    #[test]
    fn set_queue_purges_streaming_leftovers() {
        // Streaming rows (track_id NULL) used to survive set_queue — autoplay
        // leftovers were IMMORTAL and interleaved with every later album play
        // (Villerio: 10 DSD tracks woven into 77 stale Qobuz entries).
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        repo.append_streaming_queue(
            1,
            &[
                (
                    "qobuz".into(),
                    "111".into(),
                    "Ghost One".into(),
                    None,
                    None,
                    200_000,
                    None,
                    None,
                    None,
                ),
                (
                    "qobuz".into(),
                    "222".into(),
                    "Ghost Two".into(),
                    None,
                    None,
                    200_000,
                    None,
                    None,
                    None,
                ),
            ],
        )
        .unwrap();
        assert_eq!(repo.count_all(1).unwrap(), 2);

        let mut t = Track::new("Real Local".into());
        t.file_path = Some("/real.dsf".into());
        let tid = track_repo.create(&t).unwrap();
        repo.set_queue(1, &[tid]).unwrap();

        // Full replacement: the ghosts are gone, only the local track remains.
        assert_eq!(repo.count_all(1).unwrap(), 1);
        let current = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current.track_id, tid);
    }

    #[test]
    fn queue_add_tracks() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let mut t3 = Track::new("C".into());
        t3.file_path = Some("/c.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();
        let tid3 = track_repo.create(&t3).unwrap();

        repo.set_queue(1, &[tid1]).unwrap();
        repo.add_tracks(1, &[tid2, tid3], None).unwrap();

        assert_eq!(repo.count(1).unwrap(), 3);

        let queue = repo.get_queue(1).unwrap();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].track_id, tid1);
        assert_eq!(queue[1].track_id, tid2);
        assert_eq!(queue[2].track_id, tid3);
    }

    #[test]
    fn queue_add_at_position() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1]).unwrap();
        repo.add_tracks(1, &[tid2], Some(0)).unwrap();

        assert_eq!(repo.count(1).unwrap(), 2);
    }

    #[test]
    fn queue_insert_at_occupied_position_shifts_no_duplicates() {
        // Bertrand: a "Play next" (insert at the position right after current)
        // on an album produced a queue with TWO entries at the same position and
        // a duplicated track, which broke "play from here" / gapless advance.
        // Inserting at an occupied position must shift the existing rows up so
        // positions stay unique and contiguous.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut ids = Vec::new();
        for name in ["A", "B", "C"] {
            let mut t = Track::new(name.into());
            t.file_path = Some(format!("/{name}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }
        repo.set_queue(1, &ids).unwrap(); // A@0, B@1, C@2

        let mut x = Track::new("X".into());
        x.file_path = Some("/x.flac".into());
        let xid = track_repo.create(&x).unwrap();

        // "Play next" while A (position 0) is current → insert at position 1.
        repo.add_tracks(1, &[xid], Some(1)).unwrap();

        let queue = repo.get_queue(1).unwrap();
        // Expected order: A, X, B, C with contiguous unique positions 0..3.
        let order: Vec<i64> = queue.iter().map(|q| q.track_id).collect();
        assert_eq!(order, vec![ids[0], xid, ids[1], ids[2]]);
        let positions: Vec<i64> = queue.iter().map(|q| q.position).collect();
        assert_eq!(positions, vec![0, 1, 2, 3]);
        // No duplicate positions.
        let mut seen = std::collections::HashSet::new();
        assert!(
            positions.iter().all(|p| seen.insert(*p)),
            "duplicate position in queue: {positions:?}"
        );
    }

    #[test]
    fn queue_get_queue_ordered() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut tracks = Vec::new();
        for i in 0..5 {
            let mut t = Track::new(format!("Track {i}"));
            t.file_path = Some(format!("/{i}.flac"));
            let id = track_repo.create(&t).unwrap();
            tracks.push(id);
        }

        repo.set_queue(1, &tracks).unwrap();
        let queue = repo.get_queue(1).unwrap();
        assert_eq!(queue.len(), 5);
        for (i, item) in queue.iter().enumerate() {
            assert_eq!(item.position, i as i64);
            assert_eq!(item.track_id, tracks[i]);
        }
    }

    #[test]
    fn queue_empty_zone() {
        let db = test_db();
        let repo = PlayQueueRepo::new(db);

        let queue = repo.get_queue(1).unwrap();
        assert!(queue.is_empty());
        assert!(repo.get_current(1).unwrap().is_none());
        assert_eq!(repo.count(1).unwrap(), 0);
    }

    #[test]
    fn queue_first_track_is_current() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("First".into());
        t1.file_path = Some("/first.flac".into());
        let mut t2 = Track::new("Second".into());
        t2.file_path = Some("/second.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1, tid2]).unwrap();
        let current = repo.get_current(1).unwrap().unwrap();
        assert_eq!(current.track_id, tid1);
        assert!(current.is_current);
    }

    #[test]
    fn queue_streaming_queue() {
        let db = test_db();
        let repo = PlayQueueRepo::new(db);

        // Song 1 = disc 1 / track 1, Song 2 = disc 2 / track 1: a multi-disc album
        // whose per-disc numbering must survive the queue round-trip (#1062).
        let tracks = vec![
            (
                "src-1".into(),
                "Song 1".into(),
                "Artist 1".into(),
                Some("Album 1".into()),
                Some("http://cover1.jpg".into()),
                300_000i64,
                Some("tidal".into()),
                Some(1),
                Some(1),
            ),
            (
                "src-2".into(),
                "Song 2".into(),
                "Artist 2".into(),
                None,
                None,
                250_000i64,
                Some("tidal".into()),
                Some(1),
                Some(2),
            ),
        ];

        repo.set_streaming_queue(1, &tracks).unwrap();
        let queue = repo.get_streaming_queue(1).unwrap();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0]["title"], "Song 1");
        assert_eq!(queue[0]["artist_name"], "Artist 1");
        assert_eq!(queue[0]["duration_ms"], 300_000);
        assert_eq!(queue[0]["source"], "tidal");
        // #1062: the album's own track/disc numbers are persisted and read back,
        // so disc 2 track 1 stays "1 / disc 2", not conflated with `position`.
        assert_eq!(queue[0]["track_number"], 1);
        assert_eq!(queue[0]["disc_number"], 1);
        assert_eq!(queue[1]["title"], "Song 2");
        assert!(queue[1]["album_title"].is_null());
        assert_eq!(queue[1]["source"], "tidal");
        assert_eq!(queue[1]["track_number"], 1);
        assert_eq!(queue[1]["disc_number"], 2);
    }

    #[test]
    fn queue_streaming_queue_replace() {
        let db = test_db();
        let repo = PlayQueueRepo::new(db);

        let tracks1 = vec![(
            "id1".into(),
            "Old".into(),
            "Old Artist".into(),
            None,
            None,
            100_000i64,
            Some("qobuz".into()),
            None,
            None,
        )];
        repo.set_streaming_queue(1, &tracks1).unwrap();

        let tracks2 = vec![(
            "id2".into(),
            "New".into(),
            "New Artist".into(),
            None,
            None,
            200_000i64,
            Some("tidal".into()),
            None,
            None,
        )];
        repo.set_streaming_queue(1, &tracks2).unwrap();

        let queue = repo.get_streaming_queue(1).unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0]["title"], "New");
        assert_eq!(queue[0]["source"], "tidal");
    }

    #[test]
    fn queue_local_and_streaming_coexist_separately() {
        // Both subsets live in queue_items with independent position spaces.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("Local".into());
        t1.file_path = Some("/l.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        repo.set_queue(1, &[tid1]).unwrap();

        repo.append_streaming_queue(
            1,
            &[(
                "s1".into(),
                "Stream".into(),
                "SA".into(),
                None,
                None,
                123_000i64,
                Some("tidal".into()),
                None,
                None,
            )],
        )
        .unwrap();

        assert_eq!(repo.count(1).unwrap(), 1);
        assert_eq!(repo.count_streaming(1).unwrap(), 1);
        // Local read is unaffected by the streaming row.
        let local = repo.get_queue(1).unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].track_id, tid1);
        // clear() removes both subsets.
        repo.clear(1).unwrap();
        assert_eq!(repo.count(1).unwrap(), 0);
        assert_eq!(repo.count_streaming(1).unwrap(), 0);
    }

    #[test]
    fn remove_pos_deletes_by_ordinal_with_noncontiguous_positions() {
        // Reproduces Cyrille's .15 bug: a stray Qobuz row ("Piste inconnue")
        // that couldn't be deleted. When local and streaming rows end up with a
        // non-contiguous position space, the client's ordinal (Nth visible row)
        // no longer equals the stored `position` column, so the old
        // `DELETE WHERE position = ?` was a no-op.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db.clone());

        let mut ids = Vec::new();
        for name in ["A", "B", "C"] {
            let mut t = Track::new(name.into());
            t.file_path = Some(format!("/{name}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }
        repo.set_queue(1, &ids).unwrap(); // local rows at positions 0,1,2

        // Stray streaming row with a position that doesn't continue the space.
        db.execute(
            "INSERT INTO queue_items (zone_id, position, source_id, title, source, is_current) \
             VALUES (1, 99, 'qobuz-x', '', 'qobuz', 0)",
            &[],
        )
        .unwrap();

        // Ordered queue: [A(0), B(1), C(2), stray(99)] — stray is ordinal 3.
        let ordered = repo.get_ordered(1).unwrap();
        assert_eq!(ordered.len(), 4);
        assert!(ordered[3].track_id.is_none(), "stray streaming row is last");

        // Removing ordinal 3 must delete the stray row (old code deleted nothing
        // because no row had position == 3).
        assert!(repo.remove_pos(1, 3).unwrap());

        let after = repo.get_ordered(1).unwrap();
        assert_eq!(after.len(), 3);
        assert!(
            after.iter().all(|e| e.track_id.is_some()),
            "stray streaming row gone"
        );
        // Positions renumbered contiguous so future ordinal ops stay correct.
        assert_eq!(
            after.iter().map(|e| e.position).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        // Out-of-range ordinal is a safe no-op.
        assert!(!repo.remove_pos(1, 9).unwrap());
        assert_eq!(repo.get_ordered(1).unwrap().len(), 3);
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::insert_queue_row(&s).contains("VALUES (?, ?, ?, ?, 'local')"));
        assert!(sql::insert_queue_row(&p).contains("VALUES ($1, $2, $3, $4, 'local')"));
        assert!(
            sql::insert_streaming(&p)
                .contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)")
        );
        // The new per-album numbering columns must ride along the streaming insert.
        assert!(sql::insert_streaming(&s).contains("track_number"));
        assert!(sql::insert_streaming(&s).contains("disc_number"));
        assert!(sql::get_queue(&s).contains("queue_items"));
        assert!(sql::select_streaming(&s).contains("track_id IS NULL"));
    }

    #[test]
    fn queue_multiple_zones() {
        let db = test_db();
        db.execute(
            "INSERT INTO zones (name, output_type) VALUES ('Second', 'dlna')",
            &[],
        )
        .unwrap();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut t1 = Track::new("A".into());
        t1.file_path = Some("/a.flac".into());
        let mut t2 = Track::new("B".into());
        t2.file_path = Some("/b.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();
        let tid2 = track_repo.create(&t2).unwrap();

        repo.set_queue(1, &[tid1]).unwrap();
        repo.set_queue(2, &[tid2]).unwrap();

        assert_eq!(repo.count(1).unwrap(), 1);
        assert_eq!(repo.count(2).unwrap(), 1);

        let q1 = repo.get_queue(1).unwrap();
        assert_eq!(q1[0].track_id, tid1);

        let q2 = repo.get_queue(2).unwrap();
        assert_eq!(q2[0].track_id, tid2);
    }

    #[test]
    fn with_backend_constructor() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let mut t = Track::new("X".into());
        t.file_path = Some("/x.flac".into());
        let tid = track_repo.create(&t).unwrap();

        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = PlayQueueRepo::with_backend(backend);
        repo.set_queue(1, &[tid]).unwrap();
        assert_eq!(repo.count(1).unwrap(), 1);
    }

    // ── Unified single-position-space API (Lot 1) ─────────────────────────

    fn local(track_id: i64) -> QueueInput {
        QueueInput::Local { track_id }
    }
    fn streaming(id: &str, title: &str) -> QueueInput {
        QueueInput::Streaming {
            source: "qobuz".into(),
            source_id: id.into(),
            title: title.into(),
            artist: "Artist".into(),
            album: Some("Album".into()),
            cover_url: None,
            duration_ms: 200_000,
            track_number: None,
            disc_number: None,
        }
    }

    #[test]
    fn unified_append_and_get_ordered_mixed() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut t1 = Track::new("L1".into());
        t1.file_path = Some("/1.flac".into());
        let tid1 = track_repo.create(&t1).unwrap();

        repo.append(1, &[local(tid1), streaming("q1", "Q One")])
            .unwrap();
        assert_eq!(repo.count_all(1).unwrap(), 2);

        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].position, 0);
        assert!(q[0].is_local());
        assert_eq!(q[0].track_id, Some(tid1));
        assert_eq!(q[0].title.as_deref(), Some("L1"));
        assert_eq!(q[1].position, 1);
        assert!(!q[1].is_local());
        assert_eq!(q[1].source_id.as_deref(), Some("q1"));
        assert_eq!(q[1].title.as_deref(), Some("Q One"));
        assert_eq!(q[1].source.as_deref(), Some("qobuz"));
    }

    #[test]
    fn unified_play_next_inserts_after_current_s1() {
        // Sandro S1: a local album is playing; adding a Qobuz track via
        // "Play Next" (position = current + 1) must land right after the
        // current track, NOT at the end of the album.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut t = Track::new(format!("L{i}"));
            t.file_path = Some(format!("/{i}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }
        let locals: Vec<QueueInput> = ids.iter().map(|id| local(*id)).collect();
        repo.append(1, &locals).unwrap();
        // current = position 0 → "Play Next" inserts at position 1.
        repo.insert_at(1, &[streaming("q1", "Q One")], Some(1))
            .unwrap();

        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 4);
        assert_eq!(repo.count_all(1).unwrap(), 4);
        assert_eq!(q[0].track_id, Some(ids[0]));
        assert!(!q[1].is_local(), "Qobuz track must sit right after current");
        assert_eq!(q[1].source_id.as_deref(), Some("q1"));
        assert_eq!(q[2].track_id, Some(ids[1]));
        assert_eq!(q[3].track_id, Some(ids[2]));
        for (i, e) in q.iter().enumerate() {
            assert_eq!(e.position, i as i64, "positions must stay contiguous");
        }
    }

    #[test]
    fn insert_at_rend_la_position_effective_pas_celle_demandee() {
        // #2079 — « Lecture suivante » réussit toujours, y compris quand la
        // position demandée n'existe pas : `insert_at` la ramène en fin de
        // file. Les deux issues sont des `Ok`, et rien ne les distinguait.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut ids = Vec::new();
        for i in 0..2 {
            let mut t = Track::new(format!("L{i}"));
            t.file_path = Some(format!("/{i}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }
        let locals: Vec<QueueInput> = ids.iter().map(|id| local(*id)).collect();
        repo.append(1, &locals).unwrap();

        // Un vrai « juste après la piste en cours » : demandé 1, obtenu 1.
        let suivant = repo
            .insert_at(1, &[streaming("q1", "Q One")], Some(1))
            .unwrap();
        assert_eq!(suivant, Some(1), "la position demandée était tenable");

        // Une position hors file : demandée 99, la piste atterrit en FIN de
        // file — trois lignes occupent 0..2, donc en 3. C'est le cas que
        // l'appelant doit pouvoir distinguer.
        let ramene = repo
            .insert_at(1, &[streaming("q2", "Q Two")], Some(99))
            .unwrap();
        assert_eq!(
            ramene,
            Some(3),
            "hors file, l'insertion est ramenée en fin de file — et le dit"
        );
        assert_ne!(ramene, Some(99), "on rend le résultat, pas la demande");

        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 4);
        assert_eq!(q[1].source_id.as_deref(), Some("q1"));
        assert_eq!(q[3].source_id.as_deref(), Some("q2"));

        // Un ajout en fin de file (position None) rend lui aussi sa position.
        assert_eq!(
            repo.insert_at(1, &[streaming("q3", "Q Three")], None)
                .unwrap(),
            Some(4)
        );
        // Rien à insérer : aucune ligne, donc aucune position à annoncer.
        assert_eq!(repo.insert_at(1, &[], Some(0)).unwrap(), None);
    }

    #[test]
    fn unified_get_at_remove_move_set_current() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut t = Track::new("L0".into());
        t.file_path = Some("/0.flac".into());
        let tid = track_repo.create(&t).unwrap();
        repo.append(
            1,
            &[local(tid), streaming("q1", "Q1"), streaming("q2", "Q2")],
        )
        .unwrap();

        // get_at resolves any position, source-agnostic.
        assert_eq!(repo.get_at(1, 0).unwrap().unwrap().track_id, Some(tid));
        assert_eq!(
            repo.get_at(1, 2).unwrap().unwrap().source_id.as_deref(),
            Some("q2")
        );
        assert!(repo.get_at(1, 9).unwrap().is_none());

        // set_current can mark a streaming item (position 1).
        repo.set_current_pos(1, 1).unwrap();
        let cur = repo
            .get_ordered(1)
            .unwrap()
            .into_iter()
            .find(|e| e.is_current)
            .unwrap();
        assert_eq!(cur.source_id.as_deref(), Some("q1"));

        // remove middle → gap closes, positions stay contiguous.
        assert!(repo.remove_pos(1, 1).unwrap());
        let q = repo.get_ordered(1).unwrap();
        assert_eq!(q.len(), 2);
        assert_eq!(q[0].track_id, Some(tid));
        assert_eq!(q[1].source_id.as_deref(), Some("q2"));
        for (i, e) in q.iter().enumerate() {
            assert_eq!(e.position, i as i64);
        }

        // move the streaming item to the front.
        repo.move_pos(1, 1, 0).unwrap();
        let q2 = repo.get_ordered(1).unwrap();
        assert_eq!(q2[0].source_id.as_deref(), Some("q2"));
        assert_eq!(q2[1].track_id, Some(tid));
        for (i, e) in q2.iter().enumerate() {
            assert_eq!(e.position, i as i64);
        }
    }

    #[test]
    fn unified_get_at_resolves_local_after_streaming_s2() {
        // Sandro S2: a Qobuz track is current, a local track is added "next".
        // get_at must resolve the LOCAL track at the next position. The old code
        // offset into the streaming table (position - local_count) and never
        // found the local "next", so the zone froze on manual Next.
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut t = Track::new("Local Next".into());
        t.file_path = Some("/n.flac".into());
        let tid = track_repo.create(&t).unwrap();

        // Queue: [qobuz@0 (current), local@1].
        repo.append(1, &[streaming("q1", "Q Now")]).unwrap();
        repo.set_current_pos(1, 0).unwrap();
        repo.insert_at(1, &[local(tid)], Some(1)).unwrap();

        assert_eq!(repo.count_all(1).unwrap(), 2);
        let at1 = repo.get_at(1, 1).unwrap().unwrap();
        assert!(
            at1.is_local(),
            "position 1 must resolve to the local next track"
        );
        assert_eq!(at1.track_id, Some(tid));
        let at0 = repo.get_at(1, 0).unwrap().unwrap();
        assert!(!at0.is_local());
        assert_eq!(at0.source_id.as_deref(), Some("q1"));
    }
    // ─────────────────────────────────────────────────────────────────────
    // #2055 — « Quand j'appuie sur la fleche suivant du bandeau du bas il
    // choisit une piste aleatoire. Heureusement cela reste dans le meme
    // album. » (Tades, Tune 0.9.92 Windows, forum Mozaiklabs)
    //
    // La CAUSE n'est pas le drapeau aleatoire — celui-la, #3066 l'a rendu
    // visible dans les trois charges utiles de zone. C'est la file elle-meme :
    // son espace de positions n'etait pas UN espace, et rien ne le disait.
    //
    // `append_streaming_queue`, appele par l'autoplay du sondeur, numerotait
    // a partir de `count_streaming` — 0 sur la file d'un album. Les pistes
    // ajoutees s'ecrivaient PAR-DESSUS les premieres pistes de l'album.
    // ─────────────────────────────────────────────────────────────────────

    /// Prepare une zone dans l'etat exact de Tades : un album local en cours,
    /// puis l'autoplay du sondeur qui ajoute des pistes de service derriere.
    /// Rend (repo, nombre de pistes de l'album, nombre d'ajouts).
    fn file_album_puis_autoplay() -> (PlayQueueRepo, usize, usize) {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut ids = Vec::new();
        for i in 0..5 {
            let mut t = Track::new(format!("Album {i}"));
            t.file_path = Some(format!("/album/{i}.flac"));
            ids.push(track_repo.create(&t).unwrap());
        }
        repo.set_queue(1, &ids).unwrap();
        // Exactement ce que fait `PositionPoller::autoplay_streaming_*` quand
        // la file arrive en fin d'album (`poller.rs`, seul appelant).
        let ajouts: Vec<StreamingQueueItem> = (0..3)
            .map(|i| {
                (
                    format!("auto{i}"),
                    format!("Suggestion {i}"),
                    "Voisin".to_string(),
                    None,
                    None,
                    200_000i64,
                    Some("qobuz".to_string()),
                    None,
                    None,
                )
            })
            .collect();
        repo.append_streaming_queue(1, &ajouts).unwrap();
        (repo, ids.len(), ajouts.len())
    }

    /// 🔴 CONTRE-EPREUVE sur un FAIT DE BASE, jamais sur un code HTTP.
    ///
    /// D'une file connue de N pistes, en position k, « suivant » rend la piste
    /// k+1 — la MEME a chaque appel. C'est tout ce que l'utilisateur demande,
    /// et c'est exactement ce que la file ne tenait plus.
    ///
    /// Avant le correctif, sur 5 pistes d'album + 3 ajouts d'autoplay :
    ///   count_all = 8, mais les positions ne vont que de 0 a 4 ;
    ///   get_at(5), get_at(6), get_at(7) rendent None ;
    ///   les positions 0, 1 et 2 portent DEUX lignes chacune.
    /// `next_position` calcule pourtant 0→1→…→7 depuis `count_all` : arrive a
    /// 5, `play_from_queue` echoue sur « no queue item at position » alors que
    /// la route « suivant » a deja repondu `"playing"`.
    #[test]
    fn suivant_rend_la_piste_k_plus_1_la_meme_a_chaque_appel() {
        let (repo, album, ajouts) = file_album_puis_autoplay();
        let total = repo.count_all(1).unwrap();
        assert_eq!(
            total,
            (album + ajouts) as i64,
            "la file compte l'album et les ajouts"
        );

        // 1. Un espace de positions et un seul : aucune collision.
        let ordonnee = repo.get_ordered(1).unwrap();
        let positions: Vec<i64> = ordonnee.iter().map(|e| e.position).collect();
        assert_eq!(
            positions,
            (0..total).collect::<Vec<_>>(),
            "les positions doivent former 0..N-1 sans trou ni doublon — \
             c'est l'espace unique que la migration 53 a etabli. \
             Avant #2055 les trois ajouts de l'autoplay retombaient sur \
             0, 1, 2, par-dessus les pistes de l'album : {positions:?}"
        );

        // 2. Chacune des N positions porte une ligne — y compris celles que
        //    l'aleatoire peut tirer. Avant le correctif, les positions 5, 6 et
        //    7 que `count_all` annonce n'avaient AUCUNE ligne.
        for p in 0..total {
            assert!(
                repo.get_at(1, p).unwrap().is_some(),
                "la file annonce {total} pistes, mais la position {p} est vide"
            );
        }

        // 3. L'album reste en tete, dans son ordre, et les ajouts suivent.
        //    « Heureusement cela reste dans le meme album » : ce qui restait
        //    joignable, c'etaient justement les lignes de l'album.
        for (i, e) in ordonnee.iter().enumerate() {
            if i < album {
                assert!(e.is_local(), "position {i} doit rester la piste d'album");
            } else {
                assert!(!e.is_local(), "position {i} doit etre un ajout d'autoplay");
            }
        }

        // 4. Le fait de base : de k, « suivant » rend k+1, et la meme ligne a
        //    chaque appel. On interroge la MEME decision que la route
        //    `POST /api/v1/zones/{id}/next` (`next_position_manual`) et le
        //    MEME resolveur que `Orchestrator::play_from_queue` (`get_at`).
        for k in 0..total {
            let etat = crate::playback::ZoneState {
                state: crate::playback::PlayState::Playing,
                queue_position: k,
                queue_length: total,
                repeat: crate::playback::RepeatMode::Off,
                shuffle: false,
                ..Default::default()
            };
            let suivant = crate::poller::PositionPoller::next_position_manual(&etat);
            if k == total - 1 {
                assert_eq!(suivant, None, "en fin de file, « suivant » arrete");
                continue;
            }
            assert_eq!(suivant, Some(k + 1), "de {k}, « suivant » vise {}", k + 1);
            let vise = suivant.unwrap();
            let une = repo.get_at(1, vise).unwrap().unwrap_or_else(|| {
                panic!(
                    "aucune ligne a la position {vise} que « suivant » vient de \
                     designer : la route a repondu \"playing\", et rien ne sort"
                )
            });
            let deux = repo.get_at(1, vise).unwrap().expect("seconde lecture");
            assert_eq!(
                une.id, deux.id,
                "deux appels a la position {vise} rendent deux lignes \
                 differentes : l'ordre de resolution n'est pas total, et \
                 « suivant » devient imprevisible (le mal de #3074)"
            );
        }
    }

    /// 🟢 TEMOIN, vert des deux cotes — l'aleatoire ASSUME ne change pas.
    ///
    /// Quand l'utilisateur arme lui-meme la lecture aleatoire, « suivant »
    /// reste aleatoire, mais ne repasse pas deux fois sur la meme piste avant
    /// d'avoir epuise la file : l'ordre materialise est une permutation, et
    /// `next_position` la suit. Ce correctif ne touche pas cette decision — il
    /// rend seulement joignable chacune des positions qu'elle designe.
    #[test]
    fn temoin_l_aleatoire_assume_epuise_la_file_sans_doublon() {
        // Aucune lecture en base ici, a dessein : ce temoin doit etre vert
        // AVANT comme APRES. Que chaque position tiree porte reellement une
        // ligne, c'est la contre-epreuve ci-dessus qui l'exige — et elle,
        // elle etait rouge.
        let (repo, _, _) = file_album_puis_autoplay();
        let total = repo.count_all(1).unwrap();
        let ordre = crate::playback::generate_shuffle_order(total as usize, 0);

        let mut vues = std::collections::HashSet::new();
        let mut position = ordre[0] as i64;
        vues.insert(position);
        for index in 0..total - 1 {
            let etat = crate::playback::ZoneState {
                state: crate::playback::PlayState::Playing,
                queue_position: position,
                queue_length: total,
                repeat: crate::playback::RepeatMode::Off,
                shuffle: true,
                shuffle_order: ordre.clone(),
                shuffle_index: index,
                ..Default::default()
            };
            position = crate::poller::PositionPoller::next_position_manual(&etat)
                .expect("l'aleatoire doit avancer tant que la file n'est pas epuisee");
            assert!(
                vues.insert(position),
                "la position {position} revient avant que la file soit epuisee"
            );
        }
        assert_eq!(
            vues.len(),
            total as usize,
            "un cycle aleatoire doit passer par TOUTES les pistes, exactement une fois"
        );
    }

    /// 🟢 TEMOIN, vert des deux cotes — le bouclage en fin de file ne bouge pas.
    ///
    /// Repetition desarmee : « suivant » s'arrete a la derniere piste.
    /// Repetition totale : il revient a la premiere, et cette premiere est
    /// joignable. Aucune de ces deux regles n'est touchee ici.
    #[test]
    fn temoin_le_bouclage_en_fin_de_file_ne_change_pas() {
        let (repo, _, _) = file_album_puis_autoplay();
        let total = repo.count_all(1).unwrap();
        let derniere = total - 1;

        let arret = crate::playback::ZoneState {
            state: crate::playback::PlayState::Playing,
            queue_position: derniere,
            queue_length: total,
            repeat: crate::playback::RepeatMode::Off,
            shuffle: false,
            ..Default::default()
        };
        assert_eq!(
            crate::poller::PositionPoller::next_position_manual(&arret),
            None,
            "repetition desarmee : la file s'arrete a la derniere piste"
        );

        let boucle = crate::playback::ZoneState {
            repeat: crate::playback::RepeatMode::All,
            ..arret
        };
        assert_eq!(
            crate::poller::PositionPoller::next_position_manual(&boucle),
            Some(0),
            "repetition totale : la file revient a la premiere piste"
        );
        assert!(
            repo.get_at(1, 0).unwrap().is_some(),
            "et cette premiere piste doit exister"
        );
    }

    // ── #3231 — la file creuse (Pierre M, fil forum 978) ──────────────────
    //
    // Aucun test ne couvrait les positions creuses. `set_queue` saute en silence
    // tout identifiant sans ligne dans `tracks`, et prenait l'INDICE DE BOUCLE
    // comme position : chaque saut laissait un trou, pendant que `count_all`
    // rendait un compte DENSE. Les trois epreuves qui suivent tiennent le cas de
    // Pierre M, la perte dite, et le temoin.

    /// Construit une demande a la Pierre M : `presentes` pistes reellement creees,
    /// disseminees dans une liste qui contient aussi `absentes` identifiants
    /// n'ayant AUCUNE ligne dans `tracks`. Rend (repo, liste demandee, ids reels).
    fn demande_avec_pistes_absentes(
        presentes: usize,
        absentes: usize,
    ) -> (PlayQueueRepo, Vec<i64>, Vec<i64>) {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);

        let mut reels = Vec::new();
        for n in 0..presentes {
            let mut t = Track::new(format!("Titre {n}"));
            t.file_path = Some(format!("/3231/{n}.flac"));
            t.duration_ms = 180_000;
            reels.push(track_repo.create(&t).unwrap());
        }
        // Des identifiants tres au-dessus de tout ce que la base a distribue :
        // ils ne peuvent pas exister. C'est exactement l'etat d'une file
        // persistee avant un rescan qui a redistribue les identifiants.
        let fantomes: Vec<i64> = (0..absentes).map(|n| 900_000 + n as i64).collect();

        // Entrelacer pour que les trous ne soient pas tous en fin de liste : la
        // version fautive produisait alors des positions eparpillees sur toute
        // l'etendue 0..demandees-1.
        let mut demandee = Vec::new();
        let mut it_reels = reels.iter();
        let mut it_fantomes = fantomes.iter();
        loop {
            let f = it_fantomes.next();
            let r = it_reels.next();
            if f.is_none() && r.is_none() {
                break;
            }
            if let Some(f) = f {
                demandee.push(*f);
            }
            if let Some(r) = r {
                demandee.push(*r);
            }
        }
        (repo, demandee, reels)
    }

    /// 🔴 #3231 — LE CAS DE PIERRE M.
    ///
    /// « Une compilation de 190 titres bascule en suggestions apres 4 titres. »
    ///
    /// On demande 190 titres dont 185 n'existent pas. La file jouable doit
    /// contenir TOUTES les lignes reellement inserees, et le compte doit les
    /// refleter — ce qui veut dire, concretement, que la marche du sondeur doit
    /// pouvoir resoudre CHAQUE position de 0 a `count_all - 1`.
    ///
    /// Avec l'indice de boucle comme position, les cinq survivants atterrissaient
    /// aux positions 0, 2, 4, 6, 8 pendant que `count_all` repondait 5 :
    /// `next_position` s'arretait a 4, et les positions 1 et 3 ne resolvaient
    /// AUCUNE ligne. La zone tombait dans l'autoplay.
    #[test]
    fn cas_pierre_m_positions_denses_et_compte_coherent() {
        let (repo, demandee, reels) = demande_avec_pistes_absentes(5, 185);
        assert_eq!(
            demandee.len(),
            190,
            "la demande de Pierre M fait 190 titres"
        );

        let bilan = repo.set_queue(1, &demandee).unwrap();

        // 1. Le compte dit la verite sur ce qui a ete ecrit.
        assert_eq!(bilan.requested, 190);
        assert_eq!(bilan.inserted, 5);
        assert_eq!(
            repo.count_all(1).unwrap(),
            5,
            "count_all doit compter les lignes reellement ecrites"
        );

        // 2. Les positions sont DENSES : exactement 0..4, sans trou.
        let ordre = repo.get_ordered(1).unwrap();
        let positions: Vec<i64> = ordre.iter().map(|e| e.position).collect();
        assert_eq!(
            positions,
            vec![0, 1, 2, 3, 4],
            "les positions doivent suivre les insertions REUSSIES, pas l'indice de boucle"
        );

        // 3. Les pistes conservees sont bien les vraies, dans l'ordre demande.
        let gardees: Vec<i64> = ordre.iter().filter_map(|e| e.track_id).collect();
        assert_eq!(gardees, reels, "l'ordre demande doit etre preserve");

        // 4. L'EPREUVE QUI TRANCHE : la marche du sondeur resout chaque position.
        //    C'est le comportement que Pierre M n'avait pas.
        let total = repo.count_all(1).unwrap();
        let mut position = 0i64;
        let mut jouees = 1; // la position 0 est jouee d'emblee
        assert!(
            repo.get_at(1, 0).unwrap().is_some(),
            "la premiere position doit resoudre une ligne"
        );
        while let Some(suivante) =
            crate::poller::PositionPoller::next_position(&crate::playback::ZoneState {
                state: crate::playback::PlayState::Playing,
                queue_position: position,
                queue_length: total,
                repeat: crate::playback::RepeatMode::Off,
                shuffle: false,
                ..Default::default()
            })
        {
            assert!(
                repo.get_at(1, suivante).unwrap().is_some(),
                "position {suivante} annoncee par next_position mais AUCUNE ligne \
                 ne la porte — c'est la file creuse de #3231"
            );
            position = suivante;
            jouees += 1;
        }
        assert_eq!(
            jouees, 5,
            "la file doit jouer les 5 lignes reellement inserees, pas s'arreter avant"
        );
    }

    /// 🔴 #3231 — LA PERTE EST DITE.
    ///
    /// Une perte muette EST le defaut (#2394 : un compteur qui ment est pire
    /// qu'un compteur absent). Le compte rendu doit nommer combien de lignes sont
    /// tombees, et lesquelles.
    #[test]
    fn la_perte_est_dite_et_nomme_les_identifiants_absents() {
        let (repo, demandee, reels) = demande_avec_pistes_absentes(3, 7);

        let bilan = repo.set_queue(1, &demandee).unwrap();

        assert_eq!(bilan.requested, 10);
        assert_eq!(bilan.inserted, 3);
        assert_eq!(bilan.skipped_count(), 7, "sept lignes sont tombees");
        assert!(bilan.has_loss(), "la perte doit etre signalee");
        // Les identifiants tombes sont NOMMES : sans eux, impossible de dire si
        // la cause est un rescan qui a redistribue les identifiants ou une
        // requete client qui en a invente.
        assert_eq!(bilan.skipped, (900_000..900_007).collect::<Vec<i64>>());
        for id in &bilan.skipped {
            assert!(
                !reels.contains(id),
                "aucun identifiant reellement insere ne doit figurer parmi les absents"
            );
        }
        // Et la somme est juste : rien ne disparait entre les deux compteurs.
        assert_eq!(
            bilan.inserted + bilan.skipped_count(),
            bilan.requested,
            "insere + absent doit rendre exactement le demande"
        );
    }

    /// 🔴 #3231 — corollaire : la ligne COURANTE suit elle aussi les insertions.
    ///
    /// `is_current` valait `i == 0`, l'indice de boucle. Quand le tout premier
    /// identifiant demande etait justement celui qui manquait, la file se
    /// retrouvait SANS AUCUNE ligne courante.
    #[test]
    fn la_premiere_ligne_ecrite_est_courante_meme_si_la_premiere_demandee_manque() {
        let db = test_db();
        let track_repo = TrackRepo::new(db.clone());
        let repo = PlayQueueRepo::new(db);
        let mut t = Track::new("Survivante".into());
        t.file_path = Some("/3231/survivante.flac".into());
        let tid = track_repo.create(&t).unwrap();

        // Le premier identifiant demande n'existe pas.
        let bilan = repo.set_queue(1, &[900_001, tid]).unwrap();
        assert_eq!(bilan.inserted, 1);

        let courante = repo
            .get_current(1)
            .unwrap()
            .expect("la file doit avoir une ligne courante");
        assert_eq!(courante.track_id, tid);
        assert_eq!(courante.position, 0, "et elle doit etre a la position 0");
    }

    /// 🟢 TEMOIN — une file dont TOUTES les pistes existent ne change pas.
    ///
    /// Meme compte, memes positions, meme ordre, meme ligne courante, et un
    /// compte rendu qui n'annonce aucune perte.
    #[test]
    fn temoin_file_entierement_valide_inchangee() {
        let (repo, demandee, reels) = demande_avec_pistes_absentes(6, 0);
        assert_eq!(demandee, reels, "aucun fantome dans cette demande");

        let bilan = repo.set_queue(1, &demandee).unwrap();

        assert_eq!(bilan.requested, 6);
        assert_eq!(bilan.inserted, 6);
        assert_eq!(bilan.skipped_count(), 0);
        assert!(!bilan.has_loss(), "aucune perte a annoncer");

        assert_eq!(repo.count_all(1).unwrap(), 6);
        let ordre = repo.get_ordered(1).unwrap();
        assert_eq!(
            ordre.iter().map(|e| e.position).collect::<Vec<i64>>(),
            vec![0, 1, 2, 3, 4, 5],
            "les positions d'une file saine restent 0..N-1"
        );
        assert_eq!(
            ordre
                .iter()
                .filter_map(|e| e.track_id)
                .collect::<Vec<i64>>(),
            reels,
            "l'ordre est preserve a l'identique"
        );
        let courante = repo.get_current(1).unwrap().expect("ligne courante");
        assert_eq!(courante.track_id, reels[0]);
        assert_eq!(courante.position, 0);

        // Et la marche parcourt bien les six.
        let total = repo.count_all(1).unwrap();
        let mut position = 0i64;
        let mut jouees = 1;
        while let Some(suivante) =
            crate::poller::PositionPoller::next_position(&crate::playback::ZoneState {
                state: crate::playback::PlayState::Playing,
                queue_position: position,
                queue_length: total,
                repeat: crate::playback::RepeatMode::Off,
                shuffle: false,
                ..Default::default()
            })
        {
            assert!(repo.get_at(1, suivante).unwrap().is_some());
            position = suivante;
            jouees += 1;
        }
        assert_eq!(jouees, 6);
    }

    /// 🔴 #3231 — la liste `IN` doit etre juste sur LES DEUX moteurs.
    ///
    /// SQLite numerote ses marqueurs implicitement (`?`), PostgreSQL les numerote
    /// a la main (`$1..$n`) : une liste construite pour l'un est fausse pour
    /// l'autre, et le moteur PG refuserait la requete ou lierait les mauvaises
    /// valeurs. Cette epreuve ne touche aucune base — elle s'execute toujours,
    /// quel que soit le moteur disponible.
    #[test]
    fn liste_in_des_pistes_existantes_sur_les_deux_moteurs() {
        assert_eq!(
            sql::tracks_existing_in(&SqliteDialect, 3),
            "SELECT id FROM tracks WHERE id IN (?, ?, ?)"
        );
        assert_eq!(
            sql::tracks_existing_in(&PostgresDialect, 3),
            "SELECT id FROM tracks WHERE id IN ($1, $2, $3)"
        );
        // Un seul identifiant reste une liste valide sur les deux moteurs.
        assert_eq!(
            sql::tracks_existing_in(&SqliteDialect, 1),
            "SELECT id FROM tracks WHERE id IN (?)"
        );
        assert_eq!(
            sql::tracks_existing_in(&PostgresDialect, 1),
            "SELECT id FROM tracks WHERE id IN ($1)"
        );
        // Le paquet plein : la numerotation PG doit aller jusqu'a $500 sans
        // trou ni decalage.
        let plein = sql::tracks_existing_in(&PostgresDialect, 500);
        assert!(
            plein.contains("($1, $2, "),
            "la numerotation PG commence a $1"
        );
        assert!(
            plein.ends_with("$499, $500)"),
            "et va jusqu'a $500 : {plein}"
        );
    }

    /// 🔴 #3231 — cas limite : une file dont AUCUNE piste n'existe.
    ///
    /// Elle doit rester vide et le DIRE, sans jamais annoncer un compte non nul.
    #[test]
    fn file_entierement_absente_rend_une_file_vide_et_le_dit() {
        let (repo, demandee, _) = demande_avec_pistes_absentes(0, 4);
        let bilan = repo.set_queue(1, &demandee).unwrap();
        assert_eq!(bilan.requested, 4);
        assert_eq!(bilan.inserted, 0);
        assert_eq!(bilan.skipped_count(), 4);
        assert_eq!(repo.count_all(1).unwrap(), 0);
        assert!(repo.get_current(1).unwrap().is_none());
    }
}
