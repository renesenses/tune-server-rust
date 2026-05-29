use serde::{Deserialize, Serialize};
use tracing::debug;

const MB_API: &str = "https://musicbrainz.org/ws/2";
const MB_UA: &str = "TuneServer/1.0 (contact@mozaiklabs.fr)";
const MB_RATE_LIMIT_MS: u64 = 1100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MBReleaseMatch {
    pub release_id: String,
    pub release_group_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub score: i32,
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub async fn lookup_release(
    title: &str,
    artist: &str,
    track_count: Option<i32>,
    year: Option<i32>,
) -> Option<MBReleaseMatch> {
    let mut query_parts = vec![
        format!("release:\"{title}\""),
        format!("artist:\"{artist}\""),
    ];
    if let Some(tc) = track_count {
        query_parts.push(format!("tracks:{tc}"));
    }
    if let Some(y) = year {
        query_parts.push(format!("date:{y}"));
    }
    let query = query_parts.join(" AND ");

    let client = reqwest::Client::new();
    let resp = client
        .get(&format!("{MB_API}/release"))
        .query(&[
            ("query", &query),
            ("limit", &"5".to_string()),
            ("fmt", &"json".to_string()),
        ])
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        debug!(status = %resp.status(), "mb_release_search_http_error");
        return None;
    }

    let data: serde_json::Value = resp.json().await.ok()?;
    let releases = data.get("releases")?.as_array()?;

    let norm_title = normalize(title);
    let norm_artist = normalize(artist);

    let mut best: Option<MBReleaseMatch> = None;
    let mut best_score = 0;

    for rel in releases {
        let rel_title = rel.get("title")?.as_str().unwrap_or("");
        let rel_score = rel.get("score").and_then(|v| v.as_i64()).unwrap_or(0) as i32;

        let artist_credit = rel.get("artist-credit").and_then(|v| v.as_array());
        let rel_artist = artist_credit
            .map(|credits| {
                credits
                    .iter()
                    .filter_map(|ac| ac.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();

        let norm_rel_title = normalize(rel_title);
        if norm_rel_title != norm_title
            && !norm_title.contains(&norm_rel_title)
            && !norm_rel_title.contains(&norm_title)
        {
            continue;
        }

        let norm_rel_artist = normalize(&rel_artist);
        if !norm_artist.is_empty()
            && !norm_rel_artist.is_empty()
            && !norm_artist.contains(&norm_rel_artist)
            && !norm_rel_artist.contains(&norm_artist)
        {
            continue;
        }

        let rg = rel.get("release-group");
        let rg_id = rg
            .and_then(|g| g.get("id"))
            .and_then(|v| v.as_str())
            .map(String::from);

        if rel_score > best_score {
            best = Some(MBReleaseMatch {
                release_id: rel.get("id")?.as_str()?.to_string(),
                release_group_id: rg_id,
                title: rel_title.to_string(),
                artist: rel_artist,
                score: rel_score,
            });
            best_score = rel_score;
        }
    }

    best.filter(|m| m.score >= 80)
}

pub async fn rate_limit_delay() {
    tokio::time::sleep(std::time::Duration::from_millis(MB_RATE_LIMIT_MS)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_text() {
        assert_eq!(normalize("Kind of Blue"), "kind of blue");
        assert_eq!(normalize("Hello, World!"), "hello world");
        assert_eq!(normalize("  spaces  "), "spaces");
    }

    #[test]
    fn mb_release_match_serde() {
        let m = MBReleaseMatch {
            release_id: "abc-123".into(),
            release_group_id: Some("def-456".into()),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            score: 95,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MBReleaseMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.score, 95);
        assert_eq!(back.release_id, "abc-123");
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalize_unicode() {
        assert_eq!(normalize("Café Résumé"), "café résumé");
    }
}
