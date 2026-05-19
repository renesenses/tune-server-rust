use serde::Deserialize;
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
}

impl Default for TuneConfig {
    fn default() -> Self {
        Self {
            port: 8085,
            db_path: "tune.db".into(),
            web_dir: "web".into(),
            artwork_dir: "artwork_cache".into(),
            music_dirs: vec![],
            auto_scan: false,
            qobuz_app_id: String::new(),
            qobuz_app_secret: String::new(),
            log_level: "info".into(),
        }
    }
}

impl TuneConfig {
    pub fn load() -> Self {
        let mut config = Self::default();

        for path in &["tune.toml", "/etc/tune/tune.toml"] {
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(file_config) = toml::from_str::<TuneConfig>(&content) {
                    info!(path, "config_loaded");
                    config = file_config;
                    break;
                }
            }
        }

        if let Ok(v) = std::env::var("TUNE_PORT") {
            if let Ok(p) = v.parse() { config.port = p; }
        }
        if let Ok(v) = std::env::var("TUNE_DB_PATH") { config.db_path = v; }
        if let Ok(v) = std::env::var("TUNE_WEB_DIR") { config.web_dir = v; }
        if let Ok(v) = std::env::var("TUNE_ARTWORK_DIR") { config.artwork_dir = v; }
        if let Ok(v) = std::env::var("TUNE_AUTO_SCAN") { config.auto_scan = v == "true"; }
        if let Ok(v) = std::env::var("QOBUZ_APP_ID") { if !v.is_empty() { config.qobuz_app_id = v; } }
        if let Ok(v) = std::env::var("QOBUZ_APP_SECRET") { if !v.is_empty() { config.qobuz_app_secret = v; } }
        if let Ok(v) = std::env::var("TUNE_LOG_LEVEL") { config.log_level = v; }
        if let Ok(v) = std::env::var("TUNE_MUSIC_DIRS") {
            config.music_dirs = v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
        }

        config
    }
}
