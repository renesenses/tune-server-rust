//! Les clés des clients Subsonic / OpenSubsonic (#7855).
//!
//! Une clé est rendue **une seule fois**, à sa création : la table ne garde
//! que son empreinte SHA-256. Elle ne se relit donc jamais, ni par la liste,
//! ni par la base. Une clé perdue se révoque et se recrée.
//!
//! La révocation ne supprime pas la ligne : elle pose `revoquee_le`, et une
//! clé révoquée ne s'authentifie plus jamais.

use std::sync::Arc;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::backend::{DbBackend, ToSqlValue};

/// Préfixe lisible des clés : il permet de reconnaître une clé Tune collée
/// au mauvais endroit (et à un analyseur de secrets de la repérer).
pub const PREFIXE_CLE: &str = "tune_sk_";

/// Une clé telle que la liste la montre : jamais la clé elle-même.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CleClient {
    pub id: String,
    pub profile_id: i64,
    pub nom: String,
    pub cree_le: String,
    pub revoquee_le: Option<String>,
}

/// Ce que rend la création : la clé en clair, montrée une seule fois.
#[derive(Debug, Clone)]
pub struct CleCreee {
    pub cle: CleClient,
    pub secret: String,
}

pub struct ClesClientsRepo {
    db: Arc<dyn DbBackend>,
}

/// L'empreinte stockée d'une clé : SHA-256 en hexadécimal minuscule.
pub fn empreinte(secret: &str) -> String {
    Sha256::digest(secret.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn nouveau_secret() -> Result<String, String> {
    let mut octets = [0u8; 32];
    getrandom::getrandom(&mut octets).map_err(|e| format!("random source unavailable: {e}"))?;
    let corps: String = octets.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{PREFIXE_CLE}{corps}"))
}

fn maintenant() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

const COLONNES: &str = "id, profile_id, nom, cree_le, revoquee_le";

fn ligne(row: &[super::backend::SqlValue]) -> Option<CleClient> {
    Some(CleClient {
        id: row.first()?.as_string()?,
        profile_id: row.get(1)?.as_i64()?,
        nom: row.get(2)?.as_string().unwrap_or_default(),
        cree_le: row.get(3)?.as_string().unwrap_or_default(),
        revoquee_le: row.get(4).and_then(|v| v.as_string()),
    })
}

impl ClesClientsRepo {
    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    /// Crée une clé pour `profile_id`. Le secret n'est rendu qu'ici.
    pub fn creer(&self, profile_id: i64, nom: &str) -> Result<CleCreee, String> {
        let secret = nouveau_secret()?;
        let id = uuid::Uuid::new_v4().to_string();
        let cree_le = maintenant();
        let empreinte = empreinte(&secret);
        self.db.execute(
            "INSERT INTO cles_clients (id, profile_id, nom, empreinte_sha256, cree_le) VALUES (?, ?, ?, ?, ?)",
            &[
                &id as &dyn ToSqlValue,
                &profile_id,
                &nom,
                &empreinte,
                &cree_le,
            ],
        )?;
        Ok(CleCreee {
            cle: CleClient {
                id,
                profile_id,
                nom: nom.to_string(),
                cree_le,
                revoquee_le: None,
            },
            secret,
        })
    }

    /// Les clés d'un profil, révoquées comprises, les plus récentes d'abord.
    pub fn lister(&self, profile_id: i64) -> Result<Vec<CleClient>, String> {
        let rows = self.db.query_many(
            &format!(
                "SELECT {COLONNES} FROM cles_clients WHERE profile_id = ? ORDER BY cree_le DESC, id"
            ),
            &[&profile_id as &dyn ToSqlValue],
        )?;
        Ok(rows.iter().filter_map(|r| ligne(r)).collect())
    }

    /// Révoque une clé du profil. `Ok(false)` si elle n'existe pas pour ce
    /// profil ; révoquer deux fois garde la première date.
    pub fn revoquer(&self, profile_id: i64, id: &str) -> Result<bool, String> {
        let existe = self.db.query_one(
            "SELECT id FROM cles_clients WHERE id = ? AND profile_id = ?",
            &[&id as &dyn ToSqlValue, &profile_id],
        )?;
        if existe.is_none() {
            return Ok(false);
        }
        let quand = maintenant();
        self.db.execute(
            "UPDATE cles_clients SET revoquee_le = ? WHERE id = ? AND profile_id = ? AND revoquee_le IS NULL",
            &[&quand as &dyn ToSqlValue, &id, &profile_id],
        )?;
        Ok(true)
    }

    /// La clé active qui correspond à `secret`, ou `None` (inconnue,
    /// révoquée, ou dont le profil a été supprimé). Le secret n'est ni
    /// journalisé ni stocké.
    pub fn authentifier(&self, secret: &str) -> Result<Option<CleClient>, String> {
        if secret.is_empty() {
            return Ok(None);
        }
        let attendue = empreinte(secret);
        let row = self.db.query_one(
            &format!(
                "SELECT {COLONNES}, empreinte_sha256 FROM cles_clients \
                 WHERE empreinte_sha256 = ? AND revoquee_le IS NULL \
                 AND EXISTS (SELECT 1 FROM profiles WHERE profiles.id = cles_clients.profile_id)"
            ),
            &[&attendue as &dyn ToSqlValue],
        )?;
        let Some(row) = row else {
            return Ok(None);
        };
        let stockee = row.get(5).and_then(|v| v.as_string()).unwrap_or_default();
        if !bool::from(stockee.as_bytes().ct_eq(attendue.as_bytes())) {
            return Ok(None);
        }
        Ok(ligne(&row))
    }
}

#[cfg(test)]
#[path = "cles_clients_tests.rs"]
mod tests;
