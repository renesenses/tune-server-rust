//! Configuration snapshot export/import.
//!
//! Exports all restorable server configuration (zones, settings, playlists,
//! favorites, radios, alarms, EQ, room profiles, streaming tokens) into a
//! single JSON-serialisable [`ConfigSnapshot`].  Sensitive keys (jwt_secret,
//! api_key, license_key, etc.) are excluded.
//!
//! # Streaming tokens
//!
//! Streaming credentials are OAuth refresh tokens for paid accounts, and a
//! snapshot leaves the machine: `cloud-push` PUTs it to mozaiklabs.fr. They
//! used to be XOR'd with a fixed key compiled into every binary, which is not
//! encryption — anyone holding a Tune build could read every token in every
//! snapshot they could reach (audit item 7).
//!
//! They are now sealed in a [`Envelope`]: a random data key encrypts them, and
//! that data key is wrapped under both the user's passphrase and a recovery key
//! shown once. Without one of those two secrets the tokens are unreadable, so
//! the cloud store holds an opaque blob.
//!
//! Two consequences worth knowing:
//!
//! - [`export_config`] produces a snapshot with **no tokens at all**. Sealing
//!   requires the passphrase, so it is [`export_config_sealed`] that carries
//!   them. Everything else — zones, playlists, favourites — restores without
//!   any secret, so an unattended `cloud-pull` onto a fresh machine still
//!   rebuilds the install and only asks for a passphrase to re-attach the
//!   streaming services.
//! - Snapshots written before this change are still readable on import
//!   ([`deobfuscate`]), because users have them. Nothing produces that format
//!   any more.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::db::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;
use crate::secret_envelope::{Envelope, RecoveryKey};

/// Settings key holding this install's token envelope: the wrapped data key
/// plus the tokens sealed at the last export. Safe at rest — without the
/// passphrase or the recovery key it is an opaque blob.
pub const ENVELOPE_SETTING: &str = "config_backup_envelope";

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
    /// **Legacy** streaming tokens — hex-encoded XOR-obfuscated JSON, as
    /// written by builds before the envelope (audit item 7). Read on import so
    /// existing backups still restore; never written by this build, and
    /// omitted from the JSON when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub streaming_tokens: Vec<(String, String)>,
    /// Streaming tokens sealed under the install's passphrase + recovery key.
    /// `None` when the snapshot carries no tokens — either no envelope is
    /// configured, or it was produced by [`export_config`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_tokens: Option<Envelope>,
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

// ── Token envelope ──────────────────────────────────────────────────

/// Read this install's stored token envelope, if one has been set up.
pub fn load_envelope(backend: &Arc<dyn DbBackend>) -> Result<Option<Envelope>, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(raw) = settings.get(ENVELOPE_SETTING)? else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| format!("stored envelope is unreadable: {e}"))
}

fn store_envelope(backend: &Arc<dyn DbBackend>, envelope: &Envelope) -> Result<(), String> {
    let raw = serde_json::to_string(envelope).map_err(|e| format!("serialize envelope: {e}"))?;
    SettingsRepo::with_backend(backend.clone()).set(ENVELOPE_SETTING, &raw)
}

/// Whether a passphrase has been set up for this install's streaming tokens.
pub fn envelope_configured(backend: &Arc<dyn DbBackend>) -> Result<bool, String> {
    Ok(load_envelope(backend)?.is_some())
}

/// Set up the token envelope, sealing the current streaming tokens under
/// `passphrase`, and return the recovery key.
///
/// **The recovery key is returned exactly once and never stored.** Display it
/// and let the user write it down; there is no way to produce it again.
///
/// Refuses to run when an envelope already exists — silently replacing it would
/// strand every snapshot sealed under the old key, including ones already
/// pushed to the cloud. Rotating the passphrase is
/// [`change_envelope_passphrase`]; starting over is [`reset_envelope`].
pub fn setup_envelope(
    backend: &Arc<dyn DbBackend>,
    passphrase: &str,
) -> Result<RecoveryKey, String> {
    if load_envelope(backend)?.is_some() {
        return Err(
            "a token passphrase is already configured; change it instead of replacing it".into(),
        );
    }
    let tokens = collect_streaming_tokens(backend)?;
    let plaintext = serde_json::to_vec(&tokens).map_err(|e| format!("serialize tokens: {e}"))?;
    let (envelope, recovery) = Envelope::seal_new(&plaintext, passphrase)?;
    store_envelope(backend, &envelope)?;
    info!(services = tokens.len(), "config_backup_envelope_created");
    Ok(recovery)
}

/// Rotate the passphrase. `current_secret` may be the old passphrase *or* the
/// recovery key — a forgotten passphrase is exactly when this is needed. The
/// recovery key itself keeps working.
///
/// **Rotation is not retroactive.** Each snapshot embeds the key slots as they
/// stood when it was sealed, so one already written — or already pushed to the
/// cloud, where we cannot reach it — still opens with the *old* passphrase.
/// The recovery key opens both old and new. Tell the user this: "change your
/// passphrase" reads as "the old one stops working everywhere", and here it
/// does not.
pub fn change_envelope_passphrase(
    backend: &Arc<dyn DbBackend>,
    current_secret: &str,
    new_passphrase: &str,
) -> Result<(), String> {
    let envelope =
        load_envelope(backend)?.ok_or_else(|| "no token passphrase is configured".to_string())?;
    let rotated = envelope.change_passphrase(current_secret, new_passphrase)?;
    store_envelope(backend, &rotated)?;
    info!("config_backup_envelope_passphrase_changed");
    Ok(())
}

/// Discard the envelope and start over with a new passphrase and recovery key.
///
/// Every snapshot sealed under the previous key becomes unreadable — including
/// any already pushed to the cloud. For when both secrets are lost.
pub fn reset_envelope(
    backend: &Arc<dyn DbBackend>,
    passphrase: &str,
) -> Result<RecoveryKey, String> {
    SettingsRepo::with_backend(backend.clone()).set(ENVELOPE_SETTING, "")?;
    warn!("config_backup_envelope_reset");
    setup_envelope(backend, passphrase)
}

// ── Legacy token obfuscation ────────────────────────────────────────
// Pre-envelope snapshots XOR'd tokens with a fixed key compiled into the
// binary. Kept only so those backups still restore (audit item 7); nothing
// produces this format any more.

const OBFUSCATION_KEY: &[u8; 32] = b"TuneConfigBackup2026-obfuscate!!";

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

/// Build a configuration snapshot **without** streaming tokens.
///
/// Sealing tokens requires the passphrase, which this signature has no way to
/// receive — see [`export_config_sealed`]. Everything else is here, so an
/// unattended push or a restore onto a fresh machine still rebuilds the
/// install; only the streaming services need re-attaching.
pub fn export_config(backend: &Arc<dyn DbBackend>) -> Result<ConfigSnapshot, String> {
    build_snapshot(backend, None)
}

/// Build a snapshot whose streaming tokens are sealed under this install's
/// envelope.
///
/// `passphrase` may also be the recovery key. Fails when no envelope has been
/// set up ([`setup_envelope`]) — rather than silently falling back to a
/// token-less snapshot, which would look like a successful backup while losing
/// exactly the part the user asked to protect.
pub fn export_config_sealed(
    backend: &Arc<dyn DbBackend>,
    passphrase: &str,
) -> Result<ConfigSnapshot, String> {
    let envelope = load_envelope(backend)?.ok_or_else(|| {
        "no token passphrase is configured — set one up before exporting tokens".to_string()
    })?;

    // Re-seal under the existing data key so the passphrase and recovery key
    // the user already holds keep opening this snapshot, while the tokens
    // themselves are refreshed to what the vault holds right now.
    let tokens = collect_streaming_tokens(backend)?;
    let plaintext = serde_json::to_vec(&tokens).map_err(|e| format!("serialize tokens: {e}"))?;
    let resealed = envelope.reseal(passphrase, &plaintext)?;
    store_envelope(backend, &resealed)?;

    build_snapshot(backend, Some(resealed))
}

fn build_snapshot(
    backend: &Arc<dyn DbBackend>,
    sealed_tokens: Option<Envelope>,
) -> Result<ConfigSnapshot, String> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let version = crate::version().to_string();

    let zones = export_zones(backend)?;
    let (settings, eq_presets, room_profiles) = export_settings(backend)?;
    let playlists = export_playlists(backend)?;
    let favorites = export_favorites(backend)?;
    let radio_stations = export_radios(backend)?;
    let alarms = export_alarms(backend)?;

    info!(
        zones = zones.len(),
        settings = settings.len(),
        playlists = playlists.len(),
        favorites = favorites.len(),
        radios = radio_stations.len(),
        alarms = alarms.len(),
        eq = eq_presets.len(),
        rooms = room_profiles.len(),
        tokens_sealed = sealed_tokens.is_some(),
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
        streaming_tokens: Vec::new(),
        sealed_tokens,
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

/// The streaming credentials vault, as plain JSON.
///
/// Only ever handed to [`Envelope::seal_new`] / [`Envelope::reseal`] — this
/// value must not reach a snapshot, a log line or an HTTP response.
fn collect_streaming_tokens(
    backend: &Arc<dyn DbBackend>,
) -> Result<serde_json::Map<String, Value>, String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(json_str) = settings.get("credentials_vault")? else {
        return Ok(serde_json::Map::new());
    };
    if json_str.is_empty() {
        return Ok(serde_json::Map::new());
    }
    serde_json::from_str(&json_str).map_err(|e| format!("vault parse: {e}"))
}

// ── Import ──────────────────────────────────────────────────────────

/// Restore configuration from a snapshot, without unsealing streaming tokens.
///
/// Sealed tokens are skipped with a warning — use [`import_config_with_secret`]
/// to restore them. Everything else comes back, so a restore is useful even
/// with no secret at hand.
pub fn import_config(
    backend: &Arc<dyn DbBackend>,
    snapshot: ConfigSnapshot,
) -> Result<ImportReport, String> {
    import_config_with_secret(backend, snapshot, None)
}

/// Restore configuration, unsealing streaming tokens with `secret` — either the
/// passphrase or the recovery key.
///
/// A wrong secret is reported as a warning, not an error: the rest of the
/// configuration has already been restored by then, and failing the whole
/// operation would throw that away over a mistyped passphrase.
pub fn import_config_with_secret(
    backend: &Arc<dyn DbBackend>,
    snapshot: ConfigSnapshot,
    secret: Option<&str>,
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
        restore_tokens(backend, &snapshot, secret, &mut report.warnings)?;

    info!(?report, "config_snapshot_imported");
    Ok(report)
}

/// Restore streaming tokens from whichever form the snapshot carries: the
/// sealed envelope, or the legacy XOR blobs of a pre-envelope backup.
fn restore_tokens(
    backend: &Arc<dyn DbBackend>,
    snapshot: &ConfigSnapshot,
    secret: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<usize, String> {
    if let Some(envelope) = &snapshot.sealed_tokens {
        let Some(secret) = secret else {
            warnings.push(
                "snapshot contains sealed streaming tokens; supply the passphrase or recovery \
                 key to restore them"
                    .into(),
            );
            return Ok(0);
        };
        let plaintext = match envelope.open(secret) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "config_import_token_unseal_failed");
                warnings.push(format!("streaming tokens: {e}"));
                return Ok(0);
            }
        };
        let vault: serde_json::Map<String, Value> = serde_json::from_slice(&plaintext)
            .map_err(|e| format!("sealed tokens are not a credentials vault: {e}"))?;

        // Adopt the envelope so this machine can re-seal on its next export
        // with the same passphrase and recovery key the user already holds.
        store_envelope(backend, envelope)?;

        return merge_into_vault(backend, vault);
    }

    if !snapshot.streaming_tokens.is_empty() {
        warnings.push(
            "snapshot uses the legacy obfuscated token format; re-export it to seal the tokens"
                .into(),
        );
        return import_legacy_tokens(backend, &snapshot.streaming_tokens, warnings);
    }

    Ok(0)
}

/// Merge restored credentials into the existing vault, keeping services the
/// snapshot does not mention.
fn merge_into_vault(
    backend: &Arc<dyn DbBackend>,
    incoming: serde_json::Map<String, Value>,
) -> Result<usize, String> {
    if incoming.is_empty() {
        return Ok(0);
    }
    let settings = SettingsRepo::with_backend(backend.clone());
    let existing_json = settings.get("credentials_vault")?.unwrap_or_default();
    let mut vault: serde_json::Map<String, Value> = if existing_json.is_empty() {
        serde_json::Map::new()
    } else {
        serde_json::from_str(&existing_json).unwrap_or_default()
    };

    let count = incoming.len();
    for (service, cred) in incoming {
        vault.insert(service, cred);
    }
    let vault_json = serde_json::to_string(&vault).map_err(|e| e.to_string())?;
    settings.set("credentials_vault", &vault_json)?;
    Ok(count)
}

/// Une restauration ne rearme JAMAIS le volume fixe.
///
/// Meme raisonnement que `autoplay_enabled` plus bas (#1132), applique au
/// reglage le plus bruyant du bloc. Armer le volume fixe n'est pas un reglage
/// mais une COMMANDE a 100 % : la route `PATCH /zones/{id}` la refuse tant que
/// le client n'a pas confirme (`fixed_volume_confirmation_required`,
/// `tune-server/src/routes/zones.rs`). Une restauration ecrit la colonne en SQL
/// direct et ne rencontre donc jamais cette confirmation : une zone revenait
/// armee sans que personne ne l'ait demande (#2395, #2477). L'utilisateur
/// re-arme consciemment par la route, ou pas du tout.
const FIXED_VOLUME_JAMAIS_REARME: i64 = 0;

fn import_zones(
    backend: &Arc<dyn DbBackend>,
    zones: &[Value],
    _warnings: &mut Vec<String>,
) -> Result<usize, String> {
    let mut count = 0;
    for z in zones {
        let name = z["name"].as_str().unwrap_or("Unnamed Zone");
        let name = name.to_string();

        let existing = backend.query_one(
            "SELECT id FROM zones WHERE name = ?",
            &[&name as &dyn ToSqlValue],
        )?;

        let output_type = z["output_type"].as_str().map(|s| s.to_string());
        let output_device_id = z["output_device_id"].as_str().map(|s| s.to_string());
        // #2886 — `as_i64()` rendait None des que le volume
        // sauvegarde portait une virgule, et la restauration
        // reposait alors 50 % en travers du reglage de l'utilisateur.
        //
        // Le `unwrap_or(50.0)` reste un niveau INVENTE quand le champ manque :
        // une sauvegarde sans `volume` repose 50 % en travers du reglage, tout
        // comme la virgule le faisait. Signale, pas corrige ici — le sujet de
        // ce correctif est le volume fixe, et changer ce defaut demande de
        // decider ce que vaut une sauvegarde muette sur le volume.
        let volume = z["volume"].as_f64().unwrap_or(50.0);
        let muted = z["muted"].as_i64().unwrap_or(0);
        let gapless_enabled = z["gapless_enabled"].as_i64().unwrap_or(1);
        let max_sample_rate = z["max_sample_rate"].as_i64();
        let fixed_volume = FIXED_VOLUME_JAMAIS_REARME;
        // autoplay defaults OFF: the schema default is 0 and
        // migration 46 (autoplay_default_off) forces it off. A
        // backup that predates the autoplay field must NOT silently
        // re-enable endless auto-DJ, which appends random tracks
        // when a launched playlist ends (#1132).
        let autoplay_enabled = z["autoplay_enabled"].as_i64().unwrap_or(0);

        // Le `volume` d'une sauvegarde prise zone ARMEE est l'artefact de
        // l'armement, pas une preference : le contrat « volume fixe » impose
        // 100 %, c'est lui qui a ecrit ce 100, pas l'utilisateur. Le reposer
        // sur une zone qui revient desarmee ferait afficher 100 a la facade —
        // et la prochaine commande de volume l'enverrait a l'appareil. On ne
        // devine aucun niveau de remplacement : sans memoire, on laisse en
        // place ce qui existe (UPDATE) ou le defaut du schema (INSERT).
        let restaurer_le_volume = z["fixed_volume"].as_i64().unwrap_or(0) == 0;

        if existing.is_some() {
            let mut affectations: Vec<&str> = Vec::with_capacity(8);
            let mut params: Vec<&dyn ToSqlValue> = Vec::with_capacity(9);

            affectations.push("output_type = ?");
            params.push(&output_type);
            affectations.push("output_device_id = ?");
            params.push(&output_device_id);
            if restaurer_le_volume {
                affectations.push("volume = ?");
                params.push(&volume);
            }
            affectations.push("muted = ?");
            params.push(&muted);
            affectations.push("gapless_enabled = ?");
            params.push(&gapless_enabled);
            affectations.push("max_sample_rate = ?");
            params.push(&max_sample_rate);
            affectations.push("fixed_volume = ?");
            params.push(&fixed_volume);
            affectations.push("autoplay_enabled = ?");
            params.push(&autoplay_enabled);
            params.push(&name);

            let sql = format!(
                "UPDATE zones SET {} WHERE name = ?",
                affectations.join(", ")
            );
            backend.execute(&sql, &params)?;
        } else {
            let mut colonnes: Vec<&str> = Vec::with_capacity(9);
            let mut params: Vec<&dyn ToSqlValue> = Vec::with_capacity(9);

            colonnes.push("name");
            params.push(&name);
            colonnes.push("output_type");
            params.push(&output_type);
            colonnes.push("output_device_id");
            params.push(&output_device_id);
            if restaurer_le_volume {
                colonnes.push("volume");
                params.push(&volume);
            }
            colonnes.push("muted");
            params.push(&muted);
            colonnes.push("gapless_enabled");
            params.push(&gapless_enabled);
            colonnes.push("max_sample_rate");
            params.push(&max_sample_rate);
            colonnes.push("fixed_volume");
            params.push(&fixed_volume);
            colonnes.push("autoplay_enabled");
            params.push(&autoplay_enabled);

            let marqueurs = vec!["?"; colonnes.len()].join(", ");
            let sql = format!(
                "INSERT INTO zones ({}) VALUES ({})",
                colonnes.join(", "),
                marqueurs
            );
            backend.execute(&sql, &params)?;
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

        let pl_id = backend.execute_returning_id(
            "INSERT INTO playlists (name, description) VALUES (?, ?)",
            &[
                &name.to_string() as &dyn ToSqlValue,
                &desc.map(|s| s.to_string()) as &dyn ToSqlValue,
            ],
        )?;

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

/// Read tokens from a pre-envelope snapshot. Kept so existing backups still
/// restore; nothing writes this format any more (audit item 7).
fn import_legacy_tokens(
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

    /// The legacy reader must keep working: users hold snapshots in the old
    /// format and a restore has to accept them. The *writer* is gone — this
    /// re-creates a blob the way pre-envelope builds did.
    #[test]
    fn legacy_obfuscated_tokens_still_decode() {
        let data = b"hello streaming token";
        let encoded: String = data
            .iter()
            .enumerate()
            .map(|(i, b)| format!("{:02x}", b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()]))
            .collect();
        assert_eq!(deobfuscate(&encoded).unwrap(), data);
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
            sealed_tokens: None,
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

    // ── Sealed streaming tokens ─────────────────────────────────────

    const VAULT: &str = r#"{"tidal":{"refresh_token":"tidal-refresh-secret"}}"#;

    fn seeded_backend() -> Arc<dyn DbBackend> {
        use crate::db::migrations;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        SettingsRepo::with_backend(backend.clone())
            .set("credentials_vault", VAULT)
            .unwrap();
        backend
    }

    /// The heart of audit item 7: a snapshot that leaves the machine must not
    /// carry a recoverable token. Previously `cloud-push` PUT them to
    /// mozaiklabs.fr XOR'd with a key compiled into every binary.
    #[test]
    fn a_sealed_snapshot_leaks_no_token() {
        let backend = seeded_backend();
        setup_envelope(&backend, "correct horse").unwrap();

        let snapshot = export_config_sealed(&backend, "correct horse").unwrap();
        let json = serde_json::to_string(&snapshot).unwrap();

        assert!(!json.contains("tidal-refresh-secret"));
        assert!(!json.contains("correct horse"));
        assert!(snapshot.sealed_tokens.is_some());
        assert!(snapshot.streaming_tokens.is_empty());
    }

    /// The plain export must never carry tokens: it is what an unattended
    /// cloud-push sends, with no passphrase to seal them.
    #[test]
    fn the_plain_export_carries_no_tokens() {
        let backend = seeded_backend();
        setup_envelope(&backend, "pw").unwrap();

        let snapshot = export_config(&backend).unwrap();
        assert!(snapshot.sealed_tokens.is_none());
        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(!json.contains("tidal-refresh-secret"));
    }

    #[test]
    fn sealed_tokens_restore_onto_a_fresh_machine() {
        let backend = seeded_backend();
        setup_envelope(&backend, "pw").unwrap();
        let snapshot = export_config_sealed(&backend, "pw").unwrap();

        let fresh = seeded_backend();
        SettingsRepo::with_backend(fresh.clone())
            .set("credentials_vault", "")
            .unwrap();

        let report = import_config_with_secret(&fresh, snapshot, Some("pw")).unwrap();
        assert_eq!(report.streaming_tokens_restored, 1);

        let vault = SettingsRepo::with_backend(fresh.clone())
            .get("credentials_vault")
            .unwrap()
            .unwrap();
        assert!(vault.contains("tidal-refresh-secret"));
    }

    /// The reason JP asked for this scheme: a forgotten passphrase must not
    /// destroy the backup.
    #[test]
    fn the_recovery_key_restores_a_snapshot() {
        let backend = seeded_backend();
        let recovery = setup_envelope(&backend, "forgotten").unwrap();
        let snapshot = export_config_sealed(&backend, "forgotten").unwrap();

        let fresh = seeded_backend();
        let report = import_config_with_secret(&fresh, snapshot, Some(recovery.display())).unwrap();
        assert_eq!(report.streaming_tokens_restored, 1);
    }

    /// Everything except the tokens must restore with no secret at all —
    /// otherwise a cloud-pull onto a new machine is useless without a
    /// passphrase.
    #[test]
    fn a_restore_without_the_secret_still_rebuilds_the_install() {
        let backend = seeded_backend();
        backend
            .execute(
                "INSERT INTO zones (name, volume) VALUES (?, ?)",
                &[
                    &"Kitchen".to_string() as &dyn ToSqlValue,
                    &42i64 as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        setup_envelope(&backend, "pw").unwrap();
        let snapshot = export_config_sealed(&backend, "pw").unwrap();

        let fresh = seeded_backend();
        let report = import_config(&fresh, snapshot).unwrap();

        assert_eq!(report.zones_restored, 1);
        assert_eq!(report.streaming_tokens_restored, 0);
        assert!(
            report.warnings.iter().any(|w| w.contains("passphrase")),
            "the user must be told why the tokens are missing: {:?}",
            report.warnings
        );
    }

    #[test]
    fn a_wrong_secret_warns_instead_of_losing_the_whole_restore() {
        let backend = seeded_backend();
        setup_envelope(&backend, "right").unwrap();
        let snapshot = export_config_sealed(&backend, "right").unwrap();

        let fresh = seeded_backend();
        let report = import_config_with_secret(&fresh, snapshot, Some("wrong")).unwrap();
        assert_eq!(report.streaming_tokens_restored, 0);
        assert!(!report.warnings.is_empty());
    }

    #[test]
    fn sealing_without_a_configured_passphrase_fails_loudly() {
        let backend = seeded_backend();
        // No setup_envelope: must error, not quietly drop the tokens and
        // report a successful backup.
        assert!(export_config_sealed(&backend, "pw").is_err());
    }

    #[test]
    fn an_envelope_is_not_silently_replaced() {
        let backend = seeded_backend();
        setup_envelope(&backend, "first").unwrap();
        assert!(
            setup_envelope(&backend, "second").is_err(),
            "replacing the key would strand every snapshot already pushed"
        );
    }

    /// Rotating the passphrase does **not** reach back into snapshots already
    /// written: each one embeds the key slots as they stood when it was
    /// sealed, and a copy pushed to the cloud is out of our hands anyway. So
    /// an old snapshot opens with the *old* passphrase — and, crucially, with
    /// the recovery key, which rotation never invalidates. Snapshots taken
    /// after the rotation take the new passphrase.
    #[test]
    fn rotation_applies_to_new_snapshots_not_old_ones() {
        let backend = seeded_backend();
        let recovery = setup_envelope(&backend, "old").unwrap();
        let before = export_config_sealed(&backend, "old").unwrap();

        change_envelope_passphrase(&backend, "old", "new").unwrap();
        let after = export_config_sealed(&backend, "new").unwrap();

        let restore = |snap: ConfigSnapshot, secret: &str| {
            import_config_with_secret(&seeded_backend(), snap, Some(secret))
                .unwrap()
                .streaming_tokens_restored
        };

        // The pre-rotation snapshot keeps its original slots.
        assert_eq!(restore(before.clone(), "old"), 1);
        assert_eq!(restore(before.clone(), "new"), 0);
        // The emergency kit spans the rotation — that is its whole purpose.
        assert_eq!(restore(before, recovery.display()), 1);

        // Snapshots sealed after the rotation take the new passphrase, and the
        // recovery key still works on them too.
        assert_eq!(restore(after.clone(), "new"), 1);
        assert_eq!(restore(after, recovery.display()), 1);
    }

    /// A pre-envelope backup must still restore — users have them on disk.
    #[test]
    fn a_legacy_snapshot_still_restores() {
        let backend = seeded_backend();
        let cred = serde_json::json!({"refresh_token": "legacy-secret"});
        let bytes = serde_json::to_vec(&cred).unwrap();
        let encoded: String = bytes
            .iter()
            .enumerate()
            .map(|(i, b)| format!("{:02x}", b ^ OBFUSCATION_KEY[i % OBFUSCATION_KEY.len()]))
            .collect();

        let mut snapshot = export_config(&backend).unwrap();
        snapshot.streaming_tokens = vec![("tidal".into(), encoded)];

        let fresh = seeded_backend();
        let report = import_config(&fresh, snapshot).unwrap();
        assert_eq!(report.streaming_tokens_restored, 1);
        assert!(
            report.warnings.iter().any(|w| w.contains("legacy")),
            "the user should be nudged to re-export: {:?}",
            report.warnings
        );
    }

    fn backend_sqlite() -> Arc<dyn DbBackend> {
        use crate::db::migrations;
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn zone_armee_restauree_sur_une_zone_existante() {
        scenarios_zones::une_zone_armee_ne_revient_ni_armee_ni_a_100(&backend_sqlite());
    }

    #[test]
    fn zone_armee_restauree_sur_une_zone_absente() {
        scenarios_zones::une_zone_armee_absente_prend_le_defaut_du_schema(&backend_sqlite());
    }

    #[test]
    fn temoin_une_sauvegarde_desarmee_repose_bien_son_volume() {
        scenarios_zones::temoin_une_sauvegarde_desarmee_repose_son_volume(&backend_sqlite());
    }

    #[test]
    fn temoin_le_reste_du_bloc_ne_change_pas() {
        scenarios_zones::temoin_les_autres_champs_du_bloc_ne_bougent_pas(&backend_sqlite());
    }
}

/// Contre-epreuves de `import_zones`, ecrites contre un `DbBackend` quelconque.
///
/// La requete `UPDATE` est desormais BATIE : elle porte ou non la colonne
/// `volume` selon la sauvegarde. Une requete batie doit rendre le meme resultat
/// sur les deux moteurs — ces scenarios sont donc joues deux fois, sur SQLite
/// par le `mod tests` ci-dessus et sur une VRAIE base PostgreSQL par
/// `db::postgres_e2e::pg_config_backup_zones` (etape dediee de
/// `.github/workflows/test-postgres.yml`).
///
/// Chaque scenario nettoie ses propres lignes a l'entree : la base PostgreSQL
/// de la CI est partagee par toute la suite.
#[cfg(test)]
pub(crate) mod scenarios_zones {
    use super::*;

    /// Lit une colonne numerique sans presumer de son type de stockage :
    /// `zones.fixed_volume` est un INTEGER sur les deux moteurs, mais
    /// `PG_FULL_SCHEMA` la declare TEXT (cf. `db/pg_sqlite_type_parity.rs`).
    fn nombre(v: Option<&SqlValue>) -> Option<f64> {
        let v = v?;
        v.as_f64()
            .or_else(|| v.as_i64().map(|n| n as f64))
            .or_else(|| v.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
            .or_else(|| v.as_string().and_then(|s| s.trim().parse::<f64>().ok()))
    }

    fn effacer(backend: &Arc<dyn DbBackend>, nom: &str) {
        backend
            .execute(
                "DELETE FROM zones WHERE name = ?",
                &[&nom.to_string() as &dyn ToSqlValue],
            )
            .unwrap();
    }

    fn ligne(backend: &Arc<dyn DbBackend>, nom: &str) -> Vec<SqlValue> {
        backend
            .query_one(
                "SELECT volume, fixed_volume, muted, gapless_enabled, \
                 max_sample_rate, autoplay_enabled, output_type \
                 FROM zones WHERE name = ?",
                &[&nom.to_string() as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap_or_else(|| panic!("la zone « {nom} » devrait exister apres restauration"))
    }

    fn volume(backend: &Arc<dyn DbBackend>, nom: &str) -> f64 {
        nombre(ligne(backend, nom).first()).expect("volume illisible")
    }

    fn fixed_volume(backend: &Arc<dyn DbBackend>, nom: &str) -> f64 {
        nombre(ligne(backend, nom).get(1)).expect("fixed_volume illisible")
    }

    fn sauvegarde(nom: &str, fixed: i64, vol: f64) -> Value {
        serde_json::json!({
            "name": nom,
            "output_type": "dlna",
            "output_device_id": "dev-p2c",
            "volume": vol,
            "muted": 0,
            "gapless_enabled": 1,
            "max_sample_rate": 192_000,
            "fixed_volume": fixed,
            "autoplay_enabled": 0,
        })
    }

    fn restaurer(backend: &Arc<dyn DbBackend>, zone: Value) {
        let mut avertissements = Vec::new();
        assert_eq!(
            import_zones(backend, std::slice::from_ref(&zone), &mut avertissements).unwrap(),
            1
        );
    }

    /// 🔴 avant / 🟢 apres, sur la BRANCHE `UPDATE` : une sauvegarde prise zone
    /// armee (`fixed_volume = 1`, donc `volume = 100` ecrit par l'armement)
    /// restauree sur une zone reglee a 20 % laisse `fixed_volume = 0` ET
    /// `volume = 20`.
    pub(crate) fn une_zone_armee_ne_revient_ni_armee_ni_a_100(backend: &Arc<dyn DbBackend>) {
        const NOM: &str = "P2C Salon";
        effacer(backend, NOM);
        backend
            .execute(
                "INSERT INTO zones (name, volume, fixed_volume) VALUES (?, ?, ?)",
                &[
                    &NOM.to_string() as &dyn ToSqlValue,
                    &20.0f64 as &dyn ToSqlValue,
                    &0i64 as &dyn ToSqlValue,
                ],
            )
            .unwrap();

        restaurer(backend, sauvegarde(NOM, 1, 100.0));

        assert_eq!(
            fixed_volume(backend, NOM),
            0.0,
            "une restauration ne doit JAMAIS rearmer le volume fixe : \
             la confirmation de la route n'a pas ete rencontree (#2395, #2477)"
        );
        assert_eq!(
            volume(backend, NOM),
            20.0,
            "le 100 d'une sauvegarde armee est l'artefact de l'armement, \
             pas une preference : le reglage en place reste en place"
        );

        effacer(backend, NOM);
    }

    /// Meme regle sur la branche `INSERT` : la zone est absente, la colonne
    /// `volume` n'est pas ecrite du tout, donc la ligne prend le defaut du
    /// schema — aucun niveau invente (ni 50, ni 20).
    pub(crate) fn une_zone_armee_absente_prend_le_defaut_du_schema(backend: &Arc<dyn DbBackend>) {
        const NOM: &str = "P2C Cuisine";
        const TEMOIN: &str = "P2C Temoin defaut";
        effacer(backend, NOM);
        effacer(backend, TEMOIN);

        // Le defaut du schema, mesure et non presume : une ligne nue.
        backend
            .execute(
                "INSERT INTO zones (name) VALUES (?)",
                &[&TEMOIN.to_string() as &dyn ToSqlValue],
            )
            .unwrap();
        let defaut = volume(backend, TEMOIN);

        restaurer(backend, sauvegarde(NOM, 1, 100.0));

        assert_eq!(fixed_volume(backend, NOM), 0.0);
        assert_eq!(
            volume(backend, NOM),
            defaut,
            "sans zone existante il n'y a rien a preserver : le schema decide, \
             pas la sauvegarde d'une zone armee"
        );
        assert_ne!(volume(backend, NOM), 100.0);

        effacer(backend, NOM);
        effacer(backend, TEMOIN);
    }

    /// Temoin vert des deux cotes : une sauvegarde DESARMEE restaure son volume
    /// exactement comme avant, en `UPDATE` comme en `INSERT`.
    pub(crate) fn temoin_une_sauvegarde_desarmee_repose_son_volume(backend: &Arc<dyn DbBackend>) {
        const EXISTANTE: &str = "P2C Bureau";
        const ABSENTE: &str = "P2C Chambre";
        effacer(backend, EXISTANTE);
        effacer(backend, ABSENTE);

        backend
            .execute(
                "INSERT INTO zones (name, volume) VALUES (?, ?)",
                &[
                    &EXISTANTE.to_string() as &dyn ToSqlValue,
                    &20.0f64 as &dyn ToSqlValue,
                ],
            )
            .unwrap();

        restaurer(backend, sauvegarde(EXISTANTE, 0, 35.0));
        restaurer(backend, sauvegarde(ABSENTE, 0, 35.0));

        assert_eq!(volume(backend, EXISTANTE), 35.0);
        assert_eq!(volume(backend, ABSENTE), 35.0);
        assert_eq!(fixed_volume(backend, EXISTANTE), 0.0);
        assert_eq!(fixed_volume(backend, ABSENTE), 0.0);

        effacer(backend, EXISTANTE);
        effacer(backend, ABSENTE);
    }

    /// Temoin vert : le reste du bloc garde son comportement, que la
    /// sauvegarde soit armee ou non — seule la colonne `volume` sort de la
    /// requete, et seulement quand la sauvegarde est armee.
    pub(crate) fn temoin_les_autres_champs_du_bloc_ne_bougent_pas(backend: &Arc<dyn DbBackend>) {
        const NOM: &str = "P2C Terrasse";

        for armee in [0i64, 1i64] {
            effacer(backend, NOM);
            backend
                .execute(
                    "INSERT INTO zones (name, volume) VALUES (?, ?)",
                    &[
                        &NOM.to_string() as &dyn ToSqlValue,
                        &20.0f64 as &dyn ToSqlValue,
                    ],
                )
                .unwrap();

            let mut zone = sauvegarde(NOM, armee, 100.0);
            zone["muted"] = serde_json::json!(1);
            zone["gapless_enabled"] = serde_json::json!(0);
            zone["autoplay_enabled"] = serde_json::json!(1);
            restaurer(backend, zone);

            let r = ligne(backend, NOM);
            assert_eq!(nombre(r.get(2)), Some(1.0), "muted, armee={armee}");
            assert_eq!(
                nombre(r.get(3)),
                Some(0.0),
                "gapless_enabled, armee={armee}"
            );
            assert_eq!(
                nombre(r.get(4)),
                Some(192_000.0),
                "max_sample_rate, armee={armee}"
            );
            assert_eq!(
                nombre(r.get(5)),
                Some(1.0),
                "autoplay_enabled, armee={armee}"
            );
            assert_eq!(
                r.get(6).and_then(|v| v.as_string()).as_deref(),
                Some("dlna"),
                "output_type, armee={armee}"
            );
        }

        effacer(backend, NOM);
    }
}
