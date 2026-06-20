use serde::Deserialize;
use std::collections::HashMap;
use tracing::info;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TuneConfig {
    pub port: u16,
    pub db_path: String,
    pub web_dir: String,
    pub artwork_dir: String,
    pub music_dirs: Vec<String>,
    pub auto_scan: bool,
    pub qobuz_app_id: String,
    pub qobuz_app_secret: String,
    pub log_level: String,
    pub dlna_play_delay_ms: u64,
    #[serde(default)]
    pub device_delays: HashMap<String, u64>,
    #[serde(default)]
    pub spotify_client_id: Option<String>,
    #[serde(default)]
    pub spotify_redirect_uri: Option<String>,
    #[serde(default)]
    pub discogs_token: Option<String>,
    #[serde(default)]
    pub acoustid_api_key: Option<String>,
    #[serde(default)]
    pub openai_api_key: Option<String>,
    #[serde(default)]
    pub advertised_ip: Option<String>,
    /// PostgreSQL connection string. When set (or via `TUNE_DATABASE_URL`
    /// env), the server boots with PostgreSQL instead of SQLite.
    /// Format: `postgres://user:pass@host:5432/dbname`
    #[serde(default)]
    pub database_url: Option<String>,
    /// Audio host backend on Windows: "auto", "wasapi", or "asio".
    #[serde(default = "default_audio_backend")]
    pub local_audio_backend: String,
    /// When true, use exclusive/bit-perfect audio mode (CoreAudio hog mode
    /// on macOS, ASIO exclusive on Windows).
    #[serde(default)]
    pub local_exclusive_mode: bool,
    /// Tidal audio quality: "HI_RES_LOSSLESS", "HI_RES", "LOSSLESS", or "HIGH".
    /// Defaults to "HI_RES_LOSSLESS" (FLAC 24-bit up to 192kHz).
    #[serde(default = "default_tidal_quality")]
    pub tidal_quality: String,
}

fn default_audio_backend() -> String {
    "auto".into()
}

fn default_tidal_quality() -> String {
    "HI_RES_LOSSLESS".into()
}

impl TuneConfig {
    pub fn play_delay_for(&self, device_name: &str) -> u64 {
        self.device_delays
            .iter()
            .find(|(pattern, _)| device_name.to_lowercase().contains(&pattern.to_lowercase()))
            .map(|(_, delay)| *delay)
            .unwrap_or(self.dlna_play_delay_ms)
    }
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self {
            port: 8888,
            db_path: "tune.db".into(),
            web_dir: "web".into(),
            artwork_dir: "artwork_cache".into(),
            music_dirs: vec![],
            auto_scan: false,
            qobuz_app_id: String::new(),
            qobuz_app_secret: String::new(),
            log_level: "info".into(),
            dlna_play_delay_ms: 0,
            device_delays: HashMap::new(),
            spotify_client_id: None,
            spotify_redirect_uri: None,
            discogs_token: None,
            acoustid_api_key: None,
            openai_api_key: None,
            advertised_ip: None,
            database_url: None,
            local_audio_backend: "auto".into(),
            local_exclusive_mode: false,
            tidal_quality: "HI_RES_LOSSLESS".into(),
        }
    }
}

impl TuneConfig {
    pub fn server_ip(&self) -> String {
        if let Some(ref ip) = self.advertised_ip {
            return ip.clone();
        }
        tune_core::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into())
    }
}

impl TuneConfig {
    pub fn load() -> Self {
        let mut config = Self::default();

        let search_paths = {
            let mut paths = vec!["tune.toml".to_string(), "/etc/tune/tune.toml".to_string()];
            #[cfg(target_os = "windows")]
            if let Ok(appdata) = std::env::var("APPDATA") {
                paths.insert(0, format!("{appdata}\\Tune\\tune.toml"));
            }
            #[cfg(target_os = "macos")]
            if let Ok(home) = std::env::var("HOME") {
                paths.push(format!("{home}/.config/tune/tune.toml"));
            }
            paths
        };

        for path in &search_paths {
            if let Ok(content) = std::fs::read_to_string(path)
                && let Ok(file_config) = toml::from_str::<TuneConfig>(&content)
            {
                info!(path, "config_loaded");
                config = file_config;
                break;
            }
        }

        if let Ok(v) = std::env::var("TUNE_PORT")
            && let Ok(p) = v.parse()
        {
            config.port = p;
        }
        if let Ok(v) = std::env::var("TUNE_DB_PATH") {
            config.db_path = v;
        }

        // On Windows, resolve relative db_path to a writable location
        // (Program Files is read-only for standard users)
        #[cfg(target_os = "windows")]
        if !std::path::Path::new(&config.db_path).is_absolute() {
            let data_dir = std::env::var("LOCALAPPDATA")
                .map(|d| format!("{d}\\TuneServer"))
                .unwrap_or_else(|_| "TuneServer".into());
            std::fs::create_dir_all(&data_dir).ok();
            config.db_path = format!("{data_dir}\\{}", config.db_path);
            config.artwork_dir = format!("{data_dir}\\{}", config.artwork_dir);
        }

        // On macOS, resolve a relative db_path to an absolute location so Tune
        // works correctly regardless of the working directory (e.g. LaunchAgent
        // starts with CWD = "/").  Backward-compat: if tune.db already exists
        // in the CWD, keep using it so existing installs are not affected.
        #[cfg(target_os = "macos")]
        if !std::path::Path::new(&config.db_path).is_absolute() {
            let cwd_db = std::path::Path::new(&config.db_path);
            if !cwd_db.exists() {
                // Resolve to ~/Library/Application Support/Tune/tune.db
                if let Ok(home) = std::env::var("HOME") {
                    let app_support =
                        std::path::PathBuf::from(&home).join("Library/Application Support/Tune");
                    if std::fs::create_dir_all(&app_support).is_ok() {
                        let abs_path = app_support.join(&config.db_path);
                        info!(
                            path = %abs_path.display(),
                            "db_path_resolved_to_app_support"
                        );
                        config.db_path = abs_path.to_string_lossy().into_owned();
                    }
                }
            } else {
                info!(path = %cwd_db.display(), "db_path_using_existing_local_db");
            }
        }

        if let Ok(v) = std::env::var("TUNE_WEB_DIR") {
            config.web_dir = v;
        }
        if let Ok(v) = std::env::var("TUNE_ARTWORK_DIR") {
            config.artwork_dir = v;
        }
        if let Ok(v) = std::env::var("TUNE_AUTO_SCAN") {
            config.auto_scan = v == "true";
        }
        if let Ok(v) = std::env::var("QOBUZ_APP_ID")
            && !v.is_empty()
        {
            config.qobuz_app_id = v;
        }
        if let Ok(v) = std::env::var("QOBUZ_APP_SECRET")
            && !v.is_empty()
        {
            config.qobuz_app_secret = v;
        }
        if let Ok(v) = std::env::var("TUNE_LOG_LEVEL").or_else(|_| std::env::var("TUNE_LOG")) {
            config.log_level = v;
        }
        if let Ok(v) = std::env::var("TUNE_SPOTIFY_CLIENT_ID")
            && !v.is_empty()
        {
            config.spotify_client_id = Some(v);
        }
        if let Ok(v) = std::env::var("TUNE_SPOTIFY_REDIRECT_URI")
            && !v.is_empty()
        {
            config.spotify_redirect_uri = Some(v);
        }
        if let Ok(v) = std::env::var("TUNE_DISCOGS_TOKEN")
            && !v.is_empty()
        {
            config.discogs_token = Some(v);
        }
        if let Ok(v) = std::env::var("TUNE_ACOUSTID_API_KEY")
            && !v.is_empty()
        {
            config.acoustid_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("TUNE_OPENAI_API_KEY")
            && !v.is_empty()
        {
            config.openai_api_key = Some(v);
        }
        if let Ok(v) = std::env::var("TUNE_ADVERTISED_IP")
            && !v.is_empty()
        {
            config.advertised_ip = Some(v);
        }
        if let Ok(v) = std::env::var("TUNE_DATABASE_URL")
            && !v.is_empty()
        {
            config.database_url = Some(v);
        }
        // Also accept TUNE_DB_URL as a shorter alias.
        if config.database_url.is_none() {
            if let Ok(v) = std::env::var("TUNE_DB_URL")
                && !v.is_empty()
            {
                config.database_url = Some(v);
            }
        }
        // TUNE_DB_ENGINE=postgres constructs the DSN from individual env vars.
        if config.database_url.is_none() {
            if let Ok(engine) = std::env::var("TUNE_DB_ENGINE") {
                if engine.eq_ignore_ascii_case("postgres")
                    || engine.eq_ignore_ascii_case("postgresql")
                {
                    let host = std::env::var("TUNE_DB_HOST").unwrap_or_else(|_| "localhost".into());
                    let port = std::env::var("TUNE_DB_PORT").unwrap_or_else(|_| "5432".into());
                    let name = std::env::var("TUNE_DB_NAME").unwrap_or_else(|_| "tune".into());
                    let user = std::env::var("TUNE_DB_USER").unwrap_or_else(|_| "tune".into());
                    let pass = std::env::var("TUNE_DB_PASS").unwrap_or_default();
                    let url = if pass.is_empty() {
                        format!("postgresql://{user}@{host}:{port}/{name}")
                    } else {
                        format!("postgresql://{user}:{pass}@{host}:{port}/{name}")
                    };
                    config.database_url = Some(url);
                }
            }
        }
        if let Ok(v) = std::env::var("TUNE_LOCAL_AUDIO_BACKEND")
            .or_else(|_| std::env::var("TUNE_AUDIO_BACKEND"))
            && !v.is_empty()
        {
            config.local_audio_backend = v;
        }
        if let Ok(v) = std::env::var("TUNE_LOCAL_EXCLUSIVE_MODE") {
            config.local_exclusive_mode = matches!(v.to_lowercase().as_str(), "true" | "1" | "yes");
        }
        // If ASIO backend is explicitly requested, enable exclusive mode
        // automatically (ASIO is inherently exclusive).
        if config.local_audio_backend.to_lowercase() == "asio" && !config.local_exclusive_mode {
            config.local_exclusive_mode = true;
        }
        if let Ok(v) = std::env::var("TUNE_TIDAL_QUALITY")
            && !v.is_empty()
        {
            config.tidal_quality = v;
        }
        if let Ok(v) = std::env::var("TUNE_MUSIC_DIRS") {
            let trimmed = v.trim();
            if trimmed.starts_with('[') {
                // JSON array format: ["/path1", "/path2"] (compatible with v1 Python config)
                if let Ok(parsed) = serde_json::from_str::<Vec<String>>(trimmed) {
                    config.music_dirs = parsed;
                } else {
                    config.music_dirs = trimmed
                        .split(',')
                        .map(|s| {
                            s.trim()
                                .trim_matches(|c| c == '[' || c == ']' || c == '"')
                                .to_string()
                        })
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            } else {
                config.music_dirs = trimmed
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }

        config
    }
}
