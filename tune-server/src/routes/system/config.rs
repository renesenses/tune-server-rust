use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::history_repo::HistoryRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

pub(super) async fn version() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "engine": "rust",
    }))
}

pub(super) async fn health(State(state): State<AppState>) -> Json<Value> {
    let tracks_result = TrackRepo::with_backend(state.backend.clone()).count();
    let albums_result = AlbumRepo::with_backend(state.backend.clone()).count();
    let uptime_secs = state.started_at.elapsed().as_secs();

    let db_status = if tracks_result.is_ok() {
        "connected"
    } else {
        "error"
    };
    let tracks = tracks_result.unwrap_or(0);
    let albums = albums_result.unwrap_or(0);

    Json(json!({
        "status": "ok",
        "version": tune_core::version(),
        "uptime_seconds": uptime_secs,
        "db": db_status,
        "tracks": tracks,
        "albums": albums,
    }))
}

pub(super) async fn stats(State(state): State<AppState>) -> Json<Value> {
    let artists = ArtistRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let albums = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let tracks = TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let listens = HistoryRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    // Use timeout to avoid blocking if scanner/outputs mutex is held (e.g. during SSDP scan)
    let devices = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        state.scanner.lock().await.devices().await.len()
    })
    .await
    .unwrap_or(0);
    let outputs = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        state.outputs.lock().await.list().len()
    })
    .await
    .unwrap_or(0);

    Json(json!({
        "artists": artists,
        "albums": albums,
        "tracks": tracks,
        "listens": listens,
        "zones": zones,
        "devices": devices,
        "outputs": outputs,
        "server_version": tune_core::version(),
        "server_engine": "rust",
    }))
}

pub(super) async fn get_config(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let all = settings.all().unwrap_or_default();
    let mut config = serde_json::Map::new();
    for (k, v) in all {
        if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
            config.insert(k, parsed);
        } else {
            config.insert(k, Value::String(v));
        }
    }
    let defaults: Vec<(&str, Value)> = vec![
        ("api_port", json!(state.port)),
        ("stream_port", json!(state.port)),
        ("tidal_enabled", json!(true)),
        ("qobuz_enabled", json!(true)),
        ("youtube_enabled", json!(true)),
        ("spotify_enabled", json!(false)),
        ("deezer_enabled", json!(true)),
        ("amazon_music_enabled", json!(false)),
        ("discovery_enabled", json!(true)),
        ("zone_auto_create", json!(true)),
        ("squeezebox_enabled", json!(false)),
        ("db_engine", json!(state.backend.engine().as_str())),
        ("db_connected", json!(true)),
        ("metadata_readonly", json!(false)),
        ("enrich_on_scan", json!(false)),
        ("quality_split", json!(true)),
        ("resample_policy", json!("none")),
        ("audio_buffer_kb", json!(256)),
        ("prebuffer_seconds", json!(1.0)),
        ("prefetch_mode", json!("30s")),
        (
            "local_audio_backend",
            json!(state.config.local_audio_backend),
        ),
        (
            "local_exclusive_mode",
            json!(state.config.local_exclusive_mode),
        ),
    ];
    for (k, v) in defaults {
        config.entry(k.to_string()).or_insert(v);
    }
    config
        .entry("server_version".to_string())
        .or_insert(json!(tune_core::version()));
    config
        .entry("server_engine".to_string())
        .or_insert(json!("rust"));
    // Ensure onboarding_completed is always present as a boolean
    let onboarding_complete = config
        .get("onboarding_complete")
        .and_then(|v| v.as_str())
        .map(|v| v == "true")
        .or_else(|| config.get("onboarding_complete").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    config
        .entry("onboarding_completed".to_string())
        .or_insert(json!(onboarding_complete));
    // Derived boolean: web client checks discogs_token_set to display badge.
    // Check both the DB setting and the env/toml fallback so that users
    // who set TUNE_DISCOGS_TOKEN in .env or tune.toml also see it as configured.
    let discogs_token_set = config
        .get("discogs_token")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
        || state
            .config
            .discogs_token
            .as_deref()
            .is_some_and(|s| !s.is_empty());
    config.insert("discogs_token_set".to_string(), json!(discogs_token_set));
    // Premium licensing info
    let license_state = state.license.license_state().await;
    let premium_tier = license_state.tier;
    let zone_limit = if premium_tier == tune_core::license::Tier::Premium {
        serde_json::Value::Null
    } else {
        json!(state.license.free_zone_limit())
    };
    let mut premium_features = serde_json::Map::new();
    for f in tune_core::license::Feature::all_premium() {
        let key = serde_json::to_value(f)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        let enabled = state.license.check_feature(*f).await;
        premium_features.insert(key, json!(enabled));
    }
    // Masked license key: show only the last 4 characters.
    let license_key_masked = license_state.license_key.as_deref().map(|k| {
        if k.len() <= 4 {
            k.to_string()
        } else {
            let visible = &k[k.len() - 4..];
            let masked = "*".repeat(k.len() - 4);
            format!("{masked}{visible}")
        }
    });
    config.insert("premium_tier".to_string(), json!(premium_tier));
    config.insert(
        "premium_features".to_string(),
        Value::Object(premium_features),
    );
    config.insert("zone_limit".to_string(), zone_limit);
    config.insert("license_key_masked".to_string(), json!(license_key_masked));
    // Redact secrets before returning. The verbatim settings dump above includes
    // raw credentials that the web client never reads (it uses discogs_token_set,
    // license_key_masked and the streaming status store). Never expose them.
    config.remove("license_key");
    config.remove("discogs_token");
    if let Some(Value::Object(qobuz)) = config.get_mut("auth_tokens_qobuz") {
        for k in ["stored_password", "user_auth_token", "app_secret"] {
            if qobuz.contains_key(k) {
                qobuz.insert(k.to_string(), json!("********"));
            }
        }
    }
    Json(Value::Object(config))
}

pub(super) async fn get_settings(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let music_dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| state.config.music_dirs.clone());
    let db_path = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| state.config.db_path.clone());
    let onboarding_completed = settings
        .get("onboarding_complete")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let theme = read_profile_pref(&settings, profile.id(), "theme");

    Json(json!({
        "music_dirs": music_dirs,
        "db_path": db_path,
        "web_dir": state.config.web_dir,
        "artwork_dir": state.config.artwork_dir,
        "port": state.port,
        "auto_scan": state.config.auto_scan,
        "onboarding_completed": onboarding_completed,
        "server_version": tune_core::version(),
        "server_engine": "rust",
        "theme": theme,
    }))
}

#[derive(Deserialize)]
pub(super) struct ConfigPatch(pub(super) serde_json::Map<String, Value>);

pub(super) async fn update_config(
    State(state): State<AppState>,
    Json(body): Json<ConfigPatch>,
) -> Result<impl IntoResponse, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    for (key, value) in body.0 {
        let str_val = if value.is_string() {
            value
                .as_str()
                .ok_or_else(|| AppError::bad_request("expected string"))?
                .to_string()
        } else {
            value.to_string()
        };
        if let Err(e) = settings.set(&key, &str_val) {
            return Ok((StatusCode::INTERNAL_SERVER_ERROR, e).into_response());
        }
    }
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
pub(super) struct ThemeRequest {
    theme: String,
}

pub(super) async fn set_theme(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<ThemeRequest>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    write_profile_pref(&settings, profile.id(), "theme", &body.theme);
    Json(json!({ "theme": body.theme }))
}

pub(super) async fn get_theme(
    State(state): State<AppState>,
    profile: ActiveProfile,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let theme = read_profile_pref(&settings, profile.id(), "theme");
    Json(json!({ "theme": theme }))
}

pub(super) async fn get_env() -> Json<Value> {
    let port = std::env::var("TUNE_PORT").unwrap_or_else(|_| "8085".into());
    let db = std::env::var("TUNE_DB_PATH").unwrap_or_else(|_| "tune.db".into());

    Json(json!({
        "TUNE_PORT": port,
        "TUNE_DB_PATH": db,
    }))
}

pub(super) async fn get_mode(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mode = settings
        .get("server_mode")
        .ok()
        .flatten()
        .unwrap_or_else(|| "server".into());
    Json(json!({ "mode": mode }))
}

#[derive(Deserialize)]
pub(super) struct SetMode {
    mode: String,
}

pub(super) async fn set_mode(
    State(state): State<AppState>,
    Json(body): Json<SetMode>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("server_mode", &body.mode).ok();
    Json(json!({ "mode": body.mode }))
}

#[derive(Deserialize)]
pub(super) struct ExportConfigQuery {
    #[serde(default)]
    include_secrets: bool,
}

pub(super) async fn export_config(
    State(state): State<AppState>,
    Query(q): Query<ExportConfigQuery>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let all = settings.all().unwrap_or_default();
    let mut config = serde_json::Map::new();
    for (k, v) in all {
        if let Ok(parsed) = serde_json::from_str::<Value>(&v) {
            config.insert(k, parsed);
        } else {
            config.insert(k, Value::String(v));
        }
    }
    // By default, omit secrets so a shared or leaked backup file carries no
    // credentials. import_config merges (it only sets keys present in the
    // payload), so restoring a redacted backup to the SAME server leaves the
    // existing secrets untouched. Pass ?include_secrets=true for a full backup
    // when migrating to a fresh server.
    if !q.include_secrets {
        config.remove("license_key");
        config.remove("discogs_token");
        config.remove("auth_tokens_qobuz");
    }
    Json(Value::Object(config))
}

pub(super) async fn import_config(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<impl IntoResponse, AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut imported = 0;
    for (key, value) in body {
        let str_val = if value.is_string() {
            value
                .as_str()
                .ok_or_else(|| AppError::bad_request("expected string"))?
                .to_string()
        } else {
            value.to_string()
        };
        if settings.set(&key, &str_val).is_ok() {
            imported += 1;
        }
    }
    Ok(Json(json!({ "imported": imported })))
}

// ---------------------------------------------------------------------------
// Default zone
// ---------------------------------------------------------------------------

pub(super) async fn get_default_zone(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let zone_id: Option<i64> = settings
        .get("default_zone_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok());
    Json(json!({ "zone_id": zone_id }))
}

#[derive(Deserialize)]
pub(super) struct DefaultZoneBody {
    zone_id: Option<i64>,
}

pub(super) async fn set_default_zone(
    State(state): State<AppState>,
    Json(body): Json<DefaultZoneBody>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match body.zone_id {
        Some(id) => {
            settings.set("default_zone_id", &id.to_string()).ok();
            Json(json!({ "zone_id": id }))
        }
        None => {
            settings.delete("default_zone_id").ok();
            Json(json!({ "zone_id": null }))
        }
    }
}

pub(super) async fn clear_cache(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("scan_result", "{}").ok();
    Json(json!({ "cleared": true }))
}

pub(super) async fn get_music_dirs(State(state): State<AppState>) -> Json<Value> {
    let dirs = super::get_music_dirs_list(&state.backend);
    Json(json!({ "dirs": dirs }))
}

#[derive(Deserialize)]
pub(super) struct BrowseDirsQuery {
    path: Option<String>,
}

pub(super) async fn browse_dirs(Query(q): Query<BrowseDirsQuery>) -> Json<Value> {
    let base = q.path.unwrap_or_else(|| {
        if cfg!(target_os = "windows") {
            "C:\\".into()
        } else {
            "/".into()
        }
    });

    let base_path = std::path::Path::new(&base);
    if !base_path.exists() || !base_path.is_dir() {
        return Json(
            json!({ "dirs": [], "parent": null, "current": base, "error": "not a directory" }),
        );
    }

    let parent = base_path.parent().map(|p| p.to_string_lossy().to_string());

    let mut dirs: Vec<Value> = Vec::new();

    // On Windows, list drives when at root
    #[cfg(target_os = "windows")]
    if base == "C:\\" || base == "\\" || base == "/" {
        for letter in b'A'..=b'Z' {
            let drive = format!("{}:\\", letter as char);
            if std::path::Path::new(&drive).exists() {
                dirs.push(json!({
                    "name": format!("{} Drive", letter as char),
                    "path": drive,
                    "has_children": true,
                }));
            }
        }
        return Json(json!({ "dirs": dirs, "parent": null, "current": base }));
    }

    if let Ok(entries) = std::fs::read_dir(base_path) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Skip hidden dirs and system dirs
            if name.starts_with('.')
                || name == "$RECYCLE.BIN"
                || name == "System Volume Information"
            {
                continue;
            }
            let has_children = std::fs::read_dir(&path)
                .map(|mut rd| rd.any(|e| e.is_ok_and(|e| e.path().is_dir())))
                .unwrap_or(false);
            dirs.push(json!({
                "name": name,
                "path": path.to_string_lossy(),
                "has_children": has_children,
            }));
        }
    }

    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });

    Json(json!({
        "dirs": dirs,
        "parent": parent,
        "current": base_path.to_string_lossy(),
    }))
}

#[derive(Deserialize)]
pub(super) struct AddMusicDir {
    path: String,
}

pub(super) async fn add_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);

    if normalized.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path is empty" })),
        )
            .into_response());
    }

    let path = std::path::Path::new(&normalized);
    if !path.exists() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "directory does not exist",
                "path": normalized,
            })),
        )
            .into_response());
    }
    if !path.is_dir() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "path is not a directory",
                "path": normalized,
            })),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if !dirs.contains(&normalized) {
        dirs.push(normalized);
    }

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();
    Ok(Json(json!({ "dirs": dirs })).into_response())
}

pub(super) async fn remove_music_dir(
    State(state): State<AppState>,
    Json(body): Json<AddMusicDir>,
) -> Result<Json<Value>, AppError> {
    let normalized = tune_core::scanner::walker::normalize_path(&body.path);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    dirs.retain(|d| {
        let norm_d = tune_core::scanner::walker::normalize_path(d);
        norm_d != normalized
    });

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();
    Ok(Json(json!({ "dirs": dirs })))
}

pub(super) async fn restart() -> impl IntoResponse {
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}

// ---------------------------------------------------------------------------
// Metadata fields configuration
// ---------------------------------------------------------------------------

/// Full catalog of available extended metadata fields.
/// (key, label_fr, category)
const METADATA_FIELDS: &[(&str, &str, &str)] = &[
    // Identification
    ("album_artist", "Artiste de l'album", "Identification"),
    ("sort_artist", "Tri artiste", "Identification"),
    ("sort_album", "Tri album", "Identification"),
    ("disc_number", "N° disque", "Identification"),
    ("disc_subtitle", "Sous-titre disque", "Identification"),
    ("track_number", "N° piste", "Identification"),
    ("genre", "Genre", "Identification"),
    ("genres", "Genres (multi)", "Identification"),
    ("year", "Année", "Identification"),
    // Crédits
    ("composer", "Compositeur", "Crédits"),
    ("conductor", "Chef d'orchestre", "Crédits"),
    ("lyricist", "Parolier", "Crédits"),
    ("performer", "Interprète", "Crédits"),
    ("remixer", "Remixeur", "Crédits"),
    ("label", "Label", "Crédits"),
    ("producer", "Producteur", "Crédits"),
    // Classification
    ("bpm", "BPM", "Classification"),
    ("mood", "Ambiance", "Classification"),
    ("grouping", "Regroupement", "Classification"),
    ("compilation", "Compilation", "Classification"),
    // Texte
    ("comment", "Commentaire", "Texte"),
    ("lyrics", "Paroles", "Texte"),
    // Identifiants
    ("isrc", "ISRC", "Identifiants"),
    ("barcode", "Code-barres", "Identifiants"),
    ("catalog_number", "Réf. catalogue", "Identifiants"),
    ("media_type", "Support", "Identifiants"),
    (
        "musicbrainz_recording_id",
        "MusicBrainz Recording ID",
        "Identifiants",
    ),
    (
        "musicbrainz_release_id",
        "MusicBrainz Release ID",
        "Identifiants",
    ),
    (
        "musicbrainz_release_group_id",
        "MusicBrainz Release Group ID",
        "Identifiants",
    ),
    // Dates
    ("release_date", "Date de sortie", "Dates"),
    ("original_date", "Date originale", "Dates"),
    ("original_year", "Année originale", "Dates"),
    // Technique
    ("format", "Format audio", "Technique"),
    ("sample_rate", "Fréquence d'échantillonnage", "Technique"),
    ("bit_depth", "Profondeur de bits", "Technique"),
    ("channels", "Canaux", "Technique"),
    ("duration_ms", "Durée", "Technique"),
    ("file_size", "Taille du fichier", "Technique"),
    ("file_path", "Chemin du fichier", "Technique"),
    ("encoder", "Encodeur", "Technique"),
    ("copyright", "Copyright", "Technique"),
    ("language", "Langue", "Technique"),
    // ReplayGain
    ("rg_track_gain", "ReplayGain piste", "ReplayGain"),
    ("rg_album_gain", "ReplayGain album", "ReplayGain"),
];

const DEFAULT_VISIBLE_FIELDS: &[&str] = &[
    "composer",
    "conductor",
    "label",
    "genre",
    "year",
    "format",
    "sample_rate",
    "bit_depth",
];

fn metadata_fields_key(pid: i64) -> String {
    format!("metadata_visible_fields:{pid}")
}

/// Read a per-profile preference stored under `key:{pid}`, falling back to the
/// legacy global `key` (installs from before per-profile prefs migrate
/// transparently on first read) then `None`.
fn read_profile_pref(settings: &SettingsRepo, pid: i64, key: &str) -> Option<String> {
    settings
        .get(&format!("{key}:{pid}"))
        .ok()
        .flatten()
        .or_else(|| settings.get(key).ok().flatten())
}

/// Persist a per-profile preference under `key:{pid}`.
fn write_profile_pref(settings: &SettingsRepo, pid: i64, key: &str, value: &str) {
    settings.set(&format!("{key}:{pid}"), value).ok();
}

/// Read the profile-scoped visible fields, falling back to the legacy global
/// key (pre-per-profile installs migrate transparently on first read) then the
/// built-in defaults.
fn read_visible_fields(settings: &SettingsRepo, pid: i64) -> Vec<String> {
    settings
        .get(&metadata_fields_key(pid))
        .ok()
        .flatten()
        .or_else(|| settings.get("metadata_visible_fields").ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| {
            DEFAULT_VISIBLE_FIELDS
                .iter()
                .map(|s| s.to_string())
                .collect()
        })
}

pub(super) async fn get_metadata_fields(
    headers: axum::http::HeaderMap,
    profile: ActiveProfile,
    State(state): State<AppState>,
) -> Json<Value> {
    // Localize the field labels + category names to the client's selected UI
    // language (sent in Accept-Language), falling back to French.
    let lang = crate::i18n::lang_from_header(&headers);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled_keys: Vec<String> = read_visible_fields(&settings, profile.id());

    // Group fields by category (stable French key), preserving catalog order.
    let mut categories: Vec<(&str, Vec<Value>)> = Vec::new();
    for &(key, _label, category) in METADATA_FIELDS {
        let enabled = enabled_keys.iter().any(|k| k == key);
        let field = json!({
            "key": key,
            "label": crate::i18n::t(&lang, &format!("metafield.{key}")),
            "enabled": enabled,
        });

        if let Some(cat) = categories.iter_mut().find(|(name, _)| *name == category) {
            cat.1.push(field);
        } else {
            categories.push((category, vec![field]));
        }
    }

    let result: Vec<Value> = categories
        .into_iter()
        .map(|(name, fields)| {
            json!({ "name": crate::i18n::t(&lang, &format!("metacat.{name}")), "fields": fields })
        })
        .collect();

    Json(json!({ "categories": result }))
}

#[derive(Deserialize)]
pub(super) struct MetadataFieldsBody {
    fields: Vec<String>,
}

pub(super) async fn set_metadata_fields(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<MetadataFieldsBody>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Only keep keys that exist in the catalog
    let valid_keys: Vec<&str> = body
        .fields
        .iter()
        .filter_map(|k| {
            METADATA_FIELDS
                .iter()
                .find(|(key, _, _)| *key == k.as_str())
                .map(|(key, _, _)| *key)
        })
        .collect();
    let json_val = serde_json::to_string(&valid_keys).unwrap_or_else(|_| "[]".into());
    // Persist under the profile-scoped key so different profiles keep separate
    // visible-field sets and an update never loses them.
    settings
        .set(&metadata_fields_key(profile.id()), &json_val)
        .ok();
    Json(json!({ "fields": valid_keys }))
}

// --- Prefetch settings ---

pub(super) async fn get_prefetch(State(state): State<AppState>) -> Json<Value> {
    let mode = tune_core::prefetch::PrefetchEngine::read_mode(&state.backend);
    let status = state.orchestrator.prefetch.status().await;
    Json(json!({
        "mode": mode.as_str(),
        "buffer": status,
    }))
}

#[derive(Deserialize)]
pub(super) struct PrefetchModeBody {
    mode: String,
}

pub(super) async fn set_prefetch(
    State(state): State<AppState>,
    Json(body): Json<PrefetchModeBody>,
) -> Json<Value> {
    let mode = tune_core::prefetch::PrefetchMode::from_str_setting(&body.mode);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("prefetch_mode", mode.as_str()).ok();

    // If switching to Off, clear any buffered data
    if mode == tune_core::prefetch::PrefetchMode::Off {
        state.orchestrator.prefetch.clear().await;
    }

    Json(json!({
        "mode": mode.as_str(),
        "ok": true,
    }))
}

// ---------------------------------------------------------------------------
// License endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct LicenseBody {
    key: String,
}

pub(super) async fn get_license(State(state): State<AppState>) -> Json<Value> {
    let ls = state.license.license_state().await;
    Json(json!({
        "tier": ls.tier,
        "license_key_masked": ls.license_key.as_deref().map(|k| {
            if k.len() <= 4 { "****".to_string() }
            else { format!("{}{}", "*".repeat(k.len() - 4), &k[k.len()-4..]) }
        }),
        "expires_at": ls.expires_at,
        "last_validated": ls.last_validated,
        "hardware_fingerprint": ls.hardware_fingerprint,
    }))
}

pub(super) async fn set_license(
    State(state): State<AppState>,
    Json(body): Json<LicenseBody>,
) -> impl IntoResponse {
    match state.license.set_license_key(&body.key).await {
        Ok(()) => Json(json!({
            "status": "ok",
            "tier": "premium",
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "status": "error",
                "message": e,
            })),
        )
            .into_response(),
    }
}

pub(super) async fn delete_license(State(state): State<AppState>) -> Json<Value> {
    state.license.clear_license().await;
    Json(json!({ "status": "ok", "tier": "free" }))
}
