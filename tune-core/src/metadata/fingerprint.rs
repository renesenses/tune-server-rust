use std::process::Stdio;

use serde::{Deserialize, Serialize};

const ACOUSTID_API: &str = "https://api.acoustid.org/v2/lookup";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintResult {
    pub duration: f64,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcoustIdMatch {
    pub recording_id: String,
    pub title: String,
    pub artist: String,
    pub score: f64,
}

pub async fn generate_fingerprint(file_path: &str) -> Result<FingerprintResult, String> {
    let output = tokio::process::Command::new("fpcalc")
        .args(["-json", file_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("fpcalc: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("fpcalc failed: {stderr}"));
    }

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("fpcalc parse: {e}"))?;

    let duration = json["duration"]
        .as_f64()
        .ok_or("no duration in fpcalc output")?;
    let fingerprint = json["fingerprint"]
        .as_str()
        .ok_or("no fingerprint in fpcalc output")?
        .to_string();

    Ok(FingerprintResult {
        duration,
        fingerprint,
    })
}

pub async fn lookup_acoustid(
    api_key: &str,
    fingerprint: &str,
    duration: f64,
) -> Result<Vec<AcoustIdMatch>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(ACOUSTID_API)
        .form(&[
            ("client", api_key),
            ("fingerprint", fingerprint),
            ("duration", &(duration as i64).to_string()),
            ("meta", "recordings"),
        ])
        .send()
        .await
        .map_err(|e| format!("acoustid: {e}"))?;

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("acoustid parse: {e}"))?;

    let results = data["results"].as_array().cloned().unwrap_or_default();

    let mut matches = Vec::new();
    for result in &results {
        let score = result["score"].as_f64().unwrap_or(0.0);
        let recordings = result["recordings"].as_array();

        if let Some(recs) = recordings {
            for rec in recs {
                let recording_id = rec["id"].as_str().unwrap_or("").to_string();
                let title = rec["title"].as_str().unwrap_or("").to_string();
                let artist = rec["artists"]
                    .as_array()
                    .and_then(|arr| arr.first())
                    .and_then(|a| a["name"].as_str())
                    .unwrap_or("")
                    .to_string();

                if !recording_id.is_empty() {
                    matches.push(AcoustIdMatch {
                        recording_id,
                        title,
                        artist,
                        score,
                    });
                }
            }
        }
    }

    matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(matches)
}

pub fn fpcalc_available() -> bool {
    std::process::Command::new("fpcalc")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_result_serialize() {
        let r = FingerprintResult {
            duration: 180.5,
            fingerprint: "AQAA...".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["duration"], 180.5);
    }

    #[test]
    fn acoustid_match_serialize() {
        let m = AcoustIdMatch {
            recording_id: "abc-123".into(),
            title: "Song".into(),
            artist: "Artist".into(),
            score: 0.95,
        };
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["score"], 0.95);
    }

    #[test]
    fn check_fpcalc() {
        let available = fpcalc_available();
        if available {
            println!("fpcalc found");
        } else {
            println!("fpcalc not found (optional)");
        }
    }
}
