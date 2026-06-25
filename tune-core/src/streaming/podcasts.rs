use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::debug;

const ITUNES_SEARCH_URL: &str = "https://itunes.apple.com/search";
const APPLE_TOP_BASE: &str = "https://rss.applemarketingtools.com/api/v2";
const USER_AGENT: &str = "Tune/2.0 (https://mozaiklabs.fr)";
/// Cache TTL for top podcasts (1 hour).
const TOP_CACHE_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Podcast {
    pub name: String,
    pub artist: String,
    pub feed_url: String,
    pub cover_url: String,
    pub description: String,
    pub episode_count: u32,
    pub source_id: String,
    /// Optional genre/category label (e.g. "News", "Music", "Culture").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// Genre IDs recognised by the Apple podcast charts API.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PodcastGenre {
    Arts = 1301,
    Comedy = 1303,
    Education = 1304,
    KidsAndFamily = 1305,
    HealthAndFitness = 1307,
    TvAndFilm = 1309,
    Music = 1310,
    News = 1311,
    ReligionAndSpirituality = 1314,
    Science = 1315,
    Sports = 1316,
    Technology = 1318,
    Business = 1321,
    Government = 1323,
    SocietyAndCulture = 1324,
    TrueCrime = 1325,
    History = 1326,
    Fiction = 1401,
}

impl PodcastGenre {
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            1301 => Some(Self::Arts),
            1303 => Some(Self::Comedy),
            1304 => Some(Self::Education),
            1305 => Some(Self::KidsAndFamily),
            1307 => Some(Self::HealthAndFitness),
            1309 => Some(Self::TvAndFilm),
            1310 => Some(Self::Music),
            1311 => Some(Self::News),
            1314 => Some(Self::ReligionAndSpirituality),
            1315 => Some(Self::Science),
            1316 => Some(Self::Sports),
            1318 => Some(Self::Technology),
            1321 => Some(Self::Business),
            1323 => Some(Self::Government),
            1324 => Some(Self::SocietyAndCulture),
            1325 => Some(Self::TrueCrime),
            1326 => Some(Self::History),
            1401 => Some(Self::Fiction),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Arts => "Arts",
            Self::Comedy => "Comedy",
            Self::Education => "Education",
            Self::KidsAndFamily => "Kids & Family",
            Self::HealthAndFitness => "Health & Fitness",
            Self::TvAndFilm => "TV & Film",
            Self::Music => "Music",
            Self::News => "News",
            Self::ReligionAndSpirituality => "Religion & Spirituality",
            Self::Science => "Science",
            Self::Sports => "Sports",
            Self::Technology => "Technology",
            Self::Business => "Business",
            Self::Government => "Government",
            Self::SocietyAndCulture => "Society & Culture",
            Self::TrueCrime => "True Crime",
            Self::History => "History",
            Self::Fiction => "Fiction",
        }
    }

    /// All available genres.
    pub fn all() -> &'static [PodcastGenre] {
        &[
            Self::Arts,
            Self::Comedy,
            Self::Education,
            Self::KidsAndFamily,
            Self::HealthAndFitness,
            Self::TvAndFilm,
            Self::Music,
            Self::News,
            Self::ReligionAndSpirituality,
            Self::Science,
            Self::Sports,
            Self::Technology,
            Self::Business,
            Self::Government,
            Self::SocietyAndCulture,
            Self::TrueCrime,
            Self::History,
            Self::Fiction,
        ]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodcastEpisode {
    pub title: String,
    pub description: String,
    pub audio_url: String,
    pub duration_ms: u64,
    pub published: String,
    pub cover_url: String,
}

pub struct PodcastService {
    client: Client,
}

impl PodcastService {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .unwrap(),
        }
    }

    /// Create a PodcastService reusing an existing reqwest client.
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        country: &str,
    ) -> Result<Vec<Podcast>, String> {
        let limit = limit.min(50).max(1);
        let cc = if country.is_empty() { "US" } else { country };
        let resp = self
            .client
            .get(ITUNES_SEARCH_URL)
            .query(&[
                ("term", query),
                ("media", "podcast"),
                ("limit", &limit.to_string()),
                ("country", cc),
            ])
            .send()
            .await
            .map_err(|e| format!("podcast search: {e}"))?;
        let data: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("podcast parse: {e}"))?;
        let results = data["results"].as_array().cloned().unwrap_or_default();
        Ok(results
            .iter()
            .filter_map(|r| {
                let feed_url = r["feedUrl"].as_str()?.to_string();
                let source_id = r["trackId"]
                    .as_u64()
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| {
                        use md5::{Digest, Md5};
                        let mut h = Md5::new();
                        h.update(feed_url.as_bytes());
                        format!("{:x}", h.finalize())
                    });
                let category = r["primaryGenreName"].as_str().map(String::from);
                Some(Podcast {
                    name: r["trackName"].as_str()?.to_string(),
                    artist: r["artistName"].as_str().unwrap_or("").to_string(),
                    feed_url,
                    cover_url: r["artworkUrl600"]
                        .as_str()
                        .or_else(|| r["artworkUrl100"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    description: r["description"]
                        .as_str()
                        .or_else(|| r["shortDescription"].as_str())
                        .unwrap_or("")
                        .to_string(),
                    episode_count: r["trackCount"].as_u64().unwrap_or(0) as u32,
                    source_id,
                    category,
                })
            })
            .collect())
    }

    pub async fn get_episodes(
        &self,
        feed_url: &str,
        limit: usize,
    ) -> Result<Vec<PodcastEpisode>, String> {
        debug!(feed_url, "podcast_feed_fetching");
        let resp = self
            .client
            .get(feed_url)
            .header(
                "Accept",
                "application/rss+xml, application/xml, text/xml, */*",
            )
            .send()
            .await
            .map_err(|e| format!("podcast feed fetch error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("podcast feed HTTP {}: {}", resp.status(), feed_url));
        }
        let xml_text = resp
            .text()
            .await
            .map_err(|e| format!("podcast feed read: {e}"))?;
        debug!(feed_url, bytes = xml_text.len(), "podcast_feed_fetched");
        parse_rss(&xml_text, limit)
    }

    /// Curated French podcasts from Radio France and other networks.
    pub fn curated_french_podcasts() -> Vec<Podcast> {
        vec![
            // ── Radio France — France Inter ──
            Podcast {
                name: "Le sept neuf".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10241.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/01/197263c5-db60-4f6f-b5e9-94c5ddb901b0/1400x1400_sc_la-grande-matinale-paracuellos.jpg".into(),
                description: "La matinale de France Inter".into(),
                episode_count: 0,
                source_id: "rf-sept-neuf".into(),
                category: Some("News".into()),
            },
            Podcast {
                name: "Affaires sensibles".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_13940.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2023/04/7b50cf5f-f5bd-4dc4-8b1d-b08666768dcf/1400x1400_sc_affaires-sensibles.jpg".into(),
                description: "Les grandes affaires qui ont marqué l'actualité".into(),
                episode_count: 0,
                source_id: "rf-affaires-sensibles".into(),
                category: Some("Society & Culture".into()),
            },
            Podcast {
                name: "Le Masque et la Plume".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_14007.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/03/50cfc964-6b7a-43f0-b42d-f798ae8819ce/1400x1400_sc_sc-rf-omm-0000041327-ite.jpg".into(),
                description: "L'émission critique cinéma, littérature et théâtre".into(),
                episode_count: 0,
                source_id: "rf-masque-plume".into(),
                category: Some("Arts".into()),
            },
            Podcast {
                name: "Boomerang".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_13937.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/2c34c20f-2aa1-45af-89bd-0851a725c323/1400x1400_bommerang.jpg".into(),
                description: "L'interview culturelle d'Augustin Trapenard".into(),
                episode_count: 0,
                source_id: "rf-boomerang".into(),
                category: Some("Arts".into()),
            },
            Podcast {
                name: "La Terre au carré".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10212.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2023/05/786780d7-1273-4959-894c-52009150c1d2/1400x1400_sc_la-terre-au-carre.jpg".into(),
                description: "L'environnement et les sciences au quotidien par Mathieu Vidard".into(),
                episode_count: 0,
                source_id: "rf-terre-carre".into(),
                category: Some("Science".into()),
            },
            Podcast {
                name: "Le Grand Dimanche Soir".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_18153.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/03/a9d633cc-02b0-4cfa-aa22-9a2836d0af41/1400x1400_sc_le-grand-dimanche-soir.jpg".into(),
                description: "Le spectacle du dimanche soir de Charline Vanhoenacker".into(),
                episode_count: 0,
                source_id: "rf-grand-dimanche".into(),
                category: Some("Comedy".into()),
            },
            // ── Radio France — France Culture ──
            Podcast {
                name: "Les pieds sur terre".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10078.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/06/a7fc766f-3e49-45a0-a41f-5512d5c0f8c1/1400x1400_les-pieds-sur-terre.jpg".into(),
                description: "Documentaires et témoignages du quotidien".into(),
                episode_count: 0,
                source_id: "rf-pieds-terre".into(),
                category: Some("Society & Culture".into()),
            },
            Podcast {
                name: "Avec philosophie".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10467.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production/2022/09/7724d8b9-cd17-41f6-b802-5db0c171842b/1400x1400_avec-philosophie-2.jpg".into(),
                description: "La philosophie au quotidien par Géraldine Muhlmann".into(),
                episode_count: 0,
                source_id: "rf-avec-philosophie".into(),
                category: Some("Education".into()),
            },
            Podcast {
                name: "Les Midis de Culture".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_12360.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/04/3fe0b371-e504-4bbf-b90c-559111dc06fb/1400x1400_sc_sc_sc-fc-midis-de-culture-3000x3000-photo.jpg".into(),
                description: "Le magazine culturel de la mi-journée de France Culture".into(),
                episode_count: 0,
                source_id: "rf-midis-culture".into(),
                category: Some("Arts".into()),
            },
            Podcast {
                name: "Le vif de l'histoire".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_11739.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2023/06/21d18070-9a66-45aa-b484-3d094c39909a/1400x1400_sc_rf_omm_0000038825_ite.jpg".into(),
                description: "L'histoire racontée par Jean Lebrun".into(),
                episode_count: 0,
                source_id: "rf-vif-histoire".into(),
                category: Some("History".into()),
            },
            // ── Radio France — FIP ──
            Podcast {
                name: "Club Jazzafip".into(),
                artist: "FIP".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_12250.xml".into(),
                cover_url: "https://www.radiofrance.fr/s3/cruiser-production-eu3/2026/03/91179aa6-0e09-4748-a281-d0f7a3259a1b/1400x1400_sc_fip-clubjazzafip-3000x3000.jpg".into(),
                description: "Le jazz sur FIP".into(),
                episode_count: 0,
                source_id: "rf-jazzafip".into(),
                category: Some("Music".into()),
            },
        ]
    }

    /// Backward-compatible alias — returns the same curated list.
    pub fn radio_france_podcasts() -> Vec<Podcast> {
        Self::curated_french_podcasts()
    }

    // ── Apple Top Podcasts ──────────────────────────────────────────

    /// Fetch the top 50 podcasts from the Apple RSS feed generator.
    /// `country` is an ISO 3166-1 alpha-2 code (e.g. "fr", "us", "de", "kr").
    /// Results are cached for 1 hour per (country, genre) key.
    pub async fn top_podcasts(
        &self,
        genre: Option<u32>,
        country: &str,
    ) -> Result<Vec<Podcast>, String> {
        let cc = country.to_lowercase();
        let url = match genre {
            Some(gid) => format!("{APPLE_TOP_BASE}/{cc}/podcasts/top/50/podcast-{gid}.json"),
            None => format!("{APPLE_TOP_BASE}/{cc}/podcasts/top/50/podcasts.json"),
        };
        let cache_key = {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(&cc, &mut h);
            std::hash::Hash::hash(&genre.unwrap_or(0), &mut h);
            std::hash::Hasher::finish(&h) as u32
        };

        // Per-genre caches stored in a static map.
        type CacheEntry = (Instant, Vec<Podcast>);
        type CacheMap = std::collections::HashMap<u32, CacheEntry>;
        static CACHE: OnceLock<Mutex<CacheMap>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));

        {
            let guard = cache.lock().await;
            if let Some((ts, data)) = guard.get(&cache_key) {
                if ts.elapsed() < TOP_CACHE_TTL && !data.is_empty() {
                    return Ok(data.clone());
                }
            }
        }

        debug!(url = %url, "apple_top_podcasts_fetch");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("apple top podcasts: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("apple top podcasts HTTP {}", resp.status()));
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("apple top podcasts parse: {e}"))?;

        let results = body["feed"]["results"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        let podcasts: Vec<Podcast> = results
            .iter()
            .filter_map(|r| {
                let apple_id = r["id"].as_str()?.to_string();
                let name = r["name"].as_str()?.to_string();
                let artist = r["artistName"].as_str().unwrap_or("").to_string();
                let artwork = r["artworkUrl100"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
                    // Upgrade to 600px artwork.
                    .replace("100x100bb", "600x600bb");
                let category = r["genres"]
                    .as_array()
                    .and_then(|g| g.first())
                    .and_then(|g| g["name"].as_str())
                    .map(String::from);
                Some(Podcast {
                    name,
                    artist,
                    feed_url: String::new(), // Apple top chart doesn't include feed URLs
                    cover_url: artwork,
                    description: String::new(),
                    episode_count: 0,
                    source_id: format!("apple-{apple_id}"),
                    category,
                })
            })
            .collect();

        debug!(count = podcasts.len(), "apple_top_podcasts_parsed");

        // Update cache.
        {
            let mut guard = cache.lock().await;
            guard.insert(cache_key, (Instant::now(), podcasts.clone()));
        }

        Ok(podcasts)
    }

    /// Return the list of available genre filters with their IDs and labels.
    pub fn available_genres() -> Vec<serde_json::Value> {
        PodcastGenre::all()
            .iter()
            .map(|g| {
                serde_json::json!({
                    "id": *g as u32,
                    "name": g.label(),
                })
            })
            .collect()
    }
}

impl Default for PodcastService {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_rss(xml: &str, limit: usize) -> Result<Vec<PodcastEpisode>, String> {
    let mut episodes = Vec::new();
    let channel_cover = extract_channel_image(xml);
    let mut search_from = 0;
    while episodes.len() < limit {
        let item_start = match xml[search_from..].find("<item") {
            Some(pos) => search_from + pos,
            None => break,
        };
        let item_end = match xml[item_start..].find("</item>") {
            Some(pos) => item_start + pos + 7,
            None => break,
        };
        let item = &xml[item_start..item_end];
        let title = extract_tag(item, "title").unwrap_or_default();
        let audio_url = extract_attr(item, "enclosure", "url").unwrap_or_default();
        if audio_url.is_empty() {
            search_from = item_end;
            continue;
        }
        let duration_text = extract_tag(item, "itunes:duration").unwrap_or_default();
        let duration_ms = parse_duration(&duration_text);
        let description = extract_tag(item, "itunes:summary")
            .or_else(|| extract_tag(item, "description"))
            .map(|d| strip_html_tags(&d))
            .unwrap_or_default();
        let published = extract_tag(item, "pubDate").unwrap_or_default();
        let cover_url =
            extract_attr(item, "itunes:image", "href").unwrap_or_else(|| channel_cover.clone());
        episodes.push(PodcastEpisode {
            title,
            description,
            audio_url,
            duration_ms,
            published,
            cover_url,
        });
        search_from = item_end;
    }
    Ok(episodes)
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = xml.find(&open)?;
    let after_open = &xml[start + open.len()..];
    let content_start = after_open.find('>')? + 1;
    let content = &after_open[content_start..];
    let end = content.find(&close)?;
    let text = content[..end].trim();
    let text = text
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(text);
    Some(text.to_string())
}

fn extract_attr(xml: &str, tag: &str, attr: &str) -> Option<String> {
    let open = format!("<{tag} ");
    let start = xml.find(&open)?;
    let tag_text = &xml[start..];
    let end = tag_text.find('>')?;
    let tag_str = &tag_text[..end];
    let attr_key = format!("{attr}=\"");
    let attr_start = tag_str.find(&attr_key)?;
    let val_start = attr_start + attr_key.len();
    let val_end = tag_str[val_start..].find('"')?;
    Some(tag_str[val_start..val_start + val_end].to_string())
}

fn extract_channel_image(xml: &str) -> String {
    if let Some(href) = extract_attr(xml, "itunes:image", "href") {
        return href;
    }
    if let Some(img) = extract_tag(xml, "image")
        && let Some(url) = extract_tag(&img, "url")
    {
        return url;
    }
    String::new()
}

fn parse_duration(text: &str) -> u64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    if let Ok(secs) = text.parse::<u64>() {
        return secs * 1000;
    }
    let parts: Vec<&str> = text.split(':').collect();
    match parts.len() {
        3 => {
            let h: u64 = parts[0].parse().unwrap_or(0);
            let m: u64 = parts[1].parse().unwrap_or(0);
            let s: u64 = parts[2].parse().unwrap_or(0);
            (h * 3600 + m * 60 + s) * 1000
        }
        2 => {
            let m: u64 = parts[0].parse().unwrap_or(0);
            let s: u64 = parts[1].parse().unwrap_or(0);
            (m * 60 + s) * 1000
        }
        _ => 0,
    }
}

fn strip_html_tags(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_duration_hms() {
        assert_eq!(parse_duration("1:30:00"), 5400000);
        assert_eq!(parse_duration("45:30"), 2730000);
        assert_eq!(parse_duration("3600"), 3600000);
        assert_eq!(parse_duration(""), 0);
    }
    #[test]
    fn strip_html() {
        assert_eq!(strip_html_tags("<p>Hello <b>world</b></p>"), "Hello world");
    }
    #[test]
    fn extract_tag_basic() {
        let xml = r#"<item><title>Episode 1</title></item>"#;
        assert_eq!(extract_tag(xml, "title"), Some("Episode 1".into()));
    }
    #[test]
    fn extract_tag_cdata() {
        let xml = r#"<title><![CDATA[My Title]]></title>"#;
        assert_eq!(extract_tag(xml, "title"), Some("My Title".into()));
    }
    #[test]
    fn extract_attr_basic() {
        let xml = r#"<enclosure url="http://audio.mp3" type="audio/mpeg"/>"#;
        assert_eq!(
            extract_attr(xml, "enclosure", "url"),
            Some("http://audio.mp3".into())
        );
    }
    #[test]
    fn parse_rss_basic() {
        let rss = r#"<rss><channel><itunes:image href="http://cover.jpg"/><item><title>Ep 1</title><enclosure url="http://audio1.mp3" type="audio/mpeg"/><itunes:duration>30:00</itunes:duration><pubDate>Mon, 01 Jan 2024</pubDate></item><item><title>Ep 2</title><enclosure url="http://audio2.mp3" type="audio/mpeg"/><itunes:duration>1500</itunes:duration></item></channel></rss>"#;
        let episodes = parse_rss(rss, 10).unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].title, "Ep 1");
        assert_eq!(episodes[0].duration_ms, 1800000);
    }
    #[test]
    fn curated_french_podcasts() {
        let podcasts = PodcastService::curated_french_podcasts();
        assert!(podcasts.len() >= 10);
        // Check cover URLs are populated.
        for p in &podcasts {
            assert!(!p.feed_url.is_empty(), "empty feed_url for {}", p.name);
            assert!(!p.cover_url.is_empty(), "empty cover_url for {}", p.name);
        }
        // Check some expected shows.
        assert!(podcasts.iter().any(|p| p.name.contains("sept neuf")));
        assert!(podcasts.iter().any(|p| p.name == "Boomerang"));
        assert!(podcasts.iter().any(|p| p.name == "Club Jazzafip"));
    }
    #[test]
    fn radio_france_alias() {
        let a = PodcastService::radio_france_podcasts();
        let b = PodcastService::curated_french_podcasts();
        assert_eq!(a.len(), b.len());
    }
    #[test]
    fn genre_enum() {
        assert_eq!(PodcastGenre::from_id(1310).unwrap().label(), "Music");
        assert!(PodcastGenre::from_id(9999).is_none());
        assert!(PodcastGenre::all().len() >= 18);
    }
    #[test]
    fn available_genres_list() {
        let genres = PodcastService::available_genres();
        assert!(genres.len() >= 18);
        assert!(genres.iter().any(|g| g["name"] == "Music"));
    }
}
