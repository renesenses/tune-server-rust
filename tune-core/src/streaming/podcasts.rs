use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const ITUNES_SEARCH_URL: &str = "https://itunes.apple.com/search";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Podcast {
    pub name: String,
    pub artist: String,
    pub feed_url: String,
    pub cover_url: String,
    pub description: String,
    pub episode_count: u32,
    pub source_id: String,
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
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
        }
    }

    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Podcast>, String> {
        let limit = limit.min(50).max(1);
        let resp = self
            .client
            .get(ITUNES_SEARCH_URL)
            .query(&[
                ("term", query),
                ("media", "podcast"),
                ("limit", &limit.to_string()),
                ("country", "FR"),
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
                })
            })
            .collect())
    }

    pub async fn get_episodes(
        &self,
        feed_url: &str,
        limit: usize,
    ) -> Result<Vec<PodcastEpisode>, String> {
        let resp = self
            .client
            .get(feed_url)
            .send()
            .await
            .map_err(|e| format!("podcast feed: {e}"))?;
        let xml_text = resp
            .text()
            .await
            .map_err(|e| format!("podcast feed read: {e}"))?;

        parse_rss(&xml_text, limit)
    }

    pub fn radio_france_podcasts() -> Vec<Podcast> {
        vec![
            Podcast {
                name: "Les Grosses Têtes".into(),
                artist: "RTL".into(),
                feed_url: "https://www.rtl.fr/podcast/les-grosses-tetes.xml".into(),
                cover_url: String::new(),
                description: "L'émission culte de RTL".into(),
                episode_count: 0,
                source_id: "rf-grosses-tetes".into(),
            },
            Podcast {
                name: "Le 7/9 de France Inter".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10239.xml".into(),
                cover_url: String::new(),
                description: "La matinale de France Inter".into(),
                episode_count: 0,
                source_id: "rf-7-9".into(),
            },
            Podcast {
                name: "Affaires sensibles".into(),
                artist: "France Inter".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_16756.xml".into(),
                cover_url: String::new(),
                description: "Les grandes affaires qui ont marqué l'actualité".into(),
                episode_count: 0,
                source_id: "rf-affaires-sensibles".into(),
            },
            Podcast {
                name: "Les pieds sur terre".into(),
                artist: "France Culture".into(),
                feed_url: "https://radiofrance-podcast.net/podcast09/rss_10078.xml".into(),
                cover_url: String::new(),
                description: "Documentaires et témoignages du quotidien".into(),
                episode_count: 0,
                source_id: "rf-pieds-terre".into(),
            },
        ]
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
        && let Some(url) = extract_tag(&img, "url") {
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
        assert_eq!(strip_html_tags("No tags"), "No tags");
    }

    #[test]
    fn extract_tag_basic() {
        let xml = r#"<item><title>Episode 1</title><link>http://x</link></item>"#;
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
        let rss = r#"
        <rss><channel>
            <itunes:image href="http://cover.jpg"/>
            <item>
                <title>Ep 1</title>
                <enclosure url="http://audio1.mp3" type="audio/mpeg"/>
                <itunes:duration>30:00</itunes:duration>
                <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
            </item>
            <item>
                <title>Ep 2</title>
                <enclosure url="http://audio2.mp3" type="audio/mpeg"/>
                <itunes:duration>1500</itunes:duration>
            </item>
        </channel></rss>"#;
        let episodes = parse_rss(rss, 10).unwrap();
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].title, "Ep 1");
        assert_eq!(episodes[0].duration_ms, 1800000);
        assert_eq!(episodes[0].cover_url, "http://cover.jpg");
        assert_eq!(episodes[1].title, "Ep 2");
        assert_eq!(episodes[1].duration_ms, 1500000);
    }

    #[test]
    fn radio_france_curated() {
        let podcasts = PodcastService::radio_france_podcasts();
        assert!(podcasts.len() >= 4);
        assert!(podcasts.iter().any(|p| p.name.contains("Grosses Têtes")));
    }
}
