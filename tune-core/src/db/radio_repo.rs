use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioStation {
    pub id: Option<i64>,
    pub name: String,
    pub url: String,
    pub homepage: Option<String>,
    pub logo_url: Option<String>,
    pub country: Option<String>,
    pub language: Option<String>,
    pub genre: Option<String>,
    pub codec: Option<String>,
    pub bitrate: Option<i32>,
    pub is_favorite: bool,
    pub last_played: Option<String>,
    pub play_count: i64,
}

pub struct RadioRepo {
    db: SqliteDb,
}

impl RadioRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn get(&self, id: i64) -> Result<Option<RadioStation>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite, last_played, play_count FROM radio_stations WHERE id = ?")
            .map_err(|e| e.to_string())?;
        stmt.query_row(params![id], |row| Ok(row_to_radio(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<RadioStation>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite, last_played, play_count FROM radio_stations ORDER BY is_favorite DESC, name COLLATE NOCASE")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| Ok(row_to_radio(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn favorites(&self) -> Result<Vec<RadioStation>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite, last_played, play_count FROM radio_stations WHERE is_favorite = 1 ORDER BY name COLLATE NOCASE")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map([], |row| Ok(row_to_radio(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn create(&self, station: &RadioStation) -> Result<i64, String> {
        self.db.execute(
            "INSERT INTO radio_stations (name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &station.name as &dyn rusqlite::types::ToSql,
                &station.url,
                &station.homepage,
                &station.logo_url,
                &station.country,
                &station.language,
                &station.genre,
                &station.codec,
                &station.bitrate,
                &station.is_favorite,
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db
            .execute("DELETE FROM radio_stations WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn set_favorite(&self, id: i64, favorite: bool) -> Result<(), String> {
        self.db.execute(
            "UPDATE radio_stations SET is_favorite = ? WHERE id = ?",
            &[&favorite as &dyn rusqlite::types::ToSql, &id],
        )?;
        Ok(())
    }

    pub fn record_play(&self, id: i64) -> Result<(), String> {
        self.db.execute(
            "UPDATE radio_stations SET play_count = play_count + 1, last_played = strftime('%Y-%m-%dT%H:%M:%SZ', 'now') WHERE id = ?",
            &[&id],
        )?;
        Ok(())
    }

    pub fn search(&self, query: &str) -> Result<Vec<RadioStation>, String> {
        let like = format!("%{query}%");
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, url, homepage, logo_url, country, language, genre, codec, bitrate, is_favorite, last_played, play_count FROM radio_stations WHERE name LIKE ? OR genre LIKE ? OR country LIKE ? ORDER BY is_favorite DESC, name COLLATE NOCASE LIMIT 50")
            .map_err(|e| e.to_string())?;
        let items = stmt
            .query_map(params![like, like, like], |row| Ok(row_to_radio(row)))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(items)
    }

    pub fn update(&self, station: &RadioStation) -> Result<(), String> {
        let Some(id) = station.id else {
            return Err("station has no id".into());
        };
        self.db.execute(
            "UPDATE radio_stations SET name = ?, url = ?, homepage = ?, logo_url = ?, country = ?, language = ?, genre = ?, codec = ?, bitrate = ? WHERE id = ?",
            &[
                &station.name as &dyn rusqlite::types::ToSql,
                &station.url,
                &station.homepage,
                &station.logo_url,
                &station.country,
                &station.language,
                &station.genre,
                &station.codec,
                &station.bitrate,
                &id,
            ],
        )?;
        Ok(())
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection().lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM radio_stations", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }
}

fn row_to_radio(row: &rusqlite::Row) -> RadioStation {
    RadioStation {
        id: row.get(0).ok(),
        name: row.get(1).unwrap_or_default(),
        url: row.get(2).unwrap_or_default(),
        homepage: row.get(3).ok().flatten(),
        logo_url: row.get(4).ok().flatten(),
        country: row.get(5).ok().flatten(),
        language: row.get(6).ok().flatten(),
        genre: row.get(7).ok().flatten(),
        codec: row.get(8).ok().flatten(),
        bitrate: row.get(9).ok().flatten(),
        is_favorite: row.get::<_, i32>(10).unwrap_or(0) != 0,
        last_played: row.get(11).ok().flatten(),
        play_count: row.get(12).unwrap_or(0),
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
        // Favorites first in list
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
}
