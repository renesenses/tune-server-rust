//! Favoris de FACETTE — mettre en favori un label, sans inventer d'entité.
//!
//! #2442 (FabienM, fil 1557) : « il manque de pouvoir mettre en favoris une
//! PLAYLIST et un LABEL ». La playlist locale porte un `INTEGER PRIMARY KEY` :
//! elle entre dans `favorites` telle quelle. Le label, lui, **n'est pas un
//! objet** — il n'existe ni table `labels`, ni identifiant, ni route
//! bibliothèque : l'onglet Labels lit une *facette* et sélectionne par CHAÎNE.
//! Or `favorites.item_id` est un entier NOT NULL.
//!
//! Plutôt que de promouvoir le label en entité (normalisation d'un champ libre
//! et sale, migration, jointures — l'option coûteuse, écartée), on stocke la
//! VALEUR telle que la facette la sélectionne aujourd'hui, dans une table à
//! part qui n'altère pas `favorites`. La colonne `facet` la rend réutilisable
//! pour le genre, le format ou l'année sans nouvelle migration.
//!
//! Ids liés en i64 : `profile_id` est INTEGER côté SQLite et BIGINT côté
//! PostgreSQL (migration 035, qui répare aussi le cas de la bascule
//! SQLite→PG où `PG_FULL_SCHEMA` l'avait créée en TEXT).

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Nom de facette du label. La seule facette exposée aujourd'hui ; la table en
/// accepte d'autres sans changement de schéma.
pub const FACET_LABEL: &str = "label";

/// Une valeur de facette mise en favori.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavoriteFacet {
    pub profile_id: i64,
    pub facet: String,
    pub value: String,
    pub created_at: Option<String>,
}

/// Constructeurs SQL agnostiques du moteur.
pub mod sql {
    use super::SqlDialect;

    pub fn add<D: SqlDialect>(d: &D) -> String {
        // `created_at` est rempli par l'expression « maintenant » du moteur
        // plutôt que par le DEFAULT de la colonne : même raison que
        // `streaming_favorites_repo` — une table montée par un autre chemin
        // pourrait ne pas porter le DEFAULT, et `ORDER BY created_at` serait
        // alors non déterministe.
        format!(
            "INSERT INTO favorite_facets (profile_id, facet, value, created_at) \
             VALUES ({}, {}, {}, {}) \
             ON CONFLICT (profile_id, facet, value) DO NOTHING",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.now_iso8601(),
        )
    }

    pub fn remove<D: SqlDialect>(d: &D) -> String {
        format!(
            "DELETE FROM favorite_facets \
             WHERE profile_id = {} AND facet = {} AND value = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }

    pub fn count_one<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT COUNT(*) FROM favorite_facets \
             WHERE profile_id = {} AND facet = {} AND value = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
        )
    }

    pub fn list_by_facet<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT profile_id, facet, value, created_at FROM favorite_facets \
             WHERE profile_id = {} AND facet = {} ORDER BY created_at DESC, value ASC",
            d.placeholder(1),
            d.placeholder(2),
        )
    }

    pub fn list_all<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT profile_id, facet, value, created_at FROM favorite_facets \
             WHERE profile_id = {} ORDER BY facet ASC, created_at DESC, value ASC",
            d.placeholder(1),
        )
    }
}

pub struct FavoriteFacetsRepo {
    db: Arc<dyn DbBackend>,
}

impl FavoriteFacetsRepo {
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

    /// Une valeur vide n'est pas un favori : la facette « label » d'un album
    /// sans label rend une chaîne vide, et un cœur posé dessus donnerait une
    /// ligne que rien ne sait rouvrir. On refuse plutôt que d'écrire un
    /// fantôme.
    fn normalise(value: &str) -> Result<String, String> {
        let v = value.trim();
        if v.is_empty() {
            return Err("valeur de facette vide".into());
        }
        Ok(v.to_string())
    }

    pub fn add(&self, profile_id: i64, facet: &str, value: &str) -> Result<(), String> {
        let value = Self::normalise(value)?;
        let sql = self.dialect_sql(sql::add, sql::add);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &facet, &value];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn remove(&self, profile_id: i64, facet: &str, value: &str) -> Result<(), String> {
        let value = Self::normalise(value)?;
        let sql = self.dialect_sql(sql::remove, sql::remove);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &facet, &value];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn is_favorite(&self, profile_id: i64, facet: &str, value: &str) -> Result<bool, String> {
        let Ok(value) = Self::normalise(value) else {
            return Ok(false);
        };
        let sql = self.dialect_sql(sql::count_one, sql::count_one);
        let params: [&dyn ToSqlValue; 3] = [&profile_id, &facet, &value];
        match self.db.query_one(&sql, &params)? {
            None => Ok(false),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0) > 0),
        }
    }

    pub fn list(&self, profile_id: i64, facet: Option<&str>) -> Result<Vec<FavoriteFacet>, String> {
        let rows = match facet {
            Some(f) => {
                let sql = self.dialect_sql(sql::list_by_facet, sql::list_by_facet);
                let params: [&dyn ToSqlValue; 2] = [&profile_id, &f];
                self.db.query_many(&sql, &params)?
            }
            None => {
                let sql = self.dialect_sql(sql::list_all, sql::list_all);
                let params: [&dyn ToSqlValue; 1] = [&profile_id];
                self.db.query_many(&sql, &params)?
            }
        };
        Ok(rows
            .iter()
            .map(|r| FavoriteFacet {
                profile_id: r.first().and_then(|v| v.as_i64()).unwrap_or(1),
                facet: r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                value: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                created_at: r.get(3).and_then(|v| v.as_string()),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn repo() -> FavoriteFacetsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        FavoriteFacetsRepo::with_backend(Arc::new(db))
    }

    #[test]
    fn un_label_peut_etre_mis_en_favori_et_relu() {
        // Le cœur du label écrit ici, PAS dans `favorites` : un label n'a pas
        // d'identifiant entier.
        let r = repo();
        r.add(1, FACET_LABEL, "ECM Records").unwrap();

        assert!(r.is_favorite(1, FACET_LABEL, "ECM Records").unwrap());
        let list = r.list(1, Some(FACET_LABEL)).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].value, "ECM Records");
        assert_eq!(list[0].facet, FACET_LABEL);
    }

    #[test]
    fn le_favori_de_label_est_par_profil() {
        let r = repo();
        r.add(1, FACET_LABEL, "ECM Records").unwrap();
        assert!(!r.is_favorite(2, FACET_LABEL, "ECM Records").unwrap());
        assert!(r.list(2, Some(FACET_LABEL)).unwrap().is_empty());
    }

    #[test]
    fn poser_deux_fois_le_meme_coeur_ne_cree_qu_une_ligne() {
        let r = repo();
        r.add(1, FACET_LABEL, "ECM Records").unwrap();
        r.add(1, FACET_LABEL, "ECM Records").unwrap();
        assert_eq!(r.list(1, Some(FACET_LABEL)).unwrap().len(), 1);
    }

    #[test]
    fn retirer_le_coeur_efface_la_ligne() {
        let r = repo();
        r.add(1, FACET_LABEL, "ECM Records").unwrap();
        r.remove(1, FACET_LABEL, "ECM Records").unwrap();
        assert!(!r.is_favorite(1, FACET_LABEL, "ECM Records").unwrap());
        assert!(r.list(1, Some(FACET_LABEL)).unwrap().is_empty());
    }

    #[test]
    fn la_valeur_est_rognee_pour_que_le_coeur_se_retrouve() {
        // La facette rend la valeur brute de la base : « ECM Records » et
        // « ECM Records  » désigneraient deux favoris distincts, et le cœur
        // resterait éteint sur l'écran qui l'a posé.
        let r = repo();
        r.add(1, FACET_LABEL, "  ECM Records  ").unwrap();
        assert!(r.is_favorite(1, FACET_LABEL, "ECM Records").unwrap());
        assert_eq!(
            r.list(1, Some(FACET_LABEL)).unwrap()[0].value,
            "ECM Records"
        );
    }

    #[test]
    fn une_valeur_vide_est_refusee() {
        let r = repo();
        assert!(r.add(1, FACET_LABEL, "   ").is_err());
        assert!(r.list(1, None).unwrap().is_empty());
    }

    #[test]
    fn une_autre_facette_cohabite_sans_migration() {
        // La table est faite pour cela : genre, format, année n'auront pas
        // besoin d'un nouveau schéma.
        let r = repo();
        r.add(1, FACET_LABEL, "ECM Records").unwrap();
        r.add(1, "genre", "Jazz").unwrap();

        assert_eq!(r.list(1, Some(FACET_LABEL)).unwrap().len(), 1);
        assert_eq!(r.list(1, Some("genre")).unwrap().len(), 1);
        assert_eq!(r.list(1, None).unwrap().len(), 2);
    }
}
