use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

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
    /// Free-tier zone cap. Premium instances are unlimited regardless.
    /// Overridable via `TUNE_FREE_MAX_ZONES`. Default 3.
    #[serde(default = "default_free_max_zones")]
    pub free_max_zones: i64,
}

fn default_free_max_zones() -> i64 {
    3
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

/// Effective SetAVTransportURI→Play delay for a device: the owning zone's
/// per-zone override (`dlna_play_delay_ms` > 0, set from the renderer panel) if
/// present, else the config default (`[device_delays]` / `dlna_play_delay_ms`
/// via `play_delay_for`). Used at every point a `DlnaOutput` is built so the
/// per-zone value survives restart/rediscovery, not only a live PATCH.
pub fn resolve_play_delay(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    config: &TuneConfig,
    device_id: &str,
    device_name: &str,
) -> u64 {
    use tune_core::db::zone_repo::ZoneRepo;
    let repo = ZoneRepo::with_backend(db.clone());
    let zone_override = repo
        .get_by_device_id(device_id)
        .ok()
        .flatten()
        .and_then(|z| z.id)
        .map(|zid| repo.get_dlna_play_delay_ms(zid))
        .filter(|d| *d > 0);
    zone_override.unwrap_or_else(|| config.play_delay_for(device_name))
}

/// Clé du réglage « silence UPnP » d'une zone. Même forme que
/// `zone_{id}_upnp_renderer` : la clé est SUPPRIMÉE à la désactivation, pour
/// que l'absence de clé et le défaut désarmé soient un seul et même état.
pub fn cle_silence_upnp(zone_id: i64) -> String {
    format!("zone_{zone_id}_upnp_silence")
}

/// L'option « silence UPnP » est-elle armée sur la zone qui porte cet appareil ?
///
/// Strictement opt-in : sans zone, sans réglage, ou sur un réglage illisible,
/// la réponse est `false` et la sortie garde le régime par défaut (évènements
/// + position mesurée). Relu à CHAQUE construction de `DlnaOutput`, comme
/// `resolve_play_delay`, pour que le choix survive à un redémarrage et à une
/// redécouverte — pas seulement à un PATCH en direct.
pub fn resolve_upnp_silence(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    device_id: &str,
) -> bool {
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::zone_repo::ZoneRepo;
    let Some(zone_id) = ZoneRepo::with_backend(db.clone())
        .get_by_device_id(device_id)
        .ok()
        .flatten()
        .and_then(|z| z.id)
    else {
        return false;
    };
    SettingsRepo::with_backend(db.clone())
        .get(&cle_silence_upnp(zone_id))
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
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
            free_max_zones: default_free_max_zones(),
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
            // `mut` is only used on macOS/Windows (which push platform paths);
            // on other targets (Linux, BSD…) nothing pushes, so silence the
            // otherwise-spurious unused_mut there.
            #[cfg_attr(
                not(any(target_os = "macos", target_os = "windows")),
                allow(unused_mut)
            )]
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
        if let Ok(v) = std::env::var("TUNE_FREE_MAX_ZONES")
            && let Ok(n) = v.parse::<i64>()
            && n > 0
        {
            config.free_max_zones = n;
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

        // macOS, #3185 : UN SEUL chemin de base, quel que soit le repertoire de
        // lancement. Le code precedent gardait la base trouvee dans le
        // repertoire courant quand il y en avait une ; le meme binaire ouvrait
        // donc DEUX bases differentes selon son lanceur — le `.command` depuis
        // le dossier d'installation, le LaunchAgent depuis `/`. C'est le
        // « si je le relance manuellement je perds les zones » du fil 616.
        //
        // La regle vit dans `plan_base_macos`, une fonction pure compilee sur
        // TOUTES les plateformes : ce bloc-ci ne fait plus que lui donner ce
        // qu'elle ne peut pas savoir (le HOME, le repertoire courant, ce qui
        // existe sur le disque) et appliquer son plan.
        #[cfg(target_os = "macos")]
        {
            let repertoire_courant = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let home = std::env::var("HOME").ok();
            let plan = plan_base_macos(
                &config.db_path,
                &config.artwork_dir,
                home.as_deref(),
                &repertoire_courant,
                |chemin| chemin.exists(),
            );
            appliquer_plan_base_macos(&mut config, plan);
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
        // Un seul réglage, deux noms : la résolution vit dans `tune-core` pour
        // que les deux chemins de configuration ne puissent pas diverger
        // (#2265). Le nom canonique gagne, l'ancien reste lu.
        if let Some(backend) = tune_core::config::local_audio_backend_from_env() {
            config.local_audio_backend = backend;
        }
        if let Ok(v) = std::env::var("TUNE_LOCAL_EXCLUSIVE_MODE") {
            config.local_exclusive_mode = matches!(v.to_lowercase().as_str(), "true" | "1" | "yes");
        }
        tune_core::config::asio_implies_exclusive(
            &config.local_audio_backend,
            &mut config.local_exclusive_mode,
        );
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

/// Resolve the directory the web client (SPA) is served from.
///
/// Historically the server served `./web` relative to the process **current
/// working directory**. That breaks after an in-app auto-update + systemd
/// restart: the auto-updater writes the fresh `web/` next to the binary, but
/// systemd restarts the process with a `WorkingDirectory` that may differ from
/// the install dir, so `./web` resolves to a stale or missing folder and the
/// browser loads an old SPA build (Fabien: Diagnostics loop + stale
/// quality_split toggle persisting after auto-update).
///
/// Resolution order:
///   1. `TUNE_WEB_DIR` — honored verbatim (absolute or relative override).
///   2. When both `<cwd>/web` and `<exe_dir>/web` exist, the **newer** one (by
///      `index.html` mtime). After an in-app auto-update + restart, the launch
///      working directory's `./web` can be a *stale* copy from an earlier
///      version, while the updater always refreshes `<exe_dir>/web`. Preferring
///      `<cwd>/web` unconditionally then served an old SPA, so the browser kept
///      showing pre-update behaviour despite a new binary (Elie: fixes appear to
///      "recur" after auto-update; same class as Fabien's stale SPA). Serving
///      the newest fixes that; Docker/manual layouts — where `<cwd>/web` is the
///      only or newest copy — are unaffected.
///   3. Whichever of the two exists.
///   4. `<cwd>/web` as a last resort (may not exist yet; ServeDir 404s cleanly).
pub fn resolve_web_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    use std::time::SystemTime;

    if let Ok(custom) = std::env::var("TUNE_WEB_DIR") {
        return PathBuf::from(custom);
    }

    let cwd_web = std::env::current_dir()
        .map(|d| d.join("web"))
        .unwrap_or_else(|_| PathBuf::from("web"));
    let exe_web = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("web")))
        .filter(|p| p.exists());

    // A web build's freshness, from its index.html mtime (rewritten with fresh
    // asset hashes on every build/copy); missing → epoch so a present dir wins.
    fn freshness(dir: &std::path::Path) -> SystemTime {
        std::fs::metadata(dir.join("index.html"))
            .or_else(|_| std::fs::metadata(dir))
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    match exe_web {
        Some(exe_web) if cwd_web.exists() => {
            if freshness(&exe_web) > freshness(&cwd_web) {
                info!(path = %exe_web.display(), "web_dir_resolved_newest_exe");
                exe_web
            } else {
                cwd_web
            }
        }
        Some(exe_web) => {
            info!(path = %exe_web.display(), "web_dir_resolved_to_binary_dir");
            exe_web
        }
        None => cwd_web,
    }
}

/// Path of the server's own log file, written on every platform (not just
/// macOS/Windows) so the Diagnostics "Export logs" button and `/system/logs`
/// return real logs regardless of how the server was launched — terminal,
/// systemd/journald, Docker, or a double-clicked .app. Before this, Linux never
/// wrote a file, so any launch where journalctl didn't apply (Docker, a bare
/// terminal, a non-matching unit name) exported an empty log.
///
/// Both the writer (main) and the reader (`/system/logs`) call this, so they
/// always agree on the path.
///
/// Resolution order:
///   1. `TUNE_LOG_FILE` — honored verbatim.
///   2. Windows: `%LOCALAPPDATA%\TuneServer\tune-server.log`.
///   3. macOS: `$HOME/Library/Logs/tune-server.log`.
///   4. Linux/other: `$XDG_STATE_HOME/tune/`, else `$HOME/.local/state/tune/`,
///      else `/tmp/` — the first user-writable location, never `/var/log`
///      (a `User=` service or a container can't write there).
///
/// Creates the parent directory. Note: append-only, no rotation (same as the
/// pre-existing macOS/Windows behavior); rotation is a separate follow-up.
pub fn default_log_file_path() -> std::path::PathBuf {
    use std::path::PathBuf;

    if let Ok(custom) = std::env::var("TUNE_LOG_FILE") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }

    let path = if cfg!(target_os = "windows") {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\ProgramData".into());
        PathBuf::from(base)
            .join("TuneServer")
            .join("tune-server.log")
    } else if cfg!(target_os = "macos") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        PathBuf::from(home)
            .join("Library/Logs")
            .join("tune-server.log")
    } else {
        let base = std::env::var("XDG_STATE_HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .map(|h| PathBuf::from(h).join(".local/state"))
            })
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        base.join("tune").join("tune-server.log")
    };

    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    path
}

/// Rotate the log file at `path` when it grows past `max_bytes`: the current
/// file is moved to `<path>.1` (replacing any previous backup) so a fresh file
/// starts. Keeps at most the current file plus one backup, bounding disk use to
/// ~2× `max_bytes` across restarts instead of growing forever — the file logger
/// is append-only and, before this, never rotated on any platform.
///
/// Called at startup, before the logger opens the file. `/system/logs` keeps
/// reading the current path unchanged.
pub fn rotate_log_file(path: &std::path::Path, max_bytes: u64) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() > max_bytes)
        .unwrap_or(false);
    if too_big {
        let mut backup = path.as_os_str().to_owned();
        backup.push(".1");
        if let Err(e) = std::fs::rename(path, &backup) {
            info!(error = %e, path = %path.display(), "log_rotate_failed");
        }
    }
}

/// Socket d'écoute IPv4 seule — comportement historique, et repli quand la
/// double pile n'est pas disponible.
pub(crate) fn ipv4_listen_socket() -> socket2::Socket {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .expect("failed to create socket");
    socket.set_reuse_address(true).ok();
    socket
}

/// Socket d'écoute double pile : une socket IPv6 avec `IPV6_V6ONLY` désactivé
/// accepte aussi les connexions IPv4 (adresses IPv4-mappées).
///
/// Firefox résout `localhost` en préférant `::1` et, contrairement à Chrome, ne
/// retombe pas systématiquement sur `127.0.0.1` : avec une socket IPv4 seule il
/// reçoit « connexion refusée » alors que le serveur tourne (#1321).
///
/// Renvoie `None` si la pile IPv6 est absente ou refuse l'option — l'appelant
/// retombe alors sur l'IPv4 seule, sans rien changer au comportement connu.
pub(crate) fn dual_stack_listen_socket(
    port: u16,
) -> Option<(socket2::Socket, std::net::SocketAddr)> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .ok()?;
    socket.set_only_v6(false).ok()?;
    socket.set_reuse_address(true).ok();
    let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
    Some((socket, addr))
}

/// Le dossier de donnees de Tune sous macOS, relatif a `$HOME`.
pub const MACOS_DATA_SUBDIR: &str = "Library/Application Support/Tune";

/// Les fichiers annexes d'une base SQLite.
///
/// Une base n'est pas UN fichier : le `-wal` porte les transactions pas
/// encore repliees, le `-shm` l'index de ce journal. Copier la base seule
/// et laisser son `-wal` derriere rend une base amputee des dernieres
/// ecritures ; l'inverse — poser un `-wal` etranger a cote d'une base —
/// fait rejouer un journal qui n'est pas le sien. Le patron est celui de
/// `tune_core::db_backup`, qui traite deja les deux suffixes ensemble dans
/// `create_backup`, `replace_database` et `prune_backups`.
const ANNEXES_SQLITE: [&str; 2] = ["-wal", "-shm"];

/// Ce que le demarrage doit faire de la base, une fois la regle appliquee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionBaseMacos {
    /// `db_path` etait deja absolu : la configuration a tranche, on n'y touche pas.
    CheminAbsolu,
    /// `HOME` est introuvable : rien a resoudre, les chemins restent tels quels.
    HomeIntrouvable,
    /// Rien a deplacer — la base d'`Application Support` est la seule.
    Aucune,
    /// Une base vit dans le repertoire de lancement et **aucune** dans
    /// `Application Support` : elle doit y etre recopiee avant l'ouverture.
    Migrer { source: PathBuf },
    /// Les DEUX existent. `Application Support` l'emporte, et l'autre est
    /// laissee EN PLACE, intacte, nommee dans le journal.
    DeuxBases { delaissee: PathBuf },
}

/// Le plan rendu par la regle : les chemins retenus, et ce qu'il faut faire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBaseMacos {
    /// Le chemin de base retenu. Il ne depend JAMAIS du repertoire courant
    /// des lors que `home` est connu : c'est tout l'objet de #3185.
    pub db_path: String,
    /// Le dossier de pochettes retenu, resolu par la meme regle.
    pub artwork_dir: String,
    /// Le dossier de donnees a creer, quand il y en a un.
    pub app_support: Option<PathBuf>,
    pub action: ActionBaseMacos,
}

/// La regle de #3185 : ou vit la base, et que faire de celle qu'on trouve
/// ailleurs.
///
/// Fonction **pure**, et volontairement **sans `cfg`** : le bloc qu'elle
/// remplace vivait sous `#[cfg(target_os = "macos")]`, donc n'existait pour
/// aucun compilateur hors d'un Mac — un test qui aurait porte le meme `cfg`
/// aurait ete vert contre rien. Tout ce que la regle a besoin de savoir lui
/// est passe : le HOME, le repertoire courant, et un predicat d'existence.
/// Le patron est celui de `tune_core::config::resolve_local_audio_backend`,
/// qui prend son `lookup` en parametre « pour que la regle soit verifiable
/// sans toucher a l'environnement du processus ».
///
/// Les regles, dans l'ordre :
/// 1. un `db_path` **absolu** est honore tel quel — l'utilisateur a decide ;
/// 2. sans `HOME`, rien n'est resolu : on ne fabrique pas un chemin au hasard ;
/// 3. sinon le chemin retenu est TOUJOURS
///    `$HOME/Library/Application Support/Tune/<db_path>`, que le repertoire
///    courant contienne une base ou non. C'est l'invariant du correctif ;
/// 4. si une base traine dans le repertoire courant et qu'il n'y en a pas
///    encore sous `Application Support`, elle est **recopiee** ([`ActionBaseMacos::Migrer`]) ;
/// 5. si les DEUX existent, `Application Support` gagne — c'est le seul choix
///    qui ne depende pas du lanceur — et **aucune des deux n'est detruite**
///    ([`ActionBaseMacos::DeuxBases`]). Le journal nomme la delaissee pour que
///    l'utilisateur puisse la recuperer ou l'effacer lui-meme.
pub fn plan_base_macos(
    db_path: &str,
    artwork_dir: &str,
    home: Option<&str>,
    repertoire_courant: &Path,
    existe: impl Fn(&Path) -> bool,
) -> PlanBaseMacos {
    let inchange = |action| PlanBaseMacos {
        db_path: db_path.to_string(),
        artwork_dir: artwork_dir.to_string(),
        app_support: None,
        action,
    };
    if Path::new(db_path).is_absolute() {
        return inchange(ActionBaseMacos::CheminAbsolu);
    }
    let Some(home) = home else {
        return inchange(ActionBaseMacos::HomeIntrouvable);
    };
    let app_support = PathBuf::from(home).join(MACOS_DATA_SUBDIR);
    let cible = app_support.join(db_path);
    let locale = repertoire_courant.join(db_path);
    // Lance DEPUIS `Application Support` : les deux chemins designent le meme
    // fichier. Rien a migrer, et surtout rien a « delaisser ».
    let action = if locale == cible {
        ActionBaseMacos::Aucune
    } else {
        match (existe(&locale), existe(&cible)) {
            (true, false) => ActionBaseMacos::Migrer { source: locale },
            (true, true) => ActionBaseMacos::DeuxBases { delaissee: locale },
            (false, _) => ActionBaseMacos::Aucune,
        }
    };
    // `artwork_dir` suivait le meme chemin dans l'ancien bloc — mais SEULEMENT
    // dans la branche « aucune base locale », donc lui aussi dependait du
    // repertoire de lancement. Il est desormais resolu dans tous les cas.
    let artwork_dir = if Path::new(artwork_dir).is_absolute() {
        artwork_dir.to_string()
    } else {
        app_support.join(artwork_dir).to_string_lossy().into_owned()
    };
    PlanBaseMacos {
        db_path: cible.to_string_lossy().into_owned(),
        artwork_dir,
        app_support: Some(app_support),
        action,
    }
}

/// Recopie une base SQLite **et ses annexes** vers `cible`, sans jamais
/// effacer la source.
///
/// Deux garanties, parce qu'il s'agit de la donnee d'un utilisateur :
///
/// * **rien n'est detruit** — on copie, on ne deplace pas. Si quoi que ce
///   soit tourne mal ensuite, la base d'origine est encore la ou elle etait ;
///   au demarrage suivant la regle la verra comme [`ActionBaseMacos::DeuxBases`]
///   et la laissera intacte ;
/// * **aucun etat a mi-chemin** — les trois fichiers sont d'abord ecrits a
///   cote de la cible sous un nom temporaire, puis mis en place par des
///   renommages faits dans le meme dossier. Un echec avant la fin retire les
///   temporaires ET les fichiers deja poses, et rend l'erreur : la cible est
///   alors exactement dans l'etat ou elle etait.
///
/// Sans `cfg` : la migration est ainsi eprouvee sur toutes les plateformes.
pub fn copier_base_sqlite(source: &Path, cible: &Path) -> Result<u64, String> {
    /// Defait ce qui a ete pose, retire les temporaires, et rend l'erreur.
    fn renoncer(
        a_poser: &[(PathBuf, PathBuf)],
        poses: &[PathBuf],
        erreur: String,
    ) -> Result<u64, String> {
        for chemin in poses {
            let _ = std::fs::remove_file(chemin);
        }
        for (temporaire, _) in a_poser {
            let _ = std::fs::remove_file(temporaire);
        }
        Err(erreur)
    }

    let nom_source = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("chemin de base invalide : {}", source.display()))?;
    let nom_cible = cible
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("chemin de base invalide : {}", cible.display()))?;
    let marque = format!("{nom_cible}.migration-{}", std::process::id());

    // (temporaire, nom definitif)
    let mut a_poser: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut poses: Vec<PathBuf> = Vec::new();

    let temporaire = cible.with_file_name(&marque);
    if let Err(e) = std::fs::copy(source, &temporaire) {
        // Rien n'a encore ete pose : il n'y a que ce temporaire a retirer, et
        // il peut n'avoir jamais ete cree.
        let _ = std::fs::remove_file(&temporaire);
        return Err(format!("copie de {} : {e}", source.display()));
    }
    a_poser.push((temporaire, cible.to_path_buf()));

    for suffixe in ANNEXES_SQLITE {
        let annexe = source.with_file_name(format!("{nom_source}{suffixe}"));
        if !annexe.exists() {
            continue;
        }
        let temporaire = cible.with_file_name(format!("{marque}{suffixe}"));
        if let Err(e) = std::fs::copy(&annexe, &temporaire) {
            let _ = std::fs::remove_file(&temporaire);
            return renoncer(
                &a_poser,
                &poses,
                format!("copie de {} : {e}", annexe.display()),
            );
        }
        a_poser.push((
            temporaire,
            cible.with_file_name(format!("{nom_cible}{suffixe}")),
        ));
    }

    for (temporaire, definitif) in &a_poser {
        if let Err(e) = std::fs::rename(temporaire, definitif) {
            return renoncer(
                &a_poser,
                &poses,
                format!("mise en place de {} : {e}", definitif.display()),
            );
        }
        poses.push(definitif.clone());
    }

    std::fs::metadata(cible)
        .map(|m| m.len())
        .map_err(|e| format!("base migree illisible a {} : {e}", cible.display()))
}

/// Applique le plan de [`plan_base_macos`] : cree le dossier de donnees,
/// migre s'il le faut, puis pose les chemins definitifs dans la configuration.
///
/// Sans `cfg`, comme la regle : les effets de bord aussi sont ainsi eprouves
/// sur Shrek. Seul l'appel — qui lit `HOME` et le repertoire courant du
/// processus — reste sous `#[cfg(target_os = "macos")]`.
///
/// Deux replis, tous deux journalises :
///
/// * `Application Support` **increable** : les chemins d'entree sont conserves,
///   c'est-a-dire exactement le comportement d'avant le correctif. Un dossier
///   de donnees inaccessible n'est pas une raison pour refuser de demarrer ;
/// * migration **echouee** : le chemin retenu reste malgre tout celui
///   d'`Application Support`, et l'echec sort en `error!`. Repartir sur la base
///   du repertoire courant reintroduirait precisement l'ambiguite que #3185
///   corrige ; la base d'origine, elle, est intacte et le journal la nomme.
pub fn appliquer_plan_base_macos(config: &mut TuneConfig, plan: PlanBaseMacos) {
    if matches!(
        plan.action,
        ActionBaseMacos::CheminAbsolu | ActionBaseMacos::HomeIntrouvable
    ) {
        if plan.action == ActionBaseMacos::HomeIntrouvable {
            warn!(db_path = %plan.db_path, "db_path_home_introuvable");
        }
        return;
    }
    let Some(app_support) = plan.app_support.as_ref() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(app_support) {
        warn!(
            path = %app_support.display(),
            error = %e,
            "db_path_app_support_increable"
        );
        return;
    }
    match &plan.action {
        ActionBaseMacos::Migrer { source } => {
            info!(
                from = %source.display(),
                to = %plan.db_path,
                "db_migration_vers_app_support"
            );
            match copier_base_sqlite(source, Path::new(&plan.db_path)) {
                Ok(octets) => info!(
                    octets,
                    from = %source.display(),
                    to = %plan.db_path,
                    "db_migration_reussie_base_d_origine_conservee"
                ),
                Err(e) => error!(
                    error = %e,
                    from = %source.display(),
                    to = %plan.db_path,
                    "db_migration_echouee_base_d_origine_intacte"
                ),
            }
        }
        ActionBaseMacos::DeuxBases { delaissee } => {
            warn!(
                retenue = %plan.db_path,
                delaissee = %delaissee.display(),
                "db_deux_bases_application_support_l_emporte_l_autre_est_laissee_intacte"
            );
        }
        _ => {}
    }
    info!(path = %plan.db_path, "db_path_resolved_to_app_support");
    config.db_path = plan.db_path;
    if config.artwork_dir != plan.artwork_dir {
        std::fs::create_dir_all(&plan.artwork_dir).ok();
        info!(path = %plan.artwork_dir, "artwork_dir_resolved_to_app_support");
        config.artwork_dir = plan.artwork_dir;
    }
}

#[cfg(test)]
mod listen_socket_tests {
    use super::*;

    /// Le point du correctif #1321 : une seule socket doit servir les clients
    /// IPv4 (Chrome, 127.0.0.1) ET IPv6 (Firefox, ::1).
    #[test]
    fn dual_stack_socket_accepts_both_families() {
        let Some((socket, addr)) = dual_stack_listen_socket(0) else {
            eprintln!("pas de pile IPv6 ici — le repli IPv4 s'applique, test sans objet");
            return;
        };
        if socket.bind(&addr.into()).is_err() {
            eprintln!("bind IPv6 refusé ici — le repli IPv4 s'applique, test sans objet");
            return;
        }
        socket.listen(8).expect("listen");
        let listener: std::net::TcpListener = socket.into();
        let port = listener.local_addr().expect("local_addr").port();

        for target in ["127.0.0.1", "::1"] {
            let client = std::net::TcpStream::connect((target, port));
            assert!(client.is_ok(), "connexion {target} refusée : {client:?}");
            drop(listener.accept().expect("accept"));
        }
    }

    #[test]
    fn ipv4_socket_is_always_available_as_fallback() {
        let socket = ipv4_listen_socket();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        socket.bind(&addr.into()).expect("bind IPv4");
    }
}
