use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceField {
    pub key: String,
    pub label: String,
    pub field_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub purpose: String,
    pub pricing: String,
    pub pricing_note: String,
    pub fields: Vec<ServiceField>,
    pub help_url: String,
    pub help_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenPayload {
    #[serde(flatten)]
    pub fields: HashMap<String, serde_json::Value>,
    pub valid: Option<bool>,
    pub validation_message: Option<String>,
    pub validated_at: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub purpose: String,
    pub pricing: String,
    pub pricing_note: String,
    pub configured: bool,
    pub source: Option<String>,
    pub valid: Option<bool>,
    pub validated_at: Option<u64>,
    pub validation_message: Option<String>,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn service_catalog() -> Vec<ServiceInfo> {
    vec![
        ServiceInfo {
            id: "musicbrainz".into(),
            name: "MusicBrainz".into(),
            kind: "no_auth".into(),
            purpose: "Années + crédits + couvertures (ID releases).".into(),
            pricing: "free".into(),
            pricing_note: "100 % gratuit, base de données ouverte.".into(),
            fields: vec![],
            help_url: "https://musicbrainz.org/".into(),
            help_steps: vec![
                "Aucun token requis — MusicBrainz est gratuit et anonyme.".into(),
            ],
        },
        ServiceInfo {
            id: "discogs".into(),
            name: "Discogs".into(),
            kind: "personal_token".into(),
            purpose: "Années + couvertures + crédits pour pressages obscurs.".into(),
            pricing: "free".into(),
            pricing_note: "Compte + token personnel gratuits ; API gratuite avec quota (60 req/min authentifié).".into(),
            fields: vec![ServiceField {
                key: "token".into(),
                label: "Personal Access Token".into(),
                field_type: "password".into(),
            }],
            help_url: "https://www.discogs.com/settings/developers".into(),
            help_steps: vec![
                "Connecte-toi sur discogs.com.".into(),
                "Va dans Settings → Developers.".into(),
                "Clique 'Generate new token' (Personal Access Token).".into(),
                "Copie le token et colle-le ici.".into(),
            ],
        },
        ServiceInfo {
            id: "lastfm".into(),
            name: "Last.fm".into(),
            kind: "api_key".into(),
            purpose: "Genres + scrobbling.".into(),
            pricing: "free".into(),
            pricing_note: "API gratuite pour usage non commercial.".into(),
            fields: vec![
                ServiceField {
                    key: "api_key".into(),
                    label: "API Key".into(),
                    field_type: "text".into(),
                },
                ServiceField {
                    key: "api_secret".into(),
                    label: "API Secret (pour scrobbling)".into(),
                    field_type: "password".into(),
                },
            ],
            help_url: "https://www.last.fm/api/account/create".into(),
            help_steps: vec![
                "Va sur last.fm/api/account/create.".into(),
                "Renseigne un nom d'application (ex: 'Tune Server').".into(),
                "Récupère 'API key' et 'Shared secret'.".into(),
                "Colle les valeurs ici.".into(),
            ],
        },
        ServiceInfo {
            id: "tidal".into(),
            name: "Tidal".into(),
            kind: "oauth".into(),
            purpose: "Streaming hi-res + années + couvertures.".into(),
            pricing: "paid".into(),
            pricing_note: "Abonnement Tidal HiFi requis.".into(),
            fields: vec![],
            help_url: "/streaming/tidal".into(),
            help_steps: vec![
                "Tidal utilise OAuth — utilise la page Streaming → Tidal.".into(),
            ],
        },
        ServiceInfo {
            id: "qobuz".into(),
            name: "Qobuz".into(),
            kind: "login_password".into(),
            purpose: "Streaming hi-res + années + couvertures.".into(),
            pricing: "paid".into(),
            pricing_note: "Abonnement Qobuz Studio requis.".into(),
            fields: vec![],
            help_url: "/streaming/qobuz".into(),
            help_steps: vec![
                "Qobuz utilise login/password — utilise la page Streaming → Qobuz.".into(),
            ],
        },
        ServiceInfo {
            id: "spotify".into(),
            name: "Spotify".into(),
            kind: "oauth".into(),
            purpose: "Streaming + connectivité.".into(),
            pricing: "freemium".into(),
            pricing_note: "Compte Spotify gratuit ou Premium requis.".into(),
            fields: vec![],
            help_url: "/streaming/spotify".into(),
            help_steps: vec![
                "Spotify utilise OAuth — utilise la page Streaming → Spotify.".into(),
            ],
        },
        ServiceInfo {
            id: "deezer".into(),
            name: "Deezer".into(),
            kind: "arl_token".into(),
            purpose: "Streaming.".into(),
            pricing: "freemium".into(),
            pricing_note: "Compte gratuit ou Deezer HiFi pour FLAC.".into(),
            fields: vec![ServiceField {
                key: "arl".into(),
                label: "ARL token (depuis cookies deezer.com)".into(),
                field_type: "password".into(),
            }],
            help_url: "/streaming/deezer".into(),
            help_steps: vec![
                "Connecte-toi sur deezer.com.".into(),
                "DevTools (F12) → Application → Cookies → cherche 'arl'.".into(),
                "Copie la valeur et colle-la ici.".into(),
            ],
        },
    ]
}

pub struct ServicesManager {
    db: std::sync::Arc<dyn crate::db::backend::DbBackend>,
}

impl ServicesManager {
    pub fn new(db: crate::db::sqlite::SqliteDb) -> Self {
        Self {
            db: std::sync::Arc::new(db),
        }
    }

    pub fn with_backend(db: std::sync::Arc<dyn crate::db::backend::DbBackend>) -> Self {
        Self { db }
    }

    pub fn load_token(&self, service: &str) -> Result<Option<TokenPayload>, String> {
        let row = self.db.query_one(
            "SELECT token_data FROM streaming_auth WHERE service = ?1",
            &[&service],
        )?;
        match row {
            Some(cols) => {
                let json_str = cols[0].as_str().ok_or("token_data not text")?;
                let payload: TokenPayload =
                    serde_json::from_str(json_str).map_err(|e| e.to_string())?;
                Ok(Some(payload))
            }
            None => Ok(None),
        }
    }

    pub fn save_token(&self, service: &str, payload: &TokenPayload) -> Result<(), String> {
        let blob = serde_json::to_string(payload).map_err(|e| e.to_string())?;
        self.db
            .execute("DELETE FROM streaming_auth WHERE service = ?1", &[&service])?;
        self.db.execute(
            "INSERT INTO streaming_auth (service, token_data) VALUES (?1, ?2)",
            &[&service, &blob.as_str()],
        )?;
        Ok(())
    }

    pub fn delete_token(&self, service: &str) -> Result<(), String> {
        self.db
            .execute("DELETE FROM streaming_auth WHERE service = ?1", &[&service])?;
        Ok(())
    }

    pub fn get_credential(&self, service: &str, key: &str) -> Option<String> {
        let payload = self.load_token(service).ok()??;
        payload
            .fields
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    pub fn list_statuses(&self) -> Vec<ServiceStatus> {
        let catalog = service_catalog();
        catalog
            .into_iter()
            .map(|info| {
                let payload = self.load_token(&info.id).ok().flatten();
                let configured = payload.is_some();
                ServiceStatus {
                    id: info.id,
                    name: info.name,
                    kind: info.kind,
                    purpose: info.purpose,
                    pricing: info.pricing,
                    pricing_note: info.pricing_note,
                    configured,
                    source: if configured { Some("db".into()) } else { None },
                    valid: payload.as_ref().and_then(|p| p.valid),
                    validated_at: payload.as_ref().and_then(|p| p.validated_at),
                    validation_message: payload.as_ref().and_then(|p| p.validation_message.clone()),
                }
            })
            .collect()
    }

    pub async fn validate_discogs(&self, token: &str) -> (bool, String) {
        if token.is_empty() {
            return (false, "Token vide.".into());
        }
        let client = crate::http::client::shared();
        let resp = client
            .get("https://api.discogs.com/oauth/identity")
            .header("Authorization", format!("Discogs token={token}"))
            .header("User-Agent", "TuneServer/1.0 +https://mozaiklabs.fr")
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                let user = data["username"].as_str().unwrap_or("?");
                (true, format!("Token valide (utilisateur: {user})."))
            }
            Ok(r) if r.status().as_u16() == 401 => (false, "Token invalide (401).".into()),
            Ok(r) => (false, format!("HTTP {} — vérifie le token.", r.status())),
            Err(e) => (false, format!("Erreur: {e}")),
        }
    }

    pub async fn validate_lastfm(&self, api_key: &str) -> (bool, String) {
        if api_key.is_empty() {
            return (false, "API Key vide.".into());
        }
        let client = crate::http::client::shared();
        let resp = client
            .get("https://ws.audioscrobbler.com/2.0/")
            .query(&[
                ("method", "auth.getToken"),
                ("api_key", api_key),
                ("format", "json"),
            ])
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => {
                let data: serde_json::Value = r.json().await.unwrap_or_default();
                if data.get("token").is_some() {
                    (true, "API Key valide.".into())
                } else {
                    let msg = data["message"].as_str().unwrap_or("erreur inconnue");
                    (false, format!("Last.fm: {msg}"))
                }
            }
            Ok(r) => (false, format!("HTTP {}", r.status())),
            Err(e) => (false, format!("Erreur: {e}")),
        }
    }

    pub async fn validate_and_save(
        &self,
        service: &str,
        mut payload: TokenPayload,
    ) -> Result<(bool, String), String> {
        let (valid, msg) = match service {
            "discogs" => {
                let token = payload
                    .fields
                    .get("token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.validate_discogs(token).await
            }
            "lastfm" => {
                let key = payload
                    .fields
                    .get("api_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                self.validate_lastfm(key).await
            }
            _ => (true, "Pas de validation disponible.".into()),
        };
        payload.valid = Some(valid);
        payload.validation_message = Some(msg.clone());
        payload.validated_at = Some(now_epoch());
        self.save_token(service, &payload)?;
        Ok((valid, msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_seven_services() {
        let catalog = service_catalog();
        assert_eq!(catalog.len(), 7);
        assert_eq!(catalog[0].id, "musicbrainz");
        assert_eq!(catalog[6].id, "deezer");
    }

    #[test]
    fn catalog_service_kinds() {
        let catalog = service_catalog();
        let kinds: Vec<&str> = catalog.iter().map(|s| s.kind.as_str()).collect();
        assert!(kinds.contains(&"no_auth"));
        assert!(kinds.contains(&"personal_token"));
        assert!(kinds.contains(&"api_key"));
        assert!(kinds.contains(&"oauth"));
        assert!(kinds.contains(&"arl_token"));
    }

    #[test]
    fn discogs_has_one_field() {
        let catalog = service_catalog();
        let discogs = catalog.iter().find(|s| s.id == "discogs").unwrap();
        assert_eq!(discogs.fields.len(), 1);
        assert_eq!(discogs.fields[0].key, "token");
    }

    #[test]
    fn lastfm_has_two_fields() {
        let catalog = service_catalog();
        let lastfm = catalog.iter().find(|s| s.id == "lastfm").unwrap();
        assert_eq!(lastfm.fields.len(), 2);
    }

    #[test]
    fn token_payload_roundtrip() {
        let mut fields = HashMap::new();
        fields.insert("token".into(), serde_json::json!("abc123"));
        let payload = TokenPayload {
            fields,
            valid: Some(true),
            validation_message: Some("OK".into()),
            validated_at: Some(1000),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: TokenPayload = serde_json::from_str(&json).unwrap();
        assert_eq!(back.valid, Some(true));
        assert_eq!(
            back.fields.get("token").and_then(|v| v.as_str()),
            Some("abc123")
        );
    }
}
