use std::collections::HashMap;
use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for album_metadata_repo.
pub mod sql {
    use super::SqlDialect;

    pub fn get_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT key, value FROM album_metadata WHERE album_id = {} ORDER BY key",
            d.placeholder(1)
        )
    }

    pub fn upsert<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO album_metadata (album_id, key, value) VALUES ({}, {}, {}) \
             ON CONFLICT (album_id, key) DO UPDATE SET value = excluded.value",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn delete_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM album_metadata WHERE album_id = {} AND key = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn delete_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM album_metadata WHERE album_id = {}",
            d.placeholder(1)
        )
    }
}

/// Clé, dans `album_metadata`, du marqueur d'édition manuelle (C3). Valeur :
/// tableau JSON de noms de champs. Voir
/// [`AlbumMetadataRepo::marquer_edition_manuelle`].
pub const CLE_EDITION_MANUELLE: &str = "edition_manuelle";

/// Album-level extended metadata (Vademecum k/v: conductor, performer,
/// barcode, catalog_number…), symmetric with [`super::track_metadata_repo`].
/// Before this store existed the web UI parked album-scoped fields on the
/// album's FIRST track, so they vanished when that track was rescanned.
pub struct AlbumMetadataRepo {
    db: Arc<dyn DbBackend>,
}

impl AlbumMetadataRepo {
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

    /// Get all metadata key-value pairs for an album.
    pub fn get_all(&self, album_id: i64) -> Result<HashMap<String, String>, String> {
        let sql = self.dialect_sql(sql::get_all, sql::get_all);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        let rows = self.db.query_many(&sql, &params)?;
        let mut map = HashMap::new();
        for cols in rows {
            let key = cols.first().and_then(|v| v.as_string()).unwrap_or_default();
            let value = cols.get(1).and_then(|v| v.as_string()).unwrap_or_default();
            if !key.is_empty() {
                map.insert(key, value);
            }
        }
        Ok(map)
    }

    /// Set a single metadata field (upsert).
    pub fn set(&self, album_id: i64, key: &str, value: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        let params: [&dyn ToSqlValue; 3] = [&album_id, &key, &value];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Set multiple metadata fields in a batch (upsert each).
    pub fn set_batch(&self, album_id: i64, fields: &HashMap<String, String>) -> Result<(), String> {
        if fields.is_empty() {
            return Ok(());
        }
        let sql = self.dialect_sql(sql::upsert, sql::upsert);
        for (key, value) in fields {
            let params: [&dyn ToSqlValue; 3] = [&album_id, &key.as_str(), &value.as_str()];
            self.db.execute(&sql, &params)?;
        }
        Ok(())
    }

    /// Marque des champs de l'album comme ÉDITÉS À LA MAIN.
    ///
    /// Arbitrage **C3** du chantier « gestion du tag compilation » (Bertrand,
    /// 14/09/2026) : « d'abord poser le marqueur d'édition manuelle, la
    /// réparation des bibliothèques déjà indexées ensuite ». Rien ne
    /// distinguait un champ corrigé par l'utilisateur d'un champ importé
    /// (cherché : `manually_edited`, `metadata_source`, `override` — aucun),
    /// donc aucune passe de réparation ne pouvait promettre de ne pas défaire
    /// une correction.
    ///
    /// Le marqueur vit ICI, dans `album_metadata`, sous la clé
    /// [`CLE_EDITION_MANUELLE`] : un tableau JSON trié de noms de champs
    /// (`"artist"`, `"title"`, `"year"`, `"is_compilation"`…). Pas de colonne :
    /// le magasin clé-valeur existe déjà sur les deux moteurs, et un tableau
    /// dit QUELS champs sont tenus, pas seulement « quelque chose l'a été ».
    ///
    /// Idempotent et cumulatif : marquer `title` puis `artist` laisse les deux.
    pub fn marquer_edition_manuelle(&self, album_id: i64, champs: &[&str]) -> Result<(), String> {
        let mut tenus = self.champs_edites_a_la_main(album_id)?;
        for c in champs {
            let c = c.trim();
            if !c.is_empty() && !tenus.iter().any(|t| t == c) {
                tenus.push(c.to_string());
            }
        }
        tenus.sort();
        let json = serde_json::to_string(&tenus).map_err(|e| e.to_string())?;
        self.set(album_id, CLE_EDITION_MANUELLE, &json)
    }

    /// Les champs de l'album tenus par une édition manuelle, triés. Vide si
    /// personne n'y a touché — ou si la valeur stockée n'est pas lisible, ce
    /// qui revient au même pour qui doit décider de réparer.
    pub fn champs_edites_a_la_main(&self, album_id: i64) -> Result<Vec<String>, String> {
        let tous = self.get_all(album_id)?;
        Ok(tous
            .get(CLE_EDITION_MANUELLE)
            .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
            .unwrap_or_default())
    }

    /// Delete a single metadata field.
    pub fn delete(&self, album_id: i64, key: &str) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_one, sql::delete_one);
        let params: [&dyn ToSqlValue; 2] = [&album_id, &key];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    /// Delete all metadata for an album.
    pub fn delete_all(&self, album_id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete_all, sql::delete_all);
        let params: [&dyn ToSqlValue; 1] = [&album_id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn setup_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Test Artist');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Test Album', 1);",
        )
        .unwrap();
        db
    }

    #[test]
    fn set_and_get() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        repo.set(1, "conductor", "Karajan").unwrap();
        repo.set(1, "barcode", "0028947758419").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta.get("conductor").unwrap(), "Karajan");
        assert_eq!(meta.get("barcode").unwrap(), "0028947758419");
    }

    #[test]
    fn upsert_overwrites() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        repo.set(1, "conductor", "Karajan").unwrap();
        repo.set(1, "conductor", "Abbado").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.get("conductor").unwrap(), "Abbado");
    }

    #[test]
    fn set_batch() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        let mut fields = HashMap::new();
        fields.insert("performer".into(), "Berliner Philharmoniker".into());
        fields.insert("catalog_number".into(), "477 5842".into());

        repo.set_batch(1, &fields).unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 2);
        assert_eq!(meta.get("catalog_number").unwrap(), "477 5842");
    }

    #[test]
    fn delete_one_and_all() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        repo.set(1, "conductor", "Karajan").unwrap();
        repo.set(1, "performer", "BPO").unwrap();
        repo.delete(1, "conductor").unwrap();

        let meta = repo.get_all(1).unwrap();
        assert_eq!(meta.len(), 1);
        assert!(meta.get("conductor").is_none());

        repo.delete_all(1).unwrap();
        assert!(repo.get_all(1).unwrap().is_empty());
    }

    #[test]
    fn empty_album_returns_empty_map() {
        let db = setup_db();
        let repo = AlbumMetadataRepo::new(db);

        let meta = repo.get_all(1).unwrap();
        assert!(meta.is_empty());
    }
    #[test]
    fn le_marqueur_d_edition_manuelle_est_cumulatif_idempotent_et_trie() {
        let repo = AlbumMetadataRepo::new(setup_db());
        assert!(repo.champs_edites_a_la_main(1).unwrap().is_empty());
        repo.marquer_edition_manuelle(1, &["title"]).unwrap();
        repo.marquer_edition_manuelle(1, &["artist", "title", " ", "artist"])
            .unwrap();
        assert_eq!(
            repo.champs_edites_a_la_main(1).unwrap(),
            vec!["artist", "title"]
        );
        // Il vit dans le magasin ordinaire, sous sa clé : lisible par
        // `GET /albums/{id}/metadata` sans route dédiée.
        assert_eq!(
            repo.get_all(1)
                .unwrap()
                .get(CLE_EDITION_MANUELLE)
                .map(String::as_str),
            Some(r#"["artist","title"]"#)
        );
    }

    #[test]
    fn une_valeur_illisible_vaut_aucun_champ_tenu() {
        let repo = AlbumMetadataRepo::new(setup_db());
        repo.set(1, CLE_EDITION_MANUELLE, "pas du json").unwrap();
        assert!(repo.champs_edites_a_la_main(1).unwrap().is_empty());
        // Et marquer par-dessus repart proprement.
        repo.marquer_edition_manuelle(1, &["year"]).unwrap();
        assert_eq!(repo.champs_edites_a_la_main(1).unwrap(), vec!["year"]);
    }
}
