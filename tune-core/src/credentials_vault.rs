use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;

const VAULT_KEY: &str = "credentials_vault";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceCredential {
    pub service: String,
    pub username: Option<String>,
    pub token: Option<String>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub extra: HashMap<String, String>,
}

pub struct CredentialsVault {
    db: Arc<dyn DbBackend>,
}

impl CredentialsVault {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    pub fn store(&self, credential: &ServiceCredential) -> Result<(), String> {
        let mut vault = self.load_all()?;
        vault.insert(credential.service.clone(), credential.clone());
        self.save_all(&vault)
    }

    pub fn get(&self, service: &str) -> Result<Option<ServiceCredential>, String> {
        let vault = self.load_all()?;
        Ok(vault.get(service).cloned())
    }

    pub fn remove(&self, service: &str) -> Result<(), String> {
        let mut vault = self.load_all()?;
        vault.remove(service);
        self.save_all(&vault)?;
        info!(service, "credential_removed");
        Ok(())
    }

    pub fn list_services(&self) -> Result<Vec<String>, String> {
        let vault = self.load_all()?;
        Ok(vault.keys().cloned().collect())
    }

    pub fn has(&self, service: &str) -> bool {
        self.get(service).ok().flatten().is_some()
    }

    fn load_all(&self) -> Result<HashMap<String, ServiceCredential>, String> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        let json_str = settings.get(VAULT_KEY).map_err(|e| e.to_string())?;

        match json_str {
            Some(s) if !s.is_empty() => {
                serde_json::from_str(&s).map_err(|e| format!("vault parse: {e}"))
            }
            _ => Ok(HashMap::new()),
        }
    }

    fn save_all(&self, vault: &HashMap<String, ServiceCredential>) -> Result<(), String> {
        let json = serde_json::to_string(vault).map_err(|e| e.to_string())?;
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings.set(VAULT_KEY, &json).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn test_db() -> SqliteDb {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        db
    }

    #[test]
    fn store_and_retrieve() {
        let db = test_db();
        let vault = CredentialsVault::new(db);

        let cred = ServiceCredential {
            service: "lastfm".into(),
            username: Some("user123".into()),
            token: Some("tok_abc".into()),
            api_key: Some("key123".into()),
            api_secret: Some("secret456".into()),
            extra: HashMap::new(),
        };

        vault.store(&cred).unwrap();
        let retrieved = vault.get("lastfm").unwrap().unwrap();
        assert_eq!(retrieved.username.as_deref(), Some("user123"));
        assert_eq!(retrieved.api_key.as_deref(), Some("key123"));
    }

    #[test]
    fn remove_credential() {
        let db = test_db();
        let vault = CredentialsVault::new(db);

        vault
            .store(&ServiceCredential {
                service: "discogs".into(),
                username: None,
                token: Some("token".into()),
                api_key: None,
                api_secret: None,
                extra: HashMap::new(),
            })
            .unwrap();

        assert!(vault.has("discogs"));
        vault.remove("discogs").unwrap();
        assert!(!vault.has("discogs"));
    }

    #[test]
    fn list_services() {
        let db = test_db();
        let vault = CredentialsVault::new(db);

        vault
            .store(&ServiceCredential {
                service: "lastfm".into(),
                username: None,
                token: None,
                api_key: None,
                api_secret: None,
                extra: HashMap::new(),
            })
            .unwrap();
        vault
            .store(&ServiceCredential {
                service: "discogs".into(),
                username: None,
                token: None,
                api_key: None,
                api_secret: None,
                extra: HashMap::new(),
            })
            .unwrap();

        let services = vault.list_services().unwrap();
        assert_eq!(services.len(), 2);
        assert!(services.contains(&"lastfm".to_string()));
        assert!(services.contains(&"discogs".to_string()));
    }

    #[test]
    fn get_nonexistent() {
        let db = test_db();
        let vault = CredentialsVault::new(db);
        assert!(vault.get("unknown").unwrap().is_none());
    }

    #[test]
    fn overwrite_credential() {
        let db = test_db();
        let vault = CredentialsVault::new(db);

        vault
            .store(&ServiceCredential {
                service: "test".into(),
                username: None,
                token: Some("old".into()),
                api_key: None,
                api_secret: None,
                extra: HashMap::new(),
            })
            .unwrap();

        vault
            .store(&ServiceCredential {
                service: "test".into(),
                username: None,
                token: Some("new".into()),
                api_key: None,
                api_secret: None,
                extra: HashMap::new(),
            })
            .unwrap();

        let cred = vault.get("test").unwrap().unwrap();
        assert_eq!(cred.token.as_deref(), Some("new"));
    }
}
