use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

const MUSICBRAINZ_API: &str = "https://musicbrainz.org/ws/2";
const COVERART_API: &str = "https://coverartarchive.org";
const USER_AGENT: &str = "TuneServer/1.0 (contact@mozaiklabs.fr)";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichmentResult {
    pub track_id: i64,
    pub musicbrainz_id: Option<String>,
    pub isrc: Option<String>,
    pub cover_url: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub label: Option<String>,
    pub composer: Option<String>,
}

/// Detailed metadata fetched from a MusicBrainz recording lookup.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RecordingDetails {
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub original_year: Option<i32>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub barcode: Option<String>,
    pub country: Option<String>,
    pub composer: Option<String>,
    pub isrc: Option<String>,
    pub release_id: Option<String>,
    pub release_group_id: Option<String>,
    pub musicbrainz_artist_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtistInfo {
    pub name: String,
    pub musicbrainz_id: Option<String>,
    pub bio: Option<String>,
    pub country: Option<String>,
    pub begin_date: Option<String>,
    pub end_date: Option<String>,
    pub tags: Vec<String>,
}

pub struct MetadataEnricher {
    client: Client,
    db: SqliteDb,
}

impl MetadataEnricher {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(USER_AGENT)
                .build()
                .unwrap(),
            db,
        }
    }

    pub async fn lookup_musicbrainz(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
    ) -> Result<Option<MusicBrainzRecording>, String> {
        let mut query_parts = vec![format!("recording:{title}")];
        if let Some(a) = artist {
            query_parts.push(format!("artist:{a}"));
        }
        if let Some(al) = album {
            query_parts.push(format!("release:{al}"));
        }
        let query = query_parts.join(" AND ");

        let resp = self
            .client
            .get(format!("{MUSICBRAINZ_API}/recording"))
            .query(&[
                ("query", &query),
                ("fmt", &"json".to_string()),
                ("limit", &"1".to_string()),
            ])
            .send()
            .await
            .map_err(|e| format!("musicbrainz: {e}"))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("mb parse: {e}"))?;
        let recordings = data["recordings"].as_array();

        let recording = recordings
            .and_then(|recs| recs.first())
            .map(|r| MusicBrainzRecording {
                id: r["id"].as_str().unwrap_or("").to_string(),
                title: r["title"].as_str().unwrap_or("").to_string(),
                isrcs: r["isrcs"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
                artist_credit: r["artist-credit"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|ac| ac["name"].as_str())
                    .map(String::from),
                release_group_id: r["releases"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|rel| rel["release-group"]["id"].as_str())
                    .map(String::from),
            });

        Ok(recording)
    }

    /// Fetch detailed metadata for a MusicBrainz recording by its ID.
    ///
    /// Queries `/ws/2/recording/{id}?inc=releases+tags&fmt=json` and parses:
    /// - genre: highest-count tag from the recording or release-group
    /// - year: first 4 chars of `releases[0].date`
    /// - label: `releases[0].label-info[0].label.name`
    /// - isrc: first ISRC if present
    /// - release_id / release_group_id from the first release
    pub async fn fetch_recording_details(
        &self,
        recording_id: &str,
    ) -> Result<RecordingDetails, String> {
        let url = format!("{MUSICBRAINZ_API}/recording/{recording_id}");
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("inc", "releases+tags+isrcs+artist-credits"),
                ("fmt", "json"),
            ])
            .send()
            .await
            .map_err(|e| format!("mb recording details: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("mb recording details: HTTP {}", resp.status()));
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("mb parse: {e}"))?;

        // --- Genre: pick highest-count tag that looks like a genre ---
        let genre = Self::pick_best_genre(&data["tags"]);

        // --- Year / label / release IDs from first release ---
        let first_release = data["releases"].as_array().and_then(|arr| arr.first());

        let year = first_release
            .and_then(|r| r["date"].as_str())
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse::<i32>().ok());

        let label = first_release
            .and_then(|r| r["label-info"].as_array())
            .and_then(|arr| arr.first())
            .and_then(|li| li["label"]["name"].as_str())
            .map(String::from);

        let release_id = first_release
            .and_then(|r| r["id"].as_str())
            .map(String::from);

        let release_group_id = first_release
            .and_then(|r| r["release-group"]["id"].as_str())
            .map(String::from);

        // --- ISRC from top-level isrcs array (if present in the response) ---
        let isrc = data["isrcs"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .map(String::from);

        let catalog_number = first_release
            .and_then(|r| r["label-info"].as_array())
            .and_then(|arr| arr.first())
            .and_then(|li| li["catalog-number"].as_str())
            .map(String::from);

        let barcode = first_release
            .and_then(|r| r["barcode"].as_str())
            .filter(|b| !b.is_empty())
            .map(String::from);

        let country = first_release
            .and_then(|r| r["country"].as_str())
            .map(String::from);

        let original_year = first_release
            .and_then(|r| r["release-group"]["first-release-date"].as_str())
            .and_then(|d| d.get(..4))
            .and_then(|y| y.parse::<i32>().ok());

        let musicbrainz_artist_id = data["artist-credit"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|ac| ac["artist"]["id"].as_str())
            .map(String::from);

        debug!(
            recording_id,
            genre = ?genre,
            year = ?year,
            label = ?label,
            catalog_number = ?catalog_number,
            country = ?country,
            "recording_details_fetched"
        );

        Ok(RecordingDetails {
            genre,
            year,
            original_year,
            label,
            catalog_number,
            barcode,
            country,
            composer: None,
            isrc,
            release_id,
            release_group_id,
            musicbrainz_artist_id,
        })
    }

    /// Pick the best genre from a MusicBrainz `tags` array.
    ///
    /// Selects the tag with the highest `count` value. Skips tags that
    /// look like identifiers or very short strings.
    fn pick_best_genre(tags_value: &serde_json::Value) -> Option<String> {
        let tags = tags_value.as_array()?;
        tags.iter()
            .filter_map(|t| {
                let name = t["name"].as_str()?;
                let count = t["count"].as_i64().unwrap_or(0);
                // Skip very short tags or obvious non-genre tags
                if name.len() < 2 {
                    return None;
                }
                Some((name.to_string(), count))
            })
            .max_by_key(|(_, count)| *count)
            .map(|(name, _)| {
                // Title-case the genre for consistency
                super::normalize_genre(&name)
            })
    }

    pub async fn fetch_cover_art(&self, release_group_id: &str) -> Result<Option<String>, String> {
        let url = format!("{COVERART_API}/release-group/{release_group_id}");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("cover art: {e}"))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("cover parse: {e}"))?;
        let front = data["images"]
            .as_array()
            .and_then(|imgs| {
                imgs.iter()
                    .find(|img| img["front"].as_bool().unwrap_or(false))
            })
            .and_then(|img| {
                img["thumbnails"]["500"]
                    .as_str()
                    .or_else(|| img["image"].as_str())
            })
            .map(String::from);

        Ok(front)
    }

    pub async fn lookup_artist(&self, artist_name: &str) -> Result<Option<ArtistInfo>, String> {
        let resp = self
            .client
            .get(format!("{MUSICBRAINZ_API}/artist"))
            .query(&[("query", artist_name), ("fmt", "json"), ("limit", "1")])
            .send()
            .await
            .map_err(|e| format!("mb artist: {e}"))?;

        if !resp.status().is_success() {
            return Ok(None);
        }

        let data: serde_json::Value = resp.json().await.map_err(|e| format!("mb parse: {e}"))?;
        let artist = data["artists"]
            .as_array()
            .and_then(|arr| arr.first())
            .map(|a| ArtistInfo {
                name: a["name"].as_str().unwrap_or("").to_string(),
                musicbrainz_id: a["id"].as_str().map(String::from),
                bio: None,
                country: a["country"].as_str().map(String::from),
                begin_date: a["life-span"]["begin"].as_str().map(String::from),
                end_date: a["life-span"]["end"].as_str().map(String::from),
                tags: a["tags"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|t| t["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default(),
            });

        Ok(artist)
    }

    pub async fn enrich_track(&self, track_id: i64) -> Result<Option<EnrichmentResult>, String> {
        let repo = TrackRepo::new(self.db.clone());
        let track = repo
            .get(track_id)
            .map_err(|e| e.to_string())?
            .ok_or("track not found")?;

        let recording = self
            .lookup_musicbrainz(
                &track.title,
                track.artist_name.as_deref(),
                track.album_title.as_deref(),
            )
            .await?;

        let recording = match recording {
            Some(r) => r,
            None => return Ok(None),
        };

        // Fetch detailed metadata (genre/year/label) from the recording
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let details = match self.fetch_recording_details(&recording.id).await {
            Ok(d) => d,
            Err(e) => {
                warn!(recording_id = %recording.id, error = %e, "fetch_recording_details_failed");
                RecordingDetails::default()
            }
        };

        let cover_url = if let Some(ref rg_id) = details
            .release_group_id
            .as_ref()
            .or(recording.release_group_id.as_ref())
        {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            self.fetch_cover_art(rg_id).await.unwrap_or(None)
        } else {
            None
        };

        let result = EnrichmentResult {
            track_id,
            musicbrainz_id: Some(recording.id),
            isrc: details.isrc.or_else(|| recording.isrcs.into_iter().next()),
            cover_url,
            genre: details.genre,
            year: details.year,
            label: details.label,
            composer: details.composer,
        };

        info!(
            track_id,
            mb_id = ?result.musicbrainz_id,
            genre = ?result.genre,
            year = ?result.year,
            label = ?result.label,
            "track_enriched"
        );
        Ok(Some(result))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MusicBrainzRecording {
    pub id: String,
    pub title: String,
    pub isrcs: Vec<String>,
    pub artist_credit: Option<String>,
    pub release_group_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enrichment_result_serialize() {
        let r = EnrichmentResult {
            track_id: 42,
            musicbrainz_id: Some("abc-123".into()),
            isrc: Some("USRC12345678".into()),
            cover_url: None,
            genre: Some("Rock".into()),
            year: Some(2020),
            label: None,
            composer: Some("John Doe".into()),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["track_id"], 42);
        assert_eq!(json["isrc"], "USRC12345678");
        assert_eq!(json["composer"], "John Doe");
    }

    #[test]
    fn recording_details_default() {
        let d = RecordingDetails::default();
        assert!(d.genre.is_none());
        assert!(d.year.is_none());
        assert!(d.label.is_none());
        assert!(d.composer.is_none());
        assert!(d.isrc.is_none());
    }

    #[test]
    fn pick_best_genre_empty() {
        let v = serde_json::json!([]);
        assert!(MetadataEnricher::pick_best_genre(&v).is_none());
    }

    #[test]
    fn pick_best_genre_picks_highest_count() {
        let v = serde_json::json!([
            {"name": "rock", "count": 3},
            {"name": "jazz", "count": 10},
            {"name": "pop", "count": 5}
        ]);
        assert_eq!(
            MetadataEnricher::pick_best_genre(&v).as_deref(),
            Some("Jazz")
        );
    }

    #[test]
    fn pick_best_genre_skips_short() {
        let v = serde_json::json!([
            {"name": "x", "count": 100},
            {"name": "rock", "count": 3}
        ]);
        assert_eq!(
            MetadataEnricher::pick_best_genre(&v).as_deref(),
            Some("Rock")
        );
    }

    #[test]
    fn artist_info_default_tags() {
        let info = ArtistInfo {
            name: "Test".into(),
            musicbrainz_id: None,
            bio: None,
            country: None,
            begin_date: None,
            end_date: None,
            tags: vec![],
        };
        assert!(info.tags.is_empty());
    }
}
