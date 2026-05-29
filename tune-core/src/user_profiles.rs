use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::info;

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
    h.write_u64(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64);
    h.finish()
}

pub struct ProfileManager {
    db: SqliteDb,
}

impl ProfileManager {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn seed_admin(&self) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();
        let has_admin: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM user_profiles WHERE is_admin = 1",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        if has_admin {
            return Ok(());
        }

        conn.execute(
            "INSERT INTO user_profiles (name, avatar_color, is_admin) VALUES (?, ?, 1)",
            params!["Admin", "#FF6B35"],
        )
        .map_err(|e| e.to_string())?;

        info!("default_admin_profile_created");
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<UserProfile>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, avatar_color, avatar_url, is_admin, pin_hash, \
                 eq_settings, quality_preference, created_at \
                 FROM user_profiles ORDER BY name",
            )
            .map_err(|e| e.to_string())?;

        let rows = stmt
            .query_map([], |row| Ok(row_to_profile(row)))
            .map_err(|e| e.to_string())?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn get(&self, profile_id: i64) -> Result<Option<UserProfile>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT id, name, avatar_color, avatar_url, is_admin, pin_hash, \
                 eq_settings, quality_preference, created_at \
                 FROM user_profiles WHERE id = ?",
            )
            .map_err(|e| e.to_string())?;

        stmt.query_row(params![profile_id], |row| Ok(row_to_profile(row)))
            .optional()
            .map_err(|e| e.to_string())
    }

    pub fn create(
        &self,
        name: &str,
        avatar_color: &str,
        pin: Option<&str>,
    ) -> Result<UserProfile, String> {
        let conn = self.db.connection().lock().unwrap();
        let pin_hash = pin.map(hash_pin);

        conn.execute(
            "INSERT INTO user_profiles (name, avatar_color, pin_hash) VALUES (?, ?, ?)",
            params![name.trim(), avatar_color, pin_hash],
        )
        .map_err(|e| e.to_string())?;

        let id = conn.last_insert_rowid();
        drop(conn);
        self.get(id)?
            .ok_or_else(|| "profile not found after create".into())
    }

    pub fn delete(&self, profile_id: i64) -> Result<(), String> {
        let conn = self.db.connection().lock().unwrap();

        let is_admin: bool = conn
            .query_row(
                "SELECT is_admin FROM user_profiles WHERE id = ?",
                params![profile_id],
                |r| r.get::<_, bool>(0),
            )
            .unwrap_or(false);

        if is_admin {
            let admin_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM user_profiles WHERE is_admin = 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if admin_count <= 1 {
                return Err("cannot delete last admin profile".into());
            }
        }

        conn.execute(
            "DELETE FROM user_profiles WHERE id = ?",
            params![profile_id],
        )
        .map_err(|e| e.to_string())?;

        Ok(())
    }

    pub fn switch(&self, profile_id: i64, pin: Option<&str>) -> Result<UserProfile, String> {
        let conn = self.db.connection().lock().unwrap();

        let pin_hash: Option<String> = conn
            .query_row(
                "SELECT pin_hash FROM user_profiles WHERE id = ?",
                params![profile_id],
                |r| r.get(0),
            )
            .map_err(|_| "profile not found".to_string())?;

        if let Some(ref stored_hash) = pin_hash {
            let provided = pin.ok_or("pin_required")?;
            if !verify_pin(provided, stored_hash) {
                return Err("invalid_pin".into());
            }
        }

        drop(conn);
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
        let conn = self.db.connection().lock().unwrap();

        if let Some(tid) = track_id {
            conn.execute(
                "INSERT OR IGNORE INTO user_favorites (user_id, track_id) VALUES (?, ?)",
                params![profile_id, tid],
            )
            .map_err(|e| e.to_string())?;
        } else if let Some(aid) = album_id {
            conn.execute(
                "INSERT OR IGNORE INTO user_favorites (user_id, album_id) VALUES (?, ?)",
                params![profile_id, aid],
            )
            .map_err(|e| e.to_string())?;
        } else if let Some(arid) = artist_id {
            conn.execute(
                "INSERT OR IGNORE INTO user_favorites (user_id, artist_id) VALUES (?, ?)",
                params![profile_id, arid],
            )
            .map_err(|e| e.to_string())?;
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
        let conn = self.db.connection().lock().unwrap();

        if let Some(tid) = track_id {
            conn.execute(
                "DELETE FROM user_favorites WHERE user_id = ? AND track_id = ?",
                params![profile_id, tid],
            )
            .map_err(|e| e.to_string())?;
        } else if let Some(aid) = album_id {
            conn.execute(
                "DELETE FROM user_favorites WHERE user_id = ? AND album_id = ?",
                params![profile_id, aid],
            )
            .map_err(|e| e.to_string())?;
        } else if let Some(arid) = artist_id {
            conn.execute(
                "DELETE FROM user_favorites WHERE user_id = ? AND artist_id = ?",
                params![profile_id, arid],
            )
            .map_err(|e| e.to_string())?;
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
        let conn = self.db.connection().lock().unwrap();

        let count: i64 = if let Some(tid) = track_id {
            conn.query_row(
                "SELECT COUNT(*) FROM user_favorites WHERE user_id = ? AND track_id = ?",
                params![profile_id, tid],
                |r| r.get(0),
            )
            .unwrap_or(0)
        } else if let Some(aid) = album_id {
            conn.query_row(
                "SELECT COUNT(*) FROM user_favorites WHERE user_id = ? AND album_id = ?",
                params![profile_id, aid],
                |r| r.get(0),
            )
            .unwrap_or(0)
        } else if let Some(arid) = artist_id {
            conn.query_row(
                "SELECT COUNT(*) FROM user_favorites WHERE user_id = ? AND artist_id = ?",
                params![profile_id, arid],
                |r| r.get(0),
            )
            .unwrap_or(0)
        } else {
            0
        };

        count > 0
    }
}

fn row_to_profile(row: &rusqlite::Row) -> UserProfile {
    let pin_hash: Option<String> = row.get(5).unwrap_or(None);
    UserProfile {
        id: row.get(0).unwrap_or(0),
        name: row.get(1).unwrap_or_default(),
        avatar_color: row.get(2).unwrap_or_else(|_| "#FF6B35".into()),
        avatar_url: row.get(3).unwrap_or(None),
        is_admin: row.get(4).unwrap_or(false),
        has_pin: pin_hash.is_some(),
        eq_settings: row.get(6).unwrap_or(None),
        quality_preference: row.get(7).unwrap_or(None),
        created_at: row.get(8).unwrap_or(None),
    }
}

use rusqlite::OptionalExtension;

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
