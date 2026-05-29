use crate::db::sqlite::SqliteDb;

pub struct ExportService {
    db: SqliteDb,
}

impl ExportService {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn export_albums_csv(&self) -> Result<String, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, title, artist_name, year, original_year, \
                 release_date, original_date, genre, track_count, \
                 disc_count, format, sample_rate, bit_depth, label, \
                 catalog_number, musicbrainz_release_id, source \
                 FROM albums ORDER BY artist_name, title",
            )
            .map_err(|e| e.to_string())?;

        let mut wtr = csv::WriterBuilder::new()
            .delimiter(b';')
            .from_writer(Vec::new());

        wtr.write_record([
            "id", "title", "artist_name", "year", "original_year",
            "release_date", "original_date", "genre", "track_count",
            "disc_count", "format", "sample_rate", "bit_depth", "label",
            "catalog_number", "musicbrainz_release_id", "source",
        ])
        .map_err(|e| e.to_string())?;

        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let record: Vec<String> = (0..17)
                .map(|i| val(row, i))
                .collect();
            wtr.write_record(&record).map_err(|e| e.to_string())?;
        }

        let bytes = wtr.into_inner().map_err(|e| e.to_string())?;
        let mut output = String::from("\u{FEFF}");
        output.push_str(&String::from_utf8_lossy(&bytes));
        Ok(output)
    }

    pub fn export_tracks_csv(&self) -> Result<String, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, title, artist_name, album_title, track_number, \
                 disc_number, disc_subtitle, duration_ms, format, \
                 sample_rate, bit_depth, channels, file_path, source, \
                 musicbrainz_recording_id \
                 FROM tracks ORDER BY artist_name, album_title, disc_number, track_number",
            )
            .map_err(|e| e.to_string())?;

        let mut wtr = csv::WriterBuilder::new()
            .delimiter(b';')
            .from_writer(Vec::new());

        wtr.write_record([
            "id", "title", "artist_name", "album_title", "track_number",
            "disc_number", "disc_subtitle", "duration", "duration_ms",
            "format", "sample_rate", "bit_depth", "channels", "file_path",
            "source", "musicbrainz_recording_id",
        ])
        .map_err(|e| e.to_string())?;

        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let duration_ms: i64 = row.get::<_, i64>(7).unwrap_or(0);
            let duration_fmt = format_duration(duration_ms);

            let mut record: Vec<String> = (0..7).map(|i| val(row, i)).collect();
            record.push(duration_fmt);
            record.push(if duration_ms > 0 {
                duration_ms.to_string()
            } else {
                String::new()
            });
            for i in 8..15 {
                record.push(val(row, i));
            }
            wtr.write_record(&record).map_err(|e| e.to_string())?;
        }

        let bytes = wtr.into_inner().map_err(|e| e.to_string())?;
        let mut output = String::from("\u{FEFF}");
        output.push_str(&String::from_utf8_lossy(&bytes));
        Ok(output)
    }

    pub fn export_artists_csv(&self) -> Result<String, String> {
        let conn = self.db.connection();
        let conn = conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, sort_name, musicbrainz_id, bio, image_path \
                 FROM artists ORDER BY sort_name, name",
            )
            .map_err(|e| e.to_string())?;

        let mut wtr = csv::WriterBuilder::new()
            .delimiter(b';')
            .from_writer(Vec::new());

        wtr.write_record(["id", "name", "sort_name", "musicbrainz_id", "bio", "image_path"])
            .map_err(|e| e.to_string())?;

        let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let record: Vec<String> = (0..6).map(|i| val(row, i)).collect();
            wtr.write_record(&record).map_err(|e| e.to_string())?;
        }

        let bytes = wtr.into_inner().map_err(|e| e.to_string())?;
        let mut output = String::from("\u{FEFF}");
        output.push_str(&String::from_utf8_lossy(&bytes));
        Ok(output)
    }
}

fn val(row: &rusqlite::Row, idx: usize) -> String {
    row.get::<_, String>(idx).unwrap_or_default()
}

fn format_duration(ms: i64) -> String {
    if ms <= 0 {
        return String::new();
    }
    let total_s = ms / 1000;
    let minutes = total_s / 60;
    let seconds = total_s % 60;
    format!("{minutes}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_format() {
        assert_eq!(format_duration(0), "");
        assert_eq!(format_duration(60_000), "1:00");
        assert_eq!(format_duration(185_000), "3:05");
        assert_eq!(format_duration(3_661_000), "61:01");
    }

    fn test_db_with_schema(schema: &str) -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.execute_batch(schema).unwrap();
        db
    }

    #[test]
    fn duration_format_edge() {
        assert_eq!(format_duration(-100), "");
        assert_eq!(format_duration(500), "0:00");
        assert_eq!(format_duration(61_999), "1:01");
    }

    #[test]
    fn export_albums_on_empty_db() {
        let db = test_db_with_schema(
            "CREATE TABLE albums(
                id INTEGER, title TEXT, artist_name TEXT, year TEXT,
                original_year TEXT, release_date TEXT, original_date TEXT,
                genre TEXT, track_count INTEGER, disc_count INTEGER,
                format TEXT, sample_rate INTEGER, bit_depth INTEGER,
                label TEXT, catalog_number TEXT, musicbrainz_release_id TEXT,
                source TEXT
            )",
        );
        let svc = ExportService::new(db);
        let csv = svc.export_albums_csv().unwrap();
        assert!(csv.starts_with('\u{FEFF}'));
        assert!(csv.contains("id;title;artist_name"));
    }

    #[test]
    fn export_tracks_on_empty_db() {
        let db = test_db_with_schema(
            "CREATE TABLE tracks(
                id INTEGER, title TEXT, artist_name TEXT, album_title TEXT,
                track_number INTEGER, disc_number INTEGER, disc_subtitle TEXT,
                duration_ms INTEGER, format TEXT, sample_rate INTEGER,
                bit_depth INTEGER, channels INTEGER, file_path TEXT,
                source TEXT, musicbrainz_recording_id TEXT
            )",
        );
        let svc = ExportService::new(db);
        let csv = svc.export_tracks_csv().unwrap();
        assert!(csv.contains("id;title;artist_name;album_title"));
    }

    #[test]
    fn export_artists_on_empty_db() {
        let db = test_db_with_schema(
            "CREATE TABLE artists(
                id INTEGER, name TEXT, sort_name TEXT,
                musicbrainz_id TEXT, bio TEXT, image_path TEXT
            )",
        );
        let svc = ExportService::new(db);
        let csv = svc.export_artists_csv().unwrap();
        assert!(csv.contains("id;name;sort_name"));
    }
}
