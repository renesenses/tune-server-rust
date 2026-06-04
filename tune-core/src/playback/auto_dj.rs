use serde_json::{Value, json};

use crate::db::sqlite::SqliteDb;

pub fn generate_queue(db: &SqliteDb, seed_track_id: i64, count: usize) -> Vec<Value> {
    let conn = db.read_connection().lock().unwrap();

    // Load seed track metadata
    let seed = conn
        .query_row(
            "SELECT t.genre, t.year, t.bpm FROM tracks t WHERE t.id = ?",
            rusqlite::params![seed_track_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i32>>(1)?,
                    row.get::<_, Option<f64>>(2)?,
                ))
            },
        )
        .ok();

    let (genre, year, bpm) = seed.unwrap_or((None, None, None));

    // Build dynamic query based on available seed metadata
    let mut conditions = vec!["t.id != ?1".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(seed_track_id)];
    let mut param_idx = 2;

    if let Some(ref g) = genre {
        conditions.push(format!("t.genre LIKE ?{param_idx}"));
        params.push(Box::new(format!("%{g}%")));
        param_idx += 1;
    }

    if let Some(y) = year {
        conditions.push(format!(
            "t.year BETWEEN ?{param_idx} AND ?{}",
            param_idx + 1
        ));
        params.push(Box::new(y - 5));
        params.push(Box::new(y + 5));
        param_idx += 2;
    }

    if let Some(b) = bpm {
        if b > 0.0 {
            conditions.push(format!("t.bpm BETWEEN ?{param_idx} AND ?{}", param_idx + 1));
            params.push(Box::new(b * 0.85));
            params.push(Box::new(b * 1.15));
        }
    }

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
         FROM tracks t \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         WHERE {where_clause} \
         ORDER BY RANDOM() LIMIT ?",
    );

    params.push(Box::new(count as i64));
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok(json!({
                "track_id": row.get::<_, i64>(0)?,
                "title": row.get::<_, String>(1)?,
                "artist": row.get::<_, Option<String>>(2)?,
                "album": row.get::<_, Option<String>>(3)?,
                "duration_ms": row.get::<_, i64>(4)?,
                "genre": row.get::<_, Option<String>>(5)?,
                "year": row.get::<_, Option<i32>>(6)?,
                "bpm": row.get::<_, Option<f64>>(7)?,
            }))
        })
        .ok();

    let mut results: Vec<Value> = rows
        .map(|r| r.collect::<Result<Vec<_>, _>>().unwrap_or_default())
        .unwrap_or_default();

    // Fallback to random if no matches
    if results.is_empty() && (genre.is_some() || year.is_some() || bpm.is_some()) {
        let mut fallback = conn
            .prepare(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
                 FROM tracks t \
                 LEFT JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE t.id != ? \
                 ORDER BY RANDOM() LIMIT ?",
            )
            .ok();
        if let Some(ref mut s) = fallback {
            results = s
                .query_map(rusqlite::params![seed_track_id, count as i64], |row| {
                    Ok(json!({
                        "track_id": row.get::<_, i64>(0)?,
                        "title": row.get::<_, String>(1)?,
                        "artist": row.get::<_, Option<String>>(2)?,
                        "album": row.get::<_, Option<String>>(3)?,
                        "duration_ms": row.get::<_, i64>(4)?,
                        "genre": row.get::<_, Option<String>>(5)?,
                        "year": row.get::<_, Option<i32>>(6)?,
                        "bpm": row.get::<_, Option<f64>>(7)?,
                    }))
                })
                .ok()
                .map(|r| r.collect::<Result<Vec<_>, _>>().unwrap_or_default())
                .unwrap_or_default();
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn empty_library_returns_empty() {
        let db = test_db();
        let result = generate_queue(&db, 1, 10);
        assert!(result.is_empty());
    }

    #[test]
    fn generates_queue_from_seed() {
        let db = test_db();
        let conn = db.connection().lock().unwrap();
        conn.execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            [],
        )
        .unwrap();
        for i in 1..=10 {
            conn.execute(
                "INSERT INTO tracks (id, title, artist_id, album_id, genre, year, duration_ms) VALUES (?, ?, 1, 1, 'Jazz', 2000, 240000)",
                rusqlite::params![i, format!("Track {i}")],
            ).unwrap();
        }
        drop(conn);

        let result = generate_queue(&db, 1, 5);
        assert_eq!(result.len(), 5);
        assert!(result.iter().all(|t| t["track_id"].as_i64().unwrap() != 1));
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mood {
    Chill,
    Party,
    Focus,
    Energetic,
}

impl Mood {
    pub fn bpm_range(&self) -> (f64, f64) {
        match self {
            Mood::Chill => (60.0, 100.0),
            Mood::Party => (110.0, 140.0),
            Mood::Focus => (80.0, 120.0),
            Mood::Energetic => (130.0, 180.0),
        }
    }

    pub fn genres(&self) -> &[&str] {
        match self {
            Mood::Chill => &[
                "jazz",
                "ambient",
                "classical",
                "folk",
                "bossa",
                "soul",
                "downtempo",
                "trip-hop",
            ],
            Mood::Party => &[
                "electronic",
                "dance",
                "pop",
                "hip-hop",
                "house",
                "techno",
                "disco",
                "funk",
            ],
            Mood::Focus => &[
                "classical",
                "ambient",
                "instrumental",
                "minimal",
                "piano",
                "soundtrack",
            ],
            Mood::Energetic => &[
                "rock",
                "metal",
                "punk",
                "electronic",
                "drum and bass",
                "hardcore",
                "garage",
            ],
        }
    }
}

pub fn generate_mood_queue(db: &SqliteDb, mood: Mood, count: usize) -> Vec<Value> {
    let conn = db.read_connection().lock().unwrap();
    let (bpm_min, bpm_max) = mood.bpm_range();
    let genres = mood.genres();

    let genre_conditions: Vec<String> = genres
        .iter()
        .map(|g| format!("t.genre LIKE '%{g}%'"))
        .collect();
    let genre_clause = if genre_conditions.is_empty() {
        "1=1".to_string()
    } else {
        format!("({})", genre_conditions.join(" OR "))
    };

    let sql = format!(
        "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
         FROM tracks t \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         WHERE ({genre_clause}) \
         AND (t.bpm IS NULL OR t.bpm BETWEEN ? AND ?) \
         ORDER BY RANDOM() LIMIT ?",
    );

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt
        .query_map(rusqlite::params![bpm_min, bpm_max, count as i64], |row| {
            Ok(json!({
                "track_id": row.get::<_, i64>(0)?,
                "title": row.get::<_, String>(1)?,
                "artist": row.get::<_, Option<String>>(2)?,
                "album": row.get::<_, Option<String>>(3)?,
                "duration_ms": row.get::<_, i64>(4)?,
                "genre": row.get::<_, Option<String>>(5)?,
                "year": row.get::<_, Option<i32>>(6)?,
                "bpm": row.get::<_, Option<f64>>(7)?,
            }))
        })
        .ok();

    let mut results: Vec<Value> = rows
        .map(|r| r.collect::<Result<Vec<_>, _>>().unwrap_or_default())
        .unwrap_or_default();

    // Fallback to random if mood filter too restrictive
    if results.is_empty() {
        let mut fallback = conn
            .prepare(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
                 FROM tracks t \
                 LEFT JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 ORDER BY RANDOM() LIMIT ?",
            )
            .ok();
        if let Some(ref mut s) = fallback {
            results = s
                .query_map(rusqlite::params![count as i64], |row| {
                    Ok(json!({
                        "track_id": row.get::<_, i64>(0)?,
                        "title": row.get::<_, String>(1)?,
                        "artist": row.get::<_, Option<String>>(2)?,
                        "album": row.get::<_, Option<String>>(3)?,
                        "duration_ms": row.get::<_, i64>(4)?,
                        "genre": row.get::<_, Option<String>>(5)?,
                        "year": row.get::<_, Option<i32>>(6)?,
                        "bpm": row.get::<_, Option<f64>>(7)?,
                    }))
                })
                .ok()
                .map(|r| r.collect::<Result<Vec<_>, _>>().unwrap_or_default())
                .unwrap_or_default();
        }
    }

    results
}

#[cfg(test)]
mod mood_tests {
    use super::*;

    #[test]
    fn mood_bpm_ranges() {
        assert_eq!(Mood::Chill.bpm_range(), (60.0, 100.0));
        assert_eq!(Mood::Party.bpm_range(), (110.0, 140.0));
        assert_eq!(Mood::Focus.bpm_range(), (80.0, 120.0));
        assert_eq!(Mood::Energetic.bpm_range(), (130.0, 180.0));
    }

    #[test]
    fn mood_genres_not_empty() {
        assert!(!Mood::Chill.genres().is_empty());
        assert!(!Mood::Party.genres().is_empty());
        assert!(!Mood::Focus.genres().is_empty());
        assert!(!Mood::Energetic.genres().is_empty());
    }

    #[test]
    fn mood_serialization() {
        let json = serde_json::to_string(&Mood::Party).unwrap();
        assert_eq!(json, "\"party\"");
        let parsed: Mood = serde_json::from_str("\"chill\"").unwrap();
        assert!(matches!(parsed, Mood::Chill));
    }
}
