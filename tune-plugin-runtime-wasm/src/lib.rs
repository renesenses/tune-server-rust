//! Embedded WASM runtime for the Tune plugin ABI — **P0 + P1** of the RFC
//! (`docs/plugins/PLUGIN_ABI_RFC.md`, §3.1–3.4, §7).
//!
//! **P0** is the minimal executable core: **load, instantiate, and call** a
//! plugin's exports with **JSON marshalling over linear memory**, inside a
//! **resource-limited** [`wasmtime`] sandbox.
//!
//! **P1** adds the **host-functions** (capabilities) a plugin may import from
//! the `"tune"` module — `host_log`, `host_queue_get`, `host_queue_add`,
//! `host_now_playing`, `host_play`, `host_pause`, `host_emit` — each **gated by
//! the plugin's granted `permissions`** (deny-by-default, RFC §3.4). The host
//! side is abstracted behind the [`HostContext`] trait so P1 is unit-testable
//! with a mock; wiring it to the real server (`AppState`) is P2 and is
//! intentionally absent here.
//!
//! **#4716** (tranche 1 de l'épique #4715, « Playlists converter ») élargit
//! cette surface à quatre permissions de plus — `playlists`, `streaming`,
//! `library` et `kv` — sur le modèle exact des précédentes. `library` est
//! arrivée après coup : sans recherche ni appariement LOCAL, le greffon ne
//! savait aller que de la bibliothèque VERS un service et refusait le sens
//! inverse. Trois règles s'y lisent :
//! aucune capacité ne SUPPRIME quoi que ce soit (ni playlist, ni piste, ni
//! favori : un greffon ne doit pas pouvoir effacer chez un service) ; toute
//! écriture chez un service purge le cache de contenu utilisateur côté hôte ;
//! et le stockage clé/valeur est cloisonné par l'identifiant du greffon, que
//! l'hôte range dans le [`Store`] au chargement et qu'aucun JSON d'entrée ne
//! peut usurper.
//!
//! # Marshalling convention
//!
//! WASM can only pass integers, so structured data crosses as **UTF-8 JSON in
//! the plugin's linear memory**, addressed by `(ptr: u32, len: u32)`. This
//! matches the convention already implemented by the Party-mode plugin crate
//! (`~/DEV/tune-plugin-party`, `dispatch_c`):
//!
//! * **Host → plugin**: the host calls the plugin's `alloc(len) -> ptr`, writes
//!   `len` bytes at `ptr`, then calls the dispatch export with `(ptr, len)`.
//! * **Plugin → host**: the dispatch export returns a **packed `u64`**:
//!   `((out_ptr as u64) << 32) | (out_len as u64)` — high 32 bits = pointer,
//!   low 32 bits = byte length. The buffer at `out_ptr` is **raw** UTF-8 JSON of
//!   `out_len` bytes (the Party crate's `pack_result`: no inner length prefix).
//! * The host reads the JSON, then frees the output buffer with
//!   `dealloc(out_ptr, out_len)` and its own input buffer with
//!   `dealloc(in_ptr, in_len)`.
//!
//! The dispatch export is resolved by name as `plugin_dispatch` (the RFC §3.3
//! name) and, if absent, `dispatch_c` (the name the Party/DJ crates actually
//! export) — so a real `party.wasm` loads unchanged. Both must have wasm type
//! `(i32, i32) -> i64`.
//!
//! Note: the DJ crate's `dispatch_c` uses an older, wider signature
//! `(action_ptr, action_len, payload_ptr, payload_len) -> ptr` with a
//! length-prefixed result; that shape does not match the RFC dispatch contract
//! and is handled by the native composition model (RFC §6), not this runtime.
//!
//! # Host-functions (P1)
//!
//! The host installs its capabilities as [`Linker`] imports under the module
//! name `"tune"`. Each JSON-returning import has wasm type `(i32, i32) -> i64`
//! (same packed-`(ptr,len)` convention as dispatch); `host_log`/`host_emit`
//! return unit. Inside an import the host: reads the input JSON from the
//! plugin's memory, checks the required permission against the set stored in the
//! [`Store`] data (denied ⇒ a graceful `{"error":"permission_denied",…}` JSON,
//! never a hard trap), calls the [`HostContext`] method, then writes the JSON
//! result back into the plugin's memory via the plugin's own `alloc` and returns
//! the packed pointer. The plugin's `memory` and `alloc` are stashed in the
//! store data after instantiation precisely so the imports can allocate and
//! write into guest memory from within a [`Caller`].
//!
//! # Input framing
//!
//! [`WasmPlugin::dispatch`] writes the raw JSON bytes it is given at `(ptr,len)`
//! (this is what the echo test and the RFC's request/response model exercise).
//! [`WasmPlugin::dispatch_action`] additionally frames the buffer as
//! `[action_len: u32 LE][action][payload]`, which is exactly what the current
//! Party crate's `dispatch_c` decodes — provided for real-plugin compatibility.
//! Both share the same output convention above.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use wasmtime::{
    Caller, Config, Engine, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc,
};

/// ABI version the host implements. A plugin whose `abi_version()` export does
/// not return this exact value is rejected at load time (RFC §4).
pub const HOST_ABI_VERSION: u32 = 1;

/// The host-side capability surface a plugin can reach through the `"tune"`
/// import module (RFC §3.4).
///
/// This is the seam between the wasm sandbox and the rest of the server: the
/// host-functions installed on the [`Linker`] are thin JSON adapters that read
/// the plugin's request, check permissions, and forward to one of these
/// methods. Keeping it a trait means P1 is fully unit-testable against a mock;
/// P2 will provide the real implementation backed by `AppState`.
///
/// The set below is *representative* — enough to exercise the `queue`,
/// `playback` and `events` permissions plus the always-allowed `log`. The
/// remaining RFC host-functions (`queue_set/remove/move`, `stop/next/prev/seek`,
/// `search`, `kv_*`, `fetch`, …) follow the identical pattern and will be added
/// as their features land.
pub trait HostContext: Send + Sync {
    /// Always-allowed diagnostic log (no permission required).
    fn log(&self, level: &str, msg: &str);
    /// `queue` — read a zone's play queue.
    fn queue_get(&self, zone: i64) -> Result<serde_json::Value, String>;
    /// `queue` — append tracks to a zone's play queue.
    fn queue_add(&self, zone: i64, tracks: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `playback` — current now-playing state for a zone.
    fn now_playing(&self, zone: i64) -> Result<serde_json::Value, String>;
    /// `playback` — start/resume playback for a zone.
    fn play(&self, zone: i64, req: serde_json::Value) -> Result<serde_json::Value, String>;
    /// `playback` — pause a zone.
    fn pause(&self, zone: i64) -> Result<serde_json::Value, String>;
    /// `events` — emit an event onto the host event bus (fire-and-forget).
    fn emit(&self, event: &str, payload: serde_json::Value);

    // -----------------------------------------------------------------------
    // #4716 — playlists, streaming, stockage clé/valeur (épique #4715)
    //
    // Trois règles tiennent toute cette tranche :
    //
    // 1. **Aucune suppression.** Ni playlist, ni piste, ni favori : la liste
    //    ci-dessous ne porte QUE de la lecture et de l'ajout. Un greffon ne
    //    doit pas pouvoir effacer chez un service — c'est irréversible et
    //    l'utilisateur n'a rien demandé. Tout ajout ici doit se lire comme tel.
    // 2. **Toute écriture chez un service purge son cache de contenu
    //    utilisateur** côté implémentation, sinon l'écran sert la liste
    //    mémorisée 2 minutes et la playlist créée « n'existe pas ».
    // 3. **Le stockage clé/valeur est cloisonné par greffon** : l'identifiant
    //    du greffon appelant est passé par l'hôte (il vient du `Store`, jamais
    //    du JSON que le greffon fournit), et préfixe la clé.
    // -----------------------------------------------------------------------

    /// `playlists` — lister les playlists locales du profil actif.
    fn playlists_list(&self, limit: i64, offset: i64) -> Result<serde_json::Value, String>;
    /// `playlists` — lire les pistes d'une playlist locale.
    fn playlist_tracks(&self, playlist_id: i64) -> Result<serde_json::Value, String>;
    /// `playlists` — créer une playlist locale (jamais en effacer une).
    fn playlist_create(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    /// `playlists` — AJOUTER des pistes à une playlist locale.
    fn playlist_add_tracks(
        &self,
        playlist_id: i64,
        track_ids: Vec<i64>,
    ) -> Result<serde_json::Value, String>;

    /// `streaming` — les services de streaming AUTHENTIFIÉS, et s'ils savent
    /// écrire.
    fn streaming_services(&self) -> Result<serde_json::Value, String>;
    /// `streaming` — les playlists de l'utilisateur chez un service.
    fn streaming_playlists(&self, service: &str) -> Result<serde_json::Value, String>;
    /// `streaming` — les pistes d'une playlist d'un service.
    fn streaming_playlist_tracks(
        &self,
        service: &str,
        playlist_id: &str,
    ) -> Result<serde_json::Value, String>;
    /// `streaming` — créer une playlist CHEZ un service (jamais en effacer
    /// une). L'implémentation purge le cache de contenu utilisateur ensuite.
    fn streaming_playlist_create(
        &self,
        service: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<serde_json::Value, String>;
    /// `streaming` — AJOUTER des pistes à une playlist d'un service.
    /// L'implémentation purge le cache de contenu utilisateur ensuite.
    fn streaming_playlist_add_tracks(
        &self,
        service: &str,
        playlist_id: &str,
        track_ids: Vec<String>,
    ) -> Result<serde_json::Value, String>;
    /// `streaming` — apparier un titre connu chez un service, avec
    /// l'appariement déjà écrit pour la fusion de playlists (jamais un second).
    ///
    /// Rend PLUSIEURS candidats classés, le verdict en tête : l'appelant qui
    /// applique ensuite sa propre règle (le greffon refuse un écart de durée de
    /// plus de 3 s) doit pouvoir redescendre d'un cran au lieu de conclure
    /// « introuvable ».
    fn streaming_match_track(
        &self,
        service: &str,
        title: &str,
        artist: &str,
        isrc: &str,
        duration_ms: u64,
    ) -> Result<serde_json::Value, String>;

    /// `library` — chercher dans la bibliothèque LOCALE (recherche plein
    /// texte du serveur, jamais une seconde).
    fn library_search(&self, query: &str, limit: i64) -> Result<serde_json::Value, String>;
    /// `library` — apparier un titre connu SUR la bibliothèque locale, dans la
    /// même forme que [`HostContext::streaming_match_track`] : sans elle, un
    /// convertisseur ne sait aller que de la bibliothèque VERS un service.
    fn library_match_track(
        &self,
        title: &str,
        artist: &str,
        isrc: &str,
        duration_ms: u64,
    ) -> Result<serde_json::Value, String>;

    /// `kv` — lire une valeur du stockage CLOISONNÉ de `plugin_id`.
    fn kv_get(&self, plugin_id: &str, key: &str) -> Result<serde_json::Value, String>;
    /// `kv` — écrire une valeur dans le stockage cloisonné de `plugin_id`.
    fn kv_set(
        &self,
        plugin_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// `kv` — lister les clés du greffon commençant par `prefix` (sans le
    /// préfixe de cloisonnement, qu'un greffon n'a jamais à connaître).
    fn kv_list(&self, plugin_id: &str, prefix: &str) -> Result<serde_json::Value, String>;
}

/// A [`HostContext`] that grants nothing useful: `log`/`emit` are no-ops and
/// every capability returns an error. Used for the P0 constructors
/// ([`WasmPlugin::load`]/[`WasmPlugin::from_bytes`]) where no host is wired —
/// combined with an empty permission set it is deny-by-default anyway.
struct NoHost;

impl HostContext for NoHost {
    fn log(&self, _level: &str, _msg: &str) {}
    fn queue_get(&self, _zone: i64) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn queue_add(
        &self,
        _zone: i64,
        _tracks: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn now_playing(&self, _zone: i64) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn play(&self, _zone: i64, _req: serde_json::Value) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn pause(&self, _zone: i64) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn emit(&self, _event: &str, _payload: serde_json::Value) {}

    fn playlists_list(&self, _limit: i64, _offset: i64) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn playlist_tracks(&self, _playlist_id: i64) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn playlist_create(
        &self,
        _name: &str,
        _description: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn playlist_add_tracks(
        &self,
        _playlist_id: i64,
        _track_ids: Vec<i64>,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn streaming_services(&self) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn streaming_playlists(&self, _service: &str) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn streaming_playlist_tracks(
        &self,
        _service: &str,
        _playlist_id: &str,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn streaming_playlist_create(
        &self,
        _service: &str,
        _name: &str,
        _description: Option<&str>,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn streaming_playlist_add_tracks(
        &self,
        _service: &str,
        _playlist_id: &str,
        _track_ids: Vec<String>,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn streaming_match_track(
        &self,
        _service: &str,
        _title: &str,
        _artist: &str,
        _isrc: &str,
        _duration_ms: u64,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn library_search(&self, _query: &str, _limit: i64) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn library_match_track(
        &self,
        _title: &str,
        _artist: &str,
        _isrc: &str,
        _duration_ms: u64,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn kv_get(&self, _plugin_id: &str, _key: &str) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn kv_set(
        &self,
        _plugin_id: &str,
        _key: &str,
        _value: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
    fn kv_list(&self, _plugin_id: &str, _prefix: &str) -> Result<serde_json::Value, String> {
        Err("no host context wired".to_string())
    }
}

/// Resource limits applied to a loaded plugin (RFC §3.1).
///
/// * `max_memory_bytes` caps the plugin's linear memory growth.
/// * `fuel` bounds the instruction budget of a **single** call; it is
///   replenished before every call so one runaway call is killed (trap →
///   `Err`) without starving later calls.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum linear-memory size, in bytes.
    pub max_memory_bytes: usize,
    /// Per-call execution fuel budget (wasmtime charges ~1 unit / instruction).
    pub fuel: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // RFC §3.1 suggests ~64 MiB.
            max_memory_bytes: 64 * 1024 * 1024,
            // Generous but finite: a normal JSON dispatch is a few thousand
            // instructions; an infinite loop exhausts this and traps.
            fuel: 100_000_000,
        }
    }
}

/// Per-`Store` host state.
///
/// Beyond the [`StoreLimits`] that enforce the memory cap (P0), it carries the
/// P1 host-function context: the [`HostContext`] implementation, the set of
/// granted permissions used for deny-by-default gating, and — populated right
/// after instantiation — the plugin's `memory` and `alloc` so host imports can
/// read from and allocate into guest memory from inside a [`Caller`].
struct StoreData {
    limits: StoreLimits,
    /// Host capability implementation the imports forward to (P1).
    ctx: Arc<dyn HostContext>,
    /// Permissions granted to this plugin (from its manifest). A host-function
    /// whose required permission is absent fails the call (RFC §3.4).
    permissions: HashSet<String>,
    /// Identifiant de manifeste du greffon qui possède ce `Store` (#4716).
    ///
    /// Il vient de l'HÔTE au chargement, jamais du JSON qu'un greffon envoie :
    /// c'est ce qui cloisonne le stockage clé/valeur. Vide quand aucun hôte
    /// n'est câblé ([`WasmPlugin::load`]) — les capacités `kv` refusent alors,
    /// faute de savoir à qui appartiendrait la clé.
    plugin_id: String,
    /// The plugin's linear memory, set after instantiation. `None` only during
    /// instantiation, before any host-function can possibly run.
    memory: Option<Memory>,
    /// The plugin's `alloc(len) -> ptr`, used by host imports to allocate the
    /// output buffer they write the JSON result into.
    alloc: Option<TypedFunc<u32, u32>>,
}

/// Process-wide wasmtime [`Engine`]. Fuel consumption is enabled here so every
/// store created from it can be fuel-metered (RFC §3.1).
fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        // Enable per-call fuel metering so a runaway plugin call is trapped
        // instead of hanging the host.
        config.consume_fuel(true);
        Engine::new(&config).expect("build wasmtime engine")
    })
}

// ---------------------------------------------------------------------------
// P1 host-function plumbing
// ---------------------------------------------------------------------------

/// Read `len` bytes at `ptr` from the plugin's linear memory, from inside a
/// host import. The `memory` is copied out of the store data (it is `Copy`),
/// releasing the borrow before the read.
fn guest_read(
    caller: &mut Caller<'_, StoreData>,
    ptr: i32,
    len: i32,
) -> Result<Vec<u8>, wasmtime::Error> {
    let memory = caller
        .data()
        .memory
        .ok_or_else(|| wasmtime::Error::msg("plugin memory not initialised"))?;
    let len = usize::try_from(len).map_err(|_| wasmtime::Error::msg("negative length"))?;
    let mut buf = vec![0u8; len];
    memory
        .read(&*caller, ptr as usize, &mut buf)
        .map_err(|e| wasmtime::Error::msg(format!("read guest memory: {e}")))?;
    Ok(buf)
}

/// Serialise `value` to JSON, `alloc` a buffer in the plugin's memory, write the
/// bytes, and return the packed `(ptr << 32) | len` (as `i64`). This is the
/// host→plugin return path shared by every JSON-returning host import.
fn guest_write_json(
    caller: &mut Caller<'_, StoreData>,
    value: &serde_json::Value,
) -> Result<i64, wasmtime::Error> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| wasmtime::Error::msg(format!("serialise host result: {e}")))?;
    let memory = caller
        .data()
        .memory
        .ok_or_else(|| wasmtime::Error::msg("plugin memory not initialised"))?;
    let alloc = caller
        .data()
        .alloc
        .clone()
        .ok_or_else(|| wasmtime::Error::msg("plugin alloc not initialised"))?;
    let out_len =
        u32::try_from(bytes.len()).map_err(|_| wasmtime::Error::msg("host result too large"))?;
    let out_ptr = alloc
        .call(&mut *caller, out_len)
        .map_err(|e| wasmtime::Error::msg(format!("plugin `alloc` trapped in host call: {e}")))?;
    memory
        .write(&mut *caller, out_ptr as usize, &bytes)
        .map_err(|e| wasmtime::Error::msg(format!("write host result to guest memory: {e}")))?;
    Ok((((out_ptr as u64) << 32) | out_len as u64) as i64)
}

/// Shared body for a JSON-in / JSON-out host import: enforce the required
/// permission (deny-by-default → graceful `permission_denied` JSON), parse the
/// input, run `f` against the [`HostContext`], then marshal the result back.
///
/// A logical error from the `HostContext` becomes `{"error": <msg>}` (not a
/// trap) so plugins can handle it; only genuine host-side failures (bad guest
/// memory, alloc trap) propagate as a wasm trap.
fn host_json_call<F>(
    caller: &mut Caller<'_, StoreData>,
    ptr: i32,
    len: i32,
    required_perm: &str,
    f: F,
) -> Result<i64, wasmtime::Error>
where
    F: FnOnce(&Arc<dyn HostContext>, serde_json::Value) -> Result<serde_json::Value, String>,
{
    host_json_call_du_greffon(caller, ptr, len, required_perm, |ctx, _greffon, v| {
        f(ctx, v)
    })
}

/// Comme [`host_json_call`], mais le corps reçoit aussi l'IDENTIFIANT du
/// greffon appelant, lu dans le [`Store`] (#4716).
///
/// Le cloisonnement du stockage clé/valeur en dépend : la clé écrite en base
/// est préfixée par cet identifiant, et comme il vient du chargement et non du
/// JSON d'entrée, un greffon ne peut pas se faire passer pour un autre ni lire
/// son état.
fn host_json_call_du_greffon<F>(
    caller: &mut Caller<'_, StoreData>,
    ptr: i32,
    len: i32,
    required_perm: &str,
    f: F,
) -> Result<i64, wasmtime::Error>
where
    F: FnOnce(&Arc<dyn HostContext>, &str, serde_json::Value) -> Result<serde_json::Value, String>,
{
    if !caller.data().permissions.contains(required_perm) {
        let denied = serde_json::json!({
            "error": "permission_denied",
            "permission": required_perm,
        });
        return guest_write_json(caller, &denied);
    }
    let input = guest_read(caller, ptr, len)?;
    let value: serde_json::Value = serde_json::from_slice(&input)
        .map_err(|e| wasmtime::Error::msg(format!("host call: invalid input JSON: {e}")))?;
    let ctx = caller.data().ctx.clone();
    let plugin_id = caller.data().plugin_id.clone();
    let result = match f(&ctx, &plugin_id, value) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "error": e }),
    };
    guest_write_json(caller, &result)
}

/// Lire un champ texte d'un objet d'entrée, vide à défaut.
fn texte(value: &serde_json::Value, cle: &str) -> String {
    value
        .get(cle)
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Lire un champ texte FACULTATIF : `None` si absent, `null` ou vide.
fn texte_facultatif(value: &serde_json::Value, cle: &str) -> Option<String> {
    value
        .get(cle)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Lire une liste d'identifiants de pistes LOCALES (des entiers).
fn ids_entiers(value: &serde_json::Value, cle: &str) -> Vec<i64> {
    value
        .get(cle)
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_i64).collect())
        .unwrap_or_default()
}

/// Lire une liste d'identifiants de pistes de SERVICE (des chaînes).
fn ids_textes(value: &serde_json::Value, cle: &str) -> Vec<String> {
    value
        .get(cle)
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Extract the `zone` field (defaulting to 0) from a host-call input object.
fn zone_of(value: &serde_json::Value) -> i64 {
    value
        .get("zone")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
}

/// Install the P1 host-functions on `linker` under the `"tune"` module.
///
/// Permission gating and marshalling live in [`host_json_call`]; each closure
/// only names its permission, unpacks its arguments, and calls the matching
/// [`HostContext`] method. `host_log` is always allowed; `host_emit` returns
/// unit so a denied `events` permission silently drops the event.
fn register_host_imports(linker: &mut Linker<StoreData>) -> Result<(), String> {
    // Always allowed: diagnostic logging.
    linker
        .func_wrap(
            "tune",
            "host_log",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<(), wasmtime::Error> {
                let input = guest_read(&mut caller, ptr, len)?;
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&input) {
                    let level = v.get("level").and_then(|x| x.as_str()).unwrap_or("info");
                    let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("");
                    caller.data().ctx.log(level, msg);
                }
                Ok(())
            },
        )
        .map_err(|e| format!("register host_log: {e}"))?;

    // `queue`
    linker
        .func_wrap(
            "tune",
            "host_queue_get",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "queue", |ctx, v| {
                    ctx.queue_get(zone_of(&v))
                })
            },
        )
        .map_err(|e| format!("register host_queue_get: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_queue_add",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "queue", |ctx, v| {
                    let tracks = v.get("tracks").cloned().unwrap_or(serde_json::Value::Null);
                    ctx.queue_add(zone_of(&v), tracks)
                })
            },
        )
        .map_err(|e| format!("register host_queue_add: {e}"))?;

    // `playback`
    linker
        .func_wrap(
            "tune",
            "host_now_playing",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playback", |ctx, v| {
                    ctx.now_playing(zone_of(&v))
                })
            },
        )
        .map_err(|e| format!("register host_now_playing: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_play",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playback", |ctx, v| {
                    let req = v.get("req").cloned().unwrap_or(serde_json::Value::Null);
                    ctx.play(zone_of(&v), req)
                })
            },
        )
        .map_err(|e| format!("register host_play: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_pause",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playback", |ctx, v| {
                    ctx.pause(zone_of(&v))
                })
            },
        )
        .map_err(|e| format!("register host_pause: {e}"))?;

    // `events` — fire-and-forget, unit return; denied ⇒ silently dropped.
    linker
        .func_wrap(
            "tune",
            "host_emit",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<(), wasmtime::Error> {
                if !caller.data().permissions.contains("events") {
                    return Ok(());
                }
                let input = guest_read(&mut caller, ptr, len)?;
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&input) {
                    let event = v
                        .get("event")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);
                    caller.data().ctx.emit(&event, payload);
                }
                Ok(())
            },
        )
        .map_err(|e| format!("register host_emit: {e}"))?;

    // -----------------------------------------------------------------------
    // #4716 — `playlists`
    //
    // Lecture et AJOUT seulement : il n'existe volontairement aucun
    // `host_playlist_delete` ni `host_playlist_remove_tracks`. Un greffon ne
    // doit pas pouvoir effacer le travail de l'utilisateur, et une capacité
    // absente est la seule garde qu'on ne puisse pas contourner.
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "tune",
            "host_playlists_list",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playlists", |ctx, v| {
                    let limit = v
                        .get("limit")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(200);
                    let offset = v
                        .get("offset")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    ctx.playlists_list(limit, offset)
                })
            },
        )
        .map_err(|e| format!("register host_playlists_list: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_playlist_tracks",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playlists", |ctx, v| {
                    let id = v
                        .get("playlist_id")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    ctx.playlist_tracks(id)
                })
            },
        )
        .map_err(|e| format!("register host_playlist_tracks: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_playlist_create",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playlists", |ctx, v| {
                    let name = texte(&v, "name");
                    let description = texte_facultatif(&v, "description");
                    ctx.playlist_create(&name, description.as_deref())
                })
            },
        )
        .map_err(|e| format!("register host_playlist_create: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_playlist_add_tracks",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "playlists", |ctx, v| {
                    let id = v
                        .get("playlist_id")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0);
                    ctx.playlist_add_tracks(id, ids_entiers(&v, "track_ids"))
                })
            },
        )
        .map_err(|e| format!("register host_playlist_add_tracks: {e}"))?;

    // -----------------------------------------------------------------------
    // #4716 — `streaming`
    //
    // Même règle : aucune capacité de suppression chez un service. Les deux
    // capacités d'écriture (`create`, `add_tracks`) purgent le cache de
    // contenu utilisateur du service côté implémentation.
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "tune",
            "host_streaming_services",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "streaming", |ctx, _v| {
                    ctx.streaming_services()
                })
            },
        )
        .map_err(|e| format!("register host_streaming_services: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_streaming_playlists",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "streaming", |ctx, v| {
                    ctx.streaming_playlists(&texte(&v, "service"))
                })
            },
        )
        .map_err(|e| format!("register host_streaming_playlists: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_streaming_playlist_tracks",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "streaming", |ctx, v| {
                    ctx.streaming_playlist_tracks(&texte(&v, "service"), &texte(&v, "playlist_id"))
                })
            },
        )
        .map_err(|e| format!("register host_streaming_playlist_tracks: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_streaming_playlist_create",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "streaming", |ctx, v| {
                    let description = texte_facultatif(&v, "description");
                    ctx.streaming_playlist_create(
                        &texte(&v, "service"),
                        &texte(&v, "name"),
                        description.as_deref(),
                    )
                })
            },
        )
        .map_err(|e| format!("register host_streaming_playlist_create: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_streaming_playlist_add_tracks",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "streaming", |ctx, v| {
                    ctx.streaming_playlist_add_tracks(
                        &texte(&v, "service"),
                        &texte(&v, "playlist_id"),
                        ids_textes(&v, "track_ids"),
                    )
                })
            },
        )
        .map_err(|e| format!("register host_streaming_playlist_add_tracks: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_streaming_match_track",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "streaming", |ctx, v| {
                    let duration_ms = v
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    ctx.streaming_match_track(
                        &texte(&v, "service"),
                        &texte(&v, "title"),
                        &texte(&v, "artist"),
                        &texte(&v, "isrc"),
                        duration_ms,
                    )
                })
            },
        )
        .map_err(|e| format!("register host_streaming_match_track: {e}"))?;

    // -----------------------------------------------------------------------
    // #4716 — `library`
    //
    // Le sens SERVICE → BIBLIOTHÈQUE, qui manquait : sans recherche locale ni
    // appariement local, un convertisseur de playlists ne savait qu'aller de
    // la bibliothèque VERS un service. Lecture seule : on ne fait que chercher
    // et apparier — aucune de ces deux capacités n'écrit, et aucune n'efface.
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "tune",
            "host_library_search",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "library", |ctx, v| {
                    let limit = v
                        .get("limit")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(20);
                    ctx.library_search(&texte(&v, "query"), limit)
                })
            },
        )
        .map_err(|e| format!("register host_library_search: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_library_match_track",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call(&mut caller, ptr, len, "library", |ctx, v| {
                    let duration_ms = v
                        .get("duration_ms")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    ctx.library_match_track(
                        &texte(&v, "title"),
                        &texte(&v, "artist"),
                        &texte(&v, "isrc"),
                        duration_ms,
                    )
                })
            },
        )
        .map_err(|e| format!("register host_library_match_track: {e}"))?;

    // -----------------------------------------------------------------------
    // #4716 — `kv` : stockage cloisonné PAR GREFFON
    //
    // L'identifiant du greffon vient du `Store`, jamais du JSON d'entrée : un
    // greffon ne peut donc ni lire ni écraser l'état d'un autre.
    // -----------------------------------------------------------------------
    linker
        .func_wrap(
            "tune",
            "host_kv_get",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call_du_greffon(&mut caller, ptr, len, "kv", |ctx, greffon, v| {
                    ctx.kv_get(greffon, &texte(&v, "key"))
                })
            },
        )
        .map_err(|e| format!("register host_kv_get: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_kv_set",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call_du_greffon(&mut caller, ptr, len, "kv", |ctx, greffon, v| {
                    let value = v.get("value").cloned().unwrap_or(serde_json::Value::Null);
                    ctx.kv_set(greffon, &texte(&v, "key"), value)
                })
            },
        )
        .map_err(|e| format!("register host_kv_set: {e}"))?;

    linker
        .func_wrap(
            "tune",
            "host_kv_list",
            |mut caller: Caller<'_, StoreData>,
             ptr: i32,
             len: i32|
             -> Result<i64, wasmtime::Error> {
                host_json_call_du_greffon(&mut caller, ptr, len, "kv", |ctx, greffon, v| {
                    ctx.kv_list(greffon, &texte(&v, "prefix"))
                })
            },
        )
        .map_err(|e| format!("register host_kv_list: {e}"))?;

    Ok(())
}

/// A loaded, instantiated WASM plugin with its exports resolved.
///
/// Owns the [`Store`] (and therefore the plugin's linear memory), so it is not
/// `Sync`; callers that share it must serialise access. Every public call is
/// fuel- and memory-bounded and can never hang the host.
pub struct WasmPlugin {
    store: Store<StoreData>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    dealloc: TypedFunc<(u32, u32), ()>,
    dispatch: TypedFunc<(u32, u32), u64>,
    /// Optional `plugin_on_event(ptr,len)` export (RFC §3.3/§3.6). Present only
    /// if the plugin exports it; [`WasmPlugin::on_event`] is a no-op otherwise.
    on_event: Option<TypedFunc<(u32, u32), ()>>,
    fuel: u64,
}

impl WasmPlugin {
    /// Load a plugin from a `.wasm` (or, with the `wat` feature, `.wat`) file,
    /// instantiate it under `limits`, resolve its exports, and verify its ABI
    /// version. No host is wired and no permissions are granted — use
    /// [`load_with_host`](WasmPlugin::load_with_host) for a plugin that needs
    /// host-functions (P1).
    ///
    /// Errors (as `String`) on: unreadable/invalid module, missing required
    /// export (`memory`, `abi_version`, `alloc`, `dealloc`,
    /// `plugin_dispatch`/`dispatch_c`), instantiation trap, or an
    /// `abi_version()` that differs from [`HOST_ABI_VERSION`].
    pub fn load(path: &Path, limits: Limits) -> Result<WasmPlugin, String> {
        Self::load_with_host(path, limits, Arc::new(NoHost), HashSet::new(), "")
    }

    /// Like [`load`](WasmPlugin::load) but wires a [`HostContext`] and the set
    /// of `permissions` granted to the plugin (from its manifest). Host imports
    /// under the `"tune"` module forward to `ctx`, gated by `permissions`
    /// (deny-by-default, RFC §3.4).
    ///
    /// `plugin_id` est l'identifiant de MANIFESTE du greffon (#4716) : il ne
    /// sert pas au chargement mais cloisonne le stockage clé/valeur, et c'est
    /// l'hôte qui le fournit — jamais le greffon.
    pub fn load_with_host(
        path: &Path,
        limits: Limits,
        ctx: Arc<dyn HostContext>,
        permissions: HashSet<String>,
        plugin_id: &str,
    ) -> Result<WasmPlugin, String> {
        let engine = engine();
        let module =
            Module::from_file(engine, path).map_err(|e| format!("load wasm module: {e}"))?;
        Self::from_module(engine, &module, limits, ctx, permissions, plugin_id)
    }

    /// Instantiate from already-compiled wat/wasm text or bytes, no host wired.
    /// Primarily for tests (avoids needing the `wasm32` toolchain).
    #[cfg(test)]
    pub fn from_bytes(bytes: impl AsRef<[u8]>, limits: Limits) -> Result<WasmPlugin, String> {
        Self::from_bytes_with_host(bytes, limits, Arc::new(NoHost), HashSet::new(), "")
    }

    /// Like [`from_bytes`](WasmPlugin::from_bytes) with a [`HostContext`] and
    /// granted `permissions` — the in-memory counterpart of
    /// [`load_with_host`](WasmPlugin::load_with_host), used by the P1 tests.
    #[cfg(test)]
    pub fn from_bytes_with_host(
        bytes: impl AsRef<[u8]>,
        limits: Limits,
        ctx: Arc<dyn HostContext>,
        permissions: HashSet<String>,
        plugin_id: &str,
    ) -> Result<WasmPlugin, String> {
        let engine = engine();
        let module = Module::new(engine, bytes).map_err(|e| format!("compile wasm module: {e}"))?;
        Self::from_module(engine, &module, limits, ctx, permissions, plugin_id)
    }

    fn from_module(
        engine: &Engine,
        module: &Module,
        limits: Limits,
        ctx: Arc<dyn HostContext>,
        permissions: HashSet<String>,
        plugin_id: &str,
    ) -> Result<WasmPlugin, String> {
        let state = StoreData {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .build(),
            ctx,
            permissions,
            plugin_id: plugin_id.to_string(),
            memory: None,
            alloc: None,
        };
        let mut store = Store::new(engine, state);
        // Enforce the linear-memory cap.
        store.limiter(|s| &mut s.limits);
        // Seed the fuel budget so instantiation (and the abi_version probe
        // below) are themselves bounded.
        store
            .set_fuel(limits.fuel)
            .map_err(|e| format!("enable fuel: {e}"))?;

        // P1: install the host-functions the plugin may import. A capability
        // whose permission is not granted still resolves, but its call returns
        // a `permission_denied` JSON (deny-by-default, RFC §3.4). A plugin that
        // imports nothing (P0 echo plugins) is unaffected.
        let mut linker: Linker<StoreData> = Linker::new(engine);
        register_host_imports(&mut linker)?;
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(|e| format!("instantiate plugin: {e}"))?;

        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| "plugin missing `memory` export".to_string())?;

        let abi_version = instance
            .get_typed_func::<(), u32>(&mut store, "abi_version")
            .map_err(|e| format!("resolve `abi_version`: {e}"))?;
        let alloc = instance
            .get_typed_func::<u32, u32>(&mut store, "alloc")
            .map_err(|e| format!("resolve `alloc`: {e}"))?;
        let dealloc = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "dealloc")
            .map_err(|e| format!("resolve `dealloc`: {e}"))?;
        // Prefer the RFC §3.3 name, fall back to the name the Party/DJ crates
        // actually export so a real party.wasm loads unchanged.
        let dispatch = instance
            .get_typed_func::<(u32, u32), u64>(&mut store, "plugin_dispatch")
            .or_else(|_| instance.get_typed_func::<(u32, u32), u64>(&mut store, "dispatch_c"))
            .map_err(|e| format!("resolve `plugin_dispatch`/`dispatch_c`: {e}"))?;
        // Optional (RFC §3.3): only plugins that subscribe to events export it.
        // A wrong-typed export is treated as absent (on_event stays a no-op).
        let on_event = instance
            .get_typed_func::<(u32, u32), ()>(&mut store, "plugin_on_event")
            .ok();

        // Stash memory + alloc in the store data so host imports can read from
        // and allocate into the plugin's memory from inside a `Caller`. Done
        // before any dispatch (hence before any host-function) can run.
        store.data_mut().memory = Some(memory);
        store.data_mut().alloc = Some(alloc.clone());

        // Verify the ABI version (RFC §4).
        let v = abi_version
            .call(&mut store, ())
            .map_err(|e| format!("call `abi_version`: {e}"))?;
        if v != HOST_ABI_VERSION {
            return Err(format!(
                "plugin ABI version mismatch: plugin reports {v}, host requires {HOST_ABI_VERSION}"
            ));
        }

        Ok(WasmPlugin {
            store,
            memory,
            alloc,
            dealloc,
            dispatch,
            on_event,
            fuel: limits.fuel,
        })
    }

    /// Dispatch a JSON request to the plugin and return its JSON response.
    ///
    /// Writes `json_in`'s bytes verbatim at `(ptr, len)` (see module docs). For
    /// the Party crate's `[action_len][action][payload]` input framing use
    /// [`dispatch_action`](WasmPlugin::dispatch_action).
    pub fn dispatch(&mut self, json_in: &str) -> Result<String, String> {
        self.call_raw(json_in.as_bytes())
    }

    /// Dispatch an HTTP route request to the plugin (RFC §3.5).
    ///
    /// The host mounts a single axum handler under `/api/v1/plugins/{id}/…`,
    /// packages the request as `{method, path, query, body}` JSON, and calls
    /// this; the plugin answers with a `{status, headers?, body}` JSON envelope.
    /// This is a thin forwarder over [`dispatch`](WasmPlugin::dispatch) — the
    /// route request/response is just the JSON payload the dispatch contract
    /// already carries — named so the P2 route-mounting call site reads for
    /// intent rather than as a bare `dispatch`.
    pub fn handle_route(&mut self, req_json: &str) -> Result<String, String> {
        self.dispatch(req_json)
    }

    /// Dispatch with the Party crate's input framing:
    /// `[action_len: u32 LE][action bytes][payload bytes]`. The response is read
    /// with the same packed-`u64` / raw-JSON convention as [`dispatch`].
    ///
    /// [`dispatch`]: WasmPlugin::dispatch
    pub fn dispatch_action(&mut self, action: &str, payload: &str) -> Result<String, String> {
        let action = action.as_bytes();
        let payload = payload.as_bytes();
        let mut buf = Vec::with_capacity(4 + action.len() + payload.len());
        buf.extend_from_slice(&(action.len() as u32).to_le_bytes());
        buf.extend_from_slice(action);
        buf.extend_from_slice(payload);
        self.call_raw(&buf)
    }

    /// Forward a subscribed `event_bus` event to the plugin (RFC §3.6).
    ///
    /// Fire-and-forget: `event_json` (the `{name, payload}` object) is written
    /// into guest memory via `alloc` and passed to the optional
    /// `plugin_on_event(ptr,len)` export — there is no return value. If the
    /// plugin does not export `plugin_on_event`, this is a **no-op `Ok`** (the
    /// plugin simply ignores events). Fuel is replenished so the call is
    /// independently bounded exactly like [`dispatch`](WasmPlugin::dispatch); a
    /// runaway `plugin_on_event` traps into `Err` instead of hanging the host.
    pub fn on_event(&mut self, event_json: &str) -> Result<(), String> {
        // Plugins without the export ignore events entirely.
        let Some(on_event) = self.on_event.clone() else {
            return Ok(());
        };

        // Fresh per-call fuel budget (see `call_raw`).
        self.store
            .set_fuel(self.fuel)
            .map_err(|e| format!("reset fuel: {e}"))?;

        let input = event_json.as_bytes();
        let in_len = u32::try_from(input.len()).map_err(|_| "event too large".to_string())?;
        let in_ptr = self
            .alloc
            .call(&mut self.store, in_len)
            .map_err(|e| format!("plugin `alloc` trapped: {e}"))?;
        self.memory
            .write(&mut self.store, in_ptr as usize, input)
            .map_err(|e| format!("write event to plugin memory: {e}"))?;

        on_event
            .call(&mut self.store, (in_ptr, in_len))
            .map_err(|e| format!("plugin on_event trapped (fuel/limit or error): {e}"))?;

        // Best-effort free of the input buffer; a dealloc trap is harmless here.
        let _ = self.dealloc.call(&mut self.store, (in_ptr, in_len));
        Ok(())
    }

    /// Core marshalling: alloc guest buffer, write input, call dispatch, read
    /// the packed result, free both buffers. Replenishes the fuel budget so
    /// this call is independently bounded.
    fn call_raw(&mut self, input: &[u8]) -> Result<String, String> {
        // Fresh per-call fuel budget: a previous runaway call cannot starve
        // this one, and this call cannot run forever.
        self.store
            .set_fuel(self.fuel)
            .map_err(|e| format!("reset fuel: {e}"))?;

        let in_len = u32::try_from(input.len()).map_err(|_| "input too large".to_string())?;
        let in_ptr = self
            .alloc
            .call(&mut self.store, in_len)
            .map_err(|e| format!("plugin `alloc` trapped: {e}"))?;
        self.memory
            .write(&mut self.store, in_ptr as usize, input)
            .map_err(|e| format!("write input to plugin memory: {e}"))?;

        let packed = self
            .dispatch
            .call(&mut self.store, (in_ptr, in_len))
            .map_err(|e| format!("plugin dispatch trapped (fuel/limit or error): {e}"))?;

        let out_ptr = (packed >> 32) as u32;
        let out_len = (packed & 0xFFFF_FFFF) as u32;

        // Read the raw JSON response before freeing anything.
        let mut out = vec![0u8; out_len as usize];
        self.memory
            .read(&self.store, out_ptr as usize, &mut out)
            .map_err(|e| format!("read plugin output: {e}"))?;

        // Free the plugin's output buffer, then our input buffer. Best-effort:
        // a dealloc trap does not corrupt the response we already read.
        let _ = self.dealloc.call(&mut self.store, (out_ptr, out_len));
        let _ = self.dealloc.call(&mut self.store, (in_ptr, in_len));

        String::from_utf8(out).map_err(|e| format!("plugin output not UTF-8: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// A minimal, hand-written WAT plugin implementing the P0 ABI:
    /// `memory`, `abi_version` (= `$abi`), a bump-allocator `alloc`, no-op
    /// `dealloc`, and a `plugin_dispatch` that **echoes** — it allocates an
    /// output buffer, `memory.copy`es the input into it, and returns the packed
    /// `(out_ptr << 32) | out_len`.
    fn echo_wat(abi: u32) -> String {
        format!(
            r#"(module
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 1024))

  (func (export "abi_version") (result i32)
    (i32.const {abi}))

  ;; Bump allocator over a fixed region; 8-byte aligned.
  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $bump))
    (global.set $bump
      (i32.and
        (i32.add (i32.add (global.get $bump) (local.get $len)) (i32.const 7))
        (i32.const -8)))
    (local.get $ptr))

  ;; No-op free (bump allocator never reclaims).
  (func (export "dealloc") (param $ptr i32) (param $len i32))

  ;; Echo: copy the input bytes into a fresh output buffer and pack ptr/len.
  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (local.get $len)))
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (local.get $len)))))
"#
        )
    }

    /// Like [`echo_wat`] but exports the dispatch function under the Party/DJ
    /// crate name `dispatch_c` instead of `plugin_dispatch`, to prove the
    /// name-fallback path (real-plugin compatibility).
    fn echo_wat_dispatch_c() -> String {
        echo_wat(HOST_ABI_VERSION).replace("\"plugin_dispatch\"", "\"dispatch_c\"")
    }

    /// A plugin whose `plugin_dispatch` runs forever — used to prove fuel
    /// exhaustion traps (returns `Err`) rather than hanging the host.
    fn runaway_wat() -> String {
        format!(
            r#"(module
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 1024))
  (func (export "abi_version") (result i32) (i32.const {abi}))
  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $bump))
    (global.set $bump (i32.add (global.get $bump) (local.get $len)))
    (local.get $ptr))
  (func (export "dealloc") (param $ptr i32) (param $len i32))
  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (loop $l (br $l))
    (unreachable)))
"#,
            abi = HOST_ABI_VERSION
        )
    }

    #[test]
    fn echo_roundtrips_via_tempfile() {
        // Write the WAT to a temp .wat file and drive the real `load` path.
        let mut f = tempfile::Builder::new()
            .suffix(".wat")
            .tempfile()
            .expect("temp file");
        f.write_all(echo_wat(HOST_ABI_VERSION).as_bytes())
            .expect("write wat");
        f.flush().expect("flush");

        let mut plugin = WasmPlugin::load(f.path(), Limits::default()).expect("load echo plugin");

        let input = r#"{"hello":"world"}"#;
        let out = plugin.dispatch(input).expect("dispatch echo");
        assert_eq!(out, input, "echo must return its input verbatim");

        // A second, differently sized payload proves the bump allocator and
        // packed pointer/length are handled correctly across calls.
        let input2 = r#"{"a":1,"b":[2,3,4],"c":"longer string here"}"#;
        let out2 = plugin.dispatch(input2).expect("dispatch echo 2");
        assert_eq!(out2, input2);
    }

    #[test]
    fn dispatch_c_export_name_resolves() {
        // Proves a real party.wasm (which exports `dispatch_c`, not
        // `plugin_dispatch`) is loadable via the name-fallback.
        let mut plugin =
            WasmPlugin::from_bytes(echo_wat_dispatch_c(), Limits::default()).expect("load");
        let input = r#"{"ping":true}"#;
        assert_eq!(plugin.dispatch(input).unwrap(), input);
    }

    #[test]
    fn abi_version_mismatch_is_rejected() {
        // `WasmPlugin` is not `Debug` (it owns a wasmtime `Store`), so match
        // rather than `expect_err`.
        let err = match WasmPlugin::from_bytes(echo_wat(HOST_ABI_VERSION + 1), Limits::default()) {
            Ok(_) => panic!("mismatched ABI must be rejected"),
            Err(e) => e,
        };
        assert!(
            err.contains("mismatch"),
            "error should mention the ABI mismatch, got: {err}"
        );
    }

    #[test]
    fn runaway_dispatch_traps_on_fuel_not_hangs() {
        // Small fuel budget: instantiation + abi_version probe fit easily, but
        // the infinite loop in plugin_dispatch exhausts it and traps.
        let limits = Limits {
            max_memory_bytes: 64 * 1024 * 1024,
            fuel: 100_000,
        };
        let mut plugin = WasmPlugin::from_bytes(runaway_wat(), limits).expect("load runaway");
        let err = plugin
            .dispatch("{}")
            .expect_err("runaway dispatch must error, never hang");
        assert!(
            err.contains("trapped"),
            "runaway call should report a trap, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // P1 — host-functions + permission gating
    // -----------------------------------------------------------------------

    /// Records every host call and returns canned data — lets a test assert
    /// exactly what the plugin invoked and with which arguments.
    #[derive(Default)]
    struct MockHostContext {
        logs: Mutex<Vec<(String, String)>>,
        queue_add_calls: Mutex<Vec<(i64, serde_json::Value)>>,
        emits: Mutex<Vec<(String, serde_json::Value)>>,
        /// #4716 — journal des capacités playlists/streaming/kv : `(nom de la
        /// méthode, arguments reçus)`. Une capacité refusée doit laisser ce
        /// journal VIDE — c'est ce que prouvent les tests de refus.
        appels_4716: Mutex<Vec<(String, serde_json::Value)>>,
    }

    impl MockHostContext {
        /// Noter un appel et rendre une réponse reconnaissable, pour que le
        /// test prouve aussi l'aller-retour hôte → greffon.
        fn noter(&self, nom: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
            self.appels_4716
                .lock()
                .unwrap()
                .push((nom.to_string(), args));
            Ok(serde_json::json!({ "ok": true, "appel": nom }))
        }

        /// Le seul appel noté, ou un échec explicite.
        fn seul_appel(&self) -> (String, serde_json::Value) {
            let appels = self.appels_4716.lock().unwrap();
            assert_eq!(appels.len(), 1, "une capacité, un appel — vu {appels:?}");
            appels[0].clone()
        }
    }

    /// Canned response `queue_add` returns, so the test can assert the round-trip.
    fn canned_queue_add() -> serde_json::Value {
        serde_json::json!({ "ok": true, "added": 1 })
    }

    impl HostContext for MockHostContext {
        fn log(&self, level: &str, msg: &str) {
            self.logs
                .lock()
                .unwrap()
                .push((level.to_string(), msg.to_string()));
        }
        fn queue_get(&self, _zone: i64) -> Result<serde_json::Value, String> {
            Ok(serde_json::json!([]))
        }
        fn queue_add(
            &self,
            zone: i64,
            tracks: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.queue_add_calls.lock().unwrap().push((zone, tracks));
            Ok(canned_queue_add())
        }
        fn now_playing(&self, _zone: i64) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn play(&self, _zone: i64, _req: serde_json::Value) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn pause(&self, _zone: i64) -> Result<serde_json::Value, String> {
            Ok(serde_json::Value::Null)
        }
        fn emit(&self, event: &str, payload: serde_json::Value) {
            self.emits
                .lock()
                .unwrap()
                .push((event.to_string(), payload));
        }

        // --- #4716 ---------------------------------------------------------
        fn playlists_list(&self, limit: i64, offset: i64) -> Result<serde_json::Value, String> {
            self.noter(
                "playlists_list",
                serde_json::json!({ "limit": limit, "offset": offset }),
            )
        }
        fn playlist_tracks(&self, playlist_id: i64) -> Result<serde_json::Value, String> {
            self.noter(
                "playlist_tracks",
                serde_json::json!({ "playlist_id": playlist_id }),
            )
        }
        fn playlist_create(
            &self,
            name: &str,
            description: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "playlist_create",
                serde_json::json!({ "name": name, "description": description }),
            )
        }
        fn playlist_add_tracks(
            &self,
            playlist_id: i64,
            track_ids: Vec<i64>,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "playlist_add_tracks",
                serde_json::json!({ "playlist_id": playlist_id, "track_ids": track_ids }),
            )
        }
        fn streaming_services(&self) -> Result<serde_json::Value, String> {
            self.noter("streaming_services", serde_json::json!({}))
        }
        fn streaming_playlists(&self, service: &str) -> Result<serde_json::Value, String> {
            self.noter(
                "streaming_playlists",
                serde_json::json!({ "service": service }),
            )
        }
        fn streaming_playlist_tracks(
            &self,
            service: &str,
            playlist_id: &str,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "streaming_playlist_tracks",
                serde_json::json!({ "service": service, "playlist_id": playlist_id }),
            )
        }
        fn streaming_playlist_create(
            &self,
            service: &str,
            name: &str,
            description: Option<&str>,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "streaming_playlist_create",
                serde_json::json!({
                    "service": service, "name": name, "description": description,
                }),
            )
        }
        fn streaming_playlist_add_tracks(
            &self,
            service: &str,
            playlist_id: &str,
            track_ids: Vec<String>,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "streaming_playlist_add_tracks",
                serde_json::json!({
                    "service": service, "playlist_id": playlist_id, "track_ids": track_ids,
                }),
            )
        }
        fn streaming_match_track(
            &self,
            service: &str,
            title: &str,
            artist: &str,
            isrc: &str,
            duration_ms: u64,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "streaming_match_track",
                serde_json::json!({
                    "service": service, "title": title, "artist": artist,
                    "isrc": isrc, "duration_ms": duration_ms,
                }),
            )
        }
        fn library_search(&self, query: &str, limit: i64) -> Result<serde_json::Value, String> {
            self.noter(
                "library_search",
                serde_json::json!({ "query": query, "limit": limit }),
            )
        }
        fn library_match_track(
            &self,
            title: &str,
            artist: &str,
            isrc: &str,
            duration_ms: u64,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "library_match_track",
                serde_json::json!({
                    "title": title, "artist": artist,
                    "isrc": isrc, "duration_ms": duration_ms,
                }),
            )
        }
        fn kv_get(&self, plugin_id: &str, key: &str) -> Result<serde_json::Value, String> {
            self.noter(
                "kv_get",
                serde_json::json!({ "plugin_id": plugin_id, "key": key }),
            )
        }
        fn kv_set(
            &self,
            plugin_id: &str,
            key: &str,
            value: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            self.noter(
                "kv_set",
                serde_json::json!({ "plugin_id": plugin_id, "key": key, "value": value }),
            )
        }
        fn kv_list(&self, plugin_id: &str, prefix: &str) -> Result<serde_json::Value, String> {
            self.noter(
                "kv_list",
                serde_json::json!({ "plugin_id": plugin_id, "prefix": prefix }),
            )
        }
    }

    /// A WAT plugin that **imports** `host_log` + `host_queue_add` from the
    /// `"tune"` module and, in `plugin_dispatch`, calls both using fixed JSON
    /// blobs placed in a data segment. It **returns the packed pointer that
    /// `host_queue_add` returned** — i.e. the plugin hands back whatever JSON
    /// the host wrote into its memory (the canned response, or the
    /// `permission_denied` error), proving the host→plugin write path.
    ///
    /// Offsets/lengths are computed in Rust so the WAT is always consistent.
    fn host_calling_wat() -> String {
        let log_json = r#"{"level":"info","msg":"hi from wasm"}"#;
        let qadd_json = r#"{"zone":7,"tracks":[{"id":"t1"}]}"#;
        // Place the two blobs at fixed low offsets, below the bump region.
        let log_off = 16usize;
        let qadd_off = log_off + log_json.len();
        // WAT string literals need `"` escaped as `\"`. The stored bytes (and
        // hence the lengths passed to the host) are the un-escaped JSON.
        let log_lit = log_json.replace('"', "\\\"");
        let qadd_lit = qadd_json.replace('"', "\\\"");
        format!(
            r#"(module
  (import "tune" "host_log" (func $host_log (param i32 i32)))
  (import "tune" "host_queue_add" (func $host_queue_add (param i32 i32) (result i64)))
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 1024))
  (data (i32.const {log_off}) "{log_json}")
  (data (i32.const {qadd_off}) "{qadd_json}")

  (func (export "abi_version") (result i32) (i32.const {abi}))

  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $bump))
    (global.set $bump
      (i32.and
        (i32.add (i32.add (global.get $bump) (local.get $len)) (i32.const 7))
        (i32.const -8)))
    (local.get $ptr))

  (func (export "dealloc") (param $ptr i32) (param $len i32))

  ;; Call host_log, then host_queue_add, and return whatever the latter
  ;; returned (packed ptr/len of the JSON the host wrote into our memory).
  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (call $host_log (i32.const {log_off}) (i32.const {log_len}))
    (call $host_queue_add (i32.const {qadd_off}) (i32.const {qadd_len}))))
"#,
            abi = HOST_ABI_VERSION,
            log_off = log_off,
            qadd_off = qadd_off,
            log_json = log_lit,
            qadd_json = qadd_lit,
            log_len = log_json.len(),
            qadd_len = qadd_json.len(),
        )
    }

    fn perms(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn host_functions_invoked_and_gated_when_permitted() {
        let mock = Arc::new(MockHostContext::default());
        let mut plugin = WasmPlugin::from_bytes_with_host(
            host_calling_wat(),
            Limits::default(),
            mock.clone() as Arc<dyn HostContext>,
            perms(&["queue", "events"]),
            "essai",
        )
        .expect("load host-calling plugin");

        let out = plugin.dispatch(r#"{"trigger":true}"#).expect("dispatch");

        // The plugin returned exactly what the host wrote for queue_add.
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("dispatch output is JSON");
        assert_eq!(
            parsed,
            canned_queue_add(),
            "queue_add result must round-trip"
        );

        // host_log was invoked with the right level + message.
        let logs = mock.logs.lock().unwrap();
        assert_eq!(
            logs.as_slice(),
            &[("info".to_string(), "hi from wasm".to_string())]
        );

        // host_queue_add was invoked with zone 7 and the right tracks.
        let calls = mock.queue_add_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "queue_add must be called exactly once");
        assert_eq!(calls[0].0, 7);
        assert_eq!(calls[0].1, serde_json::json!([{ "id": "t1" }]));
    }

    // -----------------------------------------------------------------------
    // P3 — event forwarding (`plugin_on_event`)
    // -----------------------------------------------------------------------

    /// A plugin that EXPORTS `plugin_on_event`: it stores the received event
    /// bytes at a fixed offset and remembers their length in a global; its
    /// `plugin_dispatch` then returns those stored bytes verbatim (packed
    /// ptr/len). Lets a test push an event and read it back.
    fn store_event_wat() -> String {
        format!(
            r#"(module
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 8192))
  (global $evt_len (mut i32) (i32.const 0))

  (func (export "abi_version") (result i32) (i32.const {abi}))

  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $bump))
    (global.set $bump
      (i32.and
        (i32.add (i32.add (global.get $bump) (local.get $len)) (i32.const 7))
        (i32.const -8)))
    (local.get $ptr))

  (func (export "dealloc") (param $ptr i32) (param $len i32))

  ;; Store the event bytes at offset 4096, remember the length.
  (func (export "plugin_on_event") (param $ptr i32) (param $len i32)
    (memory.copy (i32.const 4096) (local.get $ptr) (local.get $len))
    (global.set $evt_len (local.get $len)))

  ;; Return the last stored event verbatim (ignores its own input).
  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.set $out (call $alloc (global.get $evt_len)))
    (memory.copy (local.get $out) (i32.const 4096) (global.get $evt_len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (global.get $evt_len)))))
"#,
            abi = HOST_ABI_VERSION
        )
    }

    #[test]
    fn on_event_delivered_to_exporting_plugin() {
        let mut plugin =
            WasmPlugin::from_bytes(store_event_wat(), Limits::default()).expect("load");
        let event = r#"{"name":"playback.state_changed","payload":{"zone":1}}"#;
        plugin.on_event(event).expect("on_event must succeed");
        // The plugin stored the event; dispatch hands it back verbatim.
        assert_eq!(plugin.dispatch("{}").unwrap(), event);
    }

    #[test]
    fn on_event_is_noop_without_the_export() {
        // The echo plugin does NOT export `plugin_on_event`; on_event must be a
        // silent Ok, never an error.
        let mut plugin =
            WasmPlugin::from_bytes(echo_wat(HOST_ABI_VERSION), Limits::default()).expect("load");
        plugin
            .on_event(r#"{"name":"zone.created","payload":{}}"#)
            .expect("on_event on a plugin without the export must be a no-op Ok");
    }

    #[test]
    fn host_queue_add_denied_without_queue_permission() {
        let mock = Arc::new(MockHostContext::default());
        // Grant something OTHER than `queue` so the plugin still loads but the
        // queue capability is denied.
        let mut plugin = WasmPlugin::from_bytes_with_host(
            host_calling_wat(),
            Limits::default(),
            mock.clone() as Arc<dyn HostContext>,
            perms(&["playback"]),
            "essai",
        )
        .expect("load host-calling plugin");

        let out = plugin.dispatch(r#"{"trigger":true}"#).expect("dispatch");

        // The plugin sees the structured permission_denied error, not a trap.
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("dispatch output is JSON");
        assert_eq!(
            parsed,
            serde_json::json!({ "error": "permission_denied", "permission": "queue" }),
            "denied queue call must return the structured error JSON"
        );

        // The real host method was NEVER invoked.
        assert!(
            mock.queue_add_calls.lock().unwrap().is_empty(),
            "queue_add must not reach the host when the permission is denied"
        );

        // log is always allowed, so it still ran.
        assert_eq!(
            mock.logs.lock().unwrap().len(),
            1,
            "host_log is always allowed"
        );
    }

    // -----------------------------------------------------------------------
    // #4716 — playlists, streaming, stockage clé/valeur
    //
    // Chaque capacité a DEUX gardes : sans sa permission elle rend
    // `{"error":"permission_denied"}` et n'atteint PAS l'hôte (modèle :
    // `host_queue_add_denied_without_queue_permission`) ; avec sa permission
    // elle atteint l'hôte avec les bons arguments et lui rapporte sa réponse.
    // -----------------------------------------------------------------------

    /// Un greffon qui importe UNE seule fonction hôte, l'appelle avec un objet
    /// JSON fixe, et rend telle quelle la réponse packée que l'hôte lui a
    /// écrite en mémoire. Le WAT est engendré pour que décalages et longueurs
    /// restent toujours cohérents avec la charge utile.
    fn appel_hote_wat(import: &str, payload: &str) -> String {
        let offset = 16usize;
        let litteral = payload.replace('"', "\\\"");
        format!(
            r#"(module
  (import "tune" "{import}" (func $cible (param i32 i32) (result i64)))
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 1024))
  (data (i32.const {offset}) "{litteral}")

  (func (export "abi_version") (result i32) (i32.const {abi}))

  (func $alloc (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $bump))
    (global.set $bump
      (i32.and
        (i32.add (i32.add (global.get $bump) (local.get $len)) (i32.const 7))
        (i32.const -8)))
    (local.get $ptr))

  (func (export "dealloc") (param $ptr i32) (param $len i32))

  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (call $cible (i32.const {offset}) (i32.const {taille}))))
"#,
            abi = HOST_ABI_VERSION,
            taille = payload.len(),
        )
    }

    /// Identifiant de greffon utilisé par les tests de cloisonnement `kv`.
    const GREFFON_ESSAI: &str = "playlists-converter";

    /// Jouer `import` avec `payload` et les permissions `accordees`.
    fn jouer(
        import: &str,
        payload: &str,
        accordees: &[&str],
    ) -> (serde_json::Value, Arc<MockHostContext>) {
        let mock = Arc::new(MockHostContext::default());
        let mut plugin = WasmPlugin::from_bytes_with_host(
            appel_hote_wat(import, payload),
            Limits::default(),
            mock.clone() as Arc<dyn HostContext>,
            perms(accordees),
            GREFFON_ESSAI,
        )
        .expect("charger le greffon d'essai");
        let sortie = plugin
            .dispatch(r#"{"declencheur":true}"#)
            .expect("dispatch");
        let rendu: serde_json::Value =
            serde_json::from_str(&sortie).expect("la sortie du greffon est du JSON");
        (rendu, mock)
    }

    /// Sans sa permission : erreur structurée ET hôte jamais atteint.
    ///
    /// La permission accordée est délibérément une AUTRE (`queue`) : le greffon
    /// charge donc normalement, seule la capacité visée est refusée — sans quoi
    /// un « rien ne s'est passé » pourrait venir d'un chargement raté.
    fn refuse_sans_permission(import: &str, payload: &str, permission: &str) {
        let (rendu, mock) = jouer(import, payload, &["queue"]);
        assert_eq!(
            rendu,
            serde_json::json!({ "error": "permission_denied", "permission": permission }),
            "{import} sans `{permission}` doit rendre l'erreur structurée, pas un trap"
        );
        assert!(
            mock.appels_4716.lock().unwrap().is_empty(),
            "{import} ne doit PAS atteindre l'hôte quand `{permission}` est refusée"
        );
    }

    /// Avec sa permission : l'hôte est atteint avec `arguments`, et sa réponse
    /// revient au greffon.
    fn atteint_l_hote(
        import: &str,
        payload: &str,
        permission: &str,
        methode: &str,
        arguments: serde_json::Value,
    ) {
        let (rendu, mock) = jouer(import, payload, &[permission]);
        let (nom, args) = mock.seul_appel();
        assert_eq!(nom, methode, "{import} doit appeler `{methode}`");
        assert_eq!(args, arguments, "{import} doit transmettre ses arguments");
        assert_eq!(
            rendu,
            serde_json::json!({ "ok": true, "appel": methode }),
            "{import} doit rapporter au greffon la réponse de l'hôte"
        );
    }

    // --- `playlists` -------------------------------------------------------

    #[test]
    fn host_playlists_list_refuse_sans_permission_playlists() {
        refuse_sans_permission(
            "host_playlists_list",
            r#"{"limit":50,"offset":10}"#,
            "playlists",
        );
    }

    #[test]
    fn host_playlists_list_atteint_l_hote_avec_playlists() {
        atteint_l_hote(
            "host_playlists_list",
            r#"{"limit":50,"offset":10}"#,
            "playlists",
            "playlists_list",
            serde_json::json!({ "limit": 50, "offset": 10 }),
        );
    }

    #[test]
    fn host_playlist_tracks_refuse_sans_permission_playlists() {
        refuse_sans_permission("host_playlist_tracks", r#"{"playlist_id":7}"#, "playlists");
    }

    #[test]
    fn host_playlist_tracks_atteint_l_hote_avec_playlists() {
        atteint_l_hote(
            "host_playlist_tracks",
            r#"{"playlist_id":7}"#,
            "playlists",
            "playlist_tracks",
            serde_json::json!({ "playlist_id": 7 }),
        );
    }

    #[test]
    fn host_playlist_create_refuse_sans_permission_playlists() {
        refuse_sans_permission(
            "host_playlist_create",
            r#"{"name":"Transfert","description":"depuis Qobuz"}"#,
            "playlists",
        );
    }

    #[test]
    fn host_playlist_create_atteint_l_hote_avec_playlists() {
        atteint_l_hote(
            "host_playlist_create",
            r#"{"name":"Transfert","description":"depuis Qobuz"}"#,
            "playlists",
            "playlist_create",
            serde_json::json!({ "name": "Transfert", "description": "depuis Qobuz" }),
        );
    }

    #[test]
    fn host_playlist_add_tracks_refuse_sans_permission_playlists() {
        refuse_sans_permission(
            "host_playlist_add_tracks",
            r#"{"playlist_id":7,"track_ids":[1,2,3]}"#,
            "playlists",
        );
    }

    #[test]
    fn host_playlist_add_tracks_atteint_l_hote_avec_playlists() {
        atteint_l_hote(
            "host_playlist_add_tracks",
            r#"{"playlist_id":7,"track_ids":[1,2,3]}"#,
            "playlists",
            "playlist_add_tracks",
            serde_json::json!({ "playlist_id": 7, "track_ids": [1, 2, 3] }),
        );
    }

    // --- `streaming` -------------------------------------------------------

    #[test]
    fn host_streaming_services_refuse_sans_permission_streaming() {
        refuse_sans_permission("host_streaming_services", r#"{}"#, "streaming");
    }

    #[test]
    fn host_streaming_services_atteint_l_hote_avec_streaming() {
        atteint_l_hote(
            "host_streaming_services",
            r#"{}"#,
            "streaming",
            "streaming_services",
            serde_json::json!({}),
        );
    }

    #[test]
    fn host_streaming_playlists_refuse_sans_permission_streaming() {
        refuse_sans_permission(
            "host_streaming_playlists",
            r#"{"service":"qobuz"}"#,
            "streaming",
        );
    }

    #[test]
    fn host_streaming_playlists_atteint_l_hote_avec_streaming() {
        atteint_l_hote(
            "host_streaming_playlists",
            r#"{"service":"qobuz"}"#,
            "streaming",
            "streaming_playlists",
            serde_json::json!({ "service": "qobuz" }),
        );
    }

    #[test]
    fn host_streaming_playlist_tracks_refuse_sans_permission_streaming() {
        refuse_sans_permission(
            "host_streaming_playlist_tracks",
            r#"{"service":"qobuz","playlist_id":"pl-1"}"#,
            "streaming",
        );
    }

    #[test]
    fn host_streaming_playlist_tracks_atteint_l_hote_avec_streaming() {
        atteint_l_hote(
            "host_streaming_playlist_tracks",
            r#"{"service":"qobuz","playlist_id":"pl-1"}"#,
            "streaming",
            "streaming_playlist_tracks",
            serde_json::json!({ "service": "qobuz", "playlist_id": "pl-1" }),
        );
    }

    #[test]
    fn host_streaming_playlist_create_refuse_sans_permission_streaming() {
        refuse_sans_permission(
            "host_streaming_playlist_create",
            r#"{"service":"tidal","name":"Copie"}"#,
            "streaming",
        );
    }

    #[test]
    fn host_streaming_playlist_create_atteint_l_hote_avec_streaming() {
        atteint_l_hote(
            "host_streaming_playlist_create",
            r#"{"service":"tidal","name":"Copie"}"#,
            "streaming",
            "streaming_playlist_create",
            // `description` absente : l'hôte reçoit `None`, pas la chaîne vide.
            serde_json::json!({ "service": "tidal", "name": "Copie", "description": null }),
        );
    }

    #[test]
    fn host_streaming_playlist_add_tracks_refuse_sans_permission_streaming() {
        refuse_sans_permission(
            "host_streaming_playlist_add_tracks",
            r#"{"service":"tidal","playlist_id":"pl-9","track_ids":["a","b"]}"#,
            "streaming",
        );
    }

    #[test]
    fn host_streaming_playlist_add_tracks_atteint_l_hote_avec_streaming() {
        atteint_l_hote(
            "host_streaming_playlist_add_tracks",
            r#"{"service":"tidal","playlist_id":"pl-9","track_ids":["a","b"]}"#,
            "streaming",
            "streaming_playlist_add_tracks",
            serde_json::json!({
                "service": "tidal", "playlist_id": "pl-9", "track_ids": ["a", "b"],
            }),
        );
    }

    #[test]
    fn host_streaming_match_track_refuse_sans_permission_streaming() {
        refuse_sans_permission(
            "host_streaming_match_track",
            r#"{"service":"qobuz","title":"La Boheme","artist":"Aznavour","isrc":"FR123","duration_ms":210000}"#,
            "streaming",
        );
    }

    #[test]
    fn host_streaming_match_track_atteint_l_hote_avec_streaming() {
        atteint_l_hote(
            "host_streaming_match_track",
            r#"{"service":"qobuz","title":"La Boheme","artist":"Aznavour","isrc":"FR123","duration_ms":210000}"#,
            "streaming",
            "streaming_match_track",
            serde_json::json!({
                "service": "qobuz", "title": "La Boheme", "artist": "Aznavour",
                "isrc": "FR123", "duration_ms": 210_000,
            }),
        );
    }

    // --- `library` ---------------------------------------------------------

    #[test]
    fn host_library_search_refuse_sans_permission_library() {
        refuse_sans_permission(
            "host_library_search",
            r#"{"query":"La Boheme Aznavour","limit":25}"#,
            "library",
        );
    }

    #[test]
    fn host_library_search_atteint_l_hote_avec_library() {
        atteint_l_hote(
            "host_library_search",
            r#"{"query":"La Boheme Aznavour","limit":25}"#,
            "library",
            "library_search",
            serde_json::json!({ "query": "La Boheme Aznavour", "limit": 25 }),
        );
    }

    #[test]
    fn host_library_match_track_refuse_sans_permission_library() {
        refuse_sans_permission(
            "host_library_match_track",
            r#"{"title":"La Boheme","artist":"Aznavour","isrc":"FR123","duration_ms":210000}"#,
            "library",
        );
    }

    #[test]
    fn host_library_match_track_atteint_l_hote_avec_library() {
        atteint_l_hote(
            "host_library_match_track",
            r#"{"title":"La Boheme","artist":"Aznavour","isrc":"FR123","duration_ms":210000}"#,
            "library",
            "library_match_track",
            serde_json::json!({
                "title": "La Boheme", "artist": "Aznavour",
                "isrc": "FR123", "duration_ms": 210_000,
            }),
        );
    }

    /// La permission `library` n'ouvre QUE la bibliothèque : une capacité
    /// `streaming` reste refusée, et réciproquement. Sans cette contre-épreuve,
    /// un gating recopié sur la mauvaise chaîne passerait inaperçu.
    #[test]
    fn la_permission_library_n_ouvre_pas_les_autres_familles_4716() {
        let (rendu, mock) = jouer(
            "host_streaming_services",
            r#"{}"#,
            &["library", "playlists", "kv"],
        );
        assert_eq!(
            rendu,
            serde_json::json!({ "error": "permission_denied", "permission": "streaming" })
        );
        assert!(mock.appels_4716.lock().unwrap().is_empty());

        let (rendu, mock) = jouer("host_library_search", r#"{"query":"x"}"#, &["streaming"]);
        assert_eq!(
            rendu,
            serde_json::json!({ "error": "permission_denied", "permission": "library" })
        );
        assert!(mock.appels_4716.lock().unwrap().is_empty());
    }

    // --- `kv` --------------------------------------------------------------

    #[test]
    fn host_kv_get_refuse_sans_permission_kv() {
        refuse_sans_permission("host_kv_get", r#"{"key":"transfert/42"}"#, "kv");
    }

    #[test]
    fn host_kv_get_atteint_l_hote_avec_kv() {
        atteint_l_hote(
            "host_kv_get",
            r#"{"key":"transfert/42"}"#,
            "kv",
            "kv_get",
            // L'identifiant du greffon vient du `Store`, pas du JSON d'entrée.
            serde_json::json!({ "plugin_id": GREFFON_ESSAI, "key": "transfert/42" }),
        );
    }

    #[test]
    fn host_kv_set_refuse_sans_permission_kv() {
        refuse_sans_permission("host_kv_set", r#"{"key":"etat","value":{"pas":3}}"#, "kv");
    }

    #[test]
    fn host_kv_set_atteint_l_hote_avec_kv() {
        atteint_l_hote(
            "host_kv_set",
            r#"{"key":"etat","value":{"pas":3}}"#,
            "kv",
            "kv_set",
            serde_json::json!({
                "plugin_id": GREFFON_ESSAI, "key": "etat", "value": { "pas": 3 },
            }),
        );
    }

    #[test]
    fn host_kv_list_refuse_sans_permission_kv() {
        refuse_sans_permission("host_kv_list", r#"{"prefix":"transfert/"}"#, "kv");
    }

    #[test]
    fn host_kv_list_atteint_l_hote_avec_kv() {
        atteint_l_hote(
            "host_kv_list",
            r#"{"prefix":"transfert/"}"#,
            "kv",
            "kv_list",
            serde_json::json!({ "plugin_id": GREFFON_ESSAI, "prefix": "transfert/" }),
        );
    }

    /// Un greffon ne peut pas se faire passer pour un autre : un `plugin_id`
    /// glissé dans le JSON d'entrée est ignoré, l'hôte reçoit celui du `Store`.
    #[test]
    fn kv_l_identifiant_du_greffon_ne_vient_jamais_du_json_4716() {
        atteint_l_hote(
            "host_kv_get",
            r#"{"key":"etat","plugin_id":"un-autre-greffon"}"#,
            "kv",
            "kv_get",
            serde_json::json!({ "plugin_id": GREFFON_ESSAI, "key": "etat" }),
        );
    }

    /// Un `kv` sans hôte câblé n'a pas d'identifiant : les clés seraient
    /// partagées par tout le monde. La capacité doit refuser, pas ranger.
    #[test]
    fn kv_sans_identifiant_de_greffon_refuse_4716() {
        // `NoHost` + permission accordée : seule la voie du `Store` manque.
        let mut plugin = WasmPlugin::from_bytes_with_host(
            appel_hote_wat("host_kv_set", r#"{"key":"etat","value":1}"#),
            Limits::default(),
            Arc::new(NoHost) as Arc<dyn HostContext>,
            perms(&["kv"]),
            "",
        )
        .expect("charger");
        let rendu: serde_json::Value =
            serde_json::from_str(&plugin.dispatch("{}").expect("dispatch")).expect("JSON");
        assert!(
            rendu.get("error").is_some(),
            "sans identifiant de greffon, `kv` doit refuser — vu {rendu}"
        );
    }

    /// 🔴 Garde de STRUCTURE du garde-fou « aucune suppression » (#4716).
    ///
    /// Elle n'inspecte pas le texte du fichier mais le `Linker` réellement
    /// construit : la liste des imports `"tune"` est la surface qu'un greffon
    /// peut atteindre. Aucun nom n'y contient `delete`, `remove` ni `unfollow`,
    /// et la liste attendue est écrite en toutes lettres — ajouter une capacité
    /// destructrice sans le vouloir fait tomber ce test.
    #[test]
    fn aucune_capacite_hote_ne_supprime_4716() {
        let engine = engine();
        let mut store = Store::new(
            engine,
            StoreData {
                limits: StoreLimitsBuilder::new().memory_size(1 << 20).build(),
                ctx: Arc::new(NoHost),
                permissions: HashSet::new(),
                plugin_id: String::new(),
                memory: None,
                alloc: None,
            },
        );
        let mut linker: Linker<StoreData> = Linker::new(engine);
        register_host_imports(&mut linker).expect("installer les imports hôte");

        let mut noms: Vec<String> = linker
            .iter(&mut store)
            .map(|(module, nom, _)| format!("{module}::{nom}"))
            .collect();
        noms.sort();

        for nom in &noms {
            assert!(
                !nom.contains("delete") && !nom.contains("remove") && !nom.contains("unfollow"),
                "aucune capacité hôte ne doit pouvoir EFFACER — vu `{nom}`"
            );
        }

        let mut attendus: Vec<String> = [
            "host_log",
            "host_queue_get",
            "host_queue_add",
            "host_now_playing",
            "host_play",
            "host_pause",
            "host_emit",
            "host_playlists_list",
            "host_playlist_tracks",
            "host_playlist_create",
            "host_playlist_add_tracks",
            "host_streaming_services",
            "host_streaming_playlists",
            "host_streaming_playlist_tracks",
            "host_streaming_playlist_create",
            "host_streaming_playlist_add_tracks",
            "host_streaming_match_track",
            "host_library_search",
            "host_library_match_track",
            "host_kv_get",
            "host_kv_set",
            "host_kv_list",
        ]
        .iter()
        .map(|n| format!("tune::{n}"))
        .collect();
        attendus.sort();
        assert_eq!(
            noms, attendus,
            "la surface hôte doit être exactement celle-ci"
        );
    }
}
