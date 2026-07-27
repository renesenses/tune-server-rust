//! Embedded WASM runtime for the Tune plugin ABI — **P0** of the RFC
//! (`docs/plugins/PLUGIN_ABI_RFC.md`, §3.1–3.3, §7).
//!
//! This is the minimal executable core: **load, instantiate, and call** a
//! plugin's exports with **JSON marshalling over linear memory**, inside a
//! **resource-limited** [`wasmtime`] sandbox. No host-functions, HTTP route
//! mounting, or event forwarding yet — those are P1+ and are intentionally
//! absent (the [`Linker`] handed to instantiation is empty, so any attempt to
//! import a host function traps: deny-by-default).
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
//! # Input framing
//!
//! [`WasmPlugin::dispatch`] writes the raw JSON bytes it is given at `(ptr,len)`
//! (this is what the echo test and the RFC's request/response model exercise).
//! [`WasmPlugin::dispatch_action`] additionally frames the buffer as
//! `[action_len: u32 LE][action][payload]`, which is exactly what the current
//! Party crate's `dispatch_c` decodes — provided for real-plugin compatibility.
//! Both share the same output convention above.

use std::path::Path;
use std::sync::OnceLock;

use wasmtime::{
    Config, Engine, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

/// ABI version the host implements. A plugin whose `abi_version()` export does
/// not return this exact value is rejected at load time (RFC §4).
pub const HOST_ABI_VERSION: u32 = 1;

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

/// Per-`Store` host state. Holds the [`StoreLimits`] that enforce the memory
/// cap through wasmtime's `ResourceLimiter` hook.
struct StoreState {
    limits: StoreLimits,
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

/// A loaded, instantiated WASM plugin with its exports resolved.
///
/// Owns the [`Store`] (and therefore the plugin's linear memory), so it is not
/// `Sync`; callers that share it must serialise access. Every public call is
/// fuel- and memory-bounded and can never hang the host.
pub struct WasmPlugin {
    store: Store<StoreState>,
    memory: Memory,
    alloc: TypedFunc<u32, u32>,
    dealloc: TypedFunc<(u32, u32), ()>,
    dispatch: TypedFunc<(u32, u32), u64>,
    fuel: u64,
}

impl WasmPlugin {
    /// Load a plugin from a `.wasm` (or, with the `wat` feature, `.wat`) file,
    /// instantiate it under `limits`, resolve its exports, and verify its ABI
    /// version.
    ///
    /// Errors (as `String`) on: unreadable/invalid module, missing required
    /// export (`memory`, `abi_version`, `alloc`, `dealloc`,
    /// `plugin_dispatch`/`dispatch_c`), instantiation trap, or an
    /// `abi_version()` that differs from [`HOST_ABI_VERSION`].
    pub fn load(path: &Path, limits: Limits) -> Result<WasmPlugin, String> {
        let engine = engine();
        let module =
            Module::from_file(engine, path).map_err(|e| format!("load wasm module: {e}"))?;
        Self::from_module(engine, &module, limits)
    }

    /// Instantiate from already-compiled wat/wasm text or bytes. Primarily for
    /// tests (avoids needing the `wasm32` toolchain).
    #[cfg(test)]
    pub fn from_bytes(bytes: impl AsRef<[u8]>, limits: Limits) -> Result<WasmPlugin, String> {
        let engine = engine();
        let module = Module::new(engine, bytes).map_err(|e| format!("compile wasm module: {e}"))?;
        Self::from_module(engine, &module, limits)
    }

    fn from_module(engine: &Engine, module: &Module, limits: Limits) -> Result<WasmPlugin, String> {
        let state = StoreState {
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.max_memory_bytes)
                .build(),
        };
        let mut store = Store::new(engine, state);
        // Enforce the linear-memory cap.
        store.limiter(|s| &mut s.limits);
        // Seed the fuel budget so instantiation (and the abi_version probe
        // below) are themselves bounded.
        store
            .set_fuel(limits.fuel)
            .map_err(|e| format!("enable fuel: {e}"))?;

        // P0: no host-functions. An empty linker means any host import the
        // module declares fails to resolve (deny-by-default, RFC §3.4).
        let linker: Linker<StoreState> = Linker::new(engine);
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
}
