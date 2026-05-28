use std::collections::HashMap;
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
    pub dlna_play_delay_ms: u64,
    #[serde(default)]
    pub device_delays: HashMap<String, u64>,
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
        }
    }
}

impl TuneConfig {
    pub fn load() -> Self {
        let mut config = Self::default();

        let mut search_paths = vec![
            "tune.toml".to_string(),
            "/etc/tune/tune.toml".to_string(),
        ];
        #[cfg(target_os = "windows")]
        if let Ok(appdata) = std::env::var("APPDATA") {
            search_paths.insert(0, format!("{appdata}\\Tune\\tune.toml"));
        }
        #[cfg(target_os = "macos")]
        if let Ok(home) = std::env::var("HOME") {
            search_paths.push(format!("{home}/.config/tune/tune.toml"));
        }

        for path in &search_paths {
            if let Ok(content) = std::fs::read_to_string(path)
                && let Ok(file_config) = toml::from_str::<TuneConfig>(&content) {
                    info!(path, "config_loaded");
                    config = file_config;
                    break;
                }
        }

        if let Ok(v) = std::env::var("TUNE_PORT")
            && let Ok(p) = v.parse() { config.port = p; }
        if let Ok(v) = std::env::var("TUNE_DB_PATH") { config.db_path = v; }
        if let Ok(v) = std::env::var("TUNE_WEB_DIR") { config.web_dir = v; }
        if let Ok(v) = std::env::var("TUNE_ARTWORK_DIR") { config.artwork_dir = v; }
        if let Ok(v) = std::env::var("TUNE_AUTO_SCAN") { config.auto_scan = v == "true"; }
        if let Ok(v) = std::env::var("QOBUZ_APP_ID") && !v.is_empty() { config.qobuz_app_id = v; }
        if let Ok(v) = std::env::var("QOBUZ_APP_SECRET") && !v.is_empty() { config.qobuz_app_secret = v; }
        if let Ok(v) = std::env::var("TUNE_LOG_LEVEL") { config.log_level = v; }
        if let Ok(v) = std::env::var("TUNE_MUSIC_DIRS") {
            let trimmed = v.trim();
            if trimmed.starts_with('[') {
                // JSON array format: ["/path1", "/path2"] (compatible with v1 Python config)
                if let Ok(parsed) = serde_json::from_str::<Vec<String>>(trimmed) {
                    config.music_dirs = parsed;
                } else {
                    config.music_dirs = trimmed.split(',').map(|s| s.trim().trim_matches(|c| c == '[' || c == ']' || c == '"').to_string()).filter(|s| !s.is_empty()).collect();
                }
            } else {
                config.music_dirs = trimmed.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            }
        }

        config
    }
}
