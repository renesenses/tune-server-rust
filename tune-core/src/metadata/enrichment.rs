use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::info;

use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;

const MUSICBRAINZ_API: &str = "https://musicbrainz.org/ws/2";
const COVERART_API: &str = "https://coverartarchive.org";
const ACOUSTID_API: &str = "https://api.acoustid.org/v2/lookup";
const USER_AGENT: &str = "TuneServer/1.0 (https://mozaiklabs.fr)";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichmentResult {
    pub track_id: i64,
    pub musicbrainz_id: Option<String>,
    pub isrc: Option<String>,
    pub cover_url: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub label: Option<String>,
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

        let cover_url = if let Some(ref rg_id) = recording.release_group_id {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            self.fetch_cover_art(rg_id).await.unwrap_or(None)
        } else {
            None
        };

        let result = EnrichmentResult {
            track_id,
            musicbrainz_id: Some(recording.id),
            isrc: recording.isrcs.into_iter().next(),
            cover_url,
            genre: None,
            year: None,
            label: None,
        };

        info!(track_id, mb_id = ?result.musicbrainz_id, "track_enriched");
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
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["track_id"], 42);
        assert_eq!(json["isrc"], "USRC12345678");
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
