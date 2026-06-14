use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::sqlite::SqliteDb;

/// Engine-agnostic SQL builders for radio_repo.
///
/// SQLite's `COLLATE NOCASE` is replaced by `LOWER(col)` for case-
/// insensitive ordering, which works identically on both engines.
/// The `LIKE` search uses `LOWER(col) LIKE LOWER(?)` for the same
/// reason — avoiding ILIKE keeps the query portable.
pub mod sql {
    use super::SqlDialect;

    const COLS: &str = "id, name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite, last_played, play_count";

    pub fn get_by_id<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM radio_stations WHERE id = {}",
            d.placeholder(1)
        )
    }

    pub fn list_all() -> String {
        format!("SELECT {COLS} FROM radio_stations ORDER BY is_favorite DESC, LOWER(name)")
    }

    pub fn favorites() -> String {
        format!("SELECT {COLS} FROM radio_stations WHERE is_favorite = 1 ORDER BY LOWER(name)")
    }

    pub fn create<D: SqlDialect>(d: &D) -> String {
        format!(
            "INSERT INTO radio_stations (name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10)
        )
    }

    pub fn delete<D: SqlDialect>(d: &D) -> String {
        format!("DELETE FROM radio_stations WHERE id = {}", d.placeholder(1))
    }

    pub fn set_favorite<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE radio_stations SET is_favorite = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    /// `last_played` is now an ISO-8601 string parameter so the SQL no
    /// longer needs SQLite-specific `strftime('%Y-%m-%dT%H:%M:%SZ',
    /// 'now')`. The caller computes the current timestamp.
    pub fn record_play<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE radio_stations SET play_count = play_count + 1, last_played = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2)
        )
    }

    pub fn search<D: SqlDialect>(d: &D) -> String {
        format!(
            "SELECT {COLS} FROM radio_stations WHERE LOWER(name) LIKE LOWER({}) OR LOWER(genre) LIKE LOWER({}) OR LOWER(country) LIKE LOWER({}) ORDER BY is_favorite DESC, LOWER(name) LIMIT 50",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        )
    }

    pub fn update<D: SqlDialect>(d: &D) -> String {
        format!(
            "UPDATE radio_stations SET name = {}, url = {}, homepage = {}, logo_url = {}, country = {}, language = {}, genre = {}, codec = {}, bitrate = {} WHERE id = {}",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
            d.placeholder(8),
            d.placeholder(9),
            d.placeholder(10)
        )
    }

    pub fn count() -> &'static str {
        "SELECT COUNT(*) FROM radio_stations"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioStation {
    pub id: Option<i64>,
    pub name: String,
    #[serde(rename = "stream_url", alias = "url")]
    pub url: String,
    #[serde(rename = "homepage_url", alias = "homepage")]
    pub homepage: Option<String>,
    pub logo_url: Option<String>,
    pub country: Option<String>,
    pub language: Option<String>,
    pub genre: Option<String>,
    pub codec: Option<String>,
    pub bitrate: Option<i32>,
    #[serde(rename = "favorite", alias = "is_favorite")]
    pub is_favorite: bool,
    pub last_played: Option<String>,
    pub play_count: i64,
}

pub struct RadioRepo {
    db: Arc<dyn DbBackend>,
}

impl RadioRepo {
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

    pub fn get(&self, id: i64) -> Result<Option<RadioStation>, String> {
        let sql = self.dialect_sql(sql::get_by_id, sql::get_by_id);
        let params: [&dyn ToSqlValue; 1] = [&id];
        Ok(self.db.query_one(&sql, &params)?.as_ref().map(row_to_radio))
    }

    pub fn list(&self) -> Result<Vec<RadioStation>, String> {
        let rows = self.db.query_many(&sql::list_all(), &[])?;
        Ok(rows.iter().map(row_to_radio).collect())
    }

    pub fn favorites(&self) -> Result<Vec<RadioStation>, String> {
        let rows = self.db.query_many(&sql::favorites(), &[])?;
        Ok(rows.iter().map(row_to_radio).collect())
    }

    pub fn create(&self, station: &RadioStation) -> Result<i64, String> {
        let sql = self.dialect_sql(sql::create, sql::create);
        let params: [&dyn ToSqlValue; 10] = [
            &station.name,
            &station.url,
            &station.homepage,
            &station.logo_url,
            &station.country,
            &station.language,
            &station.genre,
            &station.codec,
            &station.bitrate,
            &station.is_favorite,
        ];
        self.db.execute(&sql, &params)?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::delete, sql::delete);
        let params: [&dyn ToSqlValue; 1] = [&id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn set_favorite(&self, id: i64, favorite: bool) -> Result<(), String> {
        let sql = self.dialect_sql(sql::set_favorite, sql::set_favorite);
        let params: [&dyn ToSqlValue; 2] = [&favorite, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn record_play(&self, id: i64) -> Result<(), String> {
        let sql = self.dialect_sql(sql::record_play, sql::record_play);
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let params: [&dyn ToSqlValue; 2] = [&now, &id];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn search(&self, query: &str) -> Result<Vec<RadioStation>, String> {
        let like = format!("%{query}%");
        let sql = self.dialect_sql(sql::search, sql::search);
        let params: [&dyn ToSqlValue; 3] = [&like, &like, &like];
        let rows = self.db.query_many(&sql, &params)?;
        Ok(rows.iter().map(row_to_radio).collect())
    }

    pub fn update(&self, station: &RadioStation) -> Result<(), String> {
        let Some(id) = station.id else {
            return Err("station has no id".into());
        };
        let sql = self.dialect_sql(sql::update, sql::update);
        let params: [&dyn ToSqlValue; 10] = [
            &station.name,
            &station.url,
            &station.homepage,
            &station.logo_url,
            &station.country,
            &station.language,
            &station.genre,
            &station.codec,
            &station.bitrate,
            &id,
        ];
        self.db.execute(&sql, &params)?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        match self.db.query_one(sql::count(), &[])? {
            None => Ok(0),
            Some(cols) => Ok(cols.first().and_then(|v| v.as_i64()).unwrap_or(0)),
        }
    }
}

fn row_to_radio(cols: &Vec<SqlValue>) -> RadioStation {
    RadioStation {
        id: cols.first().and_then(|v| v.as_i64()),
        name: cols.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
        url: cols.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
        homepage: cols.get(3).and_then(|v| v.as_string()),
        logo_url: cols.get(4).and_then(|v| v.as_string()),
        country: cols.get(5).and_then(|v| v.as_string()),
        language: cols.get(6).and_then(|v| v.as_string()),
        genre: cols.get(7).and_then(|v| v.as_string()),
        codec: cols.get(8).and_then(|v| v.as_string()),
        bitrate: cols.get(9).and_then(|v| v.as_i64()).map(|n| n as i32),
        is_favorite: cols.get(10).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
        last_played: cols.get(11).and_then(|v| v.as_string()),
        play_count: cols.get(12).and_then(|v| v.as_i64()).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    #[test]
    fn crud_radio_station() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        let station = RadioStation {
            id: None,
            name: "FIP".into(),
            url: "http://icecast.radiofrance.fr/fip-hifi.aac".into(),
            homepage: Some("https://www.fip.fr".into()),
            logo_url: None,
            country: Some("FR".into()),
            language: Some("fr".into()),
            genre: Some("Jazz, Eclectic".into()),
            codec: Some("AAC".into()),
            bitrate: Some(320),
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };

        let id = repo.create(&station).unwrap();
        assert!(id > 0);

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.name, "FIP");
        assert_eq!(fetched.country.as_deref(), Some("FR"));

        repo.set_favorite(id, true).unwrap();
        let favs = repo.favorites().unwrap();
        assert_eq!(favs.len(), 1);

        repo.record_play(id).unwrap();
        let updated = repo.get(id).unwrap().unwrap();
        assert_eq!(updated.play_count, 1);
        assert!(updated.last_played.is_some());

        repo.delete(id).unwrap();
        assert!(repo.get(id).unwrap().is_none());
    }

    #[test]
    fn radio_list_and_count() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        assert_eq!(repo.count().unwrap(), 0);

        let s1 = RadioStation {
            id: None,
            name: "FIP".into(),
            url: "http://fip.fr".into(),
            homepage: None,
            logo_url: None,
            country: Some("FR".into()),
            language: None,
            genre: Some("Jazz".into()),
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        let s2 = RadioStation {
            id: None,
            name: "BBC Radio 3".into(),
            url: "http://bbc.co.uk/radio3".into(),
            homepage: None,
            logo_url: None,
            country: Some("UK".into()),
            language: None,
            genre: Some("Classical".into()),
            codec: None,
            bitrate: None,
            is_favorite: true,
            last_played: None,
            play_count: 0,
        };

        repo.create(&s1).unwrap();
        repo.create(&s2).unwrap();
        assert_eq!(repo.count().unwrap(), 2);

        let all = repo.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "BBC Radio 3");
    }

    #[test]
    fn radio_search() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        for (name, genre, country) in [
            ("FIP Jazz", "Jazz", "FR"),
            ("BBC Classical", "Classical", "UK"),
            ("Jazz FM", "Jazz", "UK"),
        ] {
            repo.create(&RadioStation {
                id: None,
                name: name.into(),
                url: format!("http://{name}.com"),
                homepage: None,
                logo_url: None,
                country: Some(country.into()),
                language: None,
                genre: Some(genre.into()),
                codec: None,
                bitrate: None,
                is_favorite: false,
                last_played: None,
                play_count: 0,
            })
            .unwrap();
        }

        let jazz = repo.search("Jazz").unwrap();
        assert_eq!(jazz.len(), 2);

        let uk = repo.search("UK").unwrap();
        assert_eq!(uk.len(), 2);

        let bbc = repo.search("BBC").unwrap();
        assert_eq!(bbc.len(), 1);
    }

    #[test]
    fn radio_update() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        let station = RadioStation {
            id: None,
            name: "Old Name".into(),
            url: "http://old.com".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        let id = repo.create(&station).unwrap();

        let mut updated = repo.get(id).unwrap().unwrap();
        updated.name = "New Name".into();
        updated.url = "http://new.com".into();
        updated.country = Some("DE".into());
        repo.update(&updated).unwrap();

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.name, "New Name");
        assert_eq!(fetched.url, "http://new.com");
        assert_eq!(fetched.country.as_deref(), Some("DE"));
    }

    #[test]
    fn radio_update_no_id_fails() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        let station = RadioStation {
            id: None,
            name: "No ID".into(),
            url: "http://no-id.com".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        assert!(repo.update(&station).is_err());
    }

    #[test]
    fn radio_favorites_empty() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        assert!(repo.favorites().unwrap().is_empty());
    }

    #[test]
    fn sql_builders_replace_collate_with_lower() {
        let s = SqliteDialect;
        let p = PostgresDialect;
        assert!(!sql::list_all().contains("COLLATE"));
        assert!(!sql::favorites().contains("COLLATE"));
        assert!(!sql::search(&s).contains("COLLATE"));
        let pg_search = sql::search(&p);
        assert!(pg_search.contains("LOWER(name) LIKE LOWER($1)"));
        assert!(pg_search.ends_with("LIMIT 50"));
    }

    #[test]
    fn radio_multiple_plays() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        let repo = RadioRepo::new(db);
        let station = RadioStation {
            id: None,
            name: "Test".into(),
            url: "http://test.com".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        let id = repo.create(&station).unwrap();

        for _ in 0..5 {
            repo.record_play(id).unwrap();
        }

        let fetched = repo.get(id).unwrap().unwrap();
        assert_eq!(fetched.play_count, 5);
    }

    #[test]
    fn with_backend_constructor() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        let repo = RadioRepo::with_backend(backend);
        let station = RadioStation {
            id: None,
            name: "X".into(),
            url: "http://x.com".into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        };
        let id = repo.create(&station).unwrap();
        assert_eq!(repo.get(id).unwrap().unwrap().name, "X");
    }
}
