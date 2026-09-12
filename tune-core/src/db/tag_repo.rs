use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Les types d'objet qu'une étiquette peut porter — la liste **fermée** contre
/// laquelle tout chemin d'écriture est vérifié.
///
/// `item_tags(tag_id, item_type, item_id)` est générique par construction, et
/// c'est bien : le modèle veut qu'une étiquette se pose sur n'importe quel
/// objet musical. Mais générique ne veut pas dire *non vérifié*. Rien ne
/// validait `item_type` : un `POST` portant `"albums"` au pluriel s'insérait
/// sans un mot, créait un type parallèle que **aucune** route de lecture ne
/// nomme, et la pose devenait invisible pour toujours — l'étiquette comptée
/// dans `/tags` (le `COUNT(*)` ne filtre pas) mais introuvable dans
/// `/tags/{id}/albums`. Une faute de frappe côté client suffisait.
///
/// La liste est triée : elle sert aussi de message d'erreur, et un ordre
/// stable rend ce message diffable.
///
/// # Pourquoi `label` n'y est pas
///
/// **Un label n'a pas d'identité numérique dans ce dépôt.** Il n'existe ni
/// table `labels`, ni `label_id` local : l'onglet Labels lit la colonne libre
/// `tracks.label` en *facette* et sélectionne par CHAÎNE — c'est exactement ce
/// que constate [`favorite_facets_repo`](super::favorite_facets_repo), qui a
/// dû se donner une table à part (`favorite_facets`, `facet = 'label'`,
/// `value TEXT`) pour mettre un label en favori.
///
/// Or `item_tags.item_id` est `INTEGER NOT NULL`. Il ne peut pas porter une
/// chaîne. Accepter `item_type = "label"` reviendrait donc à écrire un entier
/// qui ne désigne rien, et à rendre l'écriture illisible par construction :
/// c'est précisément la panne silencieuse que cette liste ferme. Étiqueter un
/// label demande une décision de modèle — l'aligner sur `favorite_facets`, ou
/// donner enfin une identité aux labels — qui n'appartient pas à ce correctif.
pub const TAGGABLE_ITEM_TYPES: [&str; 4] = ["album", "artist", "playlist", "track"];

/// Vrai si `item_type` est un type d'objet étiquetable connu.
pub fn is_taggable_item_type(item_type: &str) -> bool {
    TAGGABLE_ITEM_TYPES.contains(&item_type)
}

/// Message d'erreur d'un `item_type` refusé — il **nomme les types admis**,
/// sans quoi le client ne peut pas corriger sa faute de frappe.
pub fn item_type_rejette(item_type: &str) -> String {
    format!(
        "item_type inconnu : « {item_type} » — types admis : {}",
        TAGGABLE_ITEM_TYPES.join(", ")
    )
}

fn verifier_item_type(item_type: &str) -> Result<(), String> {
    if is_taggable_item_type(item_type) {
        Ok(())
    } else {
        Err(item_type_rejette(item_type))
    }
}

/// Engine-agnostic SQL builders for tag_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn list_all() -> &'static str {
        "SELECT id, name, color FROM tags ORDER BY name"
    }

    pub fn create_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO tags (name, color) VALUES ({}, {})",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, name, color FROM tags WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn update_name<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tags SET name = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn update_color<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE tags SET color = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_by_id<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM tags WHERE id = {}", d.placeholder(1))
    }

    pub fn all_items_by_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT item_type, item_id FROM item_tags WHERE tag_id = {} ORDER BY item_type, item_id",
            d.placeholder(1)
        )
    }

    /// INSERT OR IGNORE rewritten to portable ON CONFLICT DO NOTHING.
    /// UNIQUE(tag_id, item_type, item_id) is enforced by the schema.
    pub fn tag_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES ({}, {}, {}) ON CONFLICT (tag_id, item_type, item_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn untag_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM item_tags WHERE tag_id = {} AND item_type = {} AND item_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn items_by_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT item_id FROM item_tags WHERE tag_id = {} AND item_type = {} ORDER BY item_id",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn tags_for_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT t.id, t.name, t.color FROM tags t JOIN item_tags it ON t.id = it.tag_id WHERE it.item_type = {} AND it.item_id = {} ORDER BY t.name",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn get_by_name<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, name, color FROM tags WHERE name = {}",
            d.placeholder(1)
        )
    }

    pub fn search_by_name<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT id, name, color FROM tags WHERE name LIKE {} ORDER BY name LIMIT 20",
            d.placeholder(1)
        )
    }

    /// Le compte annonce par `/tags` porte sur les DEUX espaces
    /// d'identifiants (#3699) : la table locale ET la table de streaming.
    ///
    /// Deux sous-requetes correlees plutot que deux LEFT JOIN : joindre les
    /// deux tables a la fois multiplierait les lignes (trois albums locaux et
    /// deux albums Qobuz donneraient six), et le `COUNT` mentirait par
    /// construction. Une etiquette sans aucun objet rend toujours 0, comme
    /// avant.
    pub fn count_per_tag() -> &'static str {
        "SELECT t.id, t.name, t.color, \
         (SELECT COUNT(*) FROM item_tags it WHERE it.tag_id = t.id) \
         + (SELECT COUNT(*) FROM streaming_item_tags s WHERE s.tag_id = t.id) as item_count \
         FROM tags t ORDER BY t.name"
    }

    pub fn count_per_tag_by_type<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT t.id, t.name, t.color, \
             (SELECT COUNT(*) FROM item_tags it WHERE it.tag_id = t.id AND it.item_type = {}) \
             + (SELECT COUNT(*) FROM streaming_item_tags s WHERE s.tag_id = t.id AND s.item_type = {}) as item_count \
             FROM tags t ORDER BY t.name",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    // --- Etiquettes posees sur un objet de STREAMING (#3699) ---
    //
    // Meme forme que `streaming_favorites_repo::sql` : la designation est la
    // PAIRE `source` + `source_id`, jamais un entier, et `created_at` est
    // rempli par l'expression « maintenant » du moteur plutot que par un
    // DEFAULT de colonne — la table PostgreSQL creee par `ENSURE_TABLES` n'en
    // porte pas, et sans cela la valeur serait NULL.

    /// L'instantane d'affichage est ecrit A L'ETIQUETAGE. Un second passage
    /// sur la meme paire ne cree pas de ligne (la clef primaire EST la paire)
    /// et ne rafraichit pas l'instantane : ce qui a ete range reste range tel
    /// qu'on l'avait vu.
    pub fn tag_streaming_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO streaming_item_tags \
             (tag_id, item_type, source, source_id, title, artist, album, cover_url, created_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT (tag_id, item_type, source, source_id) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.now_iso8601(),
        )
    }

    pub fn untag_streaming_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM streaming_item_tags \
             WHERE tag_id = {} AND item_type = {} AND source = {} AND source_id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
        )
    }

    /// Toutes les lignes de streaming d'une etiquette a la suppression de
    /// celle-ci. Il n'y a pas de clef etrangere : le schema PostgreSQL de
    /// bascule n'en porte aucune, et sur SQLite `PRAGMA foreign_keys` n'est
    /// pas garanti actif. Le nettoyage est donc EXPLICITE.
    pub fn untag_streaming_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM streaming_item_tags WHERE tag_id = {}",
            d.placeholder(1)
        )
    }

    const COLS_STREAMING: &str = "SELECT item_type, source, source_id, title, artist, album, cover_url \
         FROM streaming_item_tags";

    pub fn streaming_items_by_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "{COLS_STREAMING} WHERE tag_id = {} AND item_type = {} ORDER BY created_at DESC, source, source_id",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    pub fn all_streaming_items_by_tag<D: SqlDialect>(d: &D) -> String {
        format!(
            "{COLS_STREAMING} WHERE tag_id = {} ORDER BY item_type, source, source_id",
            d.placeholder(1),
        )
    }

    pub fn tags_for_streaming_item<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT t.id, t.name, t.color FROM tags t \
             JOIN streaming_item_tags s ON t.id = s.tag_id \
             WHERE s.item_type = {} AND s.source = {} AND s.source_id = {} ORDER BY t.name",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }

    pub fn items_by_any_tags<D: SqlDialect>(d: &D, count: usize) -> String {
        let placeholders: Vec<String> = (0..count).map(|i| d.placeholder(i + 1)).collect();
        format!(
            "SELECT DISTINCT item_id FROM item_tags WHERE item_type = {} AND tag_id IN ({}) ORDER BY item_id",
            d.placeholder(count + 1),
            placeholders.join(", ")
        )
    }

    pub fn items_by_all_tags<D: SqlDialect>(d: &D, count: usize) -> String {
        let placeholders: Vec<String> = (0..count).map(|i| d.placeholder(i + 1)).collect();
        format!(
            "SELECT item_id FROM item_tags WHERE item_type = {} AND tag_id IN ({}) \
             GROUP BY item_id HAVING COUNT(DISTINCT tag_id) = {} ORDER BY item_id",
            d.placeholder(count + 1),
            placeholders.join(", "),
            d.placeholder(count + 2)
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Option<i64>,
    pub name: String,
    pub color: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagWithCount {
    #[serde(flatten)]
    pub tag: Tag,
    pub count: i64,
}

/// Un objet de STREAMING porteur d'une etiquette (#3699).
///
/// Sa designation est la PAIRE `source` + `source_id` — jamais un entier :
/// un album Qobuz, Tidal ou Bandcamp n'a pas de clef primaire dans la base
/// locale, c'est tout le sujet du ticket.
///
/// Les quatre derniers champs sont l'INSTANTANE pose a l'etiquetage, sur le
/// modele de [`StreamingFavorite`](super::streaming_favorites_repo). Ils
/// existent pour que la liste par etiquette se rende SANS interroger le
/// catalogue : un album de streaming peut disparaitre, et l'ecran doit
/// continuer de s'afficher.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingTagItem {
    pub item_type: String,
    pub source: String,
    pub source_id: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
}

pub struct TagRepo {
    db: Arc<dyn DbBackend>,
}

impl TagRepo {
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

    pub fn list(&self) -> Result<Vec<Tag>, String> {
        let rows = self.db.query_many(sql::list_all(), &[])?;
        Ok(rows.iter().map(row_to_tag).collect())
    }

    pub fn create(&self, name: &str, color: Option<&str>) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create_tag, sql::create_tag);
        let color_val = color.unwrap_or("#808080");
        let params: [&dyn ToSqlValue; 2] = [&name, &color_val];
        Ok(self.db.execute_returning_id(&sql, &params)?)
    }

    pub fn get(&self, id: i64) -> Result<Option<Tag>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_tag))
    }

    pub fn update(&self, id: i64, name: Option<&str>, color: Option<&str>) -> Result<(), String> {
        if let Some(name) = name {
            let sql = self.dialect_sql(sql::update_name, sql::update_name);
            let params: [&dyn ToSqlValue; 2] = [&name, &id];
            self.db.execute(&sql, &params)?;
        }
        if let Some(color) = color {
            let sql = self.dialect_sql(sql::update_color, sql::update_color);
            let params: [&dyn ToSqlValue; 2] = [&color, &id];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    /// Supprime une etiquette — et ses poses de streaming AVEC elle (#3699).
    ///
    /// `item_tags` porte une clef etrangere `ON DELETE CASCADE` cote SQLite et
    /// sur une base PostgreSQL neuve. `streaming_item_tags` n'en porte AUCUNE,
    /// deliberement : le schema PostgreSQL de bascule n'a pas de contraintes,
    /// et sur SQLite `PRAGMA foreign_keys` n'est pas garanti actif. Le
    /// nettoyage est donc explicite, ici, avant la ligne de l'etiquette — sans
    /// quoi le compte de `/tags` continuerait de compter des poses orphelines
    /// pour une etiquette qui n'existe plus.
    pub fn delete(&self, id: i64) -> Result<(), String> {
        let purge = self.dialect_sql(sql::untag_streaming_all, sql::untag_streaming_all);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&purge, &params)?;
        let sql = self.dialect_sql(sql::delete_by_id, sql::delete_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn all_items_by_tag(&self, tag_id: i64) -> Result<Vec<(String, i64)>, String> {
        let sql = self.dialect_sql(sql::all_items_by_tag, sql::all_items_by_tag);
        let params: [&dyn ToSqlValue; 1] = [&tag_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .map(|cols| {
                (
                    cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
                    cols.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
                )
            })
            .collect())
    }

    /// Pose une étiquette sur un objet.
    ///
    /// La vérification d'`item_type` est ici, au **dépôt**, et non dans la
    /// route : c'est le seul point que tous les chemins d'écriture traversent.
    /// Une règle posée dans un handler ne protège que ce handler-là — le
    /// suivant qu'on ajoutera l'oubliera.
    pub fn tag_item(&self, tag_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        verifier_item_type(item_type)?;
        let sql = self.dialect_sql(sql::tag_item, sql::tag_item);
        let params: [&dyn ToSqlValue; 3] = [&tag_id, &item_type, &item_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Retire une étiquette d'un objet.
    ///
    /// **Volontairement sans vérification d'`item_type`** : la suppression doit
    /// rester capable d'atteindre une ligne d'un type qui n'est plus admis,
    /// sinon un enregistrement écrit avant ce garde-fou deviendrait
    /// indéracinable.
    pub fn untag_item(&self, tag_id: i64, item_type: &str, item_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::untag_item, sql::untag_item);
        let params: [&dyn ToSqlValue; 3] = [&tag_id, &item_type, &item_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn items_by_tag(&self, tag_id: i64, item_type: &str) -> Result<Vec<i64>, String> {
        let sql = self.dialect_sql(sql::items_by_tag, sql::items_by_tag);
        let params: [&dyn ToSqlValue; 2] = [&tag_id, &item_type];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    pub fn tags_for_item(&self, item_type: &str, item_id: i64) -> Result<Vec<Tag>, String> {
        let sql = self.dialect_sql(sql::tags_for_item, sql::tags_for_item);
        let params: [&dyn ToSqlValue; 2] = [&item_type, &item_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_tag).collect())
    }

    pub fn get_by_name(&self, name: &str) -> Result<Option<Tag>, String> {
        let sql = self.dialect_sql(sql::get_by_name, sql::get_by_name);
        let params: [&dyn ToSqlValue; 1] = [&name];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_tag))
    }

    pub fn search(&self, query: &str) -> Result<Vec<Tag>, String> {
        let sql = self.dialect_sql(sql::search_by_name, sql::search_by_name);
        let pattern = format!("%{query}%");
        let params: [&dyn ToSqlValue; 1] = [&pattern as &dyn ToSqlValue];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_tag).collect())
    }

    pub fn list_with_counts(&self, item_type: Option<&str>) -> Result<Vec<TagWithCount>, String> {
        let rows = if let Some(itype) = item_type {
            let sql = self.dialect_sql(sql::count_per_tag_by_type, sql::count_per_tag_by_type);
            // DEUX liaisons pour UNE valeur. Depuis #3699 la requete compte les
            // deux espaces d'identifiants, et `item_type` est filtre une fois
            // dans CHAQUE sous-requete. Sur SQLite le marqueur est `?`, qui
            // IGNORE l'indice demande : deux marqueurs reclament deux liaisons,
            // meme quand la valeur est la meme. Avec une seule, la requete
            // rendait « Wrong number of parameters passed to query. Got 1,
            // needed 2 » — donc `/api/v1/tags/?item_type=album` rendait une
            // liste VIDE (`unwrap_or_default`), sans le moindre journal.
            let params: [&dyn ToSqlValue; 2] = [&itype, &itype];
            self.db.query_many(&sql, &params)?
        } else {
            self.db.query_many(sql::count_per_tag(), &[])?
        };
        Ok(rows
            .iter()
            .map(|cols| TagWithCount {
                tag: row_to_tag(cols),
                count: cols.get(3).and_then(|v| v.as_i64()).unwrap_or(0),
            })
            .collect())
    }

    pub fn batch_tag(
        &self,
        tag_id: i64,
        item_type: &str,
        item_ids: &[i64],
    ) -> Result<usize, String> {
        // Vérifié AVANT la boucle : un lot refusé ne doit rien laisser derrière
        // lui. `batch_tag` ne passe pas par `tag_item` (il bâtit son SQL une
        // fois pour toutes) — sans cette ligne, le lot serait le trou par
        // lequel un type inconnu entrerait quand même, par centaines.
        verifier_item_type(item_type)?;
        let mut count = 0;
        let sql = self.dialect_sql(sql::tag_item, sql::tag_item);
        for &item_id in item_ids {
            let params: [&dyn ToSqlValue; 3] = [&tag_id, &item_type, &item_id];
            self.db.execute(&sql, &params)?;
            count += 1;
        }
        Ok(count)
    }

    pub fn batch_untag(
        &self,
        tag_id: i64,
        item_type: &str,
        item_ids: &[i64],
    ) -> Result<usize, String> {
        let mut count = 0;
        let sql = self.dialect_sql(sql::untag_item, sql::untag_item);
        for &item_id in item_ids {
            let params: [&dyn ToSqlValue; 3] = [&tag_id, &item_type, &item_id];
            self.db.execute(&sql, &params)?;
            count += 1;
        }
        Ok(count)
    }

    pub fn items_by_any_tags(&self, tag_ids: &[i64], item_type: &str) -> Result<Vec<i64>, String> {
        if tag_ids.is_empty() {
            return Ok(vec![]);
        }
        let sql = match self.db.engine() {
            Engine::Sqlite => sql::items_by_any_tags(&SqliteDialect, tag_ids.len()),
            Engine::Postgres => sql::items_by_any_tags(&PostgresDialect, tag_ids.len()),
        };
        let mut params: Vec<&dyn ToSqlValue> =
            tag_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
        params.push(&item_type);
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }

    // --- Etiquettes posees sur un objet de STREAMING (#3699) ---

    /// Pose une etiquette sur un objet designe par `source` + `source_id`.
    ///
    /// `item_type` passe par le MEME garde-fou que la pose locale : la liste
    /// fermee `TAGGABLE_ITEM_TYPES`. Un type inconnu ecrirait une ligne
    /// qu'aucune route de lecture ne nomme — la panne silencieuse que #2256 a
    /// fermee cote local n'a aucune raison de rouvrir ici.
    pub fn tag_streaming_item(&self, tag_id: i64, item: &StreamingTagItem) -> Result<(), String> {
        verifier_item_type(&item.item_type)?;
        if item.source.trim().is_empty() || item.source_id.trim().is_empty() {
            return Err(
                "source et source_id sont obligatoires : un objet de streaming se \
                 designe par la PAIRE, jamais par l'un des deux seul"
                    .into(),
            );
        }
        let sql = self.dialect_sql(sql::tag_streaming_item, sql::tag_streaming_item);
        let item_type = item.item_type.as_str();
        let source = item.source.as_str();
        let source_id = item.source_id.as_str();
        let title = item.title.as_deref();
        let artist = item.artist.as_deref();
        let album = item.album.as_deref();
        let cover_url = item.cover_url.as_deref();
        let params: [&dyn ToSqlValue; 8] = [
            &tag_id, &item_type, &source, &source_id, &title, &artist, &album, &cover_url,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Retire une etiquette d'un objet de streaming.
    ///
    /// **Volontairement sans verification d'`item_type`**, pour la meme raison
    /// que [`untag_item`](Self::untag_item) : une ligne ecrite avant un
    /// resserrement de la liste doit rester deracinable.
    pub fn untag_streaming_item(
        &self,
        tag_id: i64,
        item_type: &str,
        source: &str,
        source_id: &str,
    ) -> Result<(), String> {
        let sql = self.dialect_sql(sql::untag_streaming_item, sql::untag_streaming_item);
        let params: [&dyn ToSqlValue; 4] = [&tag_id, &item_type, &source, &source_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Les objets de streaming d'une etiquette, pour un type donne.
    ///
    /// Rendus depuis l'INSTANTANE de la table : aucun appel au service. C'est
    /// la garde du troisieme point du ticket — un `source_id` retire du
    /// catalogue ne peut ni vider cette liste, ni la faire attendre.
    pub fn streaming_items_by_tag(
        &self,
        tag_id: i64,
        item_type: &str,
    ) -> Result<Vec<StreamingTagItem>, String> {
        let sql = self.dialect_sql(sql::streaming_items_by_tag, sql::streaming_items_by_tag);
        let params: [&dyn ToSqlValue; 2] = [&tag_id, &item_type];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_streaming_tag_item).collect())
    }

    /// Tous les objets de streaming d'une etiquette, tous types confondus.
    pub fn all_streaming_items_by_tag(&self, tag_id: i64) -> Result<Vec<StreamingTagItem>, String> {
        let sql = self.dialect_sql(
            sql::all_streaming_items_by_tag,
            sql::all_streaming_items_by_tag,
        );
        let params: [&dyn ToSqlValue; 1] = [&tag_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_streaming_tag_item).collect())
    }

    /// Les etiquettes posees sur un objet de streaming.
    pub fn tags_for_streaming_item(
        &self,
        item_type: &str,
        source: &str,
        source_id: &str,
    ) -> Result<Vec<Tag>, String> {
        let sql = self.dialect_sql(sql::tags_for_streaming_item, sql::tags_for_streaming_item);
        let params: [&dyn ToSqlValue; 3] = [&item_type, &source, &source_id];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_tag).collect())
    }

    pub fn items_by_all_tags(&self, tag_ids: &[i64], item_type: &str) -> Result<Vec<i64>, String> {
        if tag_ids.is_empty() {
            return Ok(vec![]);
        }
        let count = tag_ids.len() as i64;
        let sql = match self.db.engine() {
            Engine::Sqlite => sql::items_by_all_tags(&SqliteDialect, tag_ids.len()),
            Engine::Postgres => sql::items_by_all_tags(&PostgresDialect, tag_ids.len()),
        };
        let mut params: Vec<&dyn ToSqlValue> =
            tag_ids.iter().map(|id| id as &dyn ToSqlValue).collect();
        params.push(&item_type);
        params.push(&count);
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows
            .into_iter()
            .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
            .collect())
    }
}

fn row_to_streaming_tag_item(cols: &Vec<SqlValue>) -> StreamingTagItem {
    StreamingTagItem {
        item_type: cols.first().and_then(|v| v.as_string()).unwrap_or_default(),
        source: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        source_id: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        title: cols.get(3).and_then(|v| v.as_string()),
        artist: cols.get(4).and_then(|v| v.as_string()),
        album: cols.get(5).and_then(|v| v.as_string()),
        cover_url: cols.get(6).and_then(|v| v.as_string()),
    }
}

fn row_to_tag(cols: &Vec<SqlValue>) -> Tag {
    Tag {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        color: cols
            .get(2)
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| "#808080".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    #[test]
    fn tags_crud_and_tagging() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);

        let id = repo.create("Jazz", Some("#FFD700")).unwrap();
        let tags = repo.list().unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].color, "#FFD700");

        repo.tag_item(id, "album", 1).unwrap();
        repo.tag_item(id, "album", 2).unwrap();

        let items = repo.items_by_tag(id, "album").unwrap();
        assert_eq!(items, vec![1, 2]);

        let album_tags = repo.tags_for_item("album", 1).unwrap();
        assert_eq!(album_tags.len(), 1);

        repo.untag_item(id, "album", 1).unwrap();
        assert_eq!(repo.items_by_tag(id, "album").unwrap(), vec![2]);

        repo.delete(id).unwrap();
        assert!(repo.list().unwrap().is_empty());
    }

    #[test]
    fn tag_update() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let id = repo.create("Rock", Some("#FF0000")).unwrap();

        repo.update(id, Some("Rock & Roll"), Some("#00FF00"))
            .unwrap();
        let tag = repo.get(id).unwrap().unwrap();
        assert_eq!(tag.name, "Rock & Roll");
        assert_eq!(tag.color, "#00FF00");
    }

    #[test]
    fn tag_default_color() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let id = repo.create("NoColor", None).unwrap();
        let tag = repo.get(id).unwrap().unwrap();
        assert_eq!(tag.color, "#808080");
    }

    #[test]
    fn tag_get_nonexistent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        assert!(repo.get(999).unwrap().is_none());
    }

    #[test]
    fn tag_all_items_by_tag() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Favorites", None).unwrap();

        repo.tag_item(tag_id, "album", 1).unwrap();
        repo.tag_item(tag_id, "album", 2).unwrap();
        repo.tag_item(tag_id, "track", 10).unwrap();
        repo.tag_item(tag_id, "artist", 5).unwrap();

        let all_items = repo.all_items_by_tag(tag_id).unwrap();
        assert_eq!(all_items.len(), 4);
        assert_eq!(all_items[0].0, "album");
        assert_eq!(all_items[0].1, 1);
    }

    #[test]
    fn tag_item_idempotent() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Test", None).unwrap();

        repo.tag_item(tag_id, "album", 1).unwrap();
        repo.tag_item(tag_id, "album", 1).unwrap();

        let items = repo.items_by_tag(tag_id, "album").unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn tag_multiple_tags_per_item() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let jazz = repo.create("Jazz", Some("#FFD700")).unwrap();
        let fav = repo.create("Favorites", Some("#FF0000")).unwrap();

        repo.tag_item(jazz, "album", 1).unwrap();
        repo.tag_item(fav, "album", 1).unwrap();

        let album_tags = repo.tags_for_item("album", 1).unwrap();
        assert_eq!(album_tags.len(), 2);
    }

    #[test]
    fn tag_delete_cascades_items() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("ToDelete", None).unwrap();
        repo.tag_item(tag_id, "album", 1).unwrap();
        repo.tag_item(tag_id, "album", 2).unwrap();

        repo.delete(tag_id).unwrap();
        assert!(repo.get(tag_id).unwrap().is_none());
    }

    #[test]
    fn sql_builders_dialect_placeholders() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(sql::create_tag(&s).contains("VALUES (?, ?)"));
        assert!(sql::create_tag(&p).contains("VALUES ($1, $2)"));
        assert!(sql::tag_item(&p).ends_with("ON CONFLICT (tag_id, item_type, item_id) DO NOTHING"));
    }

    #[test]
    fn tag_list_sorted() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        repo.create("Zebra", None).unwrap();
        repo.create("Alpha", None).unwrap();
        repo.create("Middle", None).unwrap();

        let tags = repo.list().unwrap();
        assert_eq!(tags.len(), 3);
        assert_eq!(tags[0].name, "Alpha");
        assert_eq!(tags[2].name, "Zebra");
    }

    /// Le défaut du chantier : un `item_type` mal orthographié entrait sans un
    /// mot. Il doit désormais être refusé, et **ne rien laisser en base** — un
    /// refus qui aurait quand même inséré serait pire que l'absence de règle,
    /// puisque le client croirait avoir échoué.
    #[test]
    fn tag_item_refuse_un_type_inconnu_et_n_ecrit_rien() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Jazz", None).unwrap();

        let erreur = repo.tag_item(tag_id, "albums", 1).unwrap_err();
        assert!(
            erreur.contains("albums"),
            "le message doit citer le type refusé, il dit : {erreur}"
        );
        assert!(
            erreur.contains("album") && erreur.contains("playlist"),
            "le message doit nommer les types admis, il dit : {erreur}"
        );

        assert!(
            repo.all_items_by_tag(tag_id).unwrap().is_empty(),
            "un type refusé ne doit rien écrire"
        );
    }

    /// Les cinq types du modèle, moins `label` : quatre acceptés, `label`
    /// refusé **tant qu'il n'a pas d'identité numérique** (voir la note de
    /// [`TAGGABLE_ITEM_TYPES`]).
    #[test]
    fn les_quatre_types_a_identifiant_passent_et_label_est_refuse() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Test", None).unwrap();

        for (n, t) in ["album", "artist", "playlist", "track"].iter().enumerate() {
            repo.tag_item(tag_id, t, n as i64 + 1)
                .unwrap_or_else(|e| panic!("{t} doit être accepté : {e}"));
        }
        assert_eq!(repo.all_items_by_tag(tag_id).unwrap().len(), 4);

        assert!(
            repo.tag_item(tag_id, "label", 1).is_err(),
            "`label` n'a pas d'identifiant numérique : item_id INTEGER ne peut pas le porter"
        );
    }

    /// `batch_tag` ne passe pas par `tag_item`. Sans vérification propre, il
    /// serait le trou du garde-fou — et un lot écrit **par centaines**.
    /// Le refus doit tomber AVANT la première insertion.
    #[test]
    fn batch_tag_refuse_le_lot_entier_sans_ecriture_partielle() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Lot", None).unwrap();

        assert!(repo.batch_tag(tag_id, "Album", &[1, 2, 3]).is_err());
        assert!(
            repo.all_items_by_tag(tag_id).unwrap().is_empty(),
            "aucune ligne du lot refusé ne doit rester"
        );

        assert_eq!(repo.batch_tag(tag_id, "album", &[1, 2, 3]).unwrap(), 3);
    }

    /// La suppression reste ouverte à un type qui n'est plus admis, sinon une
    /// ligne écrite avant ce garde-fou serait indéracinable.
    #[test]
    fn untag_item_atteint_encore_une_ligne_de_type_hors_liste() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = TagRepo::new(db);
        let tag_id = repo.create("Ancien", None).unwrap();

        // Écrite comme l'aurait fait la version sans garde-fou : par l'INSERT,
        // sans passer par `tag_item`.
        let params: [&dyn ToSqlValue; 1] = [&tag_id];
        repo.db
            .execute(
                "INSERT INTO item_tags (tag_id, item_type, item_id) VALUES (?, 'albums', 7)",
                &params,
            )
            .unwrap();
        assert_eq!(repo.all_items_by_tag(tag_id).unwrap().len(), 1);

        repo.untag_item(tag_id, "albums", 7).unwrap();
        assert!(
            repo.all_items_by_tag(tag_id).unwrap().is_empty(),
            "une ligne héritée doit rester supprimable"
        );
    }

    #[test]
    fn with_backend_constructor() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = TagRepo::with_backend(backend);
        let id = repo.create("X", None).unwrap();
        assert!(repo.get(id).unwrap().is_some());
    }

    // --- Etiquettes posees sur un objet de STREAMING (#3699) ---

    fn base() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    fn album_qobuz(source_id: &str, titre: &str) -> StreamingTagItem {
        StreamingTagItem {
            item_type: "album".into(),
            source: "qobuz".into(),
            source_id: source_id.into(),
            title: Some(titre.into()),
            artist: Some("Keith Jarrett".into()),
            album: None,
            cover_url: Some(format!("https://static.qobuz.com/{source_id}.jpg")),
        }
    }

    /// Poser, lire, retirer — dans l'espace du streaming.
    #[test]
    fn etiqueter_un_album_de_streaming() {
        let repo = TagRepo::new(base());
        let id = repo.create("Nuit", None).unwrap();

        repo.tag_streaming_item(id, &album_qobuz("0060254735368", "The Koln Concert"))
            .unwrap();

        let items = repo.streaming_items_by_tag(id, "album").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source, "qobuz");
        assert_eq!(items[0].source_id, "0060254735368");
        // L'instantane est bien la : c'est lui qui rendra la ligne quand le
        // catalogue ne repondra plus.
        assert_eq!(items[0].title.as_deref(), Some("The Koln Concert"));
        assert_eq!(items[0].artist.as_deref(), Some("Keith Jarrett"));

        let tags = repo
            .tags_for_streaming_item("album", "qobuz", "0060254735368")
            .unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].name, "Nuit");

        repo.untag_streaming_item(id, "album", "qobuz", "0060254735368")
            .unwrap();
        assert!(repo.streaming_items_by_tag(id, "album").unwrap().is_empty());
    }

    /// Deuxieme point du ticket : **l'unicite porte sur la PAIRE**.
    ///
    /// Etiqueter deux fois le meme album Qobuz ne doit pas creer deux lignes.
    /// Et deux albums de sources DIFFERENTES qui portent le meme `source_id`
    /// restent deux objets distincts — c'est exactement le piege des
    /// collections, ou l'identifiant 1 designe deux objets selon l'espace.
    #[test]
    fn l_unicite_porte_sur_la_paire_pas_sur_un_entier() {
        let repo = TagRepo::new(base());
        let id = repo.create("Nuit", None).unwrap();

        repo.tag_streaming_item(id, &album_qobuz("12345", "Un"))
            .unwrap();
        repo.tag_streaming_item(id, &album_qobuz("12345", "Un"))
            .unwrap();
        assert_eq!(
            repo.streaming_items_by_tag(id, "album").unwrap().len(),
            1,
            "le meme album Qobuz etiquete deux fois a cree deux lignes"
        );

        // Meme `source_id`, autre source : un AUTRE objet.
        let mut tidal = album_qobuz("12345", "Un");
        tidal.source = "tidal".into();
        repo.tag_streaming_item(id, &tidal).unwrap();
        assert_eq!(
            repo.streaming_items_by_tag(id, "album").unwrap().len(),
            2,
            "« qobuz/12345 » et « tidal/12345 » ont ete confondus : \
             un identifiant seul ne designe rien sans sa source"
        );

        // Et le meme identifiant dans l'espace LOCAL est encore un troisieme
        // objet : les deux tables ne se marchent pas dessus.
        repo.tag_item(id, "album", 12345).unwrap();
        assert_eq!(repo.items_by_tag(id, "album").unwrap(), vec![12345]);
        assert_eq!(repo.streaming_items_by_tag(id, "album").unwrap().len(), 2);
    }

    /// Un `item_type` inconnu est refuse ici AUSSI — le garde-fou de #2256 ne
    /// doit pas rouvrir par la porte du streaming.
    #[test]
    fn item_type_inconnu_refuse_dans_l_espace_du_streaming() {
        let repo = TagRepo::new(base());
        let id = repo.create("Nuit", None).unwrap();
        let mut mauvais = album_qobuz("1", "Un");
        mauvais.item_type = "albums".into();
        let err = repo.tag_streaming_item(id, &mauvais).unwrap_err();
        assert!(err.contains("albums"), "{err}");
        assert!(
            repo.streaming_items_by_tag(id, "albums")
                .unwrap()
                .is_empty()
        );
    }

    /// Une designation incomplete est refusee : la PAIRE, ou rien.
    #[test]
    fn une_source_sans_identifiant_est_refusee() {
        let repo = TagRepo::new(base());
        let id = repo.create("Nuit", None).unwrap();
        let mut sans = album_qobuz("", "Un");
        assert!(repo.tag_streaming_item(id, &sans).is_err());
        sans.source_id = "12".into();
        sans.source = "  ".into();
        assert!(repo.tag_streaming_item(id, &sans).is_err());
    }

    /// Le compte annonce par `/tags` porte sur les DEUX espaces.
    #[test]
    fn le_compte_porte_sur_les_deux_espaces() {
        let repo = TagRepo::new(base());
        let id = repo.create("Nuit", None).unwrap();
        repo.tag_item(id, "album", 1).unwrap();
        repo.tag_item(id, "album", 2).unwrap();
        repo.tag_streaming_item(id, &album_qobuz("12345", "Un"))
            .unwrap();

        let total = repo.list_with_counts(None).unwrap();
        assert_eq!(total.len(), 1);
        assert_eq!(
            total[0].count, 3,
            "le compte ignore l'espace du streaming : 2 albums locaux + 1 Qobuz"
        );

        let par_type = repo.list_with_counts(Some("album")).unwrap();
        assert_eq!(par_type[0].count, 3);
        let autres = repo.list_with_counts(Some("artist")).unwrap();
        assert_eq!(
            autres[0].count, 0,
            "une etiquette sans artiste doit rendre 0"
        );
    }

    /// Supprimer une etiquette emporte ses poses de streaming.
    ///
    /// Il n'y a pas de clef etrangere sur cette table : sans le nettoyage
    /// explicite de `delete`, les lignes survivraient a l'etiquette et le
    /// compte d'une etiquette RECREEE sous le meme identifiant repartirait
    /// faux.
    #[test]
    fn supprimer_une_etiquette_emporte_ses_poses_de_streaming() {
        let db = base();
        let repo = TagRepo::new(db);
        let id = repo.create("Nuit", None).unwrap();
        repo.tag_streaming_item(id, &album_qobuz("12345", "Un"))
            .unwrap();
        repo.delete(id).unwrap();
        assert!(
            repo.streaming_items_by_tag(id, "album").unwrap().is_empty(),
            "les poses de streaming ont survecu a la suppression de l'etiquette"
        );
        assert!(
            repo.tags_for_streaming_item("album", "qobuz", "12345")
                .unwrap()
                .is_empty()
        );
    }
}
