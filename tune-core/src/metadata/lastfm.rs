use tracing::debug;

const LASTFM_API: &str = "https://ws.audioscrobbler.com/2.0/";

#[derive(Debug, Clone, Default)]
pub struct LastfmTags {
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub bio: String,
}

pub async fn get_lastfm_tags(title: &str, artist: &str, api_key: &str) -> LastfmTags {
    if api_key.is_empty() || title.is_empty() {
        return LastfmTags::default();
    }

    let client = reqwest::Client::new();

    let track_data = client
        .get(LASTFM_API)
        .query(&[
            ("method", "track.getInfo"),
            ("track", title),
            ("artist", artist),
            ("api_key", api_key),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()
        .and_then(|r| {
            if r.status().is_success() {
                Some(r)
            } else {
                None
            }
        });

    let track_json: serde_json::Value = match track_data {
        Some(r) => r.json().await.unwrap_or_default(),
        None => return LastfmTags::default(),
    };

    let tags: Vec<String> = track_json
        .pointer("/track/toptags/tag")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let genres = tags.iter().take(3).cloned().collect();

    let bio = get_artist_bio(&client, artist, api_key).await;

    LastfmTags { genres, tags, bio }
}

async fn get_artist_bio(client: &reqwest::Client, artist: &str, api_key: &str) -> String {
    let resp = client
        .get(LASTFM_API)
        .query(&[
            ("method", "artist.getInfo"),
            ("artist", artist),
            ("api_key", api_key),
            ("format", "json"),
        ])
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok();

    let data: serde_json::Value = match resp {
        Some(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
        _ => return String::new(),
    };

    let bio = data
        .pointer("/artist/bio/summary")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    strip_html(bio)
}

fn strip_html(s: &str) -> String {
    let re = regex::Regex::new(r"<[^>]+>").unwrap();
    re.replace_all(s, "").trim().to_string()
}

pub struct LastfmEnricher {
    api_key: String,
}

impl LastfmEnricher {
    pub fn new(api_key: &str) -> Self {
        Self {
            api_key: api_key.to_string(),
        }
    }

    pub fn has_key(&self) -> bool {
        !self.api_key.is_empty()
    }

    pub async fn enrich_track(&self, title: &str, artist: &str) -> LastfmTags {
        get_lastfm_tags(title, artist, &self.api_key).await
    }

    pub async fn get_top_tags_for_artist(&self, artist: &str) -> Vec<String> {
        if self.api_key.is_empty() || artist.is_empty() {
            return vec![];
        }

        let client = reqwest::Client::new();
        let resp = client
            .get(LASTFM_API)
            .query(&[
                ("method", "artist.getTopTags"),
                ("artist", artist),
                ("api_key", &self.api_key),
                ("format", "json"),
            ])
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok();

        let data: serde_json::Value = match resp {
            Some(r) if r.status().is_success() => r.json().await.unwrap_or_default(),
            _ => return vec![],
        };

        data.pointer("/toptags/tag")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
                    .take(10)
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_html_basic() {
        assert_eq!(strip_html("Hello <b>world</b>!"), "Hello world!");
        assert_eq!(strip_html("<a href='x'>link</a>"), "link");
        assert_eq!(strip_html("no tags"), "no tags");
    }

    #[test]
    fn empty_key_returns_default() {
        let enricher = LastfmEnricher::new("");
        assert!(!enricher.has_key());
    }

    #[test]
    fn has_key() {
        let enricher = LastfmEnricher::new("abc123");
        assert!(enricher.has_key());
    }

    #[tokio::test]
    async fn no_key_no_tags() {
        let tags = get_lastfm_tags("Song", "Artist", "").await;
        assert!(tags.tags.is_empty());
        assert!(tags.genres.is_empty());
    }

    #[tokio::test]
    async fn no_title_no_tags() {
        let tags = get_lastfm_tags("", "Artist", "fake-key").await;
        assert!(tags.tags.is_empty());
    }
}
