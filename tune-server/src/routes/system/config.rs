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
        state.scanner.devices().await.len()
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

pub(super) async fn get_config(
    headers: axum::http::HeaderMap,
    State(state): State<AppState>,
) -> Json<Value> {
    let lang = crate::i18n::lang_from_header(&headers);
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
        // Default on (unchanged behaviour); scan.rs treats unset as enabled.
        // The web toggle writes "false" to opt out (JF Paquet).
        ("enrich_on_scan", json!(true)),
        // Folder → playlist discovery at scan time — opt-in (Frédéric).
        ("scan_folder_playlists", json!(false)),
        // Import of .m3u/.pls files found at scan time. A different feature
        // from the one above, and default ON since it always behaved that way.
        // The web toggle writes "false" to opt out (JP Borderies).
        ("scan_import_playlists", json!(true)),
        // Le mode PURE impose-t-il le volume à 100 % ? Inactif par défaut :
        // cocher « Audiophile » ne doit pas changer le niveau sans prévenir.
        ("audiophile_lock_volume", json!(false)),
        // Contribution de metadonnees enrichies (bios, images d'artistes) au
        // cloud communautaire. Opt-in STRICT : rien ne sort tant que
        // l'utilisateur n'a pas coche. Le libelle et la phrase qui dit ce qui
        // part sont plus bas, dans `community_contribution`.
        (
            tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY,
            json!(tune_core::cloud::consent::CONTRIBUTION_DEFAULT),
        ),
        ("quality_split", json!(true)),
        ("resample_policy", json!("none")),
        ("audio_buffer_kb", json!(256)),
        ("prebuffer_seconds", json!(1.0)),
        ("prefetch_mode", json!("30s")),
        // ReplayGain application at playback. Off by default: it multiplies
        // every sample, so it must be an explicit choice, never a surprise.
        ("replaygain_mode", json!("off")),
        ("replaygain_preamp_db", json!(0.0)),
        ("replaygain_prevent_clipping", json!(true)),
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
    // DSD → LPCM streaming toggle (Settings → Lecture). PATCH stores it as a
    // raw "true"/"false" string; surface it as a real boolean (default false)
    // so the toggle reflects the persisted state.
    let dsd_lpcm_stream = config
        .get("dsd_lpcm_stream")
        .and_then(|v| v.as_str().map(|s| s == "true").or_else(|| v.as_bool()))
        .unwrap_or(false);
    config.insert("dsd_lpcm_stream".to_string(), json!(dsd_lpcm_stream));
    // Consentement de contribution. Deux valeurs, et elles ne disent pas la
    // meme chose :
    //   - `enabled`   : le choix de l'utilisateur, relu sur la valeur BRUTE en
    //     base avec le meme lecteur que le serveur (`est_vrai`). C'est l'etat
    //     de la bascule. Passer par la carte `config` deja re-typee ferait
    //     diverger les deux — `"1"` y devient un nombre, qu'aucun `as_bool` ne
    //     rattrape, et l'ecran afficherait « non » sur un reglage pose a oui.
    //   - `effective` : ce qui va REELLEMENT se passer, `TUNE_TELEMETRY`
    //     compris. Un exploitant qui a coupe la telemetrie a l'echelle de la
    //     machine ferme la porte pour tout le monde ; sans cette seconde
    //     valeur, l'ecran promettrait un envoi qui n'aura jamais lieu.
    let contribution_enabled = settings
        .get(tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY)
        .ok()
        .flatten()
        .map(|v| tune_core::cloud::consent::est_vrai(&v))
        .unwrap_or(tune_core::cloud::consent::CONTRIBUTION_DEFAULT);
    let contribution_effective = tune_core::cloud::consent::contribution_autorisee(&settings);
    config.insert(
        tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY.to_string(),
        json!(contribution_enabled),
    );
    // Le libelle et la phrase d'explication voyagent avec le reglage : le
    // client web n'a pas a re-decrire ce qui part, et les deux ne peuvent pas
    // diverger. Traduit dans la langue choisie dans l'app (Accept-Language).
    config.insert(
        "community_contribution".to_string(),
        json!({
            "setting_key": tune_core::cloud::consent::CONTRIBUTION_SETTING_KEY,
            "enabled": contribution_enabled,
            "effective": contribution_effective,
            "default": tune_core::cloud::consent::CONTRIBUTION_DEFAULT,
            "label": crate::i18n::t(&lang, "settings.communityContribution.label"),
            "description": crate::i18n::t(&lang, "settings.communityContribution.description"),
        }),
    );
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
    // Appliance mode (Tune OS image): unlocks the host network settings UI.
    config.insert(
        "appliance".to_string(),
        json!(crate::routes::appliance::is_appliance()),
    );
    // Adresses d'accès depuis un autre appareil (Android ne résout pas .local :
    // l'IP est la seule voie universelle — harmonique131, forum-hifi p.25).
    config.insert("server_urls".to_string(), json!(server_urls(state.port)));
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

const FULL_VOLUME_CONFIRMATION_FIELD: &str = "_confirm_full_volume";

fn enables_volume_lock(body: &serde_json::Map<String, Value>) -> bool {
    body.get("audiophile_lock_volume")
        .is_some_and(|value| value.as_bool() == Some(true) || value.as_str() == Some("true"))
}

fn take_full_volume_confirmation(body: &mut serde_json::Map<String, Value>) -> bool {
    body.remove(FULL_VOLUME_CONFIRMATION_FIELD)
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn volume_lock_confirmation_required(
    body: &serde_json::Map<String, Value>,
    already_enabled: bool,
    confirmed: bool,
) -> bool {
    enables_volume_lock(body) && !already_enabled && !confirmed
}

pub(super) async fn update_config(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ConfigPatch>,
) -> Result<impl IntoResponse, AppError> {
    let mut values = body.0;
    let full_volume_confirmed = take_full_volume_confirmation(&mut values);
    let volume_lock_was_enabled = tune_core::audio::audiophile::volume_lock_enabled(&state.backend);
    if volume_lock_confirmation_required(&values, volume_lock_was_enabled, full_volume_confirmed) {
        tracing::warn!("audiophile_volume_lock_confirmation_required");
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "full_volume_confirmation_required",
                "message": "Enabling the PURE volume lock can set a device volume to 100%. Explicit confirmation is required.",
            })),
        )
            .into_response());
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    for (key, value) in values {
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

#[cfg(test)]
mod volume_lock_confirmation_tests {
    use super::{
        FULL_VOLUME_CONFIRMATION_FIELD, enables_volume_lock, take_full_volume_confirmation,
        volume_lock_confirmation_required,
    };
    use serde_json::{Map, json};

    #[test]
    fn detecte_uniquement_l_armement_du_verrou() {
        let mut enable = Map::new();
        enable.insert("audiophile_lock_volume".into(), json!(true));
        assert!(enables_volume_lock(&enable));
        assert!(volume_lock_confirmation_required(&enable, false, false));
        assert!(!volume_lock_confirmation_required(&enable, false, true));
        assert!(!volume_lock_confirmation_required(&enable, true, false));

        enable.insert("audiophile_lock_volume".into(), json!("true"));
        assert!(enables_volume_lock(&enable));

        let mut disable = Map::new();
        disable.insert("audiophile_lock_volume".into(), json!(false));
        assert!(!enables_volume_lock(&disable));
        assert!(!volume_lock_confirmation_required(&disable, true, false));

        let mut unrelated = Map::new();
        unrelated.insert("theme".into(), json!("dark"));
        assert!(!enables_volume_lock(&unrelated));
    }

    #[test]
    fn le_temoin_de_confirmation_est_reserve_et_non_persistable() {
        let mut patch = Map::new();
        patch.insert(FULL_VOLUME_CONFIRMATION_FIELD.into(), json!(true));
        assert!(take_full_volume_confirmation(&mut patch));
        assert!(!patch.contains_key(FULL_VOLUME_CONFIRMATION_FIELD));
    }
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

pub(super) async fn get_env(State(state): State<AppState>) -> Json<Value> {
    // Report what the server actually resolved, not the raw environment: the
    // old version fell back to a hard-coded "tune.db" and to port 8085, so a
    // support page could confidently name a database the server had never
    // opened — and named a SQLite file even on a PostgreSQL deployment.
    let engine = match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => "postgres",
        tune_core::db::engine::Engine::Sqlite => "sqlite",
    };
    Json(json!({
        "TUNE_PORT": state.port.to_string(),
        "TUNE_DB_PATH": state.db.as_ref().map(|_| state.config.db_path.clone()),
        "engine": engine,
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
    _admin: crate::auth::RequireAdmin,
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

    let newly_added = !dirs.contains(&normalized);
    if newly_added {
        dirs.push(normalized);
    }

    settings
        .set("music_dirs", &serde_json::to_string(&dirs)?)
        .ok();

    // Scan right away so the new folder's tracks appear without an app restart.
    // Previously add_music_dir only saved the path: the startup scan and the
    // file-watcher are both initialised once at boot with the old dir list, so a
    // folder added later was neither scanned nor watched — it only showed up
    // after a restart (Jean-Pierre).
    if newly_added {
        super::scan::spawn_library_scan(state.clone(), false, None).await;
    }
    Ok(Json(json!({ "dirs": dirs })).into_response())
}

pub(super) async fn remove_music_dir(
    _admin: crate::auth::RequireAdmin,
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

/// `POST /system/stop` — arrêter le PROCESSUS serveur, sans toucher à la
/// machine. C'est le geste qui manquait sur un poste de bureau : « Éteindre »
/// est réservé aux appliances (il coupe toute la machine), « Redémarrer »
/// revient toujours — il n'y avait aucun moyen d'ARRÊTER Tune depuis
/// l'interface (Bertrand, 25/08, confirmé absent en Expert aussi).
///
/// Honnêteté : sur une installation supervisée (systemd `Restart=always`,
/// service Windows), le superviseur peut relancer le processus aussitôt —
/// l'écran le dit dans la confirmation.
pub(super) async fn stop(_admin: crate::auth::RequireAdmin) -> impl IntoResponse {
    tokio::spawn(async {
        // Laisser la réponse HTTP partir avant de mourir.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("server_stop_requested_from_ui");
        std::process::exit(0);
    });
    Json(json!({ "stopping": true }))
}

pub(super) async fn restart(_admin: crate::auth::RequireAdmin) -> impl IntoResponse {
    tokio::spawn(async {
        // Let the HTTP response flush before we swap the process image.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // UNIX: re-exec in place with execv (same PID) so the server actually
        // comes back WITHOUT relying on an external supervisor. The previous
        // `exit(0)` only recovered when something restarted us on exit (systemd
        // Restart=always) — on a bare/manual install with no supervisor (e.g.
        // Yacine's Synology DSM scheduled task) it just killed Tune and it never
        // came back. Same approach as the update flow (#528). The listening
        // socket is CLOEXEC so exec() releases port 8888 for the new image.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            if let Ok(exe) = std::env::current_exe() {
                let args: Vec<String> = std::env::args().skip(1).collect();
                // Ne pas rouvrir le navigateur au redémarrage : l'onglet existant
                // se reconnecte tout seul (Jean, forum #1236 — deux onglets).
                unsafe { std::env::remove_var("TUNE_OPEN_BROWSER") };
                tracing::info!(exe = %exe.display(), "restart_reexec");
                let err = std::process::Command::new(&exe).args(&args).exec();
                // exec() only returns on failure → fall back to spawn+exit so a
                // supervised deployment still recovers.
                tracing::warn!(error = %err, "restart_reexec_failed — falling back to spawn+exit");
                let _ = std::process::Command::new(&exe)
                    .args(&args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .spawn();
            }
        }

        // WINDOWS: we can't exec() in place. A plain restart is NOT swapping the
        // binary (unlike the update flow, which must exit and let tune-update.bat
        // do the PID-gated swap), so we CAN relaunch the SAME exe ourselves:
        // spawn a fresh copy, then exit. Without this, `exit(0)` just killed Tune
        // on a bare Windows install with no supervisor (Mika, #1209: "Network
        // error: server unreachable" then "Failed to load zones" — the server
        // never came back and had to be relaunched by hand). The listening socket
        // is created non-inheritable (socket2 sets WSA_FLAG_NO_HANDLE_INHERIT), so
        // the child does NOT inherit it and this process's exit fully releases
        // port 8888; the child's bind() retries for ~20s (main.rs) to cover the
        // brief release window. On a supervised install the child simply races the
        // supervisor's relaunch and whichever loses exits cleanly on the bind
        // guard — no crash loop.
        #[cfg(windows)]
        {
            if let Ok(exe) = std::env::current_exe() {
                let args: Vec<String> = std::env::args().skip(1).collect();
                tracing::info!(exe = %exe.display(), "restart_windows_spawn");
                match std::process::Command::new(&exe)
                    .args(&args)
                    // Onglet existant déjà connecté — pas de nouvel onglet (#1236).
                    .env_remove("TUNE_OPEN_BROWSER")
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::inherit())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                {
                    Ok(child) => {
                        tracing::info!(pid = child.id(), "restart_windows_new_process_spawned");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "restart_windows_spawn_failed — manual restart required");
                    }
                }
                // Give the child a moment to start before we release the port.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }

        std::process::exit(0);
    });
    Json(json!({ "status": "restarting" }))
}

// ---------------------------------------------------------------------------
// Metadata fields configuration
// ---------------------------------------------------------------------------

/// Full catalog of available extended metadata fields.
/// (key, label_fr, category, scope)
///
/// `scope` says which entity a field belongs to — "track", "album" or "both" —
/// so clients can build track/album editors from the catalog instead of
/// hardcoding their own whitelist (the web UI's ALBUM_RELEVANT_KEYS).
const METADATA_FIELDS: &[(&str, &str, &str, &str)] = &[
    // Identification
    (
        "album_artist",
        "Artiste de l'album",
        "Identification",
        "both",
    ),
    ("sort_artist", "Tri artiste", "Identification", "both"),
    ("sort_album", "Tri album", "Identification", "album"),
    ("disc_number", "N° disque", "Identification", "track"),
    (
        "disc_subtitle",
        "Sous-titre disque",
        "Identification",
        "track",
    ),
    ("track_number", "N° piste", "Identification", "track"),
    ("genre", "Genre", "Identification", "both"),
    ("genres", "Genres (multi)", "Identification", "both"),
    ("year", "Année", "Identification", "both"),
    // Crédits
    ("composer", "Compositeur", "Crédits", "both"),
    ("conductor", "Chef d'orchestre", "Crédits", "both"),
    ("lyricist", "Parolier", "Crédits", "both"),
    ("performer", "Interprète", "Crédits", "both"),
    ("remixer", "Remixeur", "Crédits", "both"),
    ("label", "Label", "Crédits", "both"),
    ("producer", "Producteur", "Crédits", "both"),
    // Classification
    ("bpm", "BPM", "Classification", "track"),
    ("mood", "Ambiance", "Classification", "both"),
    ("grouping", "Regroupement", "Classification", "both"),
    ("compilation", "Compilation", "Classification", "album"),
    // Texte
    ("comment", "Commentaire", "Texte", "both"),
    ("lyrics", "Paroles", "Texte", "track"),
    // Identifiants
    ("isrc", "ISRC", "Identifiants", "track"),
    ("barcode", "Code-barres", "Identifiants", "album"),
    ("catalog_number", "Réf. catalogue", "Identifiants", "album"),
    ("media_type", "Support", "Identifiants", "album"),
    (
        "musicbrainz_recording_id",
        "MusicBrainz Recording ID",
        "Identifiants",
        "track",
    ),
    (
        "musicbrainz_release_id",
        "MusicBrainz Release ID",
        "Identifiants",
        "album",
    ),
    (
        "musicbrainz_release_group_id",
        "MusicBrainz Release Group ID",
        "Identifiants",
        "album",
    ),
    (
        "mb_release_track_id",
        "MusicBrainz Release Track ID",
        "Identifiants",
        "track",
    ),
    ("release_country", "Pays de sortie", "Identifiants", "album"),
    // Dates
    ("release_date", "Date de sortie", "Dates", "album"),
    ("original_date", "Date originale", "Dates", "album"),
    ("original_year", "Année originale", "Dates", "album"),
    // Technique
    ("format", "Format audio", "Technique", "track"),
    (
        "sample_rate",
        "Fréquence d'échantillonnage",
        "Technique",
        "track",
    ),
    ("bit_depth", "Profondeur de bits", "Technique", "track"),
    ("channels", "Canaux", "Technique", "track"),
    ("duration_ms", "Durée", "Technique", "track"),
    ("file_size", "Taille du fichier", "Technique", "track"),
    ("file_path", "Chemin du fichier", "Technique", "track"),
    ("encoder", "Encodeur", "Technique", "track"),
    (
        "encoder_software",
        "Logiciel d'encodage",
        "Technique",
        "track",
    ),
    ("source_media", "Support (MEDIA)", "Technique", "track"),
    ("copyright", "Copyright", "Technique", "both"),
    ("language", "Langue", "Technique", "both"),
    // ReplayGain
    ("rg_track_gain", "ReplayGain piste", "ReplayGain", "track"),
    ("rg_album_gain", "ReplayGain album", "ReplayGain", "album"),
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
    "release_country",
    "mb_release_track_id",
    "encoder_software",
    "source_media",
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
    for &(key, _label, category, scope) in METADATA_FIELDS {
        let enabled = enabled_keys.iter().any(|k| k == key);
        let field = json!({
            "key": key,
            "label": crate::i18n::t(&lang, &format!("metafield.{key}")),
            "enabled": enabled,
            "scope": scope,
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
                .find(|(key, _, _, _)| *key == k.as_str())
                .map(|(key, _, _, _)| *key)
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
    // Store the key as "pending" (no Premium granted yet), then confirm it with
    // the licensing server before unlocking anything. A fake key therefore never
    // unlocks Premium, while a genuine key is activated in this same round-trip.
    if let Err(e) = state.license.set_license_key(&body.key).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"status": "error", "message": e})),
        )
            .into_response();
    }

    let tier = crate::routes::cloud::validate_stored_license(&state).await;
    let premium = tier == tune_core::license::Tier::Premium;
    let ls = state.license.license_state().await;
    Json(json!({
        "status": if premium { "ok" } else { "pending" },
        "tier": ls.tier,
        "message": if premium {
            "Licence validée : Premium activé."
        } else {
            "Clé enregistrée. Premium s'activera dès qu'elle sera validée en ligne (vérifiez votre connexion et la clé)."
        },
    }))
    .into_response()
}

pub(super) async fn delete_license(State(state): State<AppState>) -> Json<Value> {
    state.license.clear_license().await;
    Json(json!({ "status": "ok", "tier": "free" }))
}

/// URLs d'accès au serveur depuis un autre appareil du réseau.
/// Priorité à TUNE_ADVERTISE_IP (VPN/NordVPN : l'IP détectée serait celle du
/// tunnel), sinon l'IP LAN détectée par la sonde UDP ; plus le nom mDNS
/// (inutile sur Android, mais pratique partout ailleurs). L'IP est recalculée
/// à chaque appel (elle change en cas de bascule filaire↔WiFi) ; le hostname
/// est mis en cache.
pub(super) fn server_urls(port: u16) -> Vec<String> {
    let mut urls = Vec::new();
    if let Ok(ip) = std::env::var("TUNE_ADVERTISE_IP") {
        if !ip.is_empty() {
            urls.push(format!("http://{ip}:{port}"));
        }
    }
    if urls.is_empty() {
        if let Some(ip) = tune_core::discovery::ssdp::get_local_ip() {
            urls.push(format!("http://{ip}:{port}"));
        }
    }
    static HOSTNAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let host = HOSTNAME.get_or_init(|| {
        std::process::Command::new("hostname")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    });
    if !host.is_empty() && host != "localhost" && !host.contains('.') {
        urls.push(format!("http://{host}.local:{port}"));
    }
    urls
}
