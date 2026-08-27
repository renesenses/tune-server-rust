//! P3 of the plugin ABI, end-to-end (`docs/plugins/PLUGIN_ABI_RFC.md` §3.3/§3.6).
//!
//! Proves the event-forwarding wire: a wasm plugin that subscribes to
//! `["playback.*"]` and EXPORTS `plugin_on_event` is loaded, the
//! `spawn_wasm_event_forwarder` task is started, and when the server
//! `event_bus` emits an event the forwarder calls the plugin's
//! `plugin_on_event` with `{name, payload}`. The plugin stores the last event
//! it received; a `plugin_dispatch` route (`GET /last-event`) hands it back, so
//! the test can read it over real HTTP through the P2 route mount + registry.
//!
//! We assert both directions of the glob subscription:
//!  * a subscribed event (`playback.state_changed`) IS delivered and echoed;
//!  * an unsubscribed event (`library.scanned`) is NOT delivered — after
//!    emitting it, `/last-event` still returns the earlier playback event.
//!
//! Whole file is gated behind `plugins-wasm`; under the default build it
//! compiles to nothing, leaving the 54 integration.rs tests untouched.
#![cfg(feature = "plugins-wasm")]

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_plugin_runtime_wasm::HOST_ABI_VERSION;
use tune_server::state::AppState;

/// A hand-written WAT plugin that EXPORTS `plugin_on_event` (RFC §3.3): it
/// copies the received `{name,payload}` JSON to a fixed offset and records its
/// length. `plugin_dispatch` (the route handler) wraps that stored event in a
/// `{"status":200,"body":<event>}` envelope and returns it — so `GET
/// /last-event` yields whatever event was last delivered. Offsets/lengths are
/// computed in Rust; the envelope prefix's `"` are escaped for the WAT literal
/// while the stored bytes stay the un-escaped JSON (same technique as the P1/P2
/// runtime tests).
fn on_event_plugin_wat() -> String {
    // Envelope wrapping: `{"status":200,"body":` + <stored event> + `}`.
    let prefix = r#"{"status":200,"body":"#;
    let prefix_esc = prefix.replace('"', "\\\"");
    let prefix_len = prefix.len();
    let prefix_off = 16usize;
    let suffix_off = 64usize;
    let store_off = 4096usize;
    let bump_start = 8192usize;
    format!(
        r#"(module
  (memory (export "memory") 4)
  (global $bump (mut i32) (i32.const {bump_start}))
  (global $evt_len (mut i32) (i32.const 0))
  (data (i32.const {prefix_off}) "{prefix_esc}")
  (data (i32.const {suffix_off}) "}}")

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

  ;; Store the delivered event bytes; remember the length.
  (func (export "plugin_on_event") (param $ptr i32) (param $len i32)
    (memory.copy (i32.const {store_off}) (local.get $ptr) (local.get $len))
    (global.set $evt_len (local.get $len)))

  ;; Build {{"status":200,"body":<stored event>}} and return packed ptr/len.
  (func (export "plugin_dispatch") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local $total i32)
    (local.set $total
      (i32.add (i32.add (i32.const {prefix_len}) (global.get $evt_len)) (i32.const 1)))
    (local.set $out (call $alloc (local.get $total)))
    (memory.copy (local.get $out) (i32.const {prefix_off}) (i32.const {prefix_len}))
    (memory.copy
      (i32.add (local.get $out) (i32.const {prefix_len}))
      (i32.const {store_off})
      (global.get $evt_len))
    (memory.copy
      (i32.add (local.get $out) (i32.add (i32.const {prefix_len}) (global.get $evt_len)))
      (i32.const {suffix_off})
      (i32.const 1))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (local.get $total)))))
"#,
        abi = HOST_ABI_VERSION,
        bump_start = bump_start,
        prefix_off = prefix_off,
        suffix_off = suffix_off,
        store_off = store_off,
        prefix_esc = prefix_esc,
        prefix_len = prefix_len,
    )
}

const MANIFEST: &str = r#"{
    "id": "eventtest",
    "name": "Event Test",
    "version": "1.0.0",
    "description": "P3 event forwarding test plugin",
    "author": "test",
    "entry_point": "main.wasm",
    "permissions": [],
    "event_subscriptions": ["playback.*"]
}"#;

/// Fetch `/api/v1/plugins/eventtest/last-event` and return the parsed body.
async fn last_event(state: &AppState) -> Value {
    let app = tune_server::routes::router(state.clone());
    let resp = app
        .oneshot(
            Request::get("/api/v1/plugins/eventtest/last-event")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET /last-event must be 200");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

/// Multi-thread runtime: the forwarder drives each `plugin_on_event` inside
/// `spawn_blocking` (the wasmtime Store isn't Sync), which needs worker threads
/// to make progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasm_plugin_receives_subscribed_events_only() {
    // Plugins dir with one event-subscribing wasm plugin. `main.wasm` holds WAT
    // text; wasmtime's `wat` feature parses it via content sniffing, so no
    // wasm32 toolchain is needed.
    let dir = tempfile::tempdir().expect("tempdir");
    let plugin_dir = dir.path().join("eventtest");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("manifest.json"), MANIFEST).unwrap();
    std::fs::write(plugin_dir.join("main.wasm"), on_event_plugin_wat()).unwrap();

    // Each `cargo test` binary is its own process, so this env var cannot leak
    // into the default integration.rs run.
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", dir.path());
        // Skip the child-process probe: from a libtest binary, spawning
        // current_exe re-runs the whole suite instead of probing.
        std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();

    // Load the plugin against the real AppState, then start the P3 forwarder
    // (it subscribes to the bus synchronously here, before we emit anything).
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    assert_eq!(
        state.wasm_plugins.get().expect("registry").len(),
        1,
        "the one plugin must have loaded"
    );
    tune_server::plugins_host::spawn_wasm_event_forwarder(&state);

    // A subscribed event: `playback.*` matches `playback.state_changed`.
    state.event_bus.emit(
        "playback.state_changed",
        json!({ "zone": 1, "state": "playing" }),
    );
    // Give the async forwarder a beat to deliver.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let body = last_event(&state).await;
    assert_eq!(
        body["name"], "playback.state_changed",
        "the subscribed event must have been delivered to plugin_on_event"
    );
    assert_eq!(
        body["payload"]["state"], "playing",
        "the event payload must round-trip verbatim through the forwarder"
    );
    assert_eq!(body["payload"]["zone"], 1);

    // An UNsubscribed event: `library.scanned` does not match `playback.*`, so
    // it must NOT reach the plugin — `/last-event` should still be the earlier
    // playback event.
    state
        .event_bus
        .emit("library.scanned", json!({ "tracks": 999 }));
    tokio::time::sleep(Duration::from_millis(300)).await;

    let body = last_event(&state).await;
    assert_eq!(
        body["name"], "playback.state_changed",
        "an event outside the subscription must NOT be delivered (last-event unchanged)"
    );
    assert!(
        body["payload"].get("tracks").is_none(),
        "the unsubscribed library.scanned payload must never have reached the plugin"
    );
}
