use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneConfig {
    // Library
    pub music_dirs: Vec<String>,
    pub scan_on_startup: bool,
    pub scan_schedule: Option<String>,
    pub quality_split: bool,
    pub watch_filesystem: bool,
    pub watcher_debounce_seconds: f64,

    // Database
    pub db_path: String,

    // Security
    pub cors_origins: Vec<String>,
    pub api_key: Option<String>,

    // Web UI
    pub web_dir: Option<String>,

    // Server
    pub api_host: String,
    pub api_port: u16,
    pub stream_host: String,
    pub stream_port: u16,
    pub advertise_ip: Option<String>,
    pub default_zone_id: Option<i64>,

    // WebSocket
    pub ws_heartbeat_interval: u32,

    // Playback
    pub stream_url_resolve_timeout: u32,
    pub pipeline_start_timeout: u32,

    // Multi-room sync
    pub sync_poll_playing_interval: f64,
    pub sync_poll_idle_interval: f64,
    pub sync_drift_threshold_ms: i32,
    pub sync_correction_cooldown_s: f64,
    pub sync_dlna_default_buffer_s: f64,
    pub dlna_settle_ms: u32,
    pub dlna_play_delay_ms: u32,
    pub dlna_slow_renderer_patterns: String,
    pub dlna_slow_startup_delay_ms: u32,
    pub dlna_slow_retry_timeout_ms: u32,
    pub dlna_slow_max_retries: u32,

    // Crossfade
    pub crossfade_enabled: bool,
    pub crossfade_duration: f64,

    // Audio
    pub default_output_format: String,
    pub max_sample_rate: u32,
    pub max_bit_depth: u32,
    pub resample_policy: String,
    pub audio_buffer_kb: u32,
    pub prebuffer_seconds: f64,
    pub local_exclusive_mode: bool,
    pub local_latency_ms: u32,

    // DSP
    pub dsp_enabled: bool,
    pub dsp_filter: String,
    pub dsp_impulse_response: String,

    // Metadata
    pub metadata_readonly: bool,
    pub metadata_fix_genres_respect_vocabulary: bool,

    // Enrichment
    pub discogs_token: String,
    pub lastfm_api_key: String,
    pub lastfm_api_secret: String,
    pub lastfm_session_key: String,
    pub lastfm_scrobble_enabled: bool,
    pub enrich_on_scan: bool,

    // Artwork
    pub artwork_cache_dir: String,
    pub artwork_max_size: u32,

    // Streaming services
    pub tidal_enabled: bool,
    pub tidal_quality: String,
    pub qobuz_enabled: bool,
    pub qobuz_app_id: Option<String>,
    pub spotify_enabled: bool,
    pub spotify_client_id: Option<String>,
    pub spotify_redirect_uri: Option<String>,
    pub spotify_connect_enabled: bool,
    pub spotify_connect_device_name: Option<String>,
    pub spotify_connect_bitrate: u32,
    pub deezer_enabled: bool,
    pub deezer_arl: Option<String>,
    pub deezer_quality: String,
    pub amazon_music_enabled: bool,
    pub youtube_enabled: bool,

    // Discovery
    pub discovery_enabled: bool,
    pub ssdp_enabled: bool,
    pub mdns_enabled: bool,
    pub cast_enabled: bool,
    pub peer_discovery_enabled: bool,

    // UPnP
    pub upnp_server_enabled: bool,
    pub upnp_server_name: String,

    // Mode
    pub mode: String,
    pub remote_host: Option<String>,
    pub remote_auto_discover: bool,

    // Network
    pub network_shares_enabled: bool,

    // Logging
    pub log_level: String,

    // Update
    pub auto_update: bool,
}

impl Default for TuneConfig {
    fn default() -> Self {
        let home = dirs_home();
        Self {
            music_dirs: vec![format!("{home}/Music")],
            scan_on_startup: true,
            scan_schedule: None,
            quality_split: true,
            watch_filesystem: true,
            watcher_debounce_seconds: 2.0,
            db_path: "tune_server.db".into(),
            cors_origins: vec!["*".into()],
            api_key: None,
            web_dir: None,
            api_host: "0.0.0.0".into(),
            api_port: 8888,
            stream_host: "0.0.0.0".into(),
            stream_port: 8080,
            advertise_ip: None,
            default_zone_id: None,
            ws_heartbeat_interval: 30,
            stream_url_resolve_timeout: 15,
            pipeline_start_timeout: 15,
            sync_poll_playing_interval: 3.0,
            sync_poll_idle_interval: 10.0,
            sync_drift_threshold_ms: 500,
            sync_correction_cooldown_s: 15.0,
            sync_dlna_default_buffer_s: 3.0,
            dlna_settle_ms: 150,
            dlna_play_delay_ms: 50,
            dlna_slow_renderer_patterns: "atoll,st300,st200,shangling,shanling,scd1".into(),
            dlna_slow_startup_delay_ms: 1500,
            dlna_slow_retry_timeout_ms: 3000,
            dlna_slow_max_retries: 2,
            crossfade_enabled: false,
            crossfade_duration: 3.0,
            default_output_format: "flac".into(),
            max_sample_rate: 192_000,
            max_bit_depth: 24,
            resample_policy: "auto".into(),
            audio_buffer_kb: 32,
            prebuffer_seconds: 0.5,
            local_exclusive_mode: false,
            local_latency_ms: 50,
            dsp_enabled: false,
            dsp_filter: String::new(),
            dsp_impulse_response: String::new(),
            metadata_readonly: true,
            metadata_fix_genres_respect_vocabulary: false,
            discogs_token: String::new(),
            lastfm_api_key: String::new(),
            lastfm_api_secret: String::new(),
            lastfm_session_key: String::new(),
            lastfm_scrobble_enabled: false,
            enrich_on_scan: false,
            artwork_cache_dir: "artwork_cache".into(),
            artwork_max_size: 1200,
            tidal_enabled: false,
            tidal_quality: "HI_RES_LOSSLESS".into(),
            qobuz_enabled: false,
            qobuz_app_id: Some("798273057".into()),
            spotify_enabled: false,
            spotify_client_id: None,
            spotify_redirect_uri: None,
            spotify_connect_enabled: false,
            spotify_connect_device_name: None,
            spotify_connect_bitrate: 320,
            deezer_enabled: false,
            deezer_arl: None,
            deezer_quality: "FLAC".into(),
            amazon_music_enabled: false,
            youtube_enabled: false,
            discovery_enabled: true,
            ssdp_enabled: true,
            mdns_enabled: true,
            cast_enabled: true,
            peer_discovery_enabled: true,
            upnp_server_enabled: true,
            upnp_server_name: "Tune Server".into(),
            mode: "server".into(),
            remote_host: None,
            remote_auto_discover: true,
            network_shares_enabled: false,
            log_level: "INFO".into(),
            auto_update: false,
        }
    }
}

impl TuneConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(dirs) = std::env::var("TUNE_MUSIC_DIRS") {
            config.music_dirs = parse_music_dirs(&dirs);
        }
        env_str("TUNE_DB_PATH", &mut config.db_path);
        env_str("TUNE_API_HOST", &mut config.api_host);
        env_u16("TUNE_API_PORT", &mut config.api_port);
        env_str("TUNE_STREAM_HOST", &mut config.stream_host);
        env_u16("TUNE_STREAM_PORT", &mut config.stream_port);
        env_opt("TUNE_ADVERTISE_IP", &mut config.advertise_ip);
        env_opt("TUNE_API_KEY", &mut config.api_key);
        env_bool("TUNE_SCAN_ON_STARTUP", &mut config.scan_on_startup);
        env_bool("TUNE_WATCH_FILESYSTEM", &mut config.watch_filesystem);
        env_bool("TUNE_CROSSFADE_ENABLED", &mut config.crossfade_enabled);
        env_f64("TUNE_CROSSFADE_DURATION", &mut config.crossfade_duration);
        env_str(
            "TUNE_DEFAULT_OUTPUT_FORMAT",
            &mut config.default_output_format,
        );
        env_u32("TUNE_MAX_SAMPLE_RATE", &mut config.max_sample_rate);
        env_u32("TUNE_MAX_BIT_DEPTH", &mut config.max_bit_depth);
        env_bool("TUNE_METADATA_READONLY", &mut config.metadata_readonly);
        env_str("TUNE_DISCOGS_TOKEN", &mut config.discogs_token);
        env_str("TUNE_LASTFM_API_KEY", &mut config.lastfm_api_key);
        env_str("TUNE_LASTFM_API_SECRET", &mut config.lastfm_api_secret);
        env_bool(
            "TUNE_LASTFM_SCROBBLE_ENABLED",
            &mut config.lastfm_scrobble_enabled,
        );
        env_bool("TUNE_ENRICH_ON_SCAN", &mut config.enrich_on_scan);
        env_bool("TUNE_TIDAL_ENABLED", &mut config.tidal_enabled);
        env_str("TUNE_TIDAL_QUALITY", &mut config.tidal_quality);
        env_bool("TUNE_QOBUZ_ENABLED", &mut config.qobuz_enabled);
        env_bool("TUNE_SPOTIFY_ENABLED", &mut config.spotify_enabled);
        env_opt("TUNE_SPOTIFY_CLIENT_ID", &mut config.spotify_client_id);
        env_opt(
            "TUNE_SPOTIFY_REDIRECT_URI",
            &mut config.spotify_redirect_uri,
        );
        env_bool(
            "TUNE_SPOTIFY_CONNECT_ENABLED",
            &mut config.spotify_connect_enabled,
        );
        env_bool("TUNE_DEEZER_ENABLED", &mut config.deezer_enabled);
        env_opt("TUNE_DEEZER_ARL", &mut config.deezer_arl);
        env_str("TUNE_DEEZER_QUALITY", &mut config.deezer_quality);
        env_bool("TUNE_DISCOVERY_ENABLED", &mut config.discovery_enabled);
        env_bool("TUNE_UPNP_SERVER_ENABLED", &mut config.upnp_server_enabled);
        env_str("TUNE_UPNP_SERVER_NAME", &mut config.upnp_server_name);
        env_str("TUNE_MODE", &mut config.mode);
        env_opt("TUNE_REMOTE_HOST", &mut config.remote_host);
        env_str("TUNE_LOG_LEVEL", &mut config.log_level);
        env_bool("TUNE_AUTO_UPDATE", &mut config.auto_update);
        config
    }

    pub fn is_slow_renderer(&self, device_name: &str) -> bool {
        let lower = device_name.to_lowercase();
        self.dlna_slow_renderer_patterns
            .split(',')
            .any(|pat| !pat.trim().is_empty() && lower.contains(pat.trim()))
    }
}

fn dirs_home() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/tmp".into())
}

fn parse_music_dirs(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.starts_with('[') {
        serde_json::from_str(trimmed).unwrap_or_else(|_| vec![trimmed.to_string()])
    } else if trimmed.contains(',') {
        // Comma-separated: works on all platforms including Windows
        trimmed
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if cfg!(target_os = "windows")
        || trimmed.contains('\\')
        || (trimmed.len() >= 2 && trimmed.as_bytes()[1] == b':')
    {
        // Single Windows path (e.g. C:\Users\Bob\Music) — do NOT split on ':'
        // as that would break the drive letter prefix.
        vec![trimmed.to_string()]
    } else {
        // Colon-separated (Unix only, e.g. /music:/data/flac)
        trimmed
            .split(':')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    }
}

fn env_str(key: &str, target: &mut String) {
    if let Ok(val) = std::env::var(key) {
        *target = val;
    }
}

fn env_opt(key: &str, target: &mut Option<String>) {
    if let Ok(val) = std::env::var(key) {
        if val.is_empty() {
            *target = None;
        } else {
            *target = Some(val);
        }
    }
}

fn env_bool(key: &str, target: &mut bool) {
    if let Ok(val) = std::env::var(key) {
        *target = matches!(val.to_lowercase().as_str(), "true" | "1" | "yes");
    }
}

fn env_u16(key: &str, target: &mut u16) {
    if let Ok(val) = std::env::var(key)
        && let Ok(n) = val.parse()
    {
        *target = n;
    }
}

fn env_u32(key: &str, target: &mut u32) {
    if let Ok(val) = std::env::var(key)
        && let Ok(n) = val.parse()
    {
        *target = n;
    }
}

fn env_f64(key: &str, target: &mut f64) {
    if let Ok(val) = std::env::var(key)
        && let Ok(n) = val.parse()
    {
        *target = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let cfg = TuneConfig::default();
        assert_eq!(cfg.api_port, 8888);
        assert_eq!(cfg.stream_port, 8080);
        assert!(cfg.scan_on_startup);
        assert!(cfg.metadata_readonly);
        assert!(!cfg.crossfade_enabled);
    }

    #[test]
    fn parse_music_dirs_json() {
        let dirs = parse_music_dirs(r#"["/music", "/data/flac"]"#);
        assert_eq!(dirs, vec!["/music", "/data/flac"]);
    }

    #[test]
    fn parse_music_dirs_json_windows() {
        let dirs = parse_music_dirs(r#"["C:\\Users\\Bob\\Music", "D:\\NAS\\Musique"]"#);
        assert_eq!(dirs, vec![r"C:\Users\Bob\Music", r"D:\NAS\Musique"]);
    }

    #[test]
    fn parse_music_dirs_colon_separated() {
        let dirs = parse_music_dirs("/music:/data/flac");
        assert_eq!(dirs, vec!["/music", "/data/flac"]);
    }

    #[test]
    fn parse_music_dirs_single() {
        let dirs = parse_music_dirs("/home/user/Music");
        assert_eq!(dirs, vec!["/home/user/Music"]);
    }

    #[test]
    fn parse_music_dirs_comma_separated() {
        let dirs = parse_music_dirs("/music, /data/flac");
        assert_eq!(dirs, vec!["/music", "/data/flac"]);
    }

    #[test]
    fn parse_music_dirs_windows_drive_path() {
        // A single Windows path with drive letter must NOT be split on ':'
        let dirs = parse_music_dirs(r"C:\Users\Bob\Music");
        assert_eq!(dirs, vec![r"C:\Users\Bob\Music"]);
    }

    #[test]
    fn parse_music_dirs_windows_comma_separated() {
        let dirs = parse_music_dirs(r"C:\Users\Bob\Music, D:\NAS\Musique");
        assert_eq!(dirs, vec![r"C:\Users\Bob\Music", r"D:\NAS\Musique"]);
    }

    #[test]
    fn parse_music_dirs_unc_path() {
        let dirs = parse_music_dirs(r"\\NAS\Musique");
        assert_eq!(dirs, vec![r"\\NAS\Musique"]);
    }

    #[test]
    fn slow_renderer_detection() {
        let cfg = TuneConfig::default();
        assert!(cfg.is_slow_renderer("Atoll ST300 Signature"));
        assert!(cfg.is_slow_renderer("shanling scd1"));
        assert!(!cfg.is_slow_renderer("Sonos One"));
    }

    #[test]
    fn config_roundtrip_json() {
        let cfg = TuneConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let back: TuneConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.api_port, 8888);
        assert_eq!(back.max_sample_rate, 192_000);
    }
}
