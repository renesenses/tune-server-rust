#![allow(unsafe_op_in_unsafe_fn)]
//! FFI bridge for embedding Tune Server in mobile apps (Flutter/Android, iOS).
//!
//! Exposes a minimal C API:
//! - `tune_server_start(port, db_path, music_dirs, web_dir)` → starts the server
//! - `tune_server_stop()` → gracefully stops the server
//! - `tune_server_status()` → returns JSON status string
//! - `tune_server_version()` → returns version string
//! - `tune_free_string(ptr)` → frees a string returned by this library
//! - `tune_server_set_container_root(root)` → racine du conteneur de l'hôte,
//!   pour relocaliser la base quand iOS déplace l'application
//! - `tune_server_scan()` → scan incrémental (retour au premier plan)
//! - `tune_server_set_device_name(name)` → nom de l'appareil, pour le nom
//!   annoncé sur le réseau (#8189)
//! - `tune_server_defer_network_discovery(defer)` → SSDP/mDNS différés
//!   jusqu'au premier usage (#8189, permission « réseau local » d'iOS)
//! - `tune_server_start_network_discovery()` → ouvre la découverte
//! - `tune_server_set_master_key(key, len)` → clé maîtresse des identifiants
//!   des sources réseau, gardée par l'hôte (trousseau, Keystore), jamais
//!   écrite sur disque (#8189)
//! - `tune_server_network_changed()` → le réseau a changé (#8189)
//!
//! Au démarrage, comme le binaire (`bootstrap.rs`) : scan des dossiers de
//! musique (`auto_scan`), puis surveillant de fichiers (kqueue sur iOS,
//! inotify sur Android) — un album déposé par l'app Fichiers ou le partage de
//! fichiers entre dans la bibliothèque sans geste.

mod relocalisation;

use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::runtime::Runtime;
use tracing::info;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
// A `Mutex<Option<_>>` (not a `OnceLock`) so each start publishes a fresh
// sender and each stop clears it — otherwise a second start/stop cycle could
// never replace the sender and restart would be broken.
static SHUTDOWN_TX: Mutex<Option<tokio::sync::watch::Sender<bool>>> = Mutex::new(None);
/// La racine du conteneur donnée par l'hôte, lue par le démarrage suivant.
static CONTAINER_ROOT: Mutex<Option<String>> = Mutex::new(None);
/// L'état du serveur en marche, pour `tune_server_scan`. Vidé à l'arrêt.
static ETAT: Mutex<Option<tune_server::state::AppState>> = Mutex::new(None);
/// Le surveillant de fichiers tourne jusqu'à la fin du processus (il n'a pas
/// d'arrêt) : un second démarrage dans le même processus ne le double pas.
static SURVEILLANT_LANCE: AtomicBool = AtomicBool::new(false);
/// L'hôte a demandé de différer la découverte réseau (#8189).
static DECOUVERTE_DIFFEREE: AtomicBool = AtomicBool::new(false);
/// Le réglage qui retient qu'on a déjà ouvert la découverte une fois : au
/// lancement suivant, elle démarre d'emblée (la permission est déjà donnée).
const REGLAGE_DECOUVERTE_OUVERTE: &str = "mobile_decouverte_reseau_ouverte";

/// Barriere d'ABI : une panique ne doit JAMAIS franchir un `extern "C"`.
///
/// Tant que le profil release portait `panic = "abort"`, la question ne se
/// posait pas — toute panique tuait le processus, ici comme ailleurs. En
/// repassant a `panic = "unwind"` (#2305), une panique qui traverse une
/// frontiere `extern "C"` fait avorter le processus : c'est la regle de Rust,
/// et c'est le pire des deux mondes pour l'hote Flutter ou Swift, qui perd
/// l'application entiere sans diagnostic.
///
/// Chaque entree convertit donc la panique en sa valeur d'echec documentee.
/// Le hook global de `bootstrap.rs` a deja ecrit `tune-crash.log` au moment de
/// la panique : on ne rejournalise pas la pile, seulement l'entree franchie.
fn barriere_abi<T>(entree: &'static str, valeur_si_panique: T, f: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(valeur) => valeur,
        Err(_) => {
            tracing::error!(entree, "tune_ffi_panique_interceptee");
            valeur_si_panique
        }
    }
}

fn get_runtime() -> &'static Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(4)
            .thread_name("tune-ffi")
            .build()
            .expect("failed to create tokio runtime")
    })
}

/// Start the Tune server on the given port.
///
/// # Arguments
/// - `port` — HTTP port (e.g. 8888)
/// - `db_path` — path to the SQLite database file
/// - `music_dirs_json` — JSON array of music directory paths, e.g. `["/sdcard/Music"]`
/// - `web_dir` — path to the web client assets directory (or null to skip)
///
/// Returns 0 on success, -1 if already running, -2 on error.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_start(
    port: u16,
    db_path: *const c_char,
    music_dirs_json: *const c_char,
    web_dir: *const c_char,
) -> i32 {
    barriere_abi("tune_server_start", -2, || {
        demarrer(port, db_path, music_dirs_json, web_dir)
    })
}

fn demarrer(
    port: u16,
    db_path: *const c_char,
    music_dirs_json: *const c_char,
    web_dir: *const c_char,
) -> i32 {
    // Validate raw pointers before dereferencing: a null `db_path` would be UB
    // if passed to `CStr::from_ptr`, so reject it with the error code instead.
    if db_path.is_null() {
        return -2; // null db_path
    }

    // Claim the running slot atomically *before* spawning so two concurrent
    // starts can't both proceed. The server task clears the flag on exit, so a
    // later start (restart) can claim it again.
    if RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return -1; // already running
    }

    let db_path = unsafe { CStr::from_ptr(db_path) }
        .to_str()
        .unwrap_or("tune.db")
        .to_string();

    let music_dirs: Vec<String> = if music_dirs_json.is_null() {
        vec![]
    } else {
        let json_str = unsafe { CStr::from_ptr(music_dirs_json) }
            .to_str()
            .unwrap_or("[]");
        serde_json::from_str(json_str).unwrap_or_default()
    };

    let web_dir = if web_dir.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(web_dir) }
                .to_str()
                .unwrap_or("")
                .to_string(),
        )
    };

    // Initialize tracing (once)
    static TRACING_INIT: std::sync::Once = std::sync::Once::new();
    TRACING_INIT.call_once(|| {
        use tracing_subscriber::EnvFilter;
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,tune_core=info,tune_server=info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .init();
    });

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    // Publish a fresh sender for this run (replacing any stale one from a
    // previous start/stop cycle), so `tune_server_stop` can signal shutdown.
    if let Ok(mut guard) = SHUTDOWN_TX.lock() {
        *guard = Some(shutdown_tx);
    }

    let rt = get_runtime();

    rt.spawn(async move {
        // RUNNING was already claimed synchronously above.
        info!(port, db = %db_path, "tune_ffi_server_starting");

        match run_server(port, db_path, music_dirs, web_dir, shutdown_rx).await {
            Ok(()) => info!("tune_ffi_server_stopped"),
            Err(e) => tracing::error!(error = %e, "tune_ffi_server_error"),
        }

        RUNNING.store(false, Ordering::SeqCst);
    });

    0
}

/// Stop the Tune server gracefully.
/// Returns 0 on success, -1 if not running.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_stop() -> i32 {
    barriere_abi("tune_server_stop", -2, || {
        if !RUNNING.load(Ordering::SeqCst) {
            return -1;
        }
        // Take the sender so it is cleared for the next start/stop cycle.
        if let Ok(mut guard) = SHUTDOWN_TX.lock()
            && let Some(tx) = guard.take()
        {
            let _ = tx.send(true);
        }
        0
    })
}

/// Returns a JSON string with the server status.
/// Caller must free the returned string with `tune_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_status() -> *mut c_char {
    barriere_abi("tune_server_status", std::ptr::null_mut(), || {
        let running = RUNNING.load(Ordering::SeqCst);
        let json = serde_json::json!({
            "running": running,
            "version": tune_core::version(),
            "engine": "rust",
        });
        let s = CString::new(json.to_string()).unwrap_or_default();
        s.into_raw()
    })
}

/// Returns the Tune server version string.
/// Caller must free the returned string with `tune_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_version() -> *mut c_char {
    barriere_abi("tune_server_version", std::ptr::null_mut(), || {
        let s = CString::new(tune_core::version()).unwrap_or_default();
        s.into_raw()
    })
}

/// Free a string previously returned by this library.
///
/// # Contrat
/// `ptr` doit provenir de `tune_server_status` ou `tune_server_version`, et
/// n'etre libere qu'une fois. C'est un contrat d'API C : `unsafe` n'a aucune
/// signification pour l'appelant Swift ou Dart, et marquer la fonction
/// `unsafe extern "C"` changerait la signature vue des liaisons existantes sans
/// rien apporter cote hote. Le lint est donc leve ICI, avec sa raison, plutot
/// que globalement.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn tune_free_string(ptr: *mut c_char) {
    barriere_abi("tune_free_string", (), || {
        if !ptr.is_null() {
            unsafe {
                let _ = CString::from_raw(ptr);
            }
        }
    })
}

/// Donne la racine du conteneur de l'application hôte (iOS :
/// `NSHomeDirectory()`), À APPELER AVANT `tune_server_start`.
///
/// Au démarrage, si la base a été écrite sous une autre racine qui n'existe
/// plus (iOS a déplacé l'application : réinstallation, restauration, mise à
/// jour), ses chemins sont réécrits sous la nouvelle. Voir `relocalisation`.
///
/// Retour : 0 succès, -2 si `root` est NULL, vide, relatif ou `/`.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_set_container_root(root: *const c_char) -> i32 {
    barriere_abi("tune_server_set_container_root", -2, || {
        if root.is_null() {
            return -2;
        }
        let Ok(racine) = unsafe { CStr::from_ptr(root) }.to_str() else {
            return -2;
        };
        let racine = racine.trim_end_matches('/');
        if !racine.starts_with('/') {
            return -2;
        }
        match CONTAINER_ROOT.lock() {
            Ok(mut g) => {
                *g = Some(racine.to_string());
                0
            }
            Err(_) => -2,
        }
    })
}

/// Lance un scan INCRÉMENTAL des dossiers de musique (les fichiers inchangés
/// sont sautés). Pour l'hôte qui revient au premier plan : un fichier déposé
/// pendant que l'application était suspendue n'a été vu par aucun
/// surveillant. Ne bloque pas : le scan tourne en tâche de fond.
///
/// Retour : 0 scan lancé, 1 un scan tourne déjà, -1 serveur pas prêt.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_scan() -> i32 {
    barriere_abi("tune_server_scan", -2, || {
        let etat = ETAT.lock().ok().and_then(|g| g.clone());
        let Some(etat) = etat else {
            return -1;
        };
        let lance = get_runtime().block_on(tune_server::routes::system::scan::spawn_library_scan(
            etat, false, None,
        ));
        if lance { 0 } else { 1 }
    })
}

/// Nom de l'appareil (iOS : `UIDevice.current.name`), pour le nom sous lequel
/// le serveur s'annonce (UPnP, mDNS) quand aucun `server_name` n'est réglé.
/// À APPELER AVANT `tune_server_start`. Un nom générique (« iPhone ») est
/// suffixé de quatre caractères stables. NULL ou vide : nom d'hôte.
///
/// Retour : 0 succès, -2 si la chaîne n'est pas de l'UTF-8.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_set_device_name(name: *const c_char) -> i32 {
    barriere_abi("tune_server_set_device_name", -2, || {
        if name.is_null() {
            tune_core::discovery::nom_annonce::definir_nom_de_l_appareil(None);
            return 0;
        }
        let Ok(nom) = unsafe { CStr::from_ptr(name) }.to_str() else {
            return -2;
        };
        tune_core::discovery::nom_annonce::definir_nom_de_l_appareil(Some(nom));
        0
    })
}

/// Différer la découverte réseau (SSDP, mDNS, annonces) jusqu'au premier
/// usage : ouverture de l'écran « Sources du réseau », recherche d'appareils,
/// ou `tune_server_start_network_discovery`. Sur iOS, la fenêtre « réseau
/// local » s'ouvre alors au moment où l'utilisateur cherche quelque chose.
/// Une fois ouverte, la découverte démarre d'emblée aux lancements suivants.
/// À APPELER AVANT `tune_server_start`. `defer` = 0 : comportement normal.
///
/// Retour : 0.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_defer_network_discovery(defer: i32) -> i32 {
    barriere_abi("tune_server_defer_network_discovery", -2, || {
        DECOUVERTE_DIFFEREE.store(defer != 0, Ordering::SeqCst);
        0
    })
}

/// Ouvre la découverte réseau différée. Sans effet si elle l'est déjà.
///
/// Retour : 0 ouverte par cet appel, 1 déjà ouverte.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_start_network_discovery() -> i32 {
    barriere_abi("tune_server_start_network_discovery", -2, || {
        if ouvrir_la_decouverte() { 0 } else { 1 }
    })
}

/// Clé maîtresse (32 octets) des identifiants des sources réseau, gardée par
/// l'application hôte — iOS : trousseau (`kSecAttrAccessibleAfterFirstUnlock
/// ThisDeviceOnly`) ; Android : chiffrée par une clé du Keystore. Tenue en
/// mémoire seulement, JAMAIS écrite sur le disque. À APPELER AVANT
/// `tune_server_start` : les sources configurées s'ouvrent au démarrage.
/// Sans elle, le serveur mobile refuse d'enregistrer un identifiant.
///
/// Retour : 0 succès, -2 si `key` est NULL ou `len` ≠ 32.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_set_master_key(key: *const u8, len: usize) -> i32 {
    barriere_abi("tune_server_set_master_key", -2, || {
        if key.is_null() || len != 32 {
            return -2;
        }
        let mut cle = [0u8; 32];
        cle.copy_from_slice(unsafe { std::slice::from_raw_parts(key, 32) });
        tune_core::sources_reseau::identifiants::installer_la_cle_de_l_hote(cle);
        {
            use std::ptr::write_volatile;
            for o in cle.iter_mut() {
                unsafe { write_volatile(o, 0) };
            }
        }
        0
    })
}

/// Le réseau a changé (iOS : `NWPathMonitor` ; Android :
/// `ConnectivityManager.NetworkCallback`) : chaque source réseau oublie ses
/// connexions et se reconnecte à la lecture suivante.
///
/// Retour : 0.
#[unsafe(no_mangle)]
pub extern "C" fn tune_server_network_changed() -> i32 {
    barriere_abi("tune_server_network_changed", -2, || {
        tune_core::sources_reseau::reseau_change();
        0
    })
}

/// Ouvre la porte de la découverte et retient, en base, qu'elle l'a été.
fn ouvrir_la_decouverte() -> bool {
    let ouverte = tune_core::discovery::porte_multicast::ouvrir();
    if ouverte && let Some(etat) = ETAT.lock().ok().and_then(|g| g.clone()) {
        let _ = tune_core::db::settings_repo::SettingsRepo::with_backend(etat.backend.clone())
            .set(REGLAGE_DECOUVERTE_OUVERTE, "1");
    }
    ouverte
}

/// Les requêtes qui valent « premier usage » de la découverte : la liste des
/// serveurs de l'écran « Sources du réseau », et la recherche d'appareils.
pub(crate) fn requete_ouvre_la_decouverte(methode: &axum::http::Method, chemin: &str) -> bool {
    use axum::http::Method;
    let chemin = chemin.trim_end_matches('/');
    (methode == Method::GET
        && (chemin == "/api/v1/network/media-servers" || chemin == "/api/v1/peers"))
        || (methode == Method::POST && chemin == "/api/v1/devices/scan")
}

/// La porte doit-elle rester fermée au démarrage ?
fn decouverte_fermee_au_demarrage(state: &tune_server::state::AppState) -> bool {
    if !DECOUVERTE_DIFFEREE.load(Ordering::SeqCst) {
        return false;
    }
    let deja = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_DECOUVERTE_OUVERTE)
        .ok()
        .flatten()
        .is_some_and(|v| v == "1");
    !deja
}

// ---------------------------------------------------------------------------
// Internal server runner
// ---------------------------------------------------------------------------

async fn run_server(
    port: u16,
    db_path: String,
    music_dirs: Vec<String>,
    web_dir: Option<String>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<(), String> {
    use tune_server::state::AppState;

    let config = config_mobile(port, db_path, music_dirs, web_dir);

    // Le serveur range plusieurs fichiers en chemin RELATIF (le cache de
    // pochettes `artwork_cache` en premier, cf. `chemins_de_donnees`). Un
    // binaire a son `WorkingDirectory` ; une application Android, elle, tourne
    // avec `/` pour répertoire courant, en lecture seule : les pochettes ne
    // s'écrivaient nulle part. On se place donc à côté de la base, seul
    // dossier dont l'hôte garantit qu'il est inscriptible.
    if let Some(dossier) = dossier_de_travail(&config.db_path)
        && let Err(e) = std::env::set_current_dir(&dossier)
    {
        tracing::warn!(dossier = %dossier.display(), error = %e, "tune_ffi_cwd_inchange");
    }

    // Initialize state
    let state = AppState::new(&config.db_path, config.port, config.clone())
        .map_err(|e| format!("init state: {e}"))?;

    demarrer_la_bibliotheque(&state, &config).await?;
    let etat_pour_l_hote = state.clone();
    #[cfg(feature = "local-audio")]
    tune_server::startup::register_local_outputs(&state).await;

    // #8189 — découverte différée (iOS) : la porte reste fermée tant que
    // l'utilisateur n'a rien cherché sur le réseau. Les annonceurs et le
    // balayage de démarrage de `spawn_background_tasks` l'attendent d'eux-mêmes ;
    // SSDP et mDNS démarrent ici à son ouverture.
    if decouverte_fermee_au_demarrage(&state) {
        info!("tune_ffi_decouverte_differee");
        tune_core::discovery::porte_multicast::fermer();
    } else {
        tune_core::discovery::porte_multicast::ouvrir();
    }
    {
        let state = state.clone();
        let config = config.clone();
        let mut arret = shutdown_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tune_core::discovery::porte_multicast::attendre() => {}
                _ = arret.changed() => return,
            }
            let oh_listener = tune_server::startup::create_oh_listener().await;
            tune_server::discovery_setup::spawn_ssdp_handler(&state, &config, oh_listener);
            let _mdns = tune_server::discovery_setup::spawn_mdns_handler(&state);
            // Le démon mDNS vit jusqu'à l'arrêt du serveur.
            let _ = arret.changed().await;
        });
    }
    tune_server::background::spawn_background_tasks(&state, &config).await;

    // Build router
    let app = tune_server::routes::router(state).layer(axum::middleware::from_fn(garde_mobile));

    // Bind and serve
    let addr: std::net::SocketAddr = ([0, 0, 0, 0], port).into();
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;

    info!(%addr, "tune_ffi_listening");
    if let Ok(mut g) = ETAT.lock() {
        *g = Some(etat_pour_l_hote);
    }

    let servi = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
            info!("tune_ffi_shutdown_signal_received");
        })
        .await
        .map_err(|e| format!("serve: {e}"));
    if let Ok(mut g) = ETAT.lock() {
        *g = None;
    }
    servi?;

    Ok(())
}

/// La configuration du serveur embarqué.
fn config_mobile(
    port: u16,
    db_path: String,
    music_dirs: Vec<String>,
    web_dir: Option<String>,
) -> tune_server::config::TuneConfig {
    let mut config = tune_server::config::TuneConfig {
        db_path,
        port,
        music_dirs,
        ..Default::default()
    };
    if let Some(ref wd) = web_dir {
        config.web_dir = wd.clone();
    }
    config.auto_scan = true;
    // Build mobile : aucune mise à jour hors du magasin d'applications. Le
    // défaut est déjà `false`, on le pose quand même pour qu'un changement de
    // défaut côté serveur ne l'active pas ici en silence.
    config.auto_update = false;
    // Capacités annoncées (`GET /system/capabilities`, #8189) : pas de
    // montage système de partages sur un téléphone.
    config.mobile = true;
    config
}

/// Ce qui, au démarrage, touche à la bibliothèque, dans l'ordre de
/// `bootstrap.rs` : relocalisation du conteneur, état initial (dont
/// `music_dirs`), scan de démarrage, surveillant de fichiers.
async fn demarrer_la_bibliotheque(
    state: &tune_server::state::AppState,
    config: &tune_server::config::TuneConfig,
) -> Result<(), String> {
    // Le conteneur a-t-il bougé ? Avant tout lecteur de chemins : réglages,
    // scan, surveillant, lecture.
    let racine = CONTAINER_ROOT.lock().ok().and_then(|g| g.clone());
    if let (Some(racine), Some(db)) = (racine, state.db.as_ref()) {
        let conn = db
            .connection()
            .lock()
            .map_err(|_| "verrou d'écriture empoisonné".to_string())?;
        let existe = |p: &str| std::path::Path::new(p).exists();
        match relocalisation::relocaliser(&conn, &racine, &config.music_dirs, &existe) {
            Ok(b) if b.ancienne.is_some() => info!(
                ancienne = b.ancienne.as_deref().unwrap_or(""),
                nouvelle = %racine,
                reecrites = b.reecrites,
                restees = b.restees,
                "tune_ffi_conteneur_relocalise"
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "tune_ffi_relocalisation_ecartee"),
        }
    }

    tune_server::startup::init_state(state, config).await;

    // Scan de démarrage et surveillant, dans l'ordre de `bootstrap.rs` : le
    // surveillant attend la fin du scan (sinon les événements rejoués
    // feraient retirer puis réinsérer ce que le scan vient d'ajouter).
    let scan_fini = if tune_server::auto_scan::scan_au_demarrage(config.auto_scan, &state.backend) {
        Some(tune_server::auto_scan::spawn_auto_scan(
            state.backend.clone(),
            state.event_bus.clone(),
        ))
    } else {
        None
    };
    if !SURVEILLANT_LANCE.swap(true, Ordering::SeqCst) {
        tune_server::auto_scan::spawn_file_watcher(
            state.backend.clone(),
            scan_fini,
            state.event_bus.clone(),
        );
    }
    Ok(())
}

/// Le dossier où se placer avant de démarrer : celui de la base, s'il est
/// désigné par un chemin absolu. Un chemin relatif ne dit rien de plus que le
/// répertoire courant, qu'on laisse alors tel quel.
pub(crate) fn dossier_de_travail(db_path: &str) -> Option<std::path::PathBuf> {
    let base = std::path::Path::new(db_path);
    if !base.is_absolute() {
        return None;
    }
    base.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
}

/// Routes que le build mobile refuse.
///
/// Le serveur tourne ici en BIBLIOTHÈQUE, dans le processus de l'application
/// hôte. Les gestes qui remplacent ou relancent un exécutable n'y ont aucun
/// sens, et certains sont dangereux :
/// - `system/update/install|apply` : une application Play ou App Store ne se
///   met pas à jour elle-même (règles des magasins) ;
/// - `system/restart` : `exec()` de `current_exe()`, c'est-à-dire du lanceur
///   de l'application hôte, pas d'un serveur ;
/// - `system/stop` : `process::exit(0)` tue l'application entière ;
/// - `appliance/*` en écriture : `systemctl poweroff`, Wi-Fi, installation
///   sur disque — propres à Tune OS.
///
/// Les lectures (`GET`) restent permises : un client qui demande l'état d'une
/// mise à jour ou de l'appliance reçoit une réponse normale.
pub(crate) fn route_refusee_sur_mobile(methode: &axum::http::Method, chemin: &str) -> bool {
    use axum::http::Method;
    let chemin = chemin.trim_end_matches('/');
    // Les routes réseau du système, lectures comprises (#8189).
    if route_reseau_refusee_sur_mobile(methode, chemin) {
        return true;
    }
    if methode == Method::GET || methode == Method::HEAD || methode == Method::OPTIONS {
        return false;
    }
    const EXACTES: &[&str] = &[
        "/api/v1/system/update/install",
        "/api/v1/system/update/apply",
        "/api/v1/system/restart",
        "/api/v1/system/stop",
    ];
    EXACTES.contains(&chemin) || chemin.starts_with("/api/v1/appliance/")
}

/// Routes réseau désactivées sur mobile (#8189).
///
/// Le montage de partages (SMB, NFS), leur exploration et le sondage d'un
/// hôte passent par des commandes du système (`mount.cifs`, `smbutil`,
/// `smbclient`, `showmount`…) qui n'existent pas dans une application, et un
/// téléphone ne monte rien. Elles sont refusées, en lecture comme en
/// écriture, sauf les deux listes tirées de la base (`/network/mounts`,
/// `/network/smb/mounts`), qui restent lisibles. Les serveurs UPnP/DLNA
/// (`/network/media-servers*`, `/network/library-sources`) ne sont pas
/// touchés : c'est l'écran « Sources du réseau ».
fn route_reseau_refusee_sur_mobile(methode: &axum::http::Method, chemin: &str) -> bool {
    use axum::http::Method;
    let Some(reste) = chemin.strip_prefix("/api/v1/network") else {
        return false;
    };
    let lecture = methode == Method::GET || methode == Method::HEAD || methode == Method::OPTIONS;
    let sous = |p: &str| reste == p || reste.starts_with(&format!("{p}/"));
    if sous("/mounts") || sous("/smb/mounts") {
        // Les listes restent lisibles ; tout geste (créer, tester, oublier) est refusé.
        return !(lecture && (reste == "/mounts" || reste == "/smb/mounts"));
    }
    sous("/smb") || sous("/shares") || sous("/scan-host")
}

async fn garde_mobile(
    requete: axum::extract::Request,
    suite: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if route_reseau_refusee_sur_mobile(requete.method(), requete.uri().path().trim_end_matches('/'))
    {
        tracing::info!(chemin = %requete.uri().path(), "tune_ffi_route_reseau_refusee_sur_mobile");
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "network_mounts_unavailable_on_mobile",
                "detail": "Les partages réseau du système (SMB, NFS) ne se montent pas dans l'application mobile.",
            })),
        )
            .into_response();
    }
    if route_refusee_sur_mobile(requete.method(), requete.uri().path()) {
        tracing::info!(chemin = %requete.uri().path(), "tune_ffi_route_refusee_sur_mobile");
        return (
            axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({
                "error": "unavailable_on_mobile",
                "detail": "Cette action n'existe pas dans l'application mobile.",
            })),
        )
            .into_response();
    }
    if requete_ouvre_la_decouverte(requete.method(), requete.uri().path())
        && !tune_core::discovery::porte_multicast::est_ouverte()
    {
        ouvrir_la_decouverte();
    }
    suite.run(requete).await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // The FFI entry points touch process-global state (`RUNNING`), so the tests
    // that manipulate it are serialized to stay deterministic under the default
    // parallel test runner.
    static TEST_GUARD: Mutex<()> = Mutex::new(());

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn null_db_path_is_rejected_without_panicking() {
        let _g = test_lock();
        // A null `db_path` must return the error code, not dereference the
        // pointer, and must not claim the running slot.
        let rc = tune_server_start(0, std::ptr::null(), std::ptr::null(), std::ptr::null());
        assert_eq!(rc, -2, "null db_path should return -2");
        assert!(
            !RUNNING.load(Ordering::SeqCst),
            "a rejected start must not mark the server running"
        );
    }

    #[test]
    fn double_start_is_rejected() {
        let _g = test_lock();
        // Simulate a server already running by claiming the slot, then a second
        // start must be rejected (-1) without spawning or binding anything.
        RUNNING.store(true, Ordering::SeqCst);
        let db = CString::new(":memory:").unwrap();
        let rc = tune_server_start(0, db.as_ptr(), std::ptr::null(), std::ptr::null());
        assert_eq!(rc, -1, "a second start should return -1 while running");
        // Restore global state for other tests.
        RUNNING.store(false, Ordering::SeqCst);
    }

    #[test]
    fn stop_when_not_running_is_rejected() {
        let _g = test_lock();
        // Stopping a server that never started must return -1, not panic.
        RUNNING.store(false, Ordering::SeqCst);
        assert_eq!(tune_server_stop(), -1);
    }

    #[test]
    fn la_racine_du_conteneur_refuse_null_relatif_et_slash() {
        let _g = test_lock();
        assert_eq!(tune_server_set_container_root(std::ptr::null()), -2);
        for refus in ["", "/", "Documents", "relatif/x"] {
            let c = CString::new(refus).unwrap();
            assert_eq!(tune_server_set_container_root(c.as_ptr()), -2, "{refus:?}");
        }
        let c = CString::new("/var/mobile/Containers/Data/Application/X/").unwrap();
        assert_eq!(tune_server_set_container_root(c.as_ptr()), 0);
        assert_eq!(
            CONTAINER_ROOT.lock().unwrap().as_deref(),
            Some("/var/mobile/Containers/Data/Application/X")
        );
        *CONTAINER_ROOT.lock().unwrap() = None;
    }

    #[test]
    fn scan_sans_serveur_rend_moins_un() {
        let _g = test_lock();
        assert!(ETAT.lock().unwrap().is_none());
        assert_eq!(tune_server_scan(), -1);
    }

    fn ecrire_wav(chemin: &std::path::Path) {
        let octets: u32 = 44_100 * 4 / 5;
        let mut v: Vec<u8> = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + octets).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&44_100u32.to_le_bytes());
        v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&octets.to_le_bytes());
        v.extend(std::iter::repeat_n(0u8, octets as usize));
        std::fs::write(chemin, v).unwrap();
    }

    fn pistes(state: &tune_server::state::AppState) -> Vec<String> {
        let conn = state.db.as_ref().unwrap().connection().lock().unwrap();
        let mut st = conn
            .prepare("SELECT file_path FROM tracks ORDER BY file_path")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    async fn attendre(
        state: &tune_server::state::AppState,
        n: usize,
        secondes: u64,
    ) -> Vec<String> {
        let fin = std::time::Instant::now() + std::time::Duration::from_secs(secondes);
        loop {
            let p = pistes(state);
            if p.len() >= n || std::time::Instant::now() > fin {
                return p;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Le parcours de l'app iOS, par le vrai chemin de démarrage
    /// (`demarrer_la_bibliotheque`) : scan du dossier de l'app, déplacement
    /// du conteneur (les chemins suivent, rien n'est réimporté), puis un
    /// fichier déposé pendant que le serveur tourne, vu par le surveillant
    /// sans aucun scan demandé.
    #[test]
    fn conteneur_deplace_puis_fichier_depose_vu_par_le_surveillant() {
        use tune_server::state::AppState;
        let _g = test_lock();
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
            .with_test_writer()
            .try_init();
        // Sous `target/`, jamais sous `/tmp` : `is_tune_temp_file` écarte du
        // scan tout ce qui vit dans le dossier temporaire du système.
        let sous = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&sous).unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("tune-ffi-conteneur-")
            .tempdir_in(&sous)
            .unwrap();
        let a = tmp.path().join("AAAA-1111");
        let b = tmp.path().join("BBBB-2222");
        let musique = |r: &std::path::Path| r.join("Documents/Music");
        let base = |r: &std::path::Path| r.join("Library/Tune/tune.db");
        std::fs::create_dir_all(musique(&a).join("Album")).unwrap();
        std::fs::create_dir_all(base(&a).parent().unwrap()).unwrap();
        ecrire_wav(&musique(&a).join("Album/01.wav"));
        let texte = |p: std::path::PathBuf| p.to_str().unwrap().to_string();

        let demarrer = |racine: &std::path::Path| {
            *CONTAINER_ROOT.lock().unwrap() = Some(texte(racine.to_path_buf()));
            let config = config_mobile(0, texte(base(racine)), vec![texte(musique(racine))], None);
            let state = AppState::new(&config.db_path, 0, config.clone()).unwrap();
            (state, config)
        };

        get_runtime().block_on(async {
            // Conteneur A. Le surveillant du processus est réservé à B.
            SURVEILLANT_LANCE.store(true, Ordering::SeqCst);
            let (state, config) = demarrer(&a);
            demarrer_la_bibliotheque(&state, &config).await.unwrap();
            let p = attendre(&state, 1, 90).await;
            assert_eq!(
                p,
                vec![texte(musique(&a).join("Album/01.wav"))],
                "scan de A"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        });

        // iOS déplace l'application : nouveau conteneur, l'ancien disparaît.
        std::fs::rename(&a, &b).unwrap();

        get_runtime().block_on(async {
            SURVEILLANT_LANCE.store(false, Ordering::SeqCst);
            let (state, config) = demarrer(&b);
            demarrer_la_bibliotheque(&state, &config).await.unwrap();
            // Relocalisée AVANT le scan : la piste suit, elle n'est pas doublée.
            assert_eq!(
                pistes(&state),
                vec![texte(musique(&b).join("Album/01.wav"))],
                "la piste n'a pas suivi le conteneur"
            );
            // Fin du scan de démarrage, armement du surveillant (+ 2 s de purge).
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            assert_eq!(pistes(&state).len(), 1, "le scan de B a réimporté la piste");

            // Dépôt par l'app Fichiers, serveur en marche, aucun scan demandé.
            std::fs::create_dir_all(musique(&b).join("Neuf")).unwrap();
            ecrire_wav(&musique(&b).join("Neuf/02.wav"));
            let p = attendre(&state, 2, 90).await;
            assert_eq!(
                p,
                vec![
                    texte(musique(&b).join("Album/01.wav")),
                    texte(musique(&b).join("Neuf/02.wav")),
                ],
                "le surveillant n'a pas importé le fichier déposé"
            );
        });
        *CONTAINER_ROOT.lock().unwrap() = None;
    }

    #[test]
    fn le_dossier_de_travail_est_celui_de_la_base() {
        assert_eq!(
            dossier_de_travail("/data/user/0/com.mozaiklabs.tune/app_flutter/tune.db"),
            Some(std::path::PathBuf::from(
                "/data/user/0/com.mozaiklabs.tune/app_flutter"
            ))
        );
        // Relatif ou nu : on ne bouge pas.
        assert_eq!(dossier_de_travail("tune.db"), None);
        assert_eq!(dossier_de_travail(":memory:"), None);
        assert_eq!(dossier_de_travail("data/tune.db"), None);
    }

    #[test]
    fn les_gestes_de_processus_sont_refuses_sur_mobile() {
        use axum::http::Method;
        for chemin in [
            "/api/v1/system/update/install",
            "/api/v1/system/update/apply",
            "/api/v1/system/update/apply/",
            "/api/v1/system/restart",
            "/api/v1/system/stop",
            "/api/v1/appliance/shutdown",
            "/api/v1/appliance/wifi/connect",
        ] {
            assert!(
                route_refusee_sur_mobile(&Method::POST, chemin),
                "{chemin} doit être refusé"
            );
        }
    }

    #[test]
    fn les_lectures_et_le_reste_passent_sur_mobile() {
        use axum::http::Method;
        // Les lectures d'état restent permises, et rien d'autre n'est touché.
        assert!(!route_refusee_sur_mobile(
            &Method::GET,
            "/api/v1/system/update/status"
        ));
        assert!(!route_refusee_sur_mobile(
            &Method::GET,
            "/api/v1/system/update/check"
        ));
        assert!(!route_refusee_sur_mobile(
            &Method::GET,
            "/api/v1/appliance/status"
        ));
        assert!(!route_refusee_sur_mobile(
            &Method::POST,
            "/api/v1/zones/1/play"
        ));
        assert!(!route_refusee_sur_mobile(
            &Method::POST,
            "/api/v1/system/scan"
        ));
        assert!(!route_refusee_sur_mobile(
            &Method::POST,
            "/api/v1/system/restarted"
        ));
    }

    /// #8189 — les routes réseau qui montent, explorent ou sondent par les
    /// commandes du système sont refusées sur mobile, en écriture comme en
    /// lecture ; les deux listes tirées de la base restent lisibles, et les
    /// serveurs UPnP/DLNA ne sont pas touchés.
    #[test]
    fn les_routes_reseau_du_systeme_sont_refusees_sur_mobile() {
        use axum::http::Method;
        for (m, chemin) in [
            (Method::POST, "/api/v1/network/smb/mount"),
            (Method::POST, "/api/v1/network/smb/mount/"),
            (Method::POST, "/api/v1/network/mounts"),
            (Method::POST, "/api/v1/network/mounts/test"),
            (Method::DELETE, "/api/v1/network/mounts/3"),
            (Method::DELETE, "/api/v1/network/smb/mounts/3"),
            (Method::POST, "/api/v1/network/smb/discover"),
            (Method::GET, "/api/v1/network/smb/discover"),
            (Method::GET, "/api/v1/network/shares"),
            (Method::GET, "/api/v1/network/shares/7"),
            (Method::GET, "/api/v1/network/scan-host"),
        ] {
            assert!(
                route_refusee_sur_mobile(&m, chemin),
                "{m} {chemin} doit être refusé"
            );
        }
        for (m, chemin) in [
            (Method::GET, "/api/v1/network/mounts"),
            (Method::GET, "/api/v1/network/smb/mounts"),
            (Method::GET, "/api/v1/network/media-servers"),
            (Method::GET, "/api/v1/network/media-servers/abc/browse"),
            (Method::POST, "/api/v1/network/media-servers/abc/indexer"),
            (
                Method::DELETE,
                "/api/v1/network/media-servers/abc/bibliotheque",
            ),
            (Method::POST, "/api/v1/network/library-sources"),
            (Method::POST, "/api/v1/network-quality"),
        ] {
            assert!(
                !route_refusee_sur_mobile(&m, chemin),
                "{m} {chemin} doit passer"
            );
        }
    }

    #[tokio::test]
    async fn la_garde_refuse_le_montage_smb_avec_un_code_explicite() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use std::sync::atomic::AtomicUsize;
        use tower::ServiceExt;

        static MONTAGES: AtomicUsize = AtomicUsize::new(0);
        let app = axum::Router::new()
            .route(
                "/api/v1/network/smb/mount",
                axum::routing::post(|| async {
                    MONTAGES.fetch_add(1, Ordering::SeqCst);
                    "monté"
                }),
            )
            .layer(axum::middleware::from_fn(garde_mobile));
        let rep = app
            .oneshot(
                Request::post("/api/v1/network/smb/mount")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::FORBIDDEN);
        let corps = axum::body::to_bytes(rep.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&corps).unwrap();
        assert_eq!(v["error"], "network_mounts_unavailable_on_mobile");
        assert_eq!(
            MONTAGES.load(Ordering::SeqCst),
            0,
            "le montage ne doit pas tourner"
        );
    }

    #[test]
    fn les_requetes_de_premier_usage_ouvrent_la_decouverte() {
        use axum::http::Method;
        assert!(requete_ouvre_la_decouverte(
            &Method::GET,
            "/api/v1/network/media-servers"
        ));
        assert!(requete_ouvre_la_decouverte(&Method::GET, "/api/v1/peers"));
        assert!(requete_ouvre_la_decouverte(
            &Method::POST,
            "/api/v1/devices/scan"
        ));
        assert!(!requete_ouvre_la_decouverte(&Method::GET, "/api/v1/zones"));
        assert!(!requete_ouvre_la_decouverte(
            &Method::GET,
            "/api/v1/system/health"
        ));
        assert!(!requete_ouvre_la_decouverte(
            &Method::GET,
            "/api/v1/network/media-servers/x/browse"
        ));
    }

    /// Différée puis ouverte par l'écran : la porte s'ouvre, la base le
    /// retient, et le démarrage suivant ne la ferme plus.
    #[test]
    fn decouverte_differee_ouverte_au_premier_usage_puis_retenue() {
        use tune_core::discovery::porte_multicast as porte;
        let _g = test_lock();
        let dossier = tempfile::tempdir().unwrap();
        let base = dossier.path().join("tune.db");
        let base = base.to_str().unwrap();
        let state = tune_server::state::AppState::new(base, 0, Default::default()).unwrap();
        assert!(
            !decouverte_fermee_au_demarrage(&state),
            "non différée par défaut"
        );
        assert_eq!(tune_server_defer_network_discovery(1), 0);
        assert!(
            decouverte_fermee_au_demarrage(&state),
            "différée, jamais ouverte"
        );

        porte::fermer();
        *ETAT.lock().unwrap() = Some(state.clone());
        let app = axum::Router::new()
            .route(
                "/api/v1/network/media-servers",
                axum::routing::get(|| async { "[]" }),
            )
            .layer(axum::middleware::from_fn(garde_mobile));
        get_runtime().block_on(async {
            use tower::ServiceExt;
            let rep = app
                .oneshot(
                    axum::http::Request::get("/api/v1/network/media-servers")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rep.status(), axum::http::StatusCode::OK);
        });
        assert!(
            porte::est_ouverte(),
            "l'écran Sources du réseau ouvre la découverte"
        );
        // Redémarrage : un nouvel état sur la même base.
        *ETAT.lock().unwrap() = None;
        drop(state);
        let relance = tune_server::state::AppState::new(base, 0, Default::default()).unwrap();
        assert!(
            !decouverte_fermee_au_demarrage(&relance),
            "au lancement suivant, la découverte démarre d'emblée"
        );
        assert_eq!(tune_server_start_network_discovery(), 1, "déjà ouverte");
        assert_eq!(tune_server_defer_network_discovery(0), 0);
    }

    #[test]
    fn la_cle_maitresse_de_l_hote_est_tenue_en_memoire() {
        let _g = test_lock();
        use tune_core::sources_reseau::identifiants as ids;
        assert_eq!(tune_server_set_master_key(std::ptr::null(), 32), -2);
        let courte = [1u8; 16];
        assert_eq!(tune_server_set_master_key(courte.as_ptr(), 16), -2);
        ids::oublier_la_cle_de_l_hote();
        assert!(!ids::cle_de_l_hote_installee());
        let cle = [3u8; 32];
        assert_eq!(tune_server_set_master_key(cle.as_ptr(), 32), 0);
        assert!(ids::cle_de_l_hote_installee());
        let i = ids::Identifiants {
            utilisateur: "u".into(),
            mot_de_passe: "p".into(),
            domaine: None,
        };
        let scelle = ids::sceller(&i, true).unwrap();
        assert_eq!(ids::ouvrir(&scelle, true).unwrap(), i);
        ids::oublier_la_cle_de_l_hote();
        assert_eq!(tune_server_network_changed(), 0);
    }

    #[test]
    fn le_nom_de_l_appareil_passe_au_nom_annonce() {
        let _g = test_lock();
        let c = CString::new("iPad de Léa").unwrap();
        assert_eq!(tune_server_set_device_name(c.as_ptr()), 0);
        assert_eq!(
            tune_core::discovery::nom_annonce::nom_annonce(None, "uuid:abcd"),
            "iPad de Léa"
        );
        assert_eq!(tune_server_set_device_name(std::ptr::null()), 0);
        assert_eq!(
            tune_core::discovery::nom_annonce::nom_de_l_appareil(),
            tune_core::discovery::system_hostname()
        );
    }

    #[tokio::test]
    async fn la_garde_rend_403_avant_le_gestionnaire() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use std::sync::atomic::AtomicUsize;
        use tower::ServiceExt;

        static APPELS: AtomicUsize = AtomicUsize::new(0);
        let app = axum::Router::new()
            .route(
                "/api/v1/system/restart",
                axum::routing::post(|| async {
                    APPELS.fetch_add(1, Ordering::SeqCst);
                    "relancé"
                }),
            )
            .route("/api/v1/zones", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(garde_mobile));

        let refus = app
            .clone()
            .oneshot(
                Request::post("/api/v1/system/restart")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refus.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            APPELS.load(Ordering::SeqCst),
            0,
            "le gestionnaire ne doit pas tourner"
        );

        let permis = app
            .oneshot(Request::post("/api/v1/zones").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(permis.status(), StatusCode::OK);
    }

    /// L'en-tete C (`include/tuneserver.h`, lu par l'app iOS) doit declarer
    /// exactement les fonctions exportees ici : une entree ajoutee d'un cote
    /// et oubliee de l'autre se verrait seulement a l'edition de liens Xcode.
    #[test]
    fn l_entete_c_suit_les_exports() {
        let source = include_str!("lib.rs");
        let entete = include_str!("../include/tuneserver.h");
        let mut exports: Vec<&str> = Vec::new();
        let mut apres_no_mangle = false;
        for ligne in source.lines() {
            let l = ligne.trim();
            if l == "#[unsafe(no_mangle)]" {
                apres_no_mangle = true;
            } else if apres_no_mangle && l.starts_with("pub extern \"C\" fn ") {
                let nom = l["pub extern \"C\" fn ".len()..]
                    .split('(')
                    .next()
                    .unwrap_or("");
                exports.push(nom);
                apres_no_mangle = false;
            }
        }
        assert!(exports.len() >= 5, "exports trouves : {exports:?}");
        for nom in &exports {
            assert!(
                entete.contains(&format!("{nom}(")),
                "{nom} absent de tuneserver.h"
            );
        }
        let declares = entete
            .lines()
            .filter(|l| {
                l.contains("tune_")
                    && l.contains('(')
                    && !l.trim_start().starts_with('*')
                    && !l.trim_start().starts_with("/*")
            })
            .count();
        assert_eq!(
            declares,
            exports.len(),
            "tuneserver.h declare une fonction que lib.rs n'exporte pas"
        );
    }
}
