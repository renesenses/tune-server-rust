use serde_json::{Value, json};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};

fn rows_to_json(rows: &[Vec<SqlValue>]) -> Vec<Value> {
    rows.iter()
        .map(|r| {
            json!({
                "track_id": r[0].as_i64().unwrap_or(0),
                "title": r[1].as_string().unwrap_or_default(),
                "artist": r[2].as_string(),
                "album": r[3].as_string(),
                "duration_ms": r[4].as_i64().unwrap_or(0),
                "genre": r[5].as_string(),
                "year": r[6].as_i64(),
                "bpm": r[7].as_f64(),
            })
        })
        .collect()
}

/// Library tracks for a list of artist names (case-insensitive match), up to
/// `per_artist` tracks each and `count` total, in the given name order so the
/// most-similar artists come first. Same JSON shape as `generate_queue`.
pub fn tracks_for_artist_names(
    db: &std::sync::Arc<dyn DbBackend>,
    names: &[String],
    per_artist: usize,
    count: usize,
) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for name in names {
        if out.len() >= count {
            break;
        }
        let lname = name.to_lowercase();
        let limit = per_artist.min(count - out.len()) as i64;
        let rows = db
            .query_many(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
                 FROM tracks t \
                 JOIN artists ar ON t.artist_id = ar.id \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE LOWER(ar.name) = ?1 \
                 ORDER BY RANDOM() LIMIT ?2",
                &[&lname, &limit],
            )
            .map(|r| rows_to_json(&r))
            .unwrap_or_default();
        out.extend(rows);
    }
    out.truncate(count);
    out
}

/// « Radio artistes similaires » at queue end: the seed is an artist NAME (so
/// a streaming now-playing works too, no local track id needed). Similar
/// names come from the mozaiklabs enrichment API and are matched against the
/// local library. Returns empty when offline or nothing matches — callers
/// fall back to the genre/BPM `generate_queue` (cloud graceful degradation:
/// Tune must work fully without mozaiklabs.fr).
pub async fn generate_similar_artists_queue(
    db: &std::sync::Arc<dyn DbBackend>,
    seed_artist: &str,
    count: usize,
) -> Vec<Value> {
    let names = similar_artist_names(db, seed_artist, 20).await;
    if names.is_empty() {
        return Vec::new();
    }
    tracks_for_artist_names(db, &names, 2, count)
}

/// Similar-artist names for a seed, from the enrichment API. Shared by the
/// local and streaming radios so they agree on « qui ressemble à qui ».
pub async fn similar_artist_names(
    db: &std::sync::Arc<dyn DbBackend>,
    seed_artist: &str,
    max: usize,
) -> Vec<String> {
    let api_base = crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("artist_enrichment_api")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://api.mozaiklabs.fr".into());
    let mut client =
        crate::metadata::artist_enrichment::ArtistEnrichmentClient::new(Some(&api_base), 5);
    let mut names: Vec<String> = client
        .get_similar(seed_artist)
        .await
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned))
        .filter(|n| !n.eq_ignore_ascii_case(seed_artist))
        .collect();
    names.truncate(max);
    names
}

/// Pick one track per similar artist from a streaming service's search.
///
/// The local radio can only ever queue what the library holds: it turns
/// artist names into `SELECT ... FROM tracks`. Someone who listens to Qobuz
/// without a local library therefore got an empty queue and silence at the end
/// of the album — the seed was handled, the results were not.
///
/// `search` is the service's own search, injected rather than taken from the
/// registry, so this stays testable without a network or a subscription.
///
/// A search that returns nothing for an artist is skipped, not fatal: a radio
/// of nine tracks beats no radio at all.
pub async fn streaming_tracks_for_artist_names<F, Fut>(
    names: &[String],
    count: usize,
    mut search: F,
) -> Vec<crate::streaming::traits::StreamTrack>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Vec<crate::streaming::traits::StreamTrack>>,
{
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in names {
        if out.len() >= count {
            break;
        }
        let found = search(name.clone()).await;
        // One track per artist: a radio that plays four titles by the same
        // artist in a row is a playlist, not a radio.
        if let Some(t) = found.into_iter().find(|t| seen.insert(t.id.clone())) {
            out.push(t);
        }
    }
    out
}

pub fn generate_queue(
    db: &std::sync::Arc<dyn DbBackend>,
    seed_track_id: i64,
    count: usize,
) -> Vec<Value> {
    // Load seed track metadata
    let seed = db
        .query_one(
            "SELECT t.genre, t.year, t.bpm FROM tracks t WHERE t.id = ?",
            &[&seed_track_id],
        )
        .ok()
        .flatten();

    let (genre, year, bpm) = seed
        .map(|r| {
            (
                r[0].as_string(),
                r[1].as_i64().map(|v| v as i32),
                r[2].as_f64(),
            )
        })
        .unwrap_or((None, None, None));

    // Build dynamic query based on available seed metadata.
    // We use positional params (?1, ?2, ...) and collect owned
    // SqlValue params so we can pass &dyn ToSqlValue slices.
    let mut conditions = vec!["t.id != ?1".to_string()];
    let mut owned_params: Vec<crate::db::backend::SqlValue> = vec![seed_track_id.to_sql_value()];
    let mut param_idx = 2;

    if let Some(ref g) = genre {
        conditions.push(format!("t.genre LIKE ?{param_idx}"));
        owned_params.push(format!("%{g}%").to_sql_value());
        param_idx += 1;
    }

    if let Some(y) = year {
        conditions.push(format!(
            "t.year BETWEEN ?{param_idx} AND ?{}",
            param_idx + 1
        ));
        owned_params.push((y - 5).to_sql_value());
        owned_params.push((y + 5).to_sql_value());
        param_idx += 2;
    }

    if let Some(b) = bpm {
        if b > 0.0 {
            conditions.push(format!("t.bpm BETWEEN ?{param_idx} AND ?{}", param_idx + 1));
            owned_params.push((b * 0.85).to_sql_value());
            owned_params.push((b * 1.15).to_sql_value());
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

    owned_params.push((count as i64).to_sql_value());
    let param_refs: Vec<&dyn ToSqlValue> =
        owned_params.iter().map(|p| p as &dyn ToSqlValue).collect();

    let mut results = db
        .query_many(&sql, &param_refs)
        .map(|r| rows_to_json(&r))
        .unwrap_or_default();

    // Fallback to random if no matches
    if results.is_empty() && (genre.is_some() || year.is_some() || bpm.is_some()) {
        let cnt = count as i64;
        results = db
            .query_many(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE t.id != ? \
             ORDER BY RANDOM() LIMIT ?",
                &[&seed_track_id, &cnt],
            )
            .map(|r| rows_to_json(&r))
            .unwrap_or_default();
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;

    fn stream_track(id: &str, artist: &str) -> crate::streaming::traits::StreamTrack {
        crate::streaming::traits::StreamTrack {
            id: id.into(),
            title: format!("Titre {id}"),
            artist: artist.into(),
            album: None,
            album_id: None,
            duration_ms: 200_000,
            cover_path: None,
            track_number: None,
            disc_number: None,
            explicit: false,
            quality: None,
            isrc: None,
        }
    }

    #[tokio::test]
    async fn streaming_radio_takes_one_track_per_artist() {
        let names: Vec<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let got = streaming_tracks_for_artist_names(&names, 10, |name| async move {
            // Chaque artiste renvoie trois titres : la radio n'en garde qu'un,
            // sinon elle enchaîne quatre morceaux du même artiste et devient
            // une playlist.
            (0..3)
                .map(|i| stream_track(&format!("{name}{i}"), &name))
                .collect()
        })
        .await;
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].artist, "A");
        assert_eq!(got[1].artist, "B");
        assert_eq!(got[2].artist, "C");
    }

    #[tokio::test]
    async fn streaming_radio_stops_at_count_and_skips_empty_results() {
        let names: Vec<String> = ["A", "B", "C", "D"].iter().map(|s| s.to_string()).collect();
        // B ne renvoie rien : on saute, on ne s'arrête pas.
        let got = streaming_tracks_for_artist_names(&names, 2, |name| async move {
            if name == "B" {
                Vec::new()
            } else {
                vec![stream_track(&format!("{name}0"), &name)]
            }
        })
        .await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].artist, "A");
        assert_eq!(got[1].artist, "C");
    }

    #[tokio::test]
    async fn streaming_radio_never_repeats_the_same_track_id() {
        // Deux artistes différents qui renvoient le MÊME enregistrement
        // (compilation, featuring) : il ne doit apparaître qu'une fois.
        let names: Vec<String> = ["A", "B"].iter().map(|s| s.to_string()).collect();
        let got = streaming_tracks_for_artist_names(&names, 10, |name| async move {
            vec![stream_track("meme-id", &name)]
        })
        .await;
        assert_eq!(got.len(), 1);
    }

    fn test_db() -> std::sync::Arc<dyn crate::db::backend::DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        std::sync::Arc::new(db)
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
        db.execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", &[])
            .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
            &[],
        )
        .unwrap();
        for i in 1..=10i64 {
            let title = format!("Track {i}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, album_id, genre, year, duration_ms) VALUES (?, ?, 1, 1, 'Jazz', 2000, 240000)",
                &[&i, &title.as_str()],
            ).unwrap();
        }

        let result = generate_queue(&db, 1, 5);
        assert_eq!(result.len(), 5);
        assert!(result.iter().all(|t| t["track_id"].as_i64().unwrap() != 1));
    }

    #[test]
    fn tracks_for_artist_names_matches_case_insensitive_in_order() {
        let db = test_db();
        db.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis'), (2, 'John Coltrane'), (3, 'Someone Else')",
            &[],
        )
        .unwrap();
        for (id, artist) in [(1i64, 1i64), (2, 1), (3, 2), (4, 2), (5, 3)] {
            let title = format!("T{id}");
            db.execute(
                "INSERT INTO tracks (id, title, artist_id, duration_ms) VALUES (?, ?, ?, 200000)",
                &[&id, &title.as_str(), &artist],
            )
            .unwrap();
        }

        let names = vec![
            "john coltrane".to_string(),
            "MILES DAVIS".to_string(),
            "Unknown Guy".to_string(),
        ];
        let result = tracks_for_artist_names(&db, &names, 2, 10);
        // Coltrane (2 tracks) first — similarity order preserved — then Davis (2).
        assert_eq!(result.len(), 4);
        assert_eq!(result[0]["artist"].as_str(), Some("John Coltrane"));
        assert_eq!(result[1]["artist"].as_str(), Some("John Coltrane"));
        assert_eq!(result[2]["artist"].as_str(), Some("Miles Davis"));

        // per_artist and count caps hold.
        let capped = tracks_for_artist_names(&db, &names, 1, 1);
        assert_eq!(capped.len(), 1);
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

pub fn generate_mood_queue(
    db: &std::sync::Arc<dyn DbBackend>,
    mood: Mood,
    count: usize,
) -> Vec<Value> {
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

    let cnt = count as i64;
    let mut results = db
        .query_many(&sql, &[&bpm_min, &bpm_max, &cnt])
        .map(|r| rows_to_json(&r))
        .unwrap_or_default();

    // Fallback to random if mood filter too restrictive
    if results.is_empty() {
        results = db
            .query_many(
                "SELECT t.id, t.title, ar.name, al.title, t.duration_ms, t.genre, t.year, t.bpm \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             ORDER BY RANDOM() LIMIT ?",
                &[&cnt],
            )
            .map(|r| rows_to_json(&r))
            .unwrap_or_default();
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
