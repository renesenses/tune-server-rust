use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::info;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: i64,
    pub name: String,
    pub avatar_color: String,
    pub avatar_url: Option<String>,
    pub is_admin: bool,
    pub has_pin: bool,
    pub eq_settings: Option<String>,
    pub quality_preference: Option<String>,
    pub created_at: Option<String>,
}

fn hash_pin(pin: &str) -> String {
    let salt = format!("{:016x}", rand_salt());
    let input = format!("{salt}${pin}");
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    format!("{salt}${digest}")
}

fn verify_pin(pin: &str, pin_hash: &str) -> bool {
    let Some((salt, expected)) = pin_hash.split_once('$') else {
        return false;
    };
    let input = format!("{salt}${pin}");
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    constant_time_eq(digest.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn rand_salt() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let s = RandomState::new();
    let mut h = s.build_hasher();
    h.write_u64(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
    );
    h.finish()
}

pub struct ProfileManager {
    db: Arc<dyn DbBackend>,
}

fn row_to_profile(r: &[crate::db::backend::SqlValue]) -> UserProfile {
    let pin_hash = r[5].as_string();
    UserProfile {
        id: r[0].as_i64().unwrap_or(0),
        name: r[1].as_string().unwrap_or_default(),
        avatar_color: r[2].as_string().unwrap_or_else(|| "#FF6B35".into()),
        avatar_url: r[3].as_string(),
        is_admin: r[4].as_bool().unwrap_or(false),
        has_pin: pin_hash.is_some(),
        eq_settings: r[6].as_string(),
        quality_preference: r[7].as_string(),
        created_at: r[8].as_string(),
    }
}

impl ProfileManager {
    pub fn new(db: SqliteDb) -> Self {
        Self { db: Arc::new(db) }
    }

    pub fn with_backend(db: Arc<dyn DbBackend>) -> Self {
        Self { db }
    }

    pub fn seed_admin(&self) -> Result<(), String> {
        let row = self.db.query_one(
            "SELECT COUNT(*) FROM user_profiles WHERE is_admin = '1'",
            &[],
        )?;
        let has_admin = row.and_then(|r| r[0].as_i64()).unwrap_or(0) > 0;

        if has_admin {
            return Ok(());
        }

        self.db.execute(
            "INSERT INTO user_profiles (name, avatar_color, is_admin) VALUES (?, ?, 1)",
            &[&"Admin", &"#FF6B35"],
        )?;

        info!("default_admin_profile_created");
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<UserProfile>, String> {
        let rows = self.db.query_many(
            "SELECT id, name, avatar_color, avatar_url, is_admin, pin_hash, \
             eq_settings, quality_preference, created_at \
             FROM user_profiles ORDER BY name",
            &[],
        )?;
        Ok(rows.iter().map(|r| row_to_profile(r)).collect())
    }

    pub fn get(&self, profile_id: i64) -> Result<Option<UserProfile>, String> {
        let row = self.db.query_one(
            "SELECT id, name, avatar_color, avatar_url, is_admin, pin_hash, \
             eq_settings, quality_preference, created_at \
             FROM user_profiles WHERE id = ?",
            &[&profile_id],
        )?;
        Ok(row.as_ref().map(|r| row_to_profile(r)))
    }

    pub fn create(
        &self,
        name: &str,
        avatar_color: &str,
        pin: Option<&str>,
    ) -> Result<UserProfile, String> {
        let pin_hash: Option<String> = pin.map(hash_pin);
        let trimmed = name.trim().to_string();

        self.db.execute(
            "INSERT INTO user_profiles (name, avatar_color, pin_hash) VALUES (?, ?, ?)",
            &[
                &trimmed as &dyn ToSqlValue,
                &avatar_color.to_string(),
                &pin_hash,
            ],
        )?;

        let id = self.db.last_insert_rowid();
        self.get(id)?
            .ok_or_else(|| "profile not found after create".into())
    }

    pub fn delete(&self, profile_id: i64) -> Result<(), String> {
        let row = self.db.query_one(
            "SELECT is_admin FROM user_profiles WHERE id = ?",
            &[&profile_id],
        )?;
        let is_admin = row.and_then(|r| r[0].as_bool()).unwrap_or(false);

        if is_admin {
            let row = self.db.query_one(
                "SELECT COUNT(*) FROM user_profiles WHERE is_admin = '1'",
                &[],
            )?;
            let admin_count = row.and_then(|r| r[0].as_i64()).unwrap_or(0);
            if admin_count <= 1 {
                return Err("cannot delete last admin profile".into());
            }
        }

        self.db
            .execute("DELETE FROM user_profiles WHERE id = ?", &[&profile_id])?;

        Ok(())
    }

    pub fn switch(&self, profile_id: i64, pin: Option<&str>) -> Result<UserProfile, String> {
        let row = self
            .db
            .query_one(
                "SELECT pin_hash FROM user_profiles WHERE id = ?",
                &[&profile_id],
            )?
            .ok_or_else(|| "profile not found".to_string())?;
        let pin_hash = row[0].as_string();

        if let Some(ref stored_hash) = pin_hash {
            let provided = pin.ok_or("pin_required")?;
            if !verify_pin(provided, stored_hash) {
                return Err("invalid_pin".into());
            }
        }

        self.get(profile_id)?
            .ok_or_else(|| "profile not found".into())
    }

    pub fn add_favorite(
        &self,
        profile_id: i64,
        track_id: Option<i64>,
        album_id: Option<i64>,
        artist_id: Option<i64>,
    ) -> Result<(), String> {
        if let Some(tid) = track_id {
            self.db.execute(
                "INSERT OR IGNORE INTO user_favorites (user_id, track_id) VALUES (?, ?)",
                &[&profile_id, &tid],
            )?;
        } else if let Some(aid) = album_id {
            self.db.execute(
                "INSERT OR IGNORE INTO user_favorites (user_id, album_id) VALUES (?, ?)",
                &[&profile_id, &aid],
            )?;
        } else if let Some(arid) = artist_id {
            self.db.execute(
                "INSERT OR IGNORE INTO user_favorites (user_id, artist_id) VALUES (?, ?)",
                &[&profile_id, &arid],
            )?;
        }

        Ok(())
    }

    pub fn remove_favorite(
        &self,
        profile_id: i64,
        track_id: Option<i64>,
        album_id: Option<i64>,
        artist_id: Option<i64>,
    ) -> Result<(), String> {
        if let Some(tid) = track_id {
            self.db.execute(
                "DELETE FROM user_favorites WHERE user_id = ? AND track_id = ?",
                &[&profile_id, &tid],
            )?;
        } else if let Some(aid) = album_id {
            self.db.execute(
                "DELETE FROM user_favorites WHERE user_id = ? AND album_id = ?",
                &[&profile_id, &aid],
            )?;
        } else if let Some(arid) = artist_id {
            self.db.execute(
                "DELETE FROM user_favorites WHERE user_id = ? AND artist_id = ?",
                &[&profile_id, &arid],
            )?;
        }

        Ok(())
    }

    pub fn is_favorite(
        &self,
        profile_id: i64,
        track_id: Option<i64>,
        album_id: Option<i64>,
        artist_id: Option<i64>,
    ) -> bool {
        let count: i64 = if let Some(tid) = track_id {
            self.db
                .query_one(
                    "SELECT COUNT(*) FROM user_favorites WHERE user_id = ? AND track_id = ?",
                    &[&profile_id, &tid],
                )
                .ok()
                .flatten()
                .and_then(|r| r[0].as_i64())
                .unwrap_or(0)
        } else if let Some(aid) = album_id {
            self.db
                .query_one(
                    "SELECT COUNT(*) FROM user_favorites WHERE user_id = ? AND album_id = ?",
                    &[&profile_id, &aid],
                )
                .ok()
                .flatten()
                .and_then(|r| r[0].as_i64())
                .unwrap_or(0)
        } else if let Some(arid) = artist_id {
            self.db
                .query_one(
                    "SELECT COUNT(*) FROM user_favorites WHERE user_id = ? AND artist_id = ?",
                    &[&profile_id, &arid],
                )
                .ok()
                .flatten()
                .and_then(|r| r[0].as_i64())
                .unwrap_or(0)
        } else {
            0
        };

        count > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_hash_and_verify() {
        let pin = "1234";
        let hashed = hash_pin(pin);
        assert!(hashed.contains('$'));
        assert!(verify_pin(pin, &hashed));
        assert!(!verify_pin("wrong", &hashed));
    }

    #[test]
    fn pin_different_salts() {
        let h1 = hash_pin("1234");
        let h2 = hash_pin("1234");
        assert_ne!(h1, h2);
        assert!(verify_pin("1234", &h1));
        assert!(verify_pin("1234", &h2));
    }

    #[test]
    fn verify_invalid_hash() {
        assert!(!verify_pin("1234", "nope"));
        assert!(!verify_pin("1234", ""));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hi", b"hello"));
    }

    #[test]
    fn profile_serialize() {
        let p = UserProfile {
            id: 1,
            name: "Test".into(),
            avatar_color: "#FF0000".into(),
            avatar_url: None,
            is_admin: false,
            has_pin: true,
            eq_settings: None,
            quality_preference: None,
            created_at: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["name"], "Test");
        assert_eq!(json["has_pin"], true);
    }
}
