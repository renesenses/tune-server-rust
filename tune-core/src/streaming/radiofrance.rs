use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

const GRAPHQL_URL: &str = "https://openapi.radiofrance.fr/v1/graphql";
const CACHE_TTL: Duration = Duration::from_secs(3600);
const PAGE_SIZE: u32 = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfShow {
    pub id: String,
    pub title: String,
    pub url: String,
    pub description: String,
    pub station: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RfEpisode {
    pub id: String,
    pub title: String,
    pub description: String,
    pub audio_url: String,
    pub duration_secs: u64,
    pub published_date: String,
    pub show_title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum RfStation {
    FranceInter,
    FranceCulture,
    FranceMusique,
    Fip,
    Mouv,
    FranceInfo,
    FranceBleu,
}

impl RfStation {
    pub fn code(&self) -> &'static str {
        match self {
            Self::FranceInter => "FRANCEINTER",
            Self::FranceCulture => "FRANCECULTURE",
            Self::FranceMusique => "FRANCEMUSIQUE",
            Self::Fip => "FIP",
            Self::Mouv => "MOUV",
            Self::FranceInfo => "FRANCEINFO",
            Self::FranceBleu => "FRANCEBLEU",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::FranceInter => "France Inter",
            Self::FranceCulture => "France Culture",
            Self::FranceMusique => "France Musique",
            Self::Fip => "FIP",
            Self::Mouv => "Mouv'",
            Self::FranceInfo => "franceinfo",
            Self::FranceBleu => "France Bleu",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code.to_uppercase().as_str() {
            "FRANCEINTER" => Some(Self::FranceInter),
            "FRANCECULTURE" => Some(Self::FranceCulture),
            "FRANCEMUSIQUE" => Some(Self::FranceMusique),
            "FIP" => Some(Self::Fip),
            "MOUV" => Some(Self::Mouv),
            "FRANCEINFO" => Some(Self::FranceInfo),
            "FRANCEBLEU" => Some(Self::FranceBleu),
            _ => None,
        }
    }

    pub fn all() -> &'static [RfStation] {
        &[
            Self::FranceInter,
            Self::FranceCulture,
            Self::FranceMusique,
            Self::Fip,
            Self::Mouv,
            Self::FranceInfo,
        ]
    }
}

struct CacheEntry {
    shows: Vec<RfShow>,
    fetched_at: Instant,
}

pub struct RadioFranceApi {
    client: Client,
    api_key: String,
    shows_cache: Mutex<HashMap<String, CacheEntry>>,
}

impl RadioFranceApi {
    pub fn new(api_key: String) -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            api_key,
            shows_cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_client(client: Client, api_key: String) -> Self {
        Self {
            client,
            api_key,
            shows_cache: Mutex::new(HashMap::new()),
        }
    }

    async fn graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let body = serde_json::json!({
            "query": query,
            "variables": variables,
        });

        let resp = self
            .client
            .post(GRAPHQL_URL)
            .header("x-token", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("radiofrance_graphql_request: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("radiofrance_graphql_{status}: {text}"));
        }

        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("radiofrance_graphql_parse: {e}"))?;

        if let Some(errors) = data.get("errors") {
            warn!(errors = %errors, "radiofrance_graphql_errors");
            return Err(format!("GraphQL errors: {errors}"));
        }

        Ok(data.get("data").cloned().unwrap_or(serde_json::Value::Null))
    }

    pub async fn list_shows(&self, station: RfStation) -> Result<Vec<RfShow>, String> {
        let code = station.code();

        if let Ok(cache) = self.shows_cache.lock() {
            if let Some(entry) = cache.get(code) {
                if entry.fetched_at.elapsed() < CACHE_TTL {
                    debug!(
                        station = code,
                        count = entry.shows.len(),
                        "radiofrance_shows_cache_hit"
                    );
                    return Ok(entry.shows.clone());
                }
            }
        }

        let query = r#"
            query GetShows($station: StationsEnum!, $first: Int!, $after: String) {
                shows(station: $station, first: $first, after: $after) {
                    edges {
                        cursor
                        node {
                            id
                            title
                            url
                            standFirst
                            podcast { rss }
                        }
                    }
                }
            }
        "#;

        let mut all_shows = Vec::new();
        let mut after: Option<String> = None;

        loop {
            let variables = serde_json::json!({
                "station": code,
                "first": PAGE_SIZE,
                "after": after,
            });

            let data = self.graphql(query, variables).await?;
            let edges = data
                .pointer("/shows/edges")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let count = edges.len();
            let mut last_cursor = None;

            for edge in &edges {
                last_cursor = edge
                    .get("cursor")
                    .and_then(|c| c.as_str())
                    .map(String::from);
                let node = match edge.get("node") {
                    Some(n) => n,
                    None => continue,
                };

                let title = node
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let url = node
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if title.is_empty() || url.is_empty() {
                    continue;
                }

                let rss = node
                    .pointer("/podcast/rss")
                    .and_then(|v| v.as_str())
                    .map(String::from);

                all_shows.push(RfShow {
                    id: node
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    title,
                    url,
                    description: node
                        .get("standFirst")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    station: station.label().to_string(),
                    cover_url: None,
                    rss_url: rss,
                });
            }

            if count < PAGE_SIZE as usize || last_cursor.is_none() {
                break;
            }
            after = last_cursor;
        }

        info!(
            station = code,
            shows = all_shows.len(),
            "radiofrance_shows_fetched"
        );

        if let Ok(mut cache) = self.shows_cache.lock() {
            cache.insert(
                code.to_string(),
                CacheEntry {
                    shows: all_shows.clone(),
                    fetched_at: Instant::now(),
                },
            );
        }

        Ok(all_shows)
    }

    pub async fn search_shows(&self, query: &str) -> Result<Vec<RfShow>, String> {
        let q = query.to_lowercase();
        let mut results = Vec::new();

        for station in RfStation::all() {
            let shows = self.list_shows(*station).await?;
            for show in shows {
                if show.title.to_lowercase().contains(&q)
                    || show.description.to_lowercase().contains(&q)
                {
                    results.push(show);
                }
            }
        }

        results.sort_by(|a, b| {
            let a_title = a.title.to_lowercase().starts_with(&q);
            let b_title = b.title.to_lowercase().starts_with(&q);
            b_title.cmp(&a_title).then(a.title.cmp(&b.title))
        });

        Ok(results)
    }

    pub async fn get_episodes(&self, show_url: &str, limit: u32) -> Result<Vec<RfEpisode>, String> {
        let query = r#"
            query GetDiffusions($url: String!, $first: Int!, $after: String) {
                diffusionsOfShowByUrl(url: $url, first: $first, after: $after) {
                    edges {
                        cursor
                        node {
                            id
                            title
                            standFirst
                            published_date
                            podcastEpisode {
                                url
                                duration
                            }
                            show {
                                title
                            }
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "url": show_url,
            "first": limit.min(100),
            "after": serde_json::Value::Null,
        });

        let data = self.graphql(query, variables).await?;
        let edges = data
            .pointer("/diffusionsOfShowByUrl/edges")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let mut episodes = Vec::new();
        for edge in &edges {
            let node = match edge.get("node") {
                Some(n) => n,
                None => continue,
            };

            let audio_url = node
                .pointer("/podcastEpisode/url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            if audio_url.is_empty() {
                continue;
            }

            let duration_secs = node
                .pointer("/podcastEpisode/duration")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let published_ts = node
                .get("published_date")
                .and_then(|v| v.as_str().or_else(|| v.as_i64().map(|_| "")))
                .unwrap_or("");

            let published = if let Ok(ts) = published_ts.parse::<i64>() {
                chrono::DateTime::from_timestamp(ts, 0)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| published_ts.to_string())
            } else {
                published_ts.to_string()
            };

            let show_title = node
                .pointer("/show/title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            episodes.push(RfEpisode {
                id: node
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                title: node
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: node
                    .get("standFirst")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                audio_url,
                duration_secs,
                published_date: published,
                show_title,
                cover_url: None,
            });
        }

        debug!(
            show = show_url,
            episodes = episodes.len(),
            "radiofrance_episodes_fetched"
        );
        Ok(episodes)
    }
}
