use md5::{Digest, Md5};
use tracing::{debug, warn};

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn build_api_sig(params: &[(&str, String)], api_secret: &str) -> String {
    let mut sorted: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    sorted.sort_by_key(|(k, _)| *k);
    let sig_input: String = sorted
        .iter()
        .map(|(k, v)| format!("{k}{v}"))
        .collect::<String>()
        + api_secret;
    md5_hex(&sig_input)
}

/// Scrobble a finished track to Last.fm.
pub async fn scrobble(
    api_key: &str,
    api_secret: &str,
    session_key: &str,
    artist: &str,
    track: &str,
    timestamp: u64,
) -> Result<(), String> {
    let mut params = vec![
        ("api_key", api_key.to_string()),
        ("artist", artist.to_string()),
        ("method", "track.scrobble".to_string()),
        ("sk", session_key.to_string()),
        ("timestamp", timestamp.to_string()),
        ("track", track.to_string()),
    ];
    let sig = build_api_sig(&params, api_secret);
    params.push(("api_sig", sig));
    params.push(("format", "json".to_string()));

    let client = reqwest::Client::new();
    let resp = client
        .post("https://ws.audioscrobbler.com/2.0/")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("scrobble send: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!(status = %status, body = %body, "lastfm_scrobble_failed");
        return Err(format!("scrobble HTTP {status}: {body}"));
    }

    debug!(artist, track, "lastfm_scrobbled");
    Ok(())
}

/// Update "Now Playing" on Last.fm when a track starts.
pub async fn update_now_playing(
    api_key: &str,
    api_secret: &str,
    session_key: &str,
    artist: &str,
    track: &str,
) -> Result<(), String> {
    let mut params = vec![
        ("api_key", api_key.to_string()),
        ("artist", artist.to_string()),
        ("method", "track.updateNowPlaying".to_string()),
        ("sk", session_key.to_string()),
        ("track", track.to_string()),
    ];
    let sig = build_api_sig(&params, api_secret);
    params.push(("api_sig", sig));
    params.push(("format", "json".to_string()));

    let client = reqwest::Client::new();
    let resp = client
        .post("https://ws.audioscrobbler.com/2.0/")
        .form(&params)
        .send()
        .await
        .map_err(|e| format!("now_playing send: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        warn!(status = %status, body = %body, "lastfm_now_playing_failed");
        return Err(format!("now_playing HTTP {status}: {body}"));
    }

    debug!(artist, track, "lastfm_now_playing_updated");
    Ok(())
}

/// Exchange a Last.fm web auth token for a session key via `auth.getSession`.
pub async fn get_session(api_key: &str, api_secret: &str, token: &str) -> Result<String, String> {
    let mut params = vec![
        ("api_key", api_key.to_string()),
        ("method", "auth.getSession".to_string()),
        ("token", token.to_string()),
    ];
    let sig = build_api_sig(&params, api_secret);
    params.push(("api_sig", sig));
    params.push(("format", "json".to_string()));

    let client = reqwest::Client::new();
    let resp = client
        .get("https://ws.audioscrobbler.com/2.0/")
        .query(&params)
        .send()
        .await
        .map_err(|e| format!("auth.getSession send: {e}"))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        warn!(status = %status, body = %body, "lastfm_auth_failed");
        return Err(format!("auth.getSession HTTP {status}: {body}"));
    }

    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("auth.getSession parse: {e}"))?;

    json["session"]["key"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| format!("auth.getSession: no session key in response: {body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_sig_is_deterministic() {
        let params = vec![
            ("method", "track.scrobble".to_string()),
            ("api_key", "abc123".to_string()),
            ("artist", "Pink Floyd".to_string()),
            ("track", "Time".to_string()),
            ("sk", "session1".to_string()),
            ("timestamp", "1700000000".to_string()),
        ];
        let sig1 = build_api_sig(&params, "secret");
        let sig2 = build_api_sig(&params, "secret");
        assert_eq!(sig1, sig2);
        assert_eq!(sig1.len(), 32); // MD5 hex
    }

    #[test]
    fn api_sig_sorted_correctly() {
        // Verify params are sorted alphabetically before hashing
        let params = vec![
            ("z_param", "last".to_string()),
            ("a_param", "first".to_string()),
        ];
        let sig = build_api_sig(&params, "secret");
        // Manual: sorted = a_param=first, z_param=last -> "a_paramfirstz_paramlastsecret"
        let expected = md5_hex("a_paramfirstz_paramlastsecret");
        assert_eq!(sig, expected);
    }
}
