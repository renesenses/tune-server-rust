use rusqlite::params;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEditRequest {
    pub track_ids: Vec<i64>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub composer: Option<String>,
    pub label: Option<String>,
    pub bpm: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameArtistRequest {
    pub old_name: String,
    pub new_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    pub updated: usize,
    pub errors: usize,
}

pub fn batch_edit_tracks(db: &SqliteDb, request: &BatchEditRequest) -> BatchResult {
    let conn = db.connection().lock().unwrap();
    let mut updated = 0;
    let mut errors = 0;

    for &track_id in &request.track_ids {
        let mut sets = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(ref genre) = request.genre {
            sets.push("genre = ?");
            values.push(Box::new(genre.clone()));
        }
        if let Some(year) = request.year {
            sets.push("year = ?");
            values.push(Box::new(year));
        }
        if let Some(ref composer) = request.composer {
            sets.push("composer = ?");
            values.push(Box::new(composer.clone()));
        }
        if let Some(ref label) = request.label {
            sets.push("label = ?");
            values.push(Box::new(label.clone()));
        }
        if let Some(bpm) = request.bpm {
            sets.push("bpm = ?");
            values.push(Box::new(bpm));
        }

        if sets.is_empty() {
            continue;
        }

        values.push(Box::new(track_id));
        let sql = format!("UPDATE tracks SET {} WHERE id = ?", sets.join(", "));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();

        match conn.execute(&sql, param_refs.as_slice()) {
            Ok(n) => updated += n,
            Err(e) => {
                warn!(track_id, error = %e, "batch_edit_error");
                errors += 1;
            }
        }
    }

    info!(updated, errors, tracks = request.track_ids.len(), "batch_edit_complete");
    BatchResult { updated, errors }
}

pub fn rename_artist(db: &SqliteDb, request: &RenameArtistRequest) -> BatchResult {
    let conn = db.connection().lock().unwrap();
    let mut updated = 0;
    let mut errors = 0;

    // Update artists table
    match conn.execute(
        "UPDATE artists SET name = ? WHERE name = ?",
        params![request.new_name, request.old_name],
    ) {
        Ok(n) => updated += n,
        Err(e) => {
            warn!(error = %e, "rename_artist_table_error");
            errors += 1;
        }
    }

    // Update album_artist field in tracks
    match conn.execute(
        "UPDATE tracks SET album_artist = ? WHERE album_artist = ?",
        params![request.new_name, request.old_name],
    ) {
        Ok(n) => updated += n,
        Err(e) => {
            warn!(error = %e, "rename_album_artist_error");
            errors += 1;
        }
    }

    info!(
        old = request.old_name,
        new = request.new_name,
        updated,
        "artist_renamed"
    );

    BatchResult { updated, errors }
}

pub fn batch_write_tags_list(db: &SqliteDb, track_ids: &[i64]) -> Vec<TagWriteJob> {
    let conn = db.connection().lock().unwrap();
    let mut jobs = Vec::new();

    for &id in track_ids {
        let result = conn.query_row(
            "SELECT t.file_path, t.title, ar.name, al.title, t.genre, t.year, t.track_number, t.disc_number, t.composer \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE t.id = ? AND t.file_path IS NOT NULL",
            params![id],
            |row| {
                Ok(TagWriteJob {
                    track_id: id,
                    file_path: row.get(0)?,
                    title: row.get(1)?,
                    artist: row.get(2)?,
                    album: row.get(3)?,
                    genre: row.get(4)?,
                    year: row.get(5)?,
                    track_number: row.get(6)?,
                    disc_number: row.get(7)?,
                    composer: row.get(8)?,
                })
            },
        );

        if let Ok(job) = result {
            jobs.push(job);
        }
    }

    jobs
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagWriteJob {
    pub track_id: i64,
    pub file_path: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub composer: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn batch_edit_empty() {
        let db = test_db();
        let req = BatchEditRequest {
            track_ids: vec![],
            genre: Some("Rock".into()),
            year: None,
            composer: None,
            label: None,
            bpm: None,
        };
        let result = batch_edit_tracks(&db, &req);
        assert_eq!(result.updated, 0);
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn batch_edit_nonexistent_track() {
        let db = test_db();
        let req = BatchEditRequest {
            track_ids: vec![999],
            genre: Some("Jazz".into()),
            year: None,
            composer: None,
            label: None,
            bpm: None,
        };
        let result = batch_edit_tracks(&db, &req);
        assert_eq!(result.updated, 0);
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn rename_artist_empty() {
        let db = test_db();
        let req = RenameArtistRequest {
            old_name: "Old".into(),
            new_name: "New".into(),
        };
        let result = rename_artist(&db, &req);
        assert_eq!(result.errors, 0);
    }

    #[test]
    fn batch_result_serialize() {
        let r = BatchResult {
            updated: 5,
            errors: 1,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["updated"], 5);
    }

    #[test]
    fn tag_write_job_serialize() {
        let job = TagWriteJob {
            track_id: 1,
            file_path: "/music/song.flac".into(),
            title: Some("Song".into()),
            artist: Some("Artist".into()),
            album: Some("Album".into()),
            genre: Some("Rock".into()),
            year: Some(2024),
            track_number: Some(1),
            disc_number: Some(1),
            composer: None,
        };
        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(json["file_path"], "/music/song.flac");
    }
}
