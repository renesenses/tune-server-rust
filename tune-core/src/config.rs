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
    /// Audio host backend on Windows: "auto", "wasapi", or "asio".
    pub local_audio_backend: String,

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
    pub listenbrainz_token: String,
    pub listenbrainz_scrobble_enabled: bool,
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
            local_audio_backend: "auto".into(),
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
            listenbrainz_token: String::new(),
            listenbrainz_scrobble_enabled: false,
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
        env_bool(
            "TUNE_LOCAL_EXCLUSIVE_MODE",
            &mut config.local_exclusive_mode,
        );
        // Le nom canonique et son ancien nom mènent au même réglage. Jusqu'ici
        // seul `tune-server` connaissait l'alias : un `.env` écrit avec
        // l'ancien nom était honoré au démarrage du serveur et ignoré par tout
        // autre chemin passant par ce `from_env` (#2265).
        if let Some(backend) = local_audio_backend_from_env() {
            config.local_audio_backend = backend;
        }
        asio_implies_exclusive(
            &config.local_audio_backend,
            &mut config.local_exclusive_mode,
        );
        env_bool("TUNE_METADATA_READONLY", &mut config.metadata_readonly);
        env_str("TUNE_DISCOGS_TOKEN", &mut config.discogs_token);
        env_str("TUNE_LASTFM_API_KEY", &mut config.lastfm_api_key);
        env_str("TUNE_LASTFM_API_SECRET", &mut config.lastfm_api_secret);
        env_bool(
            "TUNE_LASTFM_SCROBBLE_ENABLED",
            &mut config.lastfm_scrobble_enabled,
        );
        env_str("TUNE_LISTENBRAINZ_TOKEN", &mut config.listenbrainz_token);
        env_bool(
            "TUNE_LISTENBRAINZ_SCROBBLE_ENABLED",
            &mut config.listenbrainz_scrobble_enabled,
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

// ---------------------------------------------------------------------------
// Backend audio local — un seul réglage, deux noms
// ---------------------------------------------------------------------------

/// Nom **canonique** du réglage de backend audio local (`auto`, `wasapi`, `asio`).
///
/// C'est ce nom-là qu'il faut écrire dans un `.env` et citer dans la
/// documentation ou les messages destinés aux utilisateurs.
pub const LOCAL_AUDIO_BACKEND_ENV: &str = "TUNE_LOCAL_AUDIO_BACKEND";

/// Ancien nom du **même** réglage, conservé pour compatibilité ascendante.
///
/// Il a été ajouté le 20/06 (commit `eb8ebdf0`) parce qu'un testeur l'avait
/// employé de bonne foi. Les `.env` déjà écrits avec ce nom doivent continuer
/// de fonctionner : on ne le supprime pas, on cesse simplement de le
/// recommander. Le nom canonique l'emporte quand les deux sont présents.
pub const LOCAL_AUDIO_BACKEND_ENV_LEGACY: &str = "TUNE_AUDIO_BACKEND";

/// Résout le backend audio local depuis une source de variables quelconque.
///
/// `lookup` rend la valeur d'une variable, ou `None` si elle n'est pas définie.
/// Le paramètre existe pour que la règle soit vérifiable sans toucher à
/// l'environnement du processus, qui est global et partagé entre les tests.
///
/// Règles, dans l'ordre :
/// 1. le nom canonique gagne s'il est **défini**, même à vide ;
/// 2. sinon l'ancien nom est consulté ;
/// 3. une valeur vide n'écrase rien et laisse la valeur par défaut en place.
///
/// Le point 1 est délibéré : définir le nom canonique à vide neutralise
/// l'ancien nom, ce qui est la seule façon de le désactiver sans l'effacer de
/// son `.env`. C'est déjà le comportement de `tune-server`, reproduit ici tel
/// quel pour que les deux chemins de configuration répondent la même chose.
pub fn resolve_local_audio_backend<F>(lookup: F) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(LOCAL_AUDIO_BACKEND_ENV)
        .or_else(|| lookup(LOCAL_AUDIO_BACKEND_ENV_LEGACY))
        .filter(|v| !v.is_empty())
}

/// Même résolution, lue dans l'environnement du processus.
pub fn local_audio_backend_from_env() -> Option<String> {
    resolve_local_audio_backend(|key| std::env::var(key).ok())
}

/// Contrainte de plateforme qui prive le réglage « mode exclusif » de son
/// effet.
///
/// #3192 — jfpaquet (Asus Essence STX II, Windows) : Tune coupe le son de
/// toutes les autres applications, et DÉCOCHER « mode exclusif » n'y change
/// rien. Le serveur avait raison sur le fond — un pilote ASIO ne s'ouvre pas
/// en partagé, ça n'existe pas — mais il l'imposait EN SILENCE. Le défaut
/// n'est pas la règle, c'est que le réglage ment : l'utilisateur décoche une
/// case, elle reste sans effet, et rien ne le lui dit.
///
/// Les codes sont **stables** et destinés à la machine (le client les
/// traduit), sur le modèle de `LocalBackendFallback` et de `runtime_reasons`
/// du chemin du signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusiveModeConstraint {
    /// ASIO n'a **pas** de mode partagé : le pilote se prend en entier ou pas
    /// du tout. Ce n'est pas un choix de Tune, c'est la nature du pilote —
    /// d'où « imposé » et non « ignoré ».
    AsioAlwaysExclusive,
}

impl ExclusiveModeConstraint {
    /// Code stable, celui que porte la charge utile JSON.
    pub fn code(self) -> &'static str {
        match self {
            Self::AsioAlwaysExclusive => "asio_always_exclusive",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal — le serveur y écrit
    /// déjà ses `detail` en français.
    pub fn detail(self) -> &'static str {
        match self {
            Self::AsioAlwaysExclusive => {
                "ASIO prend le périphérique en exclusivité : son pilote n'a pas \
                 de mode partagé. Les autres applications n'auront plus de son \
                 sur ce périphérique. Pour le partager, choisissez un autre \
                 backend (WASAPI)."
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : une
    /// contrainte ajoutée sans code ni libellé fait tomber le test qui
    /// parcourt cette liste.
    pub const ALL: [Self; 1] = [Self::AsioAlwaysExclusive];
}

/// Ce que le mode exclusif VAUT réellement, à côté de ce que le réglage
/// demande — et pourquoi, quand les deux diffèrent.
///
/// Additif : aucun champ ne remplace `local_exclusive_mode`, qui reste publié
/// tel quel. Un client qui ne lit pas cette structure voit le même écran
/// qu'avant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExclusiveModeStatus {
    /// Ce que l'utilisateur a demandé (la case, ou le fichier de config).
    pub requested: bool,
    /// Ce qui sera réellement appliqué à l'ouverture du périphérique.
    pub effective: bool,
    /// `true` dès que la contrainte s'applique — **y compris quand la case
    /// était déjà cochée**. C'est ce champ qui doit VERROUILLER le contrôle :
    /// la question n'est pas « le réglage a-t-il été changé ? » mais « ce
    /// réglage a-t-il encore un sens ? ».
    pub forced: bool,
    /// Pourquoi. `None` = le réglage est honoré tel quel.
    pub reason: Option<ExclusiveModeConstraint>,
    /// La même chose en clair, pour un écran qui n'a pas de table de
    /// traduction.
    pub detail: Option<&'static str>,
}

/// La règle, isolée de toute plateforme pour être vérifiable partout.
///
/// `on_windows` est un PARAMÈTRE, pas un `cfg!` : le chemin ASIO ne se compile
/// et ne s'exécute que sous Windows, donc un essai entouré du même `cfg`
/// serait vert contre rien. Même intention que le `lookup` de
/// [`resolve_local_audio_backend`] — la règle doit être éprouvable sans
/// dépendre de la machine qui l'éprouve.
///
/// Le `cfg!` reste au CÂBLAGE, dans [`local_exclusive_mode_status`].
pub fn exclusive_mode_status(
    backend: &str,
    requested: bool,
    on_windows: bool,
) -> ExclusiveModeStatus {
    // Hors Windows, `asio` est une valeur héritée (#1268) : aucun host ASIO ne
    // s'y ouvre, donc rien n'est imposé et le réglage est honoré tel quel.
    let reason = (on_windows && backend.eq_ignore_ascii_case("asio"))
        .then_some(ExclusiveModeConstraint::AsioAlwaysExclusive);
    let forced = reason.is_some();
    ExclusiveModeStatus {
        requested,
        effective: requested || forced,
        forced,
        reason,
        detail: reason.map(ExclusiveModeConstraint::detail),
    }
}

/// Même règle, câblée sur la plateforme de ce binaire.
pub fn local_exclusive_mode_status(backend: &str, requested: bool) -> ExclusiveModeStatus {
    exclusive_mode_status(backend, requested, cfg!(target_os = "windows"))
}

/// ASIO est exclusif par nature : le demander implique le mode exclusif.
///
/// Partagé par les deux chemins de configuration (`tune-core` et
/// `tune-server`) pour qu'ils ne puissent pas diverger.
///
/// ⚠️ Ce chemin-ci — le fichier de config et l'environnement — n'a **jamais**
/// eu de garde de plateforme, contrairement à `AppState::effective_exclusive_mode`
/// qui en a reçu une avec #1268. Il délègue donc avec `on_windows = true`,
/// c'est-à-dire exactement ce qu'il faisait déjà : une seule règle, une seule
/// écriture, et l'écart entre les deux chemins devient visible ici au lieu
/// d'être enfoui dans deux copies.
pub fn asio_implies_exclusive(backend: &str, exclusive_mode: &mut bool) {
    *exclusive_mode = exclusive_mode_status(backend, *exclusive_mode, true).effective;
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

    /// Fabrique un `lookup` à partir d'une liste de paires, pour éprouver la
    /// résolution sans toucher à l'environnement du processus.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |key| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_string())
        }
    }

    /// Le test qui gouverne #2265 : un `.env` de testeur déjà écrit avec
    /// l'ancien nom doit continuer d'être lu. C'était vrai côté `tune-server`
    /// et faux côté `tune-core`, où seul le nom canonique était consulté.
    #[test]
    fn ancien_nom_du_backend_toujours_lu() {
        let resolved =
            resolve_local_audio_backend(env_of(&[(LOCAL_AUDIO_BACKEND_ENV_LEGACY, "asio")]));
        assert_eq!(
            resolved.as_deref(),
            Some("asio"),
            "un .env écrit avec {} doit rester lu",
            LOCAL_AUDIO_BACKEND_ENV_LEGACY
        );
    }

    /// La conséquence concrète pour l'utilisateur : demander ASIO sous
    /// l'ancien nom doit aussi basculer en mode exclusif, comme sous le nom
    /// canonique. Sinon le repli est lu mais reste sans effet audible.
    #[test]
    fn ancien_nom_asio_bascule_le_mode_exclusif() {
        let backend =
            resolve_local_audio_backend(env_of(&[(LOCAL_AUDIO_BACKEND_ENV_LEGACY, "asio")]))
                .expect("l'ancien nom doit être résolu");
        let mut exclusive = false;
        asio_implies_exclusive(&backend, &mut exclusive);
        assert!(
            exclusive,
            "ASIO demandé sous l'ancien nom doit impliquer le mode exclusif"
        );
    }

    #[test]
    fn nom_canonique_prime_sur_l_ancien() {
        let resolved = resolve_local_audio_backend(env_of(&[
            (LOCAL_AUDIO_BACKEND_ENV, "wasapi"),
            (LOCAL_AUDIO_BACKEND_ENV_LEGACY, "asio"),
        ]));
        assert_eq!(resolved.as_deref(), Some("wasapi"));
    }

    /// Le nom canonique défini à vide neutralise l'ancien nom au lieu de lui
    /// laisser la main — comportement historique de `tune-server`, qu'on ne
    /// change pas.
    #[test]
    fn nom_canonique_vide_neutralise_l_ancien() {
        let resolved = resolve_local_audio_backend(env_of(&[
            (LOCAL_AUDIO_BACKEND_ENV, ""),
            (LOCAL_AUDIO_BACKEND_ENV_LEGACY, "asio"),
        ]));
        assert_eq!(resolved, None);
    }

    #[test]
    fn aucun_nom_defini_laisse_la_valeur_par_defaut() {
        assert_eq!(resolve_local_audio_backend(env_of(&[])), None);
        assert_eq!(TuneConfig::default().local_audio_backend, "auto");
    }

    #[test]
    fn asio_implies_exclusive_est_insensible_a_la_casse_et_ne_desactive_rien() {
        let mut exclusive = false;
        asio_implies_exclusive("ASIO", &mut exclusive);
        assert!(exclusive);

        let mut already_on = true;
        asio_implies_exclusive("wasapi", &mut already_on);
        assert!(
            already_on,
            "un autre backend ne doit pas désactiver l'exclusif"
        );

        let mut off = false;
        asio_implies_exclusive("wasapi", &mut off);
        assert!(!off);
    }

    // -----------------------------------------------------------------
    // #3192 — le réglage « mode exclusif » ne doit plus MENTIR.
    //
    // Ces essais portent sur `exclusive_mode_status`, qui prend la plateforme
    // en paramètre. C'est délibéré : le chemin ASIO ne se compile que sous
    // Windows, et un essai entouré du même `cfg` ne serait exécuté par aucune
    // des cibles Linux/macOS de la CI — vert contre rien.
    // -----------------------------------------------------------------

    /// 1. ASIO + case DÉCOCHÉE : la contrainte l'emporte (c'est la nature du
    ///    pilote), **et la raison est donnée**. C'est tout le ticket : avant,
    ///    le premier point était vrai et le second manquait.
    #[test]
    fn asio_impose_l_exclusif_et_dit_pourquoi() {
        let s = exclusive_mode_status("asio", false, true);
        assert!(
            s.effective,
            "ASIO n'a pas de mode partagé : l'exclusif s'applique"
        );
        assert!(
            s.forced,
            "et le contrôle doit être annoncé comme IMPOSÉ, pas honoré"
        );
        assert_eq!(s.reason, Some(ExclusiveModeConstraint::AsioAlwaysExclusive));
        let detail = s
            .detail
            .expect("une contrainte sans explication, c'est le défaut de #3192");
        assert!(
            detail.contains("WASAPI"),
            "l'explication doit dire à l'utilisateur ce qu'il PEUT faire \
             (changer de backend), pas seulement ce qu'il subit : {detail}"
        );
        assert!(
            !s.requested,
            "`requested` doit rester ce que l'utilisateur a demandé, sinon \
             l'écran ne peut pas dire que son choix a été écrasé"
        );
    }

    /// 2. WASAPI + case décochée : le réglage est honoré, rien n'est imposé,
    ///    et le son des autres applications reste.
    #[test]
    fn wasapi_decoche_reste_partage() {
        let s = exclusive_mode_status("wasapi", false, true);
        assert!(!s.effective, "le réglage de l'utilisateur doit être honoré");
        assert!(!s.forced);
        assert_eq!(s.reason, None);
        assert_eq!(s.detail, None);
    }

    /// 3. WASAPI + case cochée : l'exclusif demandé reste l'exclusif appliqué,
    ///    et il n'est pas présenté comme imposé — l'utilisateur l'a choisi.
    #[test]
    fn wasapi_coche_reste_exclusif_et_choisi() {
        let s = exclusive_mode_status("wasapi", true, true);
        assert!(s.effective);
        assert!(
            !s.forced,
            "un exclusif CHOISI ne doit pas se présenter comme subi : sinon \
             l'écran verrouillerait une case que l'utilisateur peut décocher"
        );
        assert_eq!(s.reason, None);
    }

    /// 4. **Le témoin.** Hors Windows rien ne change : une valeur `asio`
    ///    héritée d'une bibliothèque migrée (#1268) n'impose rien, parce
    ///    qu'aucun host ASIO ne s'y ouvrira jamais.
    #[test]
    fn hors_windows_rien_n_est_impose() {
        let s = exclusive_mode_status("asio", false, false);
        assert!(
            !s.effective,
            "hors Windows, `asio` est une valeur morte : elle ne doit armer \
             aucun chemin exclusif (le hog mode CoreAudio, bien réel sur macOS)"
        );
        assert!(!s.forced);
        assert_eq!(s.reason, None);
        assert_eq!(s.detail, None);
        // Et l'exclusif explicitement demandé reste honoré, partout.
        assert!(exclusive_mode_status("alsa", true, false).effective);
        assert!(!exclusive_mode_status("alsa", true, false).forced);
    }

    /// La case DÉJÀ cochée sous ASIO reste `forced` : l'écran doit la
    /// verrouiller aussi dans ce cas, sinon l'utilisateur la décoche et
    /// retombe exactement dans le défaut.
    #[test]
    fn asio_deja_coche_reste_impose() {
        let s = exclusive_mode_status("ASIO", true, true);
        assert!(s.effective);
        assert!(
            s.forced,
            "`forced` répond à « ce réglage a-t-il encore un sens ? », pas à \
             « a-t-il été changé ? »"
        );
        assert!(s.requested, "et le choix de l'utilisateur reste lisible");
    }

    /// Contre-épreuve permanente : toute contrainte ajoutée doit avoir un code
    /// distinct et une explication non vide. Une variante posée sans câblage
    /// fait rougir ceci.
    #[test]
    fn toute_contrainte_a_un_code_distinct_et_une_explication() {
        let mut codes: Vec<&str> = Vec::new();
        for c in ExclusiveModeConstraint::ALL {
            assert!(!c.code().is_empty(), "code vide pour {c:?}");
            assert!(
                c.detail().len() > 20,
                "explication trop courte pour {c:?} : elle est destinée à un \
                 humain qui vient de perdre le son de sa visioconférence"
            );
            assert!(!codes.contains(&c.code()), "code dupliqué : {}", c.code());
            codes.push(c.code());
        }
        assert_eq!(codes.len(), ExclusiveModeConstraint::ALL.len());
    }

    /// Le code stable doit être celui que porte le JSON — pas une chaîne
    /// recopiée à côté.
    #[test]
    fn le_code_serialise_est_le_code_stable() {
        let s = exclusive_mode_status("asio", false, true);
        let v = serde_json::to_value(&s).expect("le statut doit être sérialisable");
        assert_eq!(
            v["reason"].as_str(),
            Some(ExclusiveModeConstraint::AsioAlwaysExclusive.code()),
            "le client lit ce code, il ne doit pas dériver du nom Rust"
        );
        assert_eq!(v["forced"].as_bool(), Some(true));
        assert_eq!(v["requested"].as_bool(), Some(false));
        assert_eq!(v["effective"].as_bool(), Some(true));
    }

    /// Une seule règle : le raccourci historique du chemin de configuration
    /// doit rendre exactement ce que rend la règle. S'ils divergent, un `.env`
    /// et la page de réglages ne diront plus la même chose.
    #[test]
    fn asio_implies_exclusive_est_la_meme_regle() {
        for backend in ["asio", "ASIO", "wasapi", "auto", ""] {
            for demande in [false, true] {
                let mut par_le_raccourci = demande;
                asio_implies_exclusive(backend, &mut par_le_raccourci);
                assert_eq!(
                    par_le_raccourci,
                    exclusive_mode_status(backend, demande, true).effective,
                    "divergence sur ({backend:?}, {demande})"
                );
            }
        }
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
