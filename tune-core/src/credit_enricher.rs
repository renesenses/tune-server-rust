use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

const MB_API: &str = "https://musicbrainz.org/ws/2";
const MB_UA: &str = "TuneServer/1.0 (https://github.com/renesenses/tune-server-rust)";
const RATE_LIMIT: Duration = Duration::from_millis(1100);

const INSTRUMENTS: &[&str] = &[
    "guitar",
    "bass",
    "drums",
    "piano",
    "keyboard",
    "trumpet",
    "saxophone",
    "violin",
    "cello",
    "flute",
    "harmonica",
    "organ",
    "percussion",
    "trombone",
    "clarinet",
    "harp",
    "banjo",
    "mandolin",
    "accordion",
    "synthesizer",
    "vibraphone",
    "oboe",
    "bassoon",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackCredit {
    pub name: String,
    pub role: String,
    pub instrument: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichResult {
    pub enriched: usize,
    pub total_mb_credits: usize,
    pub source: String,
    pub recording_id: Option<String>,
}

fn mb_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(MB_UA)
        .build()
        .unwrap()
}

pub async fn lookup_recording(
    client: &reqwest::Client,
    title: &str,
    artist: &str,
) -> Option<String> {
    let query = format!("recording:\"{title}\" AND artist:\"{artist}\"");
    let resp = client
        .get(format!("{MB_API}/recording"))
        .query(&[
            ("query", &query),
            ("limit", &"3".to_string()),
            ("fmt", &"json".to_string()),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let data: serde_json::Value = resp.json().await.ok()?;
    let recordings = data["recordings"].as_array()?;

    for rec in recordings {
        let rec_title = rec["title"].as_str().unwrap_or("").to_lowercase();
        if rec_title == title.to_lowercase() || rec_title.contains(&title.to_lowercase()) {
            return rec["id"].as_str().map(String::from);
        }
    }

    recordings
        .first()
        .and_then(|r| r["id"].as_str().map(String::from))
}

pub async fn get_recording_credits(
    client: &reqwest::Client,
    recording_id: &str,
) -> Vec<TrackCredit> {
    let resp = client
        .get(format!("{MB_API}/recording/{recording_id}"))
        .query(&[("inc", "artist-rels"), ("fmt", "json")])
        .send()
        .await;

    let resp = match resp {
        Ok(r) if r.status().is_success() => r,
        _ => return Vec::new(),
    };

    let data: serde_json::Value = match resp.json().await {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    let relations = match data["relations"].as_array() {
        Some(r) => r,
        None => return Vec::new(),
    };

    let mut credits = Vec::new();
    for rel in relations {
        let rel_type = rel["type"].as_str().unwrap_or("");
        let artist_name = rel["artist"]["name"].as_str().unwrap_or("");
        if artist_name.is_empty() {
            continue;
        }

        let attributes: Vec<String> = rel["attributes"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        match rel_type {
            "instrument" | "performer" => {
                let instrument = if attributes.is_empty() {
                    None
                } else {
                    Some(attributes.join(", "))
                };
                credits.push(TrackCredit {
                    name: artist_name.to_string(),
                    role: "performer".into(),
                    instrument,
                });
            }
            "vocal" => {
                let vocal_type = if attributes.is_empty() {
                    "vocals".to_string()
                } else {
                    attributes.join(", ")
                };
                credits.push(TrackCredit {
                    name: artist_name.to_string(),
                    role: "performer".into(),
                    instrument: Some(vocal_type),
                });
            }
            "producer" | "engineer" | "mix" | "conductor" => {
                credits.push(TrackCredit {
                    name: artist_name.to_string(),
                    role: rel_type.to_string(),
                    instrument: None,
                });
            }
            _ => {}
        }
    }

    credits
}

pub async fn lookup_artist_instrument(
    client: &reqwest::Client,
    artist_name: &str,
) -> Option<String> {
    let query = format!("artist:\"{artist_name}\"");
    let resp = client
        .get(format!("{MB_API}/artist"))
        .query(&[
            ("query", &query),
            ("limit", &"3".to_string()),
            ("fmt", &"json".to_string()),
        ])
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let data: serde_json::Value = resp.json().await.ok()?;
    let artists = data["artists"].as_array()?;

    for artist in artists {
        if artist["name"].as_str().unwrap_or("").to_lowercase() != artist_name.to_lowercase() {
            continue;
        }

        let disamb = artist["disambiguation"]
            .as_str()
            .unwrap_or("")
            .to_lowercase();

        for &instr in INSTRUMENTS {
            if disamb.contains(instr) {
                return Some(instr.to_string());
            }
        }

        if disamb.contains("singer") || disamb.contains("vocalist") || disamb.contains("vocal") {
            return Some("vocals".to_string());
        }

        // Check artist-rels for member-of-band with instrument attributes
        if let Some(artist_id) = artist["id"].as_str() {
            tokio::time::sleep(RATE_LIMIT).await;

            if let Ok(resp2) = client
                .get(format!("{MB_API}/artist/{artist_id}"))
                .query(&[("inc", "artist-rels"), ("fmt", "json")])
                .send()
                .await
            {
                if let Ok(detail) = resp2.json::<serde_json::Value>().await {
                    if let Some(rels) = detail["relations"].as_array() {
                        for rel in rels {
                            if rel["type"].as_str() == Some("member of band") {
                                if let Some(attrs) = rel["attributes"].as_array() {
                                    for attr in attrs {
                                        let a = attr.as_str().unwrap_or("");
                                        if !["original", "current", "past"].contains(&a) {
                                            return Some(a.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        return None;
    }

    None
}

pub async fn enrich_track(
    title: &str,
    artist: &str,
    existing_recording_id: Option<&str>,
) -> Result<EnrichResult, String> {
    let client = mb_client();

    let recording_id = if let Some(rid) = existing_recording_id {
        rid.to_string()
    } else {
        lookup_recording(&client, title, artist)
            .await
            .ok_or("not_found_on_musicbrainz")?
    };

    tokio::time::sleep(RATE_LIMIT).await;

    let credits = get_recording_credits(&client, &recording_id).await;
    if credits.is_empty() {
        return Ok(EnrichResult {
            enriched: 0,
            total_mb_credits: 0,
            source: "musicbrainz".into(),
            recording_id: Some(recording_id),
        });
    }

    let enriched = credits.iter().filter(|c| c.instrument.is_some()).count();

    info!(
        title,
        artist,
        enriched,
        total = credits.len(),
        "track_credits_enriched"
    );

    Ok(EnrichResult {
        enriched,
        total_mb_credits: credits.len(),
        source: "musicbrainz".into(),
        recording_id: Some(recording_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_serialize() {
        let c = TrackCredit {
            name: "John".into(),
            role: "performer".into(),
            instrument: Some("guitar".into()),
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["instrument"], "guitar");
    }

    #[test]
    fn instrument_list_coverage() {
        assert!(INSTRUMENTS.contains(&"guitar"));
        assert!(INSTRUMENTS.contains(&"piano"));
        assert!(INSTRUMENTS.contains(&"drums"));
        assert!(INSTRUMENTS.len() >= 20);
    }

    #[test]
    fn enrich_result_serialize() {
        let r = EnrichResult {
            enriched: 5,
            total_mb_credits: 8,
            source: "musicbrainz".into(),
            recording_id: Some("abc-123".into()),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["enriched"], 5);
        assert_eq!(json["source"], "musicbrainz");
    }

    #[test]
    fn disamb_instrument_detection() {
        let disamb = "american jazz trumpeter".to_lowercase();
        let found = INSTRUMENTS.iter().find(|&&i| disamb.contains(i));
        assert_eq!(found, Some(&"trumpet"));
    }

    #[test]
    fn disamb_vocalist_detection() {
        let disamb = "rock vocalist and songwriter".to_lowercase();
        let is_vocal =
            disamb.contains("singer") || disamb.contains("vocalist") || disamb.contains("vocal");
        assert!(is_vocal);
    }
}
