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

/// Le binaire `fpcalc` remplacé — une doublure dans les tests (#4805). `None`
/// en service : `fpcalc` est cherché dans le `PATH`.
static FPCALC_REMPLACE: std::sync::RwLock<Option<std::path::PathBuf>> =
    std::sync::RwLock::new(None);

/// **Tests seulement.** Fait lancer `chemin` à la place de `fpcalc`, et vide le
/// cache de [`fpcalc_disponible`].
#[doc(hidden)]
pub fn remplacer_fpcalc(chemin: Option<std::path::PathBuf>) {
    if let Ok(mut f) = FPCALC_REMPLACE.write() {
        *f = chemin;
    }
    if let Ok(mut c) = DISPONIBILITE.lock() {
        *c = None;
    }
}

fn binaire_fpcalc() -> std::ffi::OsString {
    FPCALC_REMPLACE
        .read()
        .ok()
        .and_then(|f| f.clone())
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| "fpcalc".into())
}

pub async fn generate_fingerprint(file_path: &str) -> Result<FingerprintResult, String> {
    // `fpcalc` lit les 120 premières secondes par défaut, comme Picard.
    // `kill_on_drop` : une passe qui abandonne une empreinte trop longue
    // (fichier sur un partage qui ne répond plus) ne laisse pas de processus.
    let output = tokio::process::Command::new(binaire_fpcalc())
        .args(["-json", file_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
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
    // Même cadence que la passe de lot (#4805) : 3 requêtes/s au plus, par le
    // limiteur PARTAGÉ.
    crate::http::fetch::ACOUSTID
        .acquire(crate::http::fetch::CLE_ACOUSTID)
        .await;
    let client = crate::http::client::shared();
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
        .inspect(|r| {
            crate::http::fetch::ACOUSTID.constater_reponse(crate::http::fetch::CLE_ACOUSTID, r)
        })
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
    std::process::Command::new(binaire_fpcalc())
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Durée de validité de [`fpcalc_disponible`]. L'écran Santé sonde en
/// boucle : lancer un processus à chaque sondage serait absurde, mais un
/// `fpcalc` installé pendant que le serveur tourne doit être vu sans
/// redémarrage.
const DISPONIBILITE_VALIDE: std::time::Duration = std::time::Duration::from_secs(60);

static DISPONIBILITE: std::sync::Mutex<Option<(std::time::Instant, bool)>> =
    std::sync::Mutex::new(None);

/// [`fpcalc_available`], mis en cache [`DISPONIBILITE_VALIDE`].
pub fn fpcalc_disponible() -> bool {
    if let Ok(c) = DISPONIBILITE.lock()
        && let Some((quand, dispo)) = *c
        && quand.elapsed() < DISPONIBILITE_VALIDE
    {
        return dispo;
    }
    let dispo = fpcalc_available();
    if let Ok(mut c) = DISPONIBILITE.lock() {
        *c = Some((std::time::Instant::now(), dispo));
    }
    dispo
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
