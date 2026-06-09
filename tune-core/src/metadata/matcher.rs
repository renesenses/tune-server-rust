use serde::{Deserialize, Serialize};

const MB_API: &str = "https://musicbrainz.org/ws/2";
const MB_UA: &str = "TuneServer/1.0 (contact@mozaiklabs.fr)";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackMatch {
    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub musicbrainz_recording_id: String,
    pub isrc: String,
    pub year: Option<i32>,
    pub label: String,
    pub score: i32,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumMatch {
    pub title: String,
    pub artist_name: String,
    pub musicbrainz_release_id: String,
    pub year: Option<i32>,
    pub label: String,
    pub barcode: String,
    pub country: String,
    pub score: i32,
    pub track_count: i32,
}

pub async fn lookup_track(title: &str, artist: &str, album: &str) -> Vec<TrackMatch> {
    let mut parts = Vec::new();
    if !title.is_empty() {
        parts.push(format!("recording:\"{title}\""));
    }
    if !artist.is_empty() {
        parts.push(format!("artist:\"{artist}\""));
    }
    if !album.is_empty() {
        parts.push(format!("release:\"{album}\""));
    }
    let query = parts.join(" AND ");
    if query.is_empty() {
        return vec![];
    }

    let client = crate::http::client::shared();
    let resp = client
        .get(format!("{MB_API}/recording"))
        .query(&[
            ("query", &query),
            ("limit", &"5".to_string()),
            ("fmt", &"json".to_string()),
        ])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    let data: serde_json::Value = match resp {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => return vec![],
    };

    let recordings = match data.get("recordings").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return vec![],
    };

    recordings
        .iter()
        .map(|rec| {
            let isrcs = rec
                .get("isrcs")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let artist_credit = rec.get("artist-credit").and_then(|v| v.as_array());
            let artist_name = artist_credit
                .map(|credits| {
                    credits
                        .iter()
                        .filter_map(|ac| ac.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();

            let releases = rec.get("releases").and_then(|v| v.as_array());
            let (album_title, year, label) = releases
                .and_then(|rels| rels.first())
                .map(|rel| {
                    let at = rel
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let date = rel.get("date").and_then(|v| v.as_str()).unwrap_or("");
                    let y = if date.len() >= 4 {
                        date[..4].parse::<i32>().ok()
                    } else {
                        None
                    };
                    let l = rel
                        .get("label-info")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|li| li.get("label"))
                        .and_then(|lb| lb.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    (at, y, l)
                })
                .unwrap_or_default();

            TrackMatch {
                title: rec
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                artist_name,
                album_title,
                musicbrainz_recording_id: rec
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                isrc: isrcs,
                year,
                label,
                score: rec.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                duration_ms: rec.get("length").and_then(|v| v.as_i64()).unwrap_or(0),
            }
        })
        .collect()
}

pub async fn lookup_album(title: &str, artist: &str) -> Vec<AlbumMatch> {
    let mut parts = Vec::new();
    if !title.is_empty() {
        parts.push(format!("release:\"{title}\""));
    }
    if !artist.is_empty() {
        parts.push(format!("artist:\"{artist}\""));
    }
    let query = parts.join(" AND ");
    if query.is_empty() {
        return vec![];
    }

    let client = crate::http::client::shared();
    let resp = client
        .get(format!("{MB_API}/release"))
        .query(&[
            ("query", &query),
            ("limit", &"5".to_string()),
            ("fmt", &"json".to_string()),
        ])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    let data: serde_json::Value = match resp {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => return vec![],
    };

    let releases = match data.get("releases").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return vec![],
    };

    releases
        .iter()
        .map(|rel| {
            let artist_credit = rel.get("artist-credit").and_then(|v| v.as_array());
            let artist_name = artist_credit
                .map(|credits| {
                    credits
                        .iter()
                        .filter_map(|ac| ac.get("name").and_then(|n| n.as_str()))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();

            let date = rel.get("date").and_then(|v| v.as_str()).unwrap_or("");
            let year = if date.len() >= 4 {
                date[..4].parse::<i32>().ok()
            } else {
                None
            };

            let label = rel
                .get("label-info")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|li| li.get("label"))
                .and_then(|lb| lb.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();

            AlbumMatch {
                title: rel
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                artist_name,
                musicbrainz_release_id: rel.get("id").and_then(|v| v.as_str()).unwrap_or("").into(),
                year,
                label,
                barcode: rel
                    .get("barcode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                country: rel
                    .get("country")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                score: rel.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                track_count: rel.get("track-count").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
            }
        })
        .collect()
}

/// Search MusicBrainz for an artist by name and return the best match MBID.
pub async fn lookup_artist(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let client = crate::http::client::shared();
    let query = format!("artist:\"{name}\"");
    let resp = client
        .get(format!("{MB_API}/artist"))
        .query(&[
            ("query", &query),
            ("limit", &"1".to_string()),
            ("fmt", &"json".to_string()),
        ])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    let data: serde_json::Value = match resp {
        Ok(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => return None,
    };

    let artists = data.get("artists")?.as_array()?;
    let best = artists.first()?;
    let score = best.get("score").and_then(|v| v.as_i64()).unwrap_or(0);
    if score < 90 {
        return None;
    }
    best.get("id").and_then(|v| v.as_str()).map(String::from)
}

/// Batch-match artists without MBID by searching MusicBrainz.
/// Returns the number of artists matched.
pub async fn batch_match_artist_mbids(db: crate::db::sqlite::SqliteDb) -> usize {
    let repo = crate::db::artist_repo::ArtistRepo::new(db);
    let artists = repo.list_without_mbid().unwrap_or_default();

    if artists.is_empty() {
        tracing::info!("batch_artist_mbid_match_skip_all_have_mbid");
        return 0;
    }

    tracing::info!(count = artists.len(), "batch_artist_mbid_match_started");
    let mut matched = 0usize;

    for (artist_id, name) in &artists {
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        if let Some(mbid) = lookup_artist(name).await {
            repo.update_mbid(*artist_id, &mbid).ok();
            matched += 1;
            tracing::debug!(artist_id, name = %name, mbid = %mbid, "artist_mbid_matched");
        }
    }

    tracing::info!(
        matched,
        total = artists.len(),
        "batch_artist_mbid_match_complete"
    );
    matched
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_match_serde() {
        let m = TrackMatch {
            title: "So What".into(),
            artist_name: "Miles Davis".into(),
            album_title: "Kind of Blue".into(),
            musicbrainz_recording_id: "abc".into(),
            isrc: "USRC1234".into(),
            year: Some(1959),
            label: "Columbia".into(),
            score: 95,
            duration_ms: 562_000,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: TrackMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.title, "So What");
        assert_eq!(back.year, Some(1959));
    }

    #[test]
    fn album_match_serde() {
        let m = AlbumMatch {
            title: "Kind of Blue".into(),
            artist_name: "Miles Davis".into(),
            musicbrainz_release_id: "xyz".into(),
            year: Some(1959),
            label: "Columbia".into(),
            barcode: "123456789".into(),
            country: "US".into(),
            score: 100,
            track_count: 5,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: AlbumMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.track_count, 5);
    }

    #[tokio::test]
    async fn empty_query_returns_empty() {
        let results = lookup_track("", "", "").await;
        assert!(results.is_empty());

        let albums = lookup_album("", "").await;
        assert!(albums.is_empty());
    }
}
