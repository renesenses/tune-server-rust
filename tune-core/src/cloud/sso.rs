use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const DEFAULT_BASE_URL: &str = "https://mozaiklabs.fr";

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

    /// Build the OAuth2 authorize URL that the browser should be redirected to.
    pub fn authorize_url(&self, redirect_uri: &str) -> String {
        format!(
            "{}/oauth/authorize?client_id={}&redirect_uri={}&response_type=code",
            self.base_url,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
        )
    }

    /// Exchange an authorization code for an access/refresh token pair.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        client_secret: &str,
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
                ("client_secret", client_secret),
            ])
            .send()
            .await
            .map_err(|e| format!("oauth token request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body, "oauth_token_exchange_failed");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_format() {
        let auth = MozaikAuth::new("my-client".into(), None);
        let url = auth.authorize_url("http://localhost:8888/auth/callback");
        assert!(url.starts_with("https://mozaiklabs.fr/oauth/authorize"));
        assert!(url.contains("client_id=my-client"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("redirect_uri="));
    }

    #[test]
    fn custom_base_url() {
        let auth = MozaikAuth::new("test".into(), Some("http://localhost:3000/"));
        let url = auth.authorize_url("http://localhost:8888/cb");
        assert!(url.starts_with("http://localhost:3000/oauth/authorize"));
    }
}
