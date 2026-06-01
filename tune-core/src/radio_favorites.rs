use crate::db::sqlite::SqliteDb;
use serde::{Deserialize, Serialize};
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
    db: SqliteDb,
}

impl RadioFavoriteRepo {
    pub fn new(db: SqliteDb) -> Self {
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

        let conn = self.db.connection();
        let conn = conn.lock().unwrap();

        let result = conn.execute(
            "INSERT OR IGNORE INTO radio_favorites \
             (title, artist, station_name, cover_url, stream_url, saved_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![title, artist, station_name, cover_url, stream_url, now],
        );

        match result {
            Ok(0) => Ok(None), // already exists
            Ok(_) => {
                let id = conn.last_insert_rowid();
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
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn list(&self, limit: usize, offset: usize) -> Result<Vec<RadioFavorite>, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, title, artist, station_name, cover_url, stream_url, saved_at \
                 FROM radio_favorites ORDER BY saved_at DESC LIMIT ?1 OFFSET ?2",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map(rusqlite::params![limit as i64, offset as i64], |row| {
                Ok(RadioFavorite {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    artist: row.get(2)?,
                    station_name: row.get(3)?,
                    cover_url: row.get(4)?,
                    stream_url: row.get(5)?,
                    saved_at: row.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(rows)
    }

    pub fn count(&self) -> Result<i64, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM radio_favorites", [], |row| row.get(0))
            .map_err(|e| e.to_string())
    }

    pub fn is_favorite(&self, title: &str, artist: &str) -> Result<bool, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM radio_favorites WHERE title = ?1 AND artist = ?2",
                [title, artist],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        Ok(count > 0)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute("DELETE FROM radio_favorites WHERE id = ?1", [id])
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn clear(&self) -> Result<(), String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        conn.execute("DELETE FROM radio_favorites", [])
            .map_err(|e| e.to_string())?;
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
