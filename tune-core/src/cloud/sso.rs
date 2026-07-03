use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, info};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

/// Baked-in OAuth client id for the public **PKCE** "Tune" client on
/// mozaiklabs.fr.
///
/// Empty until the Laravel Passport public client is created (coordinated with
/// Bertrand). While empty, SSO stays *unconfigured* and degrades gracefully —
/// Tune must keep working 100 % without mozaiklabs.fr (SSO is opt-in, never
/// blocking). Once baked, every install is SSO-capable by default.
///
/// Runtime overrides (see `tune-server` route resolution): the `mozaik_client_id`
/// setting or the `TUNE_MOZAIK_CLIENT_ID` env var take precedence over this const.
pub const DEFAULT_CLIENT_ID: &str = "";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudUser {
    pub id: i64,
    pub email: String,
    pub display_name: String,
    pub is_admin: bool,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

/// PKCE (RFC 7636) parameters for a single authorization flow.
///
/// Created at `/sso/authorize`, persisted for the duration of the browser
/// round-trip (keyed by `state`), and consumed at `/sso/callback`. The public
/// client sends no secret: the `verifier` proves it initiated the flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PkceSession {
    /// Opaque CSRF token echoed back by the authorization server.
    pub state: String,
    /// High-entropy secret kept locally, replayed at token exchange.
    pub verifier: String,
    /// `base64url(sha256(verifier))`, sent in the authorize request (S256).
    pub challenge: String,
}

impl PkceSession {
    /// Generate a fresh PKCE session: random verifier, S256 challenge, CSRF state.
    pub fn generate() -> Self {
        let verifier = generate_code_verifier();
        let challenge = generate_code_challenge(&verifier);
        let state = generate_state();
        Self {
            state,
            verifier,
            challenge,
        }
    }
}

pub struct MozaikAuth {
    pub client_id: String,
    base_url: String,
}

impl MozaikAuth {
    pub fn new(client_id: String, base_url: Option<&str>) -> Self {
        Self {
            client_id,
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    /// Build the OAuth2 **PKCE** authorize URL that the browser is redirected to.
    ///
    /// Public client: no secret is transmitted, only the S256 `code_challenge`
    /// and a CSRF `state`.
    pub fn authorize_url(&self, redirect_uri: &str, challenge: &str, state: &str) -> String {
        format!(
            "{}/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&code_challenge={}&code_challenge_method=S256&state={}",
            self.base_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(challenge),
            urlencoding::encode(state),
        )
    }

    /// Exchange an authorization code for an access/refresh token pair using
    /// **PKCE** (public client, no `client_secret`).
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<TokenResponse, String> {
        let url = format!("{}/oauth/token", self.base_url);
        let client = crate::http::client::shared();

        let resp = client
            .post(&url)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("client_id", &self.client_id),
                ("code_verifier", code_verifier),
            ])
            .send()
            .await
            .map_err(|e| format!("oauth token request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            debug!(status = %status, body = %body, "oauth_token_exchange_failed");
            return Err(format!("oauth token exchange failed: {status}"));
        }

        let token: TokenResponse = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse token response: {e}"))?;

        info!("oauth_token_exchanged");
        Ok(token)
    }

    /// Fetch the authenticated user's profile from mozaiklabs.
    pub async fn get_user(&self, access_token: &str) -> Result<CloudUser, String> {
        let url = format!("{}/api/v1/user", self.base_url);
        let client = crate::http::client::shared();

        let resp = client
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| format!("user profile request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            return Err(format!("user profile fetch failed: {status}"));
        }

        resp.json()
            .await
            .map_err(|e| format!("failed to parse user profile: {e}"))
    }
}

// ---------------------------------------------------------------------------
// PKCE helpers (RFC 7636) — no external crypto deps, mirrors the Tidal flow.
// ---------------------------------------------------------------------------

/// Generate a cryptographically random code verifier (RFC 7636 §4.1):
/// 43-128 characters from the unreserved set `[A-Z a-z 0-9 - . _ ~]`.
///
/// Entropy source: three v4 UUIDs (122 random bits each → ~366 bits total),
/// mapped onto the unreserved alphabet. 3 × 16 bytes = 48 chars, always ≥ 43.
fn generate_code_verifier() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut result = String::with_capacity(48);
    for _ in 0..3 {
        for &b in uuid::Uuid::new_v4().as_bytes() {
            if result.len() >= 128 {
                break;
            }
            result.push(CHARSET[(b as usize) % CHARSET.len()] as char);
        }
    }
    result
}

/// Compute the S256 code challenge: `base64url(sha256(verifier))`, no padding.
fn generate_code_challenge(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url_encode(&hasher.finalize())
}

/// Generate a random CSRF `state` value (URL-safe, no separators).
fn generate_state() -> String {
    uuid::Uuid::new_v4().to_string().replace('-', "")
}

/// URL-safe base64 encoding without padding (RFC 4648 §5), used for the S256
/// challenge.
fn base64url_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    let mut buf: u32 = 0;
    let mut bits = 0;
    for &byte in data {
        buf = (buf << 8) | byte as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            output.push(TABLE[((buf >> bits) & 0x3F) as usize] as char);
        }
    }
    if bits > 0 {
        buf <<= 6 - bits;
        output.push(TABLE[(buf & 0x3F) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_format() {
        let auth = MozaikAuth::new("my-client".into(), None);
        let url = auth.authorize_url("http://127.0.0.1:8888/auth/callback", "chal", "st");
        assert!(url.starts_with("https://mozaiklabs.fr/oauth/authorize"));
        assert!(url.contains("client_id=my-client"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn custom_base_url() {
        let auth = MozaikAuth::new("test".into(), Some("http://localhost:3000/"));
        let url = auth.authorize_url("http://127.0.0.1:8888/cb", "chal", "st");
        assert!(url.starts_with("http://localhost:3000/oauth/authorize"));
    }

    #[test]
    fn authorize_url_carries_pkce_params() {
        let auth = MozaikAuth::new("cid".into(), None);
        let pkce = PkceSession::generate();
        let url = auth.authorize_url("http://127.0.0.1:9000/cb", &pkce.challenge, &pkce.state);
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains(&format!("code_challenge={}", pkce.challenge)));
        assert!(url.contains(&format!("state={}", pkce.state)));
        // No client secret ever leaks into the public authorize request.
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn verifier_length_and_charset() {
        let v = generate_code_verifier();
        assert!(
            (43..=128).contains(&v.len()),
            "verifier length {} out of RFC range",
            v.len()
        );
        assert!(
            v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')),
            "verifier contains a non-unreserved char"
        );
    }

    #[test]
    fn verifier_is_random_each_time() {
        assert_ne!(generate_code_verifier(), generate_code_verifier());
    }

    #[test]
    fn challenge_is_deterministic_s256() {
        // Known RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = generate_code_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        // No base64 padding, url-safe alphabet only.
        assert!(!challenge.contains('='));
        assert!(!challenge.contains('+') && !challenge.contains('/'));
    }

    #[test]
    fn pkce_session_challenge_matches_verifier() {
        let pkce = PkceSession::generate();
        assert_eq!(pkce.challenge, generate_code_challenge(&pkce.verifier));
        assert!(!pkce.state.is_empty());
    }

    #[test]
    fn base64url_encode_known_vector() {
        // "Man" -> "TWFu" in both standard and url-safe base64.
        assert_eq!(base64url_encode(b"Man"), "TWFu");
        // Single byte 0xFF -> "_w" (url-safe: 62='-', 63='_').
        assert_eq!(base64url_encode(&[0xFF]), "_w");
    }
}
