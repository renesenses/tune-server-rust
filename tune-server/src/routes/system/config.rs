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

    // Le nom voyage AVEC la version (#2110). C'est la même requête que la barre
    // latérale fait déjà pour afficher « v0.9.117 » : la plainte d'origine est
    // qu'elle annonce une version sans dire de quelle machine elle parle. Les
    // séparer imposerait un second appel — et laisserait l'étiquette absente
    // tant qu'il n'a pas répondu.
    let server_name = resolve_server_name(
        SettingsRepo::with_backend(state.backend.clone())
            .get("server_name")
            .ok()
            .flatten()
            .as_deref(),
    );

    Json(json!({
        "status": "ok",
        "version": tune_core::version(),
        "server_name": server_name,
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
    // Nom de CETTE machine, affiché en permanence par l'interface (#2110).
    // Deux serveurs Tune sur un même réseau donnaient deux interfaces
    // identiques : Philippe et Alain ont conclu à une mise à jour ratée alors
    // qu'ils regardaient deux machines. Toujours présent, jamais vide — le
    // client peut l'afficher sans garde-fou.
    config.insert(
        "server_name".to_string(),
        json!(resolve_server_name(
            config.get("server_name").and_then(|v| v.as_str())
        )),
    );
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
    let volume_lock_was_enabled =
        tune_core::audio::audiophile::global_volume_lock_enabled(&state.backend);
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
mod nom_du_serveur_tests {
    use super::resolve_server_name;

    /// Le réglage prime, espaces compris : c'est le nom que l'utilisateur lit.
    #[test]
    fn le_reglage_prime_sur_le_nom_d_hote() {
        assert_eq!(resolve_server_name(Some("Salon")), "Salon");
        assert_eq!(resolve_server_name(Some("  Salon  ")), "Salon");
    }

    /// Absent OU vide ⇒ nom d'hôte réel. Le cas « vide » compte : le vidage du
    /// champ dans l'interface écrit une chaîne vide dans `settings`, il ne
    /// supprime pas la clé. Sans ce filtre, l'étiquette s'afficherait vide.
    #[test]
    fn le_defaut_est_le_nom_d_hote_du_systeme() {
        let attendu = tune_core::discovery::system_hostname();
        assert!(
            !attendu.is_empty(),
            "system_hostname() ne doit jamais rendre une chaîne vide : \
             c'est le défaut sur lequel l'étiquette s'appuie"
        );
        assert_eq!(resolve_server_name(None), attendu);
        assert_eq!(resolve_server_name(Some("")), attendu);
        assert_eq!(resolve_server_name(Some("   ")), attendu);
    }

    /// Contre-épreuve du défaut : le nom d'hôte doit distinguer deux machines,
    /// donc ne jamais être un identifiant technique ni une constante partagée.
    #[test]
    fn le_defaut_n_est_ni_un_uuid_ni_une_constante_de_marque() {
        let defaut = resolve_server_name(None);
        assert_ne!(
            defaut, "Tune Server",
            "« Tune Server » est le nom UPnP, identique sur les deux machines : \
             il ne désambiguïse rien"
        );
        assert_ne!(defaut, "Local", "« Local » ne nomme aucune machine");
        let ressemble_a_un_uuid =
            defaut.len() == 36 && defaut.chars().filter(|c| *c == '-').count() == 4;
        assert!(
            !ressemble_a_un_uuid,
            "le défaut ne doit pas être un UUID : l'humain doit pouvoir le lire"
        );
    }
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

    // Le retrait ne supprime RIEN, et c'est délibéré (voir le module
    // `purge_hors_perimetre` plus bas). Mais il DIT ce qu'il laisse derrière
    // lui : sans ce compte, l'écran des réglages n'a aucun moyen de proposer
    // le nettoyage, et les pistes entrent dans l'angle mort décrit en #2149.
    let restantes = pistes_sous(&state, &normalized).len() as i64;
    if restantes > 0 {
        tracing::info!(
            dossier = %normalized,
            pistes = restantes,
            "music_dir_removed_tracks_left_behind — ces pistes ne sont plus sous aucune racine \
             configurée. Le scan ne les visitera plus et ne les purgera jamais (HorsPerimetre, \
             #1943) : seul un appel explicite à /music-dirs/purge-orphans peut les retirer."
        );
    }
    Ok(Json(json!({ "dirs": dirs, "orphan_tracks": restantes })))
}

// ───────────────────────────────────────────────────────────────────────────
// Purge des pistes hors périmètre — le geste explicite qui manquait (#2149)
// ───────────────────────────────────────────────────────────────────────────
//
// ## Le défaut
//
// `remove_music_dir` retire la racine des réglages et s'arrête là. La purge de
// fin de scan, elle, classe toute piste qui n'est sous AUCUNE racine
// configurée en `VerdictPurge::HorsPerimetre` et la CONSERVE — délibérément,
// c'est le garde-fou de #1943 par lequel 21 277 pistes de Yacine étaient
// parties. Les deux comportements sont justes ; leur composition ne l'est pas :
// une racine retirée emmène ses pistes dans un angle mort permanent. Elles ne
// sont plus visitées, ne peuvent plus être purgées, et restent affichées avec
// des chemins morts (Rhorn, 0.9.75, bibliothèque migrée d'un NAS à un autre).
//
// ## Ce qui n'est PAS fait ici
//
// `verdict_purge` n'est pas touché. L'assouplir rouvrirait #1943 : une racine
// ABSENTE au scan et une racine RETIRÉE par l'utilisateur produisent le même
// état en base et ne veulent pas dire la même chose. Ce qui manquait n'est pas
// une permission de plus donnée au scan, c'est un SIGNAL utilisateur.
//
// ## Le garde-fou, et pourquoi il est structurel
//
// Un dossier peut être momentanément indisponible — montage réseau décroché,
// disque débranché. Ce cas ne doit JAMAIS coûter une piste. La protection ici
// n'est pas une heuristique de lisibilité mais une propriété de forme :
//
//   **cette route refuse toute cible qui est encore dans le périmètre.**
//
// Une racine momentanément illisible est TOUJOURS encore dans `music_dirs` —
// c'est ce qui la définit : personne ne l'a retirée. Ses pistes sont donc hors
// d'atteinte de cette route, quel que soit le corps de la requête, et sans que
// le disque soit consulté une seule fois. Le système de fichiers n'entre pas
// dans la décision : ni son état, ni sa lisibilité ne peuvent changer ce qui
// est supprimé. C'est le seul garde-fou qu'un montage qui décroche ne peut pas
// contourner.
//
// Symétriquement, une cible AU-DESSUS d'une racine vivante est refusée :
// purger `/mnt` alors que `/mnt/nas/Musique` est configuré emporterait des
// pistes vivantes.
//
// ## Et le plafond de #1943 ?
//
// Il s'applique, avec confirmation CHIFFRÉE (`confirm_purge=N`), par la
// fonction de production `purge_refusee` — la même que le scan. Une
// suppression explicitement demandée reste une suppression irréversible.

/// Pourquoi une purge explicite est refusée.
///
/// Chaque variante est un refus **structurel** : il se décide sur la liste des
/// racines configurées et le chemin demandé, avant toute lecture de la base et
/// sans jamais toucher au disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusPurge {
    /// Chemin vide : on ne purge pas « tout ».
    CibleVide,
    /// La cible EST une racine configurée, ou vit SOUS une racine configurée.
    ///
    /// C'est le garde-fou central. Un montage tombé laisse sa racine dans
    /// `music_dirs` : ses pistes tombent donc toujours ici, et rien ne part.
    DansLePerimetre,
    /// La cible est AU-DESSUS d'une racine configurée : la purger emporterait
    /// des pistes vivantes.
    ContientUneRacine,
}

impl RefusPurge {
    pub(crate) fn motif(self) -> &'static str {
        match self {
            RefusPurge::CibleVide => "cible_vide",
            RefusPurge::DansLePerimetre => "dans_le_perimetre",
            RefusPurge::ContientUneRacine => "contient_une_racine",
        }
    }

    pub(crate) fn message(self, cible: &str) -> String {
        match self {
            RefusPurge::CibleVide => {
                "Aucun dossier n'a été indiqué. Cette opération retire des pistes \
                 définitivement : elle exige un chemin précis."
                    .to_string()
            }
            RefusPurge::DansLePerimetre => format!(
                "{cible} fait encore partie des dossiers de musique. Rien n'a été retiré. \
                 Un dossier momentanément indisponible — partage réseau décroché, disque \
                 débranché — est exactement dans ce cas : il reste configuré, et ses pistes \
                 sont conservées. Retirez d'abord le dossier des réglages si vous voulez \
                 vraiment vous séparer de son contenu."
            ),
            RefusPurge::ContientUneRacine => format!(
                "{cible} contient un dossier de musique encore configuré. Rien n'a été \
                 retiré : la purge y emporterait des pistes vivantes. Visez le dossier \
                 retiré lui-même, pas un de ses parents."
            ),
        }
    }
}

/// La cible est-elle purgeable, au vu des seules racines configurées ?
///
/// `None` = purgeable. Aucune E/S : c'est ce qui rend la protection
/// insensible à l'état du disque.
pub(crate) fn refus_de_purge(cible: &str, racines: &[String]) -> Option<RefusPurge> {
    let cible = cible.trim_end_matches(['/', '\\']);
    if cible.is_empty() {
        return Some(RefusPurge::CibleVide);
    }
    for r in racines {
        let r = tune_core::scanner::walker::normalize_path(r);
        let r = r.trim_end_matches(['/', '\\']);
        if r.is_empty() {
            continue;
        }
        if super::scan::sous_le_dossier(cible, r) {
            return Some(RefusPurge::DansLePerimetre);
        }
        if super::scan::sous_le_dossier(r, cible) {
            return Some(RefusPurge::ContientUneRacine);
        }
    }
    None
}

/// Regrouper des pistes hors périmètre sous le dossier le plus HAUT qui ne
/// contient QUE des pistes hors périmètre.
///
/// Sans ce repli, l'écran listerait un dossier par album. Avec lui, Rhorn voit
/// « /Volumes/AncienNAS — 4 212 pistes », c'est-à-dire la chose qu'il a
/// effectivement débranchée.
///
/// Le repli s'arrête net dès qu'un dossier porte encore une piste vivante :
/// une cible remontée trop haut serait de toute façon refusée par
/// [`refus_de_purge`], mais mieux vaut ne jamais la proposer.
pub(crate) fn regrouper_hors_perimetre(
    hors_perimetre: &[&str],
    vivantes: &[&str],
) -> Vec<(String, usize)> {
    use std::collections::{HashMap, HashSet};

    // Tout ancêtre d'une piste vivante est un dossier vivant : on ne remonte
    // jamais au-delà.
    let mut vivants: HashSet<&str> = HashSet::new();
    for p in vivantes {
        let mut cur = *p;
        while let Some(parent) = super::scan::dossier_parent(cur) {
            cur = parent;
            if !vivants.insert(cur) {
                break; // déjà marqué : ses ancêtres le sont aussi.
            }
        }
    }

    let mut groupes: HashMap<&str, usize> = HashMap::new();
    for p in hors_perimetre {
        let mut plus_haut: Option<&str> = None;
        let mut cur = *p;
        while let Some(parent) = super::scan::dossier_parent(cur) {
            if vivants.contains(parent) {
                break;
            }
            plus_haut = Some(parent);
            cur = parent;
        }
        if let Some(d) = plus_haut {
            *groupes.entry(d).or_insert(0) += 1;
        }
    }

    let mut sortie: Vec<(String, usize)> = groupes
        .into_iter()
        .map(|(d, n)| (d.to_string(), n))
        .collect();
    sortie.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sortie
}

/// `(id, file_path)` de toutes les pistes LOCALES.
fn pistes_locales(state: &AppState) -> Vec<(i64, String)> {
    state
        .backend
        .query_many(
            "SELECT id, file_path FROM tracks WHERE source = 'local' AND file_path IS NOT NULL",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            let id = r.first()?.as_i64()?;
            let p = r.get(1)?.as_string()?;
            if p.is_empty() { None } else { Some((id, p)) }
        })
        .collect()
}

/// Pistes locales dont le chemin est sous `dossier`.
///
/// Le filtrage se fait en Rust, jamais en SQL : un `LIKE` sur un chemin
/// Windows fait de l'antislash un caractère d'échappement côté PostgreSQL, et
/// c'est déjà ce qui avait rendu des dossiers vides. `sous_le_dossier`
/// accepte les deux séparateurs.
fn pistes_sous(state: &AppState, dossier: &str) -> Vec<i64> {
    let d = dossier.trim_end_matches(['/', '\\']);
    if d.is_empty() {
        return Vec::new();
    }
    pistes_locales(state)
        .into_iter()
        .filter(|(_, p)| super::scan::sous_le_dossier(p, d))
        .map(|(id, _)| id)
        .collect()
}

/// Compter les lignes d'une table liée qui référencent ces pistes.
///
/// Les ids viennent de notre propre base et sont des `i64` : les interpoler
/// est sûr, et évite un placeholder par piste (SQLite plafonne à 999).
fn compter_liees(state: &AppState, sql_avant_in: &str, ids: &[i64]) -> i64 {
    let mut total = 0i64;
    for lot in ids.chunks(500) {
        let liste = lot
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("{sql_avant_in} ({liste})");
        if let Ok(Some(row)) = state.backend.query_one(&sql, &[]) {
            total += row.first().and_then(|v| v.as_i64()).unwrap_or(0);
        }
    }
    total
}

/// Ce que la purge emporterait, et ce qu'elle laisserait.
///
/// Rendu AVANT toute suppression, pour que l'écran puisse le montrer. Les
/// chiffres sont ceux des tables liées telles que le schéma les traite :
///
/// - `playlists` : `playlist_tracks` est en `ON DELETE CASCADE` et les clés
///   étrangères sont ACTIVES sous SQLite (`PRAGMA foreign_keys=ON`,
///   `db/sqlite.rs`). Les entrées disparaissent des listes de lecture ; les
///   listes elles-mêmes restent, éventuellement plus courtes. Aucune référence
///   pendante.
/// - `queue_items` : même cascade — les pistes quittent les files d'attente.
/// - `listen_history` : `ON DELETE SET NULL`. **L'historique n'est pas
///   effacé** : la ligne survit avec son titre et son artiste, et perd son
///   `track_id`. C'est aussi le filet de secours de la réconciliation des
///   favoris, qui y relit l'identité d'un item disparu.
/// - `favorites` : table POLYMORPHE (`item_type`/`item_id`), donc sans clé
///   étrangère — c'est là que naîtraient les références pendantes. On lance
///   `FavoritesReconciler::run(false)` après la purge : chaque favori orphelin
///   est re-rattaché par identité (chemin, puis titre+artiste) à la piste
///   vivante correspondante — le cas exact de Rhorn, dont la bibliothèque
///   existe toujours, sous un autre NAS. `false` = `delete_unresolved` :
///   **aucun favori n'est jamais supprimé par cette route.**
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ImpactPurge {
    pub tracks: i64,
    pub playlists: i64,
    pub playlist_entries: i64,
    pub favorites: i64,
    pub history_entries: i64,
    pub queue_entries: i64,
}

fn impact(state: &AppState, ids: &[i64]) -> ImpactPurge {
    if ids.is_empty() {
        return ImpactPurge::default();
    }
    ImpactPurge {
        tracks: ids.len() as i64,
        playlists: compter_liees(
            state,
            "SELECT COUNT(DISTINCT playlist_id) FROM playlist_tracks WHERE track_id IN",
            ids,
        ),
        playlist_entries: compter_liees(
            state,
            "SELECT COUNT(*) FROM playlist_tracks WHERE track_id IN",
            ids,
        ),
        favorites: compter_liees(
            state,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'track' AND item_id IN",
            ids,
        ),
        history_entries: compter_liees(
            state,
            "SELECT COUNT(*) FROM listen_history WHERE track_id IN",
            ids,
        ),
        queue_entries: compter_liees(
            state,
            "SELECT COUNT(*) FROM queue_items WHERE track_id IN",
            ids,
        ),
    }
}

fn impact_json(i: &ImpactPurge) -> Value {
    json!({
        "tracks": i.tracks,
        "playlists": i.playlists,
        "playlist_entries": i.playlist_entries,
        "favorites": i.favorites,
        "history_entries": i.history_entries,
        "queue_entries": i.queue_entries,
    })
}

/// `GET /system/music-dirs/orphans` — ce qui traîne hors du périmètre.
///
/// Lecture seule. C'est la moitié qui rattrape les dossiers **déjà** retirés :
/// proposer la purge au moment du retrait ne sert à rien à qui a retiré son
/// ancien NAS il y a trois versions.
pub(super) async fn orphan_tracks(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Json<Value> {
    let racines: Vec<String> = super::get_music_dirs_list(&state.backend)
        .iter()
        .map(|d| tune_core::scanner::walker::normalize_path(d))
        .collect();
    let toutes = pistes_locales(&state);

    // Une liste de racines VIDE ne veut pas dire « tout est orphelin » : elle
    // veut dire qu'on ne sait rien. Même prudence que `verdict_purge`.
    if racines.is_empty() {
        return Json(json!({
            "groups": [],
            "total": 0,
            "note": "Aucun dossier de musique n'est configuré : rien ne peut être déclaré \
                     hors périmètre.",
        }));
    }

    let dans_le_perimetre = |p: &str| racines.iter().any(|r| super::scan::sous_le_dossier(p, r));
    let hors_refs: Vec<&str> = toutes
        .iter()
        .filter(|(_, p)| !dans_le_perimetre(p))
        .map(|(_, p)| p.as_str())
        .collect();
    let vivantes_refs: Vec<&str> = toutes
        .iter()
        .filter(|(_, p)| dans_le_perimetre(p))
        .map(|(_, p)| p.as_str())
        .collect();

    let groupes = regrouper_hors_perimetre(&hors_refs, &vivantes_refs);
    let ids: Vec<i64> = toutes
        .iter()
        .filter(|(_, p)| !dans_le_perimetre(p))
        .map(|(id, _)| *id)
        .collect();

    Json(json!({
        "groups": groupes
            .iter()
            .map(|(d, n)| json!({ "path": d, "tracks": *n as i64 }))
            .collect::<Vec<_>>(),
        "total": ids.len() as i64,
        "impact": impact_json(&impact(&state, &ids)),
    }))
}

#[derive(Deserialize)]
pub(super) struct PurgeOrphans {
    path: String,
    /// Nombre de pistes que l'utilisateur accepte de perdre.
    ///
    /// Un NOMBRE, pas un booléen — même contrat que `?confirm_purge=N` sur
    /// `/scan` (#1943) : une confirmation périmée ne peut pas autoriser une
    /// purge plus large que celle qui a été montrée.
    #[serde(default)]
    confirm_purge: Option<u64>,
}

/// `POST /system/music-dirs/purge-orphans` — retirer les pistes d'un dossier
/// qui n'est plus dans le périmètre.
///
/// Sans `confirm_purge`, c'est un **essai à blanc** : rien n'est supprimé, on
/// rend le plan. C'est la forme que prend la promesse « retirer un dossier
/// devrait proposer de retirer aussi ce qu'il contenait » : l'écran retire le
/// dossier, appelle ceci pour obtenir les chiffres, les montre, et ne
/// rappelle avec `confirm_purge` que si l'utilisateur dit oui.
pub(super) async fn purge_orphan_tracks(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<PurgeOrphans>,
) -> Result<impl IntoResponse, AppError> {
    let cible = tune_core::scanner::walker::normalize_path(&body.path);
    let racines = super::get_music_dirs_list(&state.backend);

    if let Some(refus) = refus_de_purge(&cible, &racines) {
        tracing::warn!(
            cible = %cible,
            motif = refus.motif(),
            "purge_orphelines_refusee — aucune piste retirée."
        );
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "purged": 0,
                "refused": true,
                "reason": refus.motif(),
                "message": refus.message(&cible),
            })),
        )
            .into_response());
    }

    let ids = pistes_sous(&state, &cible);
    let plan = impact(&state, &ids);
    let total_local = pistes_locales(&state).len();

    if ids.is_empty() {
        return Ok(Json(json!({
            "purged": 0,
            "refused": false,
            "impact": impact_json(&plan),
            "message": format!("Aucune piste n'est enregistrée sous {cible}."),
        }))
        .into_response());
    }

    // Essai à blanc : pas de confirmation ⇒ pas de suppression.
    let Some(_) = body.confirm_purge else {
        return Ok(Json(json!({
            "purged": 0,
            "refused": false,
            "dry_run": true,
            "confirm_purge_required": plan.tracks,
            "impact": impact_json(&plan),
            "message": format!(
                "{} pistes seraient retirées définitivement. Rappelez cette route avec \
                 confirm_purge={} pour confirmer.",
                plan.tracks, plan.tracks
            ),
        }))
        .into_response());
    };

    // Le plafond de #1943 s'applique à une suppression explicite aussi — par
    // la fonction de PRODUCTION, pas par une copie.
    if super::scan::purge_refusee(ids.len(), total_local, body.confirm_purge) {
        tracing::error!(
            cible = %cible,
            candidats = ids.len(),
            total_local,
            confirmee = ?body.confirm_purge,
            "purge_orphelines_refusee_trop_massive — la confirmation ne couvre pas l'ampleur \
             constatée. Aucune piste retirée."
        );
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "purged": 0,
                "refused": true,
                "reason": "confirmation_insuffisante",
                "confirm_purge_required": plan.tracks,
                "impact": impact_json(&plan),
                "message": format!(
                    "La confirmation reçue ne couvre pas les {} pistes concernées. Rien n'a \
                     été retiré. Confirmez ce nombre exact pour poursuivre.",
                    plan.tracks
                ),
            })),
        )
            .into_response());
    }

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let mut purgees = 0i64;
    for id in &ids {
        if track_repo.delete(*id).is_ok() {
            purgees += 1;
        }
    }

    // Les albums et artistes devenus vides partent avec elles — sans quoi la
    // bibliothèque garde des albums à zéro piste, ce que #593 avait déjà
    // montré à l'écran.
    let albums_orphelins = AlbumRepo::with_backend(state.backend.clone())
        .delete_orphans()
        .unwrap_or(0);
    let artistes_orphelins = ArtistRepo::with_backend(state.backend.clone())
        .cleanup_orphans()
        .unwrap_or(0);

    // `favorites` n'a pas de clé étrangère : sans cette réconciliation, les
    // cœurs pointeraient des ids morts. `false` = ne JAMAIS supprimer un
    // favori qu'on n'a pas su re-rattacher.
    let favoris = tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(
        state.backend.clone(),
    )
    .run(false)
    .unwrap_or_default();

    tracing::warn!(
        cible = %cible,
        purgees,
        albums_orphelins,
        artistes_orphelins,
        favoris_rerattaches = favoris.relinked,
        favoris_non_resolus = favoris.unresolved,
        "purge_orphelines_effectuee — suppression explicitement confirmée par l'utilisateur."
    );

    Ok(Json(json!({
        "purged": purgees,
        "refused": false,
        "orphan_albums_removed": albums_orphelins,
        "orphan_artists_removed": artistes_orphelins,
        "favorites_relinked": favoris.relinked,
        "favorites_unresolved": favoris.unresolved,
        "impact": impact_json(&plan),
    }))
    .into_response())
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

/// Nom convivial de CETTE machine — la réponse à « à quel serveur je parle ? ».
///
/// Le réglage `server_name` prime ; à défaut, le nom d'hôte réel du système.
/// On passe par `tune_core::discovery::system_hostname()`, et non par le
/// `hostname` du sous-processus qu'utilise `server_urls` ci-dessous : ce
/// dernier rend une chaîne vide quand le binaire manque (conteneurs minimaux),
/// alors que `system_hostname()` interroge `gethostname(2)` et ne rend jamais
/// vide. Le défaut doit exister partout, sinon l'étiquette disparaît là où
/// elle sert le plus. C'est aussi la dérivation qui a réparé #1127, où la
/// version « variables d'environnement seules » retombait sur `tune-server`
/// sous systemd et faisait porter le même nom à tous les serveurs du réseau.
///
/// Jamais l'`instance_id` : c'est un UUID de 36 caractères, à usage cloud, créé
/// dix secondes après le démarrage par la tâche de heartbeat — illisible, et
/// absent pendant les premières secondes de vie du serveur.
pub(crate) fn resolve_server_name(configured: Option<&str>) -> String {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => name.to_string(),
        None => tune_core::discovery::system_hostname(),
    }
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

// ───────────────────────────────────────────────────────────────────────────
// #2149 — retirer un dossier des réglages laisse ses pistes en base
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod purge_hors_perimetre_tests {
    use super::{AddMusicDir, PurgeOrphans, RefusPurge, refus_de_purge, regrouper_hors_perimetre};
    use crate::auth::RequireAdmin;
    use crate::state::AppState;
    use axum::Json;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use tune_core::db::backend::ToSqlValue;
    use tune_core::db::models::Track;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::track_repo::TrackRepo;

    /// Les chemins de test sont écrits en `/` et passés par `normalize_path`,
    /// qui les retourne en antislashs sous Windows. Sans quoi ces tests
    /// seraient verts sur Mac et rouges chez Rhorn.
    fn n(p: &str) -> String {
        tune_core::scanner::walker::normalize_path(p)
    }

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    fn racines(state: &AppState, dirs: &[&str]) {
        let v: Vec<String> = dirs.iter().map(|d| n(d)).collect();
        SettingsRepo::with_backend(state.backend.clone())
            .set("music_dirs", &serde_json::to_string(&v).unwrap())
            .unwrap();
    }

    fn piste(state: &AppState, chemin: &str) -> i64 {
        let repo = TrackRepo::with_backend(state.backend.clone());
        let mut t = Track::new(format!("piste {chemin}"));
        t.file_path = Some(n(chemin));
        repo.create(&t).unwrap()
    }

    fn compte(state: &AppState) -> i64 {
        state
            .backend
            .query_one("SELECT COUNT(*) FROM tracks", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap()
    }

    fn existe(state: &AppState, id: i64) -> bool {
        state
            .backend
            .query_one(
                "SELECT COUNT(*) FROM tracks WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
            > 0
    }

    async fn retirer(state: &AppState, chemin: &str) -> serde_json::Value {
        super::remove_music_dir(
            RequireAdmin,
            State(state.clone()),
            Json(AddMusicDir {
                path: chemin.to_string(),
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("remove_music_dir a échoué"))
        .0
    }

    async fn purger(
        state: &AppState,
        chemin: &str,
        confirm: Option<u64>,
    ) -> (u16, serde_json::Value) {
        let r = super::purge_orphan_tracks(
            RequireAdmin,
            State(state.clone()),
            Json(PurgeOrphans {
                path: chemin.to_string(),
                confirm_purge: confirm,
            }),
        )
        .await
        .unwrap_or_else(|_| panic!("purge_orphan_tracks a échoué"))
        .into_response();
        let code = r.status().as_u16();
        let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (code, serde_json::from_slice(&corps).unwrap())
    }

    // ── Le défaut de Rhorn ──────────────────────────────────────────────

    /// Le cœur de #2149 : un dossier retiré, ses pistes s'en vont — mais
    /// seulement sur demande explicite et chiffrée.
    #[tokio::test]
    async fn un_dossier_retire_puis_purge_perd_ses_pistes() {
        let state = etat();
        racines(&state, &["/nas1/Musique", "/nas2/Musique"]);
        let vieille = piste(&state, "/nas1/Musique/Bach/01.flac");
        let neuve = piste(&state, "/nas2/Musique/Bach/01.flac");

        let rep = retirer(&state, "/nas1/Musique").await;
        assert_eq!(
            rep["orphan_tracks"].as_i64(),
            Some(1),
            "le retrait doit DIRE ce qu'il laisse derrière lui : {rep}"
        );
        assert!(existe(&state, vieille), "le retrait seul ne supprime rien");

        let (code, rep) = purger(&state, "/nas1/Musique", Some(1)).await;
        assert_eq!(code, 200, "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(1), "{rep}");
        assert!(!existe(&state, vieille), "la piste du dossier retiré reste");
        assert!(
            existe(&state, neuve),
            "la piste du dossier VIVANT est partie"
        );
    }

    /// Sans confirmation, c'est un essai à blanc : les chiffres, rien d'autre.
    /// C'est ce qui permet à l'écran de « proposer de retirer aussi ce qu'il
    /// contenait » avant d'agir.
    #[tokio::test]
    async fn sans_confirmation_rien_ne_part() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        for i in 0..3 {
            piste(&state, &format!("/vieux_nas/Musique/{i}.flac"));
        }
        piste(&state, "/nas1/Musique/vivante.flac");

        let (code, rep) = purger(&state, "/vieux_nas/Musique", None).await;
        assert_eq!(code, 200, "{rep}");
        assert_eq!(rep["dry_run"].as_bool(), Some(true), "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(0), "{rep}");
        assert_eq!(rep["confirm_purge_required"].as_i64(), Some(3), "{rep}");
        assert_eq!(compte(&state), 4, "un essai à blanc a supprimé des pistes");
    }

    // ── Le danger : ne pas transformer un oubli en perte de données ─────

    /// **La preuve du garde-fou.** Un dossier momentanément illisible — NAS
    /// décroché, disque débranché — reste dans `music_dirs` : personne ne l'a
    /// retiré. Il est donc encore dans le périmètre, et AUCUNE requête, quel
    /// que soit son contenu, ne peut lui prendre une piste.
    ///
    /// Le disque n'est jamais consulté : le chemin de test n'existe même pas
    /// sur la machine qui exécute ce test, et c'est le point — ni l'état ni la
    /// lisibilité du support n'entrent dans la décision.
    #[tokio::test]
    async fn un_dossier_momentanement_illisible_ne_perd_aucune_piste() {
        let state = etat();
        // Le montage est tombé, mais la racine est TOUJOURS configurée.
        racines(&state, &["/mnt/nas_decroche/Musique"]);
        let ids: Vec<i64> = (0..5)
            .map(|i| piste(&state, &format!("/mnt/nas_decroche/Musique/{i}.flac")))
            .collect();
        assert!(
            !std::path::Path::new(&n("/mnt/nas_decroche/Musique")).exists(),
            "le dossier de test doit être absent du disque, c'est tout l'objet"
        );

        // Même en confirmant le nombre exact, et même en visant un parent.
        for (cible, confirm) in [
            ("/mnt/nas_decroche/Musique", Some(5)),
            ("/mnt/nas_decroche/Musique", Some(9999)),
            ("/mnt/nas_decroche/Musique/Bach", Some(5)),
            ("/mnt/nas_decroche", Some(5)),
            ("/mnt", Some(5)),
        ] {
            let (code, rep) = purger(&state, cible, confirm).await;
            assert_eq!(code, 409, "cible {cible} n'a pas été refusée : {rep}");
            assert_eq!(rep["purged"].as_i64(), Some(0), "{rep}");
            assert_eq!(rep["refused"].as_bool(), Some(true), "{rep}");
        }
        assert_eq!(compte(&state), 5, "une piste a été perdue");
        for id in ids {
            assert!(existe(&state, id), "la piste {id} a disparu");
        }
    }

    /// Le refus est NOMMÉ, et la phrase dit ce qui s'est passé — un montage
    /// tombé ne doit pas laisser l'utilisateur devant un échec muet.
    #[test]
    fn le_refus_est_nomme_et_dit_pourquoi() {
        let r = refus_de_purge(&n("/mnt/nas/Musique"), &[n("/mnt/nas/Musique")]).unwrap();
        assert_eq!(r, RefusPurge::DansLePerimetre);
        let m = r.message(&n("/mnt/nas/Musique")).to_lowercase();
        assert!(
            m.contains("réglages") || m.contains("dossiers de musique"),
            "{m}"
        );
        assert!(m.contains("indisponible") || m.contains("décroché"), "{m}");

        assert_eq!(
            refus_de_purge(&n("/mnt"), &[n("/mnt/nas/Musique")]),
            Some(RefusPurge::ContientUneRacine),
            "purger un parent d'une racine vivante doit être refusé"
        );
        assert_eq!(
            refus_de_purge("", &[n("/mnt/nas")]),
            Some(RefusPurge::CibleVide)
        );
        assert_eq!(
            refus_de_purge(&n("/vieux_nas"), &[n("/mnt/nas/Musique")]),
            None,
            "un dossier hors périmètre doit être purgeable"
        );
    }

    /// Un préfixe de chaîne n'est pas un dossier : `/nas/Musique2` n'est pas
    /// sous `/nas/Musique`. Le garde-fou passerait à côté sinon.
    #[test]
    fn un_prefixe_de_nom_n_est_pas_un_sous_dossier() {
        assert_eq!(
            refus_de_purge(&n("/nas/Musique2"), &[n("/nas/Musique")]),
            None
        );
        assert_eq!(
            refus_de_purge(&n("/nas/Musique/Jazz"), &[n("/nas/Musique")]),
            Some(RefusPurge::DansLePerimetre)
        );
    }

    /// Le plafond de #1943 s'applique aussi à une suppression explicite : une
    /// confirmation qui ne couvre pas l'ampleur constatée ne suffit pas.
    #[tokio::test]
    async fn le_plafond_1943_s_applique_a_la_suppression_explicite() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        for i in 0..60 {
            piste(&state, &format!("/nas1/Musique/{i}.flac"));
        }
        for i in 0..40 {
            piste(&state, &format!("/vieux_nas/{i}.flac"));
        }

        // 40 sur 100 = 40 % > 20 % : confirmation trop courte ⇒ refus.
        let (code, rep) = purger(&state, "/vieux_nas", Some(10)).await;
        assert_eq!(code, 409, "{rep}");
        assert_eq!(rep["reason"].as_str(), Some("confirmation_insuffisante"));
        assert_eq!(rep["confirm_purge_required"].as_i64(), Some(40), "{rep}");
        assert_eq!(compte(&state), 100, "des pistes sont parties sur un refus");

        // Le nombre exact lève le plafond.
        let (code, rep) = purger(&state, "/vieux_nas", Some(40)).await;
        assert_eq!(code, 200, "{rep}");
        assert_eq!(rep["purged"].as_i64(), Some(40), "{rep}");
        assert_eq!(compte(&state), 60);
    }

    /// Aucune racine configurée = on ne sait rien, pas « tout est orphelin ».
    /// Même prudence que `verdict_purge`.
    #[tokio::test]
    async fn sans_aucune_racine_configuree_rien_n_est_declare_orphelin() {
        let state = etat();
        racines(&state, &[]);
        piste(&state, "/nas1/Musique/a.flac");

        let r = super::orphan_tracks(RequireAdmin, State(state.clone()))
            .await
            .0;
        assert_eq!(r["total"].as_i64(), Some(0), "{r}");
        assert_eq!(r["groups"].as_array().map(Vec::len), Some(0), "{r}");
    }

    // ── Les objets liés ─────────────────────────────────────────────────

    /// Une piste dans une liste de lecture : comportement EXPLICITE. L'entrée
    /// quitte la liste (cascade), la liste survit, les autres entrées restent,
    /// et l'impact est annoncé AVANT la suppression.
    #[tokio::test]
    async fn une_piste_en_liste_de_lecture_quitte_la_liste_sans_la_detruire() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        let vieille = piste(&state, "/vieux_nas/Bach/01.flac");
        let gardee = piste(&state, "/nas1/Musique/Bach/02.flac");
        state
            .backend
            .execute(
                "INSERT INTO playlists (id, name) VALUES (1, 'Ma liste')",
                &[],
            )
            .unwrap();
        for (pos, t) in [(0i64, vieille), (1, gardee)] {
            state
                .backend
                .execute(
                    "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (1, ?, ?)",
                    &[&t as &dyn ToSqlValue, &pos as &dyn ToSqlValue],
                )
                .unwrap();
        }

        // L'essai à blanc annonce l'impact avant d'agir.
        let (_, plan) = purger(&state, "/vieux_nas", None).await;
        assert_eq!(plan["impact"]["playlists"].as_i64(), Some(1), "{plan}");
        assert_eq!(
            plan["impact"]["playlist_entries"].as_i64(),
            Some(1),
            "{plan}"
        );

        let (code, rep) = purger(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(code, 200, "{rep}");

        let restant: Vec<i64> = state
            .backend
            .query_many(
                "SELECT track_id FROM playlist_tracks WHERE playlist_id = 1",
                &[],
            )
            .unwrap()
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.as_i64()))
            .collect();
        assert_eq!(
            restant,
            vec![gardee],
            "l'entrée de la piste retirée doit partir, l'autre rester — aucune \
             référence pendante"
        );
        let listes: i64 = state
            .backend
            .query_one("SELECT COUNT(*) FROM playlists", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap();
        assert_eq!(listes, 1, "la liste de lecture elle-même a été détruite");
    }

    /// L'historique d'écoute SURVIT : `ON DELETE SET NULL`. Une piste retirée
    /// ne réécrit pas le passé de l'utilisateur.
    #[tokio::test]
    async fn l_historique_d_ecoute_survit_a_la_purge() {
        let state = etat();
        racines(&state, &["/nas1/Musique"]);
        let vieille = piste(&state, "/vieux_nas/Bach/01.flac");
        piste(&state, "/nas1/Musique/vivante.flac");
        state
            .backend
            .execute(
                "INSERT INTO listen_history (track_id, title, artist_name, listened_at) \
                 VALUES (?, 'Toccata', 'Bach', '2026-08-01T10:00:00Z')",
                &[&vieille as &dyn ToSqlValue],
            )
            .unwrap();

        let (code, rep) = purger(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(code, 200, "{rep}");

        let rows = state
            .backend
            .query_many("SELECT track_id, title FROM listen_history", &[])
            .unwrap();
        assert_eq!(rows.len(), 1, "la ligne d'historique a été effacée");
        assert!(
            rows[0].first().and_then(|v| v.as_i64()).is_none(),
            "track_id devait passer à NULL"
        );
        assert_eq!(
            rows[0].get(1).and_then(|v| v.as_string()).as_deref(),
            Some("Toccata"),
            "le titre écouté doit rester lisible"
        );
    }

    /// Un favori de piste n'est JAMAIS supprimé par cette route. Chez Rhorn,
    /// la même musique existe sous le nouveau NAS : le favori s'y re-rattache.
    #[tokio::test]
    async fn un_favori_est_rerattache_jamais_supprime() {
        let state = etat();
        racines(&state, &["/nas2/Musique"]);
        state
            .backend
            .execute("INSERT INTO artists (name) VALUES ('Bach')", &[])
            .unwrap();
        let bach: i64 = state
            .backend
            .query_one("SELECT id FROM artists WHERE name = 'Bach'", &[])
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap();
        let repo = TrackRepo::with_backend(state.backend.clone());
        let toccata = |chemin: &str| {
            let mut t = Track::new("Toccata".into());
            t.file_path = Some(n(chemin));
            t.artist_id = Some(bach);
            t.artist_name = Some("Bach".into());
            repo.create(&t).unwrap()
        };
        // La même musique, sous l'ancien NAS et sous le nouveau — le cas exact
        // de Rhorn, qui a migré sa bibliothèque d'un support à l'autre.
        let ancienne = toccata("/vieux_nas/Bach/Toccata.flac");
        let nouvelle = toccata("/nas2/Musique/Bach/Toccata.flac");

        // Le favori pointe l'ANCIENNE piste, avec son instantané d'identité.
        state
            .backend
            .execute(
                "INSERT INTO favorites (profile_id, item_type, item_id, item_name, item_artist, \
                 item_path) VALUES (1, 'track', ?, 'Toccata', 'Bach', ?)",
                &[
                    &ancienne as &dyn ToSqlValue,
                    &n("/vieux_nas/Bach/Toccata.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();

        let (code, rep) = purger(&state, "/vieux_nas", Some(1)).await;
        assert_eq!(code, 200, "{rep}");

        let favs = state
            .backend
            .query_many(
                "SELECT item_id FROM favorites WHERE item_type = 'track'",
                &[],
            )
            .unwrap();
        assert_eq!(favs.len(), 1, "le favori a été SUPPRIMÉ : {rep}");
        assert_eq!(
            favs[0].first().and_then(|v| v.as_i64()),
            Some(nouvelle),
            "le favori devait être re-rattaché à la piste vivante"
        );
    }

    // ── Le regroupement montré à l'écran ────────────────────────────────

    /// Rhorn doit lire « /vieux_nas — 3 pistes », pas trois lignes d'albums.
    /// Et le repli s'arrête sous un dossier qui porte encore du vivant.
    #[test]
    fn les_orphelines_sont_regroupees_sous_le_plus_haut_dossier_mort() {
        let hors = [
            n("/vieux_nas/Bach/01.flac"),
            n("/vieux_nas/Bach/02.flac"),
            n("/vieux_nas/Mozart/01.flac"),
        ];
        let hors_refs: Vec<&str> = hors.iter().map(|s| s.as_str()).collect();
        let g = regrouper_hors_perimetre(&hors_refs, &[]);
        assert_eq!(g, vec![(n("/vieux_nas"), 3)], "{g:?}");

        // Une piste vivante voisine empêche de remonter jusqu'à `/data`.
        let vivante = n("/data/actuel/a.flac");
        let mortes = [n("/data/ancien/01.flac"), n("/data/ancien/02.flac")];
        let mortes_refs: Vec<&str> = mortes.iter().map(|s| s.as_str()).collect();
        let g = regrouper_hors_perimetre(&mortes_refs, &[vivante.as_str()]);
        assert_eq!(g, vec![(n("/data/ancien"), 2)], "{g:?}");
    }

    /// La route de listage rend les groupes et l'impact — c'est ce qui permet
    /// de rattraper un dossier retiré il y a trois versions.
    #[tokio::test]
    async fn la_route_de_listage_montre_les_dossiers_deja_retires() {
        let state = etat();
        racines(&state, &["/nas2/Musique"]);
        piste(&state, "/nas2/Musique/vivante.flac");
        for i in 0..4 {
            piste(&state, &format!("/vieux_nas/Bach/{i}.flac"));
        }

        let r = super::orphan_tracks(RequireAdmin, State(state.clone()))
            .await
            .0;
        assert_eq!(r["total"].as_i64(), Some(4), "{r}");
        let g = r["groups"].as_array().unwrap();
        assert_eq!(g.len(), 1, "{r}");
        assert_eq!(g[0]["path"].as_str(), Some(n("/vieux_nas").as_str()), "{r}");
        assert_eq!(g[0]["tracks"].as_i64(), Some(4), "{r}");
    }
}
