//! P2 of the plugin ABI, end-to-end (`docs/plugins/PLUGIN_ABI_RFC.md` §3.4–3.6).
//!
//! Proves the whole wire: a wasm plugin dropped in a plugins dir is scanned,
//! loaded against the **real** [`AppStateHost`] (not a mock), mounted under
//! `/api/v1/plugins/{id}/…`, and — when an HTTP request hits that route — its
//! `plugin_dispatch` runs, calls `host_queue_add`, and the host appends a
//! (streaming) track to the *actual* play queue. We assert both the HTTP
//! response envelope AND the host side effect (the real `queue_items` table
//! grew, with the source_id the plugin sent), so a stubbed host could not pass.
//!
//! Whole file is gated behind `plugins-wasm`; under the default build it
//! compiles to nothing, leaving the 54 integration.rs tests untouched.
#![cfg(feature = "plugins-wasm")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_plugin_runtime_wasm::HOST_ABI_VERSION;
use tune_server::state::AppState;

/// A hand-written WAT plugin that **imports** `host_queue_add` and, in
/// `plugin_dispatch`, calls it with a fixed streaming track for zone 1
/// (exercising the real host — a streaming item has no `tracks` FK, so it
/// persists against the empty test DB), then returns a fixed `{status,body}`
/// envelope from a data segment. Offsets/lengths are computed in Rust so the
/// WAT stays consistent; string literals escape `"` while the stored bytes (and
/// the lengths passed to the host) are the un-escaped JSON — same technique as
/// the P1 runtime tests.
fn queue_add_plugin_wat(zone: i64) -> String {
    let qadd_json = format!(
        r#"{{"zone":{zone},"tracks":[{{"source":"tidal","source_id":"track-99","title":"T","artist":"A"}}]}}"#
    );
    let qadd_json = qadd_json.as_str();
    let env_json = r#"{"status":201,"body":{"added":true,"plugin":"wasmtest"}}"#;
    let qadd_off = 16usize;
    let env_off = qadd_off + qadd_json.len();
    let qadd_lit = qadd_json.replace('"', "\\\"");
    let env_lit = env_json.replace('"', "\\\"");
    format!(
        r#"(module
  (import "tune" "host_queue_add" (func $host_queue_add (param i32 i32) (result i64)))
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const 1024))
  (data (i32.const {qadd_off}) "{qadd_lit}")
  (data (i32.const {env_off}) "{env_lit}")

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

  ;; Call host_queue_add (drop its result), then return the static envelope.
  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (drop (call $host_queue_add (i32.const {qadd_off}) (i32.const {qadd_len})))
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const {env_off})) (i64.const 32))
      (i64.extend_i32_u (i32.const {env_len})))))
"#,
        abi = HOST_ABI_VERSION,
        qadd_off = qadd_off,
        env_off = env_off,
        qadd_lit = qadd_lit,
        env_lit = env_lit,
        qadd_len = qadd_json.len(),
        env_len = env_json.len(),
    )
}

const MANIFEST: &str = r#"{
    "id": "wasmtest",
    "name": "WASM Test",
    "version": "1.0.0",
    "description": "P2 integration test plugin",
    "author": "test",
    "entry_point": "main.wasm",
    "permissions": ["queue"]
}"#;

/// Multi-thread runtime: the route handler drives the wasm call inside
/// `spawn_blocking` (the Store isn't Sync), and the host's async capabilities
/// `block_on` the runtime from that blocking thread — which needs worker
/// threads to make progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasm_plugin_route_dispatches_and_exercises_real_host() {
    let _environment = crate::lock_environment();
    // A plugins dir with one wasm plugin. `main.wasm` holds WAT text; wasmtime's
    // `wat` feature parses it via content sniffing, so a
    // real wasm32 toolchain is not needed for the test.
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin_dir = dir.path().join("wasmtest");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("manifest.json"), MANIFEST).unwrap();

    // Point the loader at our scratch dir. Each `cargo test` binary is its own
    // process, so this env var cannot leak into the default integration.rs run.
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", dir.path());
        // Skip the child-process probe: from a libtest binary, spawning
        // current_exe re-runs the whole suite instead of probing.
        std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();

    // A real zone for the plugin to target: `queue_items.zone_id` is a foreign
    // key into `zones`, so the host's append needs the zone to exist first.
    let (zone_id, _) = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get_or_create("Test Zone", Some("browser"), "test-device")
        .expect("create zone");
    std::fs::write(plugin_dir.join("main.wasm"), queue_add_plugin_wat(zone_id)).unwrap();

    // Load wasm plugins against the REAL AppState (builds AppStateHost inside).
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    let registry = state.wasm_plugins.get().expect("registry published");
    assert_eq!(registry.len(), 1, "the one plugin must have loaded");
    assert!(registry.get("wasmtest").is_some());

    let app = tune_server::routes::router(state.clone());

    // Hit the mounted plugin route.
    let resp = app
        .oneshot(
            Request::post("/api/v1/plugins/wasmtest/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // The plugin's `{status, body}` envelope became the HTTP response.
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "plugin envelope status (201) must drive the HTTP status"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["added"], Value::Bool(true));
    assert_eq!(json["plugin"], Value::String("wasmtest".into()));

    // The REAL host was exercised: host_queue_add appended the streaming track
    // to zone 1's actual play queue. A mock/stub host could not have produced
    // this row in the real `queue_items` table.
    let qr = PlayQueueRepo::with_backend(state.backend.clone());
    let count = qr.count_all(zone_id).expect("count queue");
    assert_eq!(count, 1, "host_queue_add must have appended one row");
    let entries = qr.get_ordered(zone_id).expect("ordered queue");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].source_id.as_deref(),
        Some("track-99"),
        "the appended row is the streaming track the plugin sent to host_queue_add"
    );
}
