use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RadioFavorite {
    pub id: i64,
    pub title: String,
    pub artist: String,
    pub station_name: String,
    pub cover_url: Option<String>,
    pub stream_url: Option<String>,
    pub saved_at: i64,
}

pub struct RadioFavoriteRepo {
    db: Arc<dyn DbBackend>,
}

impl RadioFavoriteRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    pub fn setup_table(&self) -> Result<(), String> {
        self.db.execute_batch(
            "CREATE TABLE IF NOT EXISTS radio_favorites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                title TEXT NOT NULL,
                artist TEXT NOT NULL DEFAULT '',
                station_name TEXT NOT NULL DEFAULT '',
                cover_url TEXT,
                stream_url TEXT,
                saved_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_rf_saved_at ON radio_favorites(saved_at);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_rf_unique
                ON radio_favorites(title, artist, station_name);",
        )
    }

    pub fn save(
        &self,
        title: &str,
        artist: &str,
        station_name: &str,
        cover_url: Option<&str>,
        stream_url: Option<&str>,
    ) -> Result<Option<RadioFavorite>, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let affected = self.db.execute(
            "INSERT OR IGNORE INTO radio_favorites \
             (title, artist, station_name, cover_url, stream_url, saved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            &[
                &title,
                &artist,
                &station_name,
                &cover_url as &dyn crate::db::backend::ToSqlValue,
                &stream_url as &dyn crate::db::backend::ToSqlValue,
                &now,
            ],
        )?;

        if affected == 0 {
            Ok(None) // already exists
        } else {
            let id = self.db.last_insert_rowid();
            Ok(Some(RadioFavorite {
                id,
                title: title.into(),
                artist: artist.into(),
                station_name: station_name.into(),
                cover_url: cover_url.map(String::from),
                stream_url: stream_url.map(String::from),
                saved_at: now,
            }))
        }
    }

    pub fn list(&self, limit: usize, offset: usize) -> Result<Vec<RadioFavorite>, String> {
        let lim = limit as i64;
        let off = offset as i64;
        let rows = self.db.query_many(
            "SELECT id, title, artist, station_name, cover_url, stream_url, saved_at \
             FROM radio_favorites ORDER BY saved_at DESC LIMIT ?1 OFFSET ?2",
            &[&lim, &off],
        )?;

        Ok(rows
            .into_iter()
            .map(|r| RadioFavorite {
                id: r[0].as_i64().unwrap_or(0),
                title: r[1].as_string().unwrap_or_default(),
                artist: r[2].as_string().unwrap_or_default(),
                station_name: r[3].as_string().unwrap_or_default(),
                cover_url: r[4].as_string(),
                stream_url: r[5].as_string(),
                saved_at: r[6].as_i64().unwrap_or(0),
            })
            .collect())
    }

    pub fn count(&self) -> Result<i64, String> {
        let row = self
            .db
            .query_one("SELECT COUNT(*) FROM radio_favorites", &[])?;
        Ok(row.and_then(|r| r[0].as_i64()).unwrap_or(0))
    }

    pub fn is_favorite(&self, title: &str, artist: &str) -> Result<bool, String> {
        let row = self.db.query_one(
            "SELECT COUNT(*) FROM radio_favorites WHERE title = ?1 AND artist = ?2",
            &[&title, &artist],
        )?;
        let count = row.and_then(|r| r[0].as_i64()).unwrap_or(0);
        Ok(count > 0)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db
            .execute("DELETE FROM radio_favorites WHERE id = ?1", &[&id])?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), String> {
        self.db.execute("DELETE FROM radio_favorites", &[])?;
        Ok(())
    }

    pub fn export_csv(&self) -> Result<String, String> {
        let favs = self.list(10_000, 0)?;
        let mut wtr = csv::WriterBuilder::new()
            .delimiter(b';')
            .from_writer(Vec::new());

        wtr.write_record(["title", "artist", "station", "cover_url", "stream_url"])
            .map_err(|e| e.to_string())?;

        for f in &favs {
            wtr.write_record([
                &f.title,
                &f.artist,
                &f.station_name,
                f.cover_url.as_deref().unwrap_or(""),
                f.stream_url.as_deref().unwrap_or(""),
            ])
            .map_err(|e| e.to_string())?;
        }

        let bytes = wtr.into_inner().map_err(|e| e.to_string())?;
        let mut output = String::from("\u{FEFF}");
        output.push_str(&String::from_utf8_lossy(&bytes));
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> RadioFavoriteRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        let repo = RadioFavoriteRepo::new(db);
        repo.setup_table().unwrap();
        repo
    }

    #[test]
    fn save_and_list() {
        let repo = setup();
        let fav = repo
            .save("Bohemian Rhapsody", "Queen", "Classic Rock FM", None, None)
            .unwrap();
        assert!(fav.is_some());
        let fav = fav.unwrap();
        assert_eq!(fav.title, "Bohemian Rhapsody");

        let list = repo.list(10, 0).unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn duplicate_save_returns_none() {
        let repo = setup();
        repo.save("Song", "Artist", "Station", None, None).unwrap();
        let dup = repo.save("Song", "Artist", "Station", None, None).unwrap();
        assert!(dup.is_none());
        assert_eq!(repo.count().unwrap(), 1);
    }

    #[test]
    fn is_favorite_check() {
        let repo = setup();
        assert!(!repo.is_favorite("X", "Y").unwrap());
        repo.save("X", "Y", "Z", None, None).unwrap();
        assert!(repo.is_favorite("X", "Y").unwrap());
    }

    #[test]
    fn delete_and_clear() {
        let repo = setup();
        let fav = repo.save("A", "B", "C", None, None).unwrap().unwrap();
        repo.save("D", "E", "F", None, None).unwrap();
        assert_eq!(repo.count().unwrap(), 2);

        repo.delete(fav.id).unwrap();
        assert_eq!(repo.count().unwrap(), 1);

        repo.clear().unwrap();
        assert_eq!(repo.count().unwrap(), 0);
    }

    #[test]
    fn export_csv_format() {
        let repo = setup();
        repo.save("Song", "Artist", "Station FM", None, Some("http://stream"))
            .unwrap();
        let csv = repo.export_csv().unwrap();
        assert!(csv.starts_with('\u{FEFF}'));
        assert!(csv.contains("title;artist;station"));
        assert!(csv.contains("Song;Artist;Station FM"));
    }
}
