//! Configuration snapshot export/import.
//!
//! Exports all restorable server configuration (zones, settings, playlists,
//! favorites, radios, alarms, EQ, room profiles, streaming tokens) into a
//! single JSON-serialisable [`ConfigSnapshot`].  Sensitive keys (jwt_secret,
//! api_key, license_key, etc.) are excluded; streaming tokens are stored as
//! opaque hex-encoded XOR-obfuscated blobs.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

// ── Snapshot ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigSnapshot {
    /// Schema version — allows forward-compatible restores.
    pub version: String,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// Full zone configurations.
    pub zones: Vec<Value>,
    /// Key-value settings (sensitive keys excluded).
    pub settings: Vec<(String, String)>,
    /// Playlists with their track lists.
    pub playlists: Vec<Value>,
    /// User favorites.
    pub favorites: Vec<Value>,
    /// Saved radio stations.
    pub radio_stations: Vec<Value>,
    /// Alarm configurations.
    pub alarms: Vec<Value>,
    /// EQ presets (stored as settings blobs).
    pub eq_presets: Vec<Value>,
    /// Room correction profiles (stored as settings blobs).
    pub room_profiles: Vec<Value>,
    /// Streaming service tokens — hex-encoded XOR-obfuscated JSON.
    pub streaming_tokens: Vec<(String, String)>,
}

// ── Import report ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportReport {
    pub zones_restored: usize,
    pub settings_restored: usize,
    pub playlists_restored: usize,
    pub favorites_restored: usize,
    pub radio_stations_restored: usize,
    pub alarms_restored: usize,
    pub eq_presets_restored: usize,
    pub room_profiles_restored: usize,
    pub streaming_tokens_restored: usize,
    pub warnings: Vec<String>,
}

// ── Sensitive key filter ────────────────────────────────────────────

const SENSITIVE_KEYS: &[&str] = &[
    "jwt_secret",
    "api_key",
    "license_key",
    "license_tier",
    "license_expires_at",
    "license_last_validated",
    "credentials_vault",
    "server_id",
    "hardware_fingerprint",
];

fn is_sensitive(key: &str) -> bool {
    SENSITIVE_KEYS.contains(&key)
}

fn is_eq_preset_key(key: &str) -> bool {
    key.starts_with("eq_preset_") || key == "eq_presets_index"
}

fn is_room_profile_key(key: &str) -> bool {
    key.starts_with("room_profile_") || key == "room_profile_index"
}

// ── Token obfuscation ───────────────────────────────────────────────
// Streaming tokens are XOR-obfuscated with a fixed key before hex
// encoding so they are not stored as raw secrets in the snapshot JSON.

const OBFUSCATION_KEY: &[u8; 32] = b"TuneConfigBackup2026-obfuscate!!";

fn obfuscate(data: &[u8]) -> String {
    let xored: Vec<u8> = data
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()])
        .collect();
    xored.iter().map(|b| format!("{b:02x}")).collect()
}

fn deobfuscate(encoded: &str) -> Result<Vec<u8>, String> {
    if encoded.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    let xored: Vec<u8> = (0..encoded.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&encoded[i..i + 2], 16)
                .map_err(|e| format!("hex decode at {i}: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(xored
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()])
        .collect())
}

// ── Export ───────────────────────────────────────────────────────────

/// Build a full configuration snapshot from the database.
pub fn export_config(backend: &Arc<dyn DbBackend>) -> Result<ConfigSnapshot, String> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let version = crate::version().to_string();

    let zones = export_zones(backend)?;
    let (settings, eq_presets, room_profiles) = export_settings(backend)?;
    let playlists = export_playlists(backend)?;
    let favorites = export_favorites(backend)?;
    let radio_stations = export_radios(backend)?;
    let alarms = export_alarms(backend)?;
    let streaming_tokens = export_streaming_tokens(backend)?;

    info!(
        zones = zones.len(),
        settings = settings.len(),
        playlists = playlists.len(),
        favorites = favorites.len(),
        radios = radio_stations.len(),
        alarms = alarms.len(),
        eq = eq_presets.len(),
        rooms = room_profiles.len(),
        tokens = streaming_tokens.len(),
        "config_snapshot_exported"
    );

    Ok(ConfigSnapshot {
        version,
        created_at: now,
        zones,
        settings,
        playlists,
        favorites,
        radio_stations,
        alarms,
        eq_presets,
        room_profiles,
        streaming_tokens,
    })
}

fn rows_to_json(rows: Vec<Vec<SqlValue>>, columns: &[&str]) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (i, col) in columns.iter().enumerate() {
                let val = row.get(i).map(sqlvalue_to_json).unwrap_or(Value::Null);
                obj.insert(col.to_string(), val);
            }
            Value::Object(obj)
        })
        .collect()
}

fn sqlvalue_to_json(v: &SqlValue) -> Value {
    match v {
        SqlValue::Null
        | SqlValue::NullInt
        | SqlValue::NullText
        | SqlValue::NullReal
        | SqlValue::NullBool
        | SqlValue::NullBlob => Value::Null,
        SqlValue::Int(n) => Value::Number((*n).into()),
        SqlValue::Real(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        SqlValue::Text(s) => Value::String(s.clone()),
        SqlValue::Bool(b) => Value::Bool(*b),
        SqlValue::Blob(data) => Value::String(data.iter().map(|b| format!("{b:02x}")).collect()),
    }
}

fn export_zones(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let cols = &[
        "id",
        "name",
        "output_type",
        "output_device_id",
        "volume",
        "muted",
        "online",
        "gapless_enabled",
        "group_id",
        "sync_delay_ms",
        "max_sample_rate",
        "fixed_volume",
        "autoplay_enabled",
    ];
    let rows = backend.query_many(
        "SELECT id, name, output_type, output_device_id, volume, muted, \
         online, gapless_enabled, group_id, sync_delay_ms, \
         max_sample_rate, fixed_volume, autoplay_enabled \
         FROM zones ORDER BY id",
        &[],
    )?;
    Ok(rows_to_json(rows, cols))
}

fn export_settings(
    backend: &Arc<dyn DbBackend>,
) -> Result<(Vec<(String, String)>, Vec<Value>, Vec<Value>), String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let all = settings.all()?;

    let mut general = Vec::new();
    let mut eq_presets = Vec::new();
    let mut room_profiles = Vec::new();

    for (key, value) in all {
        if is_sensitive(&key) {
            debug!(key = %key, "config_export_skip_sensitive");
            continue;
        }
        if is_eq_preset_key(&key) {
            eq_presets.push(serde_json::json!({ "key": key, "value": value }));
        } else if is_room_profile_key(&key) {
            room_profiles.push(serde_json::json!({ "key": key, "value": value }));
        } else {
            general.push((key, value));
        }
    }

    Ok((general, eq_presets, room_profiles))
}

fn export_playlists(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let playlist_rows = backend.query_many(
        "SELECT id, name, description FROM playlists ORDER BY id",
        &[],
    )?;

    let mut result = Vec::new();
    for row in playlist_rows {
        let id = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
        let name = row.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let desc = row.get(2).and_then(|v| v.as_string());

        let track_rows = backend.query_many(
            "SELECT pt.position, t.title, t.artist_name, t.album_title, \
             t.source, t.source_id, t.isrc, t.duration_ms \
             FROM playlist_tracks pt \
             JOIN tracks t ON t.id = pt.track_id \
             WHERE pt.playlist_id = ? ORDER BY pt.position",
            &[&id as &dyn ToSqlValue],
        )?;

        let tracks: Vec<Value> = track_rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "position": r.first().and_then(|v| v.as_i64()).unwrap_or(0),
                    "title": r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                    "artist_name": r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                    "album_title": r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
                    "source": r.get(4).and_then(|v| v.as_string()).unwrap_or_default(),
                    "source_id": r.get(5).and_then(|v| v.as_string()).unwrap_or_default(),
                    "isrc": r.get(6).and_then(|v| v.as_string()),
                    "duration_ms": r.get(7).and_then(|v| v.as_i64()).unwrap_or(0),
                })
            })
            .collect();

        result.push(serde_json::json!({
            "id": id,
            "name": name,
            "description": desc,
            "tracks": tracks,
        }));
    }
    Ok(result)
}

fn export_favorites(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let cols = &["id", "profile_id", "item_type", "item_id", "created_at"];
    let rows = backend.query_many(
        "SELECT id, profile_id, item_type, item_id, created_at \
         FROM favorites ORDER BY id",
        &[],
    )?;
    Ok(rows_to_json(rows, cols))
}

fn export_radios(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let cols = &[
        "id",
        "name",
        "url",
        "homepage",
        "logo_url",
        "country",
        "language",
        "genre",
        "codec",
        "bitrate",
        "is_favorite",
    ];
    let rows = backend.query_many(
        "SELECT id, name, url, homepage, logo_url, country, \
         language, genre, codec, bitrate, is_favorite \
         FROM radio_stations ORDER BY id",
        &[],
    )?;
    Ok(rows_to_json(rows, cols))
}

fn export_alarms(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let cols = &[
        "id",
        "zone_id",
        "time",
        "enabled",
        "days",
        "source_type",
        "source_id",
        "volume",
        "fade_in_seconds",
        "name",
    ];
    let rows = backend.query_many(
        "SELECT id, zone_id, time, enabled, days, source_type, \
         source_id, volume, fade_in_seconds, name \
         FROM alarms ORDER BY id",
        &[],
    )?;
    Ok(rows_to_json(rows, cols))
}

fn export_streaming_tokens(backend: &Arc<dyn DbBackend>) -> Result<Vec<(String, String)>, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let vault_json = settings.get("credentials_vault")?;

    let Some(json_str) = vault_json else {
        return Ok(Vec::new());
    };
    if json_str.is_empty() {
        return Ok(Vec::new());
    }

    let vault: serde_json::Map<String, Value> =
        serde_json::from_str(&json_str).map_err(|e| format!("vault parse: {e}"))?;

    let mut tokens = Vec::new();
    for (service, cred_value) in &vault {
        let cred_bytes =
            serde_json::to_vec(cred_value).map_err(|e| format!("serialize cred: {e}"))?;
        tokens.push((service.clone(), obfuscate(&cred_bytes)));
    }
    Ok(tokens)
}

// ── Import ──────────────────────────────────────────────────────────

/// Restore configuration from a snapshot.  Upserts data — does not
/// delete existing rows, only inserts or updates.
pub fn import_config(
    backend: &Arc<dyn DbBackend>,
    snapshot: ConfigSnapshot,
) -> Result<ImportReport, String> {
    let mut report = ImportReport {
        zones_restored: 0,
        settings_restored: 0,
        playlists_restored: 0,
        favorites_restored: 0,
        radio_stations_restored: 0,
        alarms_restored: 0,
        eq_presets_restored: 0,
        room_profiles_restored: 0,
        streaming_tokens_restored: 0,
        warnings: Vec::new(),
    };

    report.zones_restored = import_zones(backend, &snapshot.zones, &mut report.warnings)?;
    report.settings_restored = import_settings(backend, &snapshot.settings, &mut report.warnings)?;
    report.playlists_restored =
        import_playlists(backend, &snapshot.playlists, &mut report.warnings)?;
    report.favorites_restored =
        import_favorites(backend, &snapshot.favorites, &mut report.warnings)?;
    report.radio_stations_restored =
        import_radios(backend, &snapshot.radio_stations, &mut report.warnings)?;
    report.alarms_restored = import_alarms(backend, &snapshot.alarms, &mut report.warnings)?;
    report.eq_presets_restored =
        import_eq_presets(backend, &snapshot.eq_presets, &mut report.warnings)?;
    report.room_profiles_restored =
        import_room_profiles(backend, &snapshot.room_profiles, &mut report.warnings)?;
    report.streaming_tokens_restored =
        import_streaming_tokens(backend, &snapshot.streaming_tokens, &mut report.warnings)?;

    info!(?report, "config_snapshot_imported");
    Ok(report)
}

fn import_zones(
    backend: &Arc<dyn DbBackend>,
    zones: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let mut count = 0;
    for z in zones {
        let name = z["name"].as_str().unwrap_or("Unnamed Zone");

        let existing = backend.query_one(
            "SELECT id FROM zones WHERE name = ?",
            &[&name.to_string() as &dyn ToSqlValue],
        )?;

        if existing.is_some() {
            backend.execute(
                "UPDATE zones SET output_type = ?, output_device_id = ?, volume = ?, \
                 muted = ?, gapless_enabled = ?, max_sample_rate = ?, \
                 fixed_volume = ?, autoplay_enabled = ? \
                 WHERE name = ?",
                &[
                    &z["output_type"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                    &z["output_device_id"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                    &z["volume"].as_i64().unwrap_or(50) as &dyn ToSqlValue,
                    &z["muted"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                    &z["gapless_enabled"].as_i64().unwrap_or(1) as &dyn ToSqlValue,
                    &z["max_sample_rate"].as_i64() as &dyn ToSqlValue,
                    &z["fixed_volume"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                    // autoplay defaults OFF: the schema default is 0 and
                    // migration 46 (autoplay_default_off) forces it off. A
                    // backup that predates the autoplay field must NOT silently
                    // re-enable endless auto-DJ, which appends random tracks
                    // when a launched playlist ends (#1132).
                    &z["autoplay_enabled"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                    &name.to_string() as &dyn ToSqlValue,
                ],
            )?;
        } else {
            backend.execute(
                "INSERT INTO zones (name, output_type, output_device_id, volume, \
                 muted, gapless_enabled, max_sample_rate, fixed_volume, autoplay_enabled) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    &name.to_string() as &dyn ToSqlValue,
                    &z["output_type"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                    &z["output_device_id"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                    &z["volume"].as_i64().unwrap_or(50) as &dyn ToSqlValue,
                    &z["muted"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                    &z["gapless_enabled"].as_i64().unwrap_or(1) as &dyn ToSqlValue,
                    &z["max_sample_rate"].as_i64() as &dyn ToSqlValue,
                    &z["fixed_volume"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                    // autoplay defaults OFF (see UPDATE branch above, #1132).
                    &z["autoplay_enabled"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                ],
            )?;
        }
        count += 1;
    }
    Ok(count)
}

fn import_settings(
    backend: &Arc<dyn DbBackend>,
    settings_list: &[(String, String)],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut count = 0;
    for (key, value) in settings_list {
        if is_sensitive(key) {
            continue;
        }
        settings.set(key, value)?;
        count += 1;
    }
    Ok(count)
}

fn import_playlists(
    backend: &Arc<dyn DbBackend>,
    playlists: &[Value],
    warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let mut count = 0;
    for pl in playlists {
        let name = pl["name"].as_str().unwrap_or("Unnamed");
        let desc = pl["description"].as_str();

        let existing = backend.query_one(
            "SELECT id FROM playlists WHERE name = ?",
            &[&name.to_string() as &dyn ToSqlValue],
        )?;

        if existing.is_some() {
            debug!(name, "config_import_playlist_exists_skip");
            warnings.push(format!("playlist '{name}' already exists, skipped"));
            continue;
        }

        backend.execute(
            "INSERT INTO playlists (name, description) VALUES (?, ?)",
            &[
                &name.to_string() as &dyn ToSqlValue,
                &desc.map(|s| s.to_string()) as &dyn ToSqlValue,
            ],
        )?;
        let pl_id = backend.last_insert_rowid();

        if let Some(tracks) = pl["tracks"].as_array() {
            for t in tracks {
                let title = t["title"].as_str().unwrap_or_default();
                let artist = t["artist_name"].as_str().unwrap_or_default();
                let source = t["source"].as_str().unwrap_or("local");
                let source_id = t["source_id"].as_str().unwrap_or_default();
                let position = t["position"].as_i64().unwrap_or(0);

                let track_row = if !source_id.is_empty() {
                    backend.query_one(
                        "SELECT id FROM tracks WHERE source = ? AND source_id = ?",
                        &[
                            &source.to_string() as &dyn ToSqlValue,
                            &source_id.to_string() as &dyn ToSqlValue,
                        ],
                    )?
                } else {
                    backend.query_one(
                        "SELECT id FROM tracks WHERE title = ? AND artist_name = ? LIMIT 1",
                        &[
                            &title.to_string() as &dyn ToSqlValue,
                            &artist.to_string() as &dyn ToSqlValue,
                        ],
                    )?
                };

                if let Some(row) = track_row {
                    let track_id = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
                    backend.execute(
                        "INSERT INTO playlist_tracks (playlist_id, track_id, position) \
                         VALUES (?, ?, ?)",
                        &[
                            &pl_id as &dyn ToSqlValue,
                            &track_id as &dyn ToSqlValue,
                            &position as &dyn ToSqlValue,
                        ],
                    )?;
                }
            }
        }

        count += 1;
    }
    Ok(count)
}

fn import_favorites(
    backend: &Arc<dyn DbBackend>,
    favorites: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let mut count = 0;
    for fav in favorites {
        let profile_id = fav["profile_id"].as_i64().unwrap_or(1);
        let item_type = fav["item_type"].as_str().unwrap_or_default();
        let item_id = fav["item_id"].as_i64().unwrap_or(0);

        let affected = backend.execute(
            "INSERT OR IGNORE INTO favorites (profile_id, item_type, item_id) \
             VALUES (?, ?, ?)",
            &[
                &profile_id as &dyn ToSqlValue,
                &item_type.to_string() as &dyn ToSqlValue,
                &item_id as &dyn ToSqlValue,
            ],
        )?;
        if affected > 0 {
            count += 1;
        }
    }
    Ok(count)
}

fn import_radios(
    backend: &Arc<dyn DbBackend>,
    radios: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let mut count = 0;
    for r in radios {
        let name = r["name"].as_str().unwrap_or_default();
        let url = r["url"].as_str().unwrap_or_default();

        if name.is_empty() || url.is_empty() {
            continue;
        }

        let existing = backend.query_one(
            "SELECT id FROM radio_stations WHERE name = ? AND url = ?",
            &[
                &name.to_string() as &dyn ToSqlValue,
                &url.to_string() as &dyn ToSqlValue,
            ],
        )?;

        if existing.is_some() {
            continue;
        }

        backend.execute(
            "INSERT INTO radio_stations (name, url, homepage, logo_url, country, \
             language, genre, codec, bitrate, is_favorite) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &name.to_string() as &dyn ToSqlValue,
                &url.to_string() as &dyn ToSqlValue,
                &r["homepage"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                &r["logo_url"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                &r["country"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                &r["language"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                &r["genre"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                &r["codec"].as_str().map(|s| s.to_string()) as &dyn ToSqlValue,
                &r["bitrate"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
                &r["is_favorite"].as_i64().unwrap_or(0) as &dyn ToSqlValue,
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

fn import_alarms(
    backend: &Arc<dyn DbBackend>,
    alarms: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let mut count = 0;
    for a in alarms {
        let zone_id = a["zone_id"].as_i64();
        let time = a["time"].as_str().unwrap_or("07:00");
        let name = a["name"].as_str().unwrap_or("Alarm");

        let existing = if let Some(zid) = zone_id {
            backend.query_one(
                "SELECT id FROM alarms WHERE zone_id = ? AND time = ? AND name = ?",
                &[
                    &zid as &dyn ToSqlValue,
                    &time.to_string() as &dyn ToSqlValue,
                    &name.to_string() as &dyn ToSqlValue,
                ],
            )?
        } else {
            None
        };

        if existing.is_some() {
            continue;
        }

        backend.execute(
            "INSERT INTO alarms (zone_id, time, enabled, days, source_type, \
             source_id, volume, fade_in_seconds, name) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                &zone_id as &dyn ToSqlValue,
                &time.to_string() as &dyn ToSqlValue,
                &a["enabled"].as_i64().unwrap_or(1) as &dyn ToSqlValue,
                &a["days"].as_str().unwrap_or("1,2,3,4,5,6,7").to_string() as &dyn ToSqlValue,
                &a["source_type"].as_str().unwrap_or("playlist").to_string() as &dyn ToSqlValue,
                &a["source_id"].as_i64() as &dyn ToSqlValue,
                &a["volume"].as_f64().unwrap_or(0.3) as &dyn ToSqlValue,
                &a["fade_in_seconds"].as_i64().unwrap_or(30) as &dyn ToSqlValue,
                &name.to_string() as &dyn ToSqlValue,
            ],
        )?;
        count += 1;
    }
    Ok(count)
}

fn import_eq_presets(
    backend: &Arc<dyn DbBackend>,
    presets: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut count = 0;
    for p in presets {
        let key = p["key"].as_str().unwrap_or_default();
        let value = p["value"].as_str().unwrap_or_default();
        if !key.is_empty() {
            settings.set(key, value)?;
            count += 1;
        }
    }
    Ok(count)
}

fn import_room_profiles(
    backend: &Arc<dyn DbBackend>,
    profiles: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut count = 0;
    for p in profiles {
        let key = p["key"].as_str().unwrap_or_default();
        let value = p["value"].as_str().unwrap_or_default();
        if !key.is_empty() {
            settings.set(key, value)?;
            count += 1;
        }
    }
    Ok(count)
}

fn import_streaming_tokens(
    backend: &Arc<dyn DbBackend>,
    tokens: &[(String, String)],
    warnings: &mut Vec<String>,
) -> Result<usize, String> {
    if tokens.is_empty() {
        return Ok(0);
    }

    let settings = SettingsRepo::with_backend(backend.clone());
    let existing_json = settings.get("credentials_vault")?.unwrap_or_default();
    let mut vault: serde_json::Map<String, Value> = if existing_json.is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_str(&existing_json).unwrap_or_else(|_| serde_json::Map::new())
    };

    let mut count = 0;
    for (service, encoded) in tokens {
        match deobfuscate(encoded) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(cred_value) => {
                    vault.insert(service.clone(), cred_value);
                    count += 1;
                }
                Err(e) => {
                    warn!(service, error = %e, "config_import_token_parse_failed");
                    warnings.push(format!("token for '{service}': parse error"));
                }
            },
            Err(e) => {
                warn!(service, error = %e, "config_import_token_decode_failed");
                warnings.push(format!("token for '{service}': decode error"));
            }
        }
    }

    let vault_json = serde_json::to_string(&vault).map_err(|e| e.to_string())?;
    settings.set("credentials_vault", &vault_json)?;

    Ok(count)
}

// ── Snapshot fingerprint ────────────────────────────────────────────

impl ConfigSnapshot {
    /// SHA-256 digest of the snapshot content (for cloud deduplication).
    pub fn fingerprint(&self) -> String {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(&json);
        format!("{:x}", hasher.finalize())
    }

    /// Approximate size in bytes when serialised as JSON.
    pub fn size_bytes(&self) -> usize {
        serde_json::to_vec(self).map(|v| v.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obfuscate_roundtrip() {
        let data = b"hello streaming token";
        let encoded = obfuscate(data);
        let decoded = deobfuscate(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn sensitive_keys_filtered() {
        assert!(is_sensitive("jwt_secret"));
        assert!(is_sensitive("api_key"));
        assert!(is_sensitive("license_key"));
        assert!(is_sensitive("credentials_vault"));
        assert!(!is_sensitive("music_dirs"));
        assert!(!is_sensitive("theme"));
    }

    #[test]
    fn eq_and_room_key_detection() {
        assert!(is_eq_preset_key("eq_preset_rock"));
        assert!(is_eq_preset_key("eq_presets_index"));
        assert!(!is_eq_preset_key("music_dirs"));

        assert!(is_room_profile_key("room_profile_1"));
        assert!(is_room_profile_key("room_profile_index"));
        assert!(!is_room_profile_key("theme"));
    }

    #[test]
    fn snapshot_fingerprint_deterministic() {
        let snap = ConfigSnapshot {
            version: "0.8.0".into(),
            created_at: "2026-06-25T00:00:00Z".into(),
            zones: vec![],
            settings: vec![],
            playlists: vec![],
            favorites: vec![],
            radio_stations: vec![],
            alarms: vec![],
            eq_presets: vec![],
            room_profiles: vec![],
            streaming_tokens: vec![],
        };
        let fp1 = snap.fingerprint();
        let fp2 = snap.fingerprint();
        assert_eq!(fp1, fp2);
        assert_eq!(fp1.len(), 64);
    }

    #[test]
    fn export_import_roundtrip() {
        use crate::db::migrations;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        // Seed a zone
        backend
            .execute(
                "INSERT INTO zones (name, volume) VALUES (?, ?)",
                &[
                    &"Living Room".to_string() as &dyn ToSqlValue,
                    &80i64 as &dyn ToSqlValue,
                ],
            )
            .unwrap();

        // Seed a setting
        let settings = SettingsRepo::with_backend(backend.clone());
        settings.set("theme", "dark").unwrap();

        let snapshot = export_config(&backend).unwrap();
        assert_eq!(snapshot.zones.len(), 1);
        assert!(snapshot.settings.iter().any(|(k, _)| k == "theme"));

        // Import into a fresh DB
        let db2 = SqliteDb::open_in_memory().unwrap();
        db2.init_schema().unwrap();
        migrations::run_migrations(&db2).unwrap();
        let backend2: Arc<dyn DbBackend> = Arc::new(db2);

        let report = import_config(&backend2, snapshot).unwrap();
        assert_eq!(report.zones_restored, 1);
        assert!(report.settings_restored > 0);

        // Verify zone exists
        let row = backend2
            .query_one(
                "SELECT name, volume FROM zones WHERE name = ?",
                &[&"Living Room".to_string() as &dyn ToSqlValue],
            )
            .unwrap();
        assert!(row.is_some());
    }
}
