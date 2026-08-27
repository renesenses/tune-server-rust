//! End-to-end proof that the **real, built** Party-mode plugin
//! (`~/DEV/tune-plugin-party`, committed here as a test fixture under
//! `tests/fixtures/plugins/party/`) runs through the P2 server wiring.
//!
//! Unlike `plugin_wasm_routes.rs` — which drives a hand-written WAT plugin to
//! prove the *wiring* — this test loads the genuine `party.wasm` (wasm32,
//! `abi_version=1`, route-based `plugin_dispatch`, importing the `"tune"`
//! host-functions `host_now_playing` / `host_queue_get` / `host_queue_add` /
//! `host_log`) against the **real** [`AppStateHost`], mounts it under
//! `/api/v1/plugins/party/…`, and exercises it over HTTP.
//!
//! Each step proves the plugin↔host round trip actually happened:
//!
//! * `GET  /status` → the body's `now_playing` is produced by the real host
//!   (`host_now_playing` → `PlaybackManager::get_state`), not a stub.
//! * `POST /add`    → the plugin calls `host_queue_add`, which appends to the
//!   **actual** `queue_items` table — we assert a real row was created (a stub
//!   host could not produce it) with the `track_id` the plugin forwarded.
//! * `GET  /queue`  → the plugin's `host_queue_get` reflects that same row.
//! * `POST /vote` + `/queue` → the plugin-local vote tally changes.
//!
//! Whole file is gated behind `plugins-wasm`; under the default build it
//! compiles to nothing, leaving the default integration tests untouched.
#![cfg(feature = "plugins-wasm")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_server::state::AppState;

/// Absolute path to the committed fixture plugins dir, resolved at compile time
/// from `CARGO_MANIFEST_DIR` so the test is independent of the process CWD.
fn fixtures_plugins_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("plugins")
}

/// Drive one HTTP request against the mounted plugin router and return
/// `(status, json_body)`.
async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = if body.is_null() {
        Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap()
    } else {
        Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Multi-thread runtime: the route handler drives the wasm call inside
/// `spawn_blocking` (the Store isn't `Sync`), and the host's async capabilities
/// `block_on` the runtime from that blocking thread — which needs worker
/// threads to make progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_party_plugin_round_trips_through_p2_wiring() {
    let _environment = crate::lock_environment();
    // Point the loader at the COMMITTED fixture dir (contains `party/`). Each
    // `cargo test` binary is its own process, so this env var cannot leak into
    // the default integration test run.
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", fixtures_plugins_dir());
        // Skip the child-process probe: from a libtest binary, spawning
        // current_exe re-runs the whole suite instead of probing.
        std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
    }

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();

    // A real zone for the plugin to target: `queue_items.zone_id` is a foreign
    // key into `zones`, so the host's append needs the zone to exist first.
    let (zone_id, _) = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get_or_create("Party Zone", Some("browser"), "party-device")
        .expect("create zone");

    // The real Party plugin's `/add` forwards a LOCAL `{track_id}` to
    // `host_queue_add`, and `queue_items.track_id` has an ON-enforced FK into
    // `tracks`. Seed one real track so the host append satisfies the FK — this
    // is the row the plugin will queue.
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, artist_id, file_path, duration_ms) \
             VALUES ('Dancing Queen', NULL, '/music/party/dancing_queen.flac', 231000)",
            &[],
        )
        .expect("insert track");
    let track_id = state.backend.last_insert_rowid();
    assert!(track_id > 0, "seeded track must have a real id");

    // Load wasm plugins against the REAL AppState (builds AppStateHost inside).
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    let registry = state.wasm_plugins.get().expect("registry published");
    assert!(
        registry.get("party").is_some(),
        "the real party.wasm must have loaded (abi_version, exports and \
         `tune` imports all resolved against the runtime)"
    );

    let app = tune_server::routes::router(state.clone());

    // ---- (a) GET /status : now_playing comes from the REAL host -----------
    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/v1/plugins/party/status?zone={zone_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "status route → 200");
    assert!(
        body.get("enabled").is_some(),
        "status body carries the plugin's `enabled` flag: {body}"
    );
    let np = body
        .get("now_playing")
        .expect("status body has now_playing");
    // The host's `host_now_playing` (playback::get_state) ran — the plugin did
    // NOT get a `permission_denied`/`host unavailable` back (those would appear
    // as an `error` string). A real now-playing state object is returned.
    assert!(
        np.is_object() && np.get("error").is_none(),
        "now_playing must be a real host state object, not an error: {np}"
    );

    // ---- (b) POST /add : plugin → host_queue_add → real queue_items row ---
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/plugins/party/add",
        json!({
            "zone": zone_id,
            "track_id": track_id,
            "title": "Dancing Queen",
            "artist_name": "ABBA",
            "added_by": "e2e-test",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "add route → 200");
    assert_eq!(
        body["added"],
        Value::Bool(true),
        "plugin reports the add succeeded: {body}"
    );
    assert_eq!(
        body["result"]["ok"],
        Value::Bool(true),
        "the host's queue_add envelope round-tripped back into the plugin body: {body}"
    );
    assert_eq!(body["result"]["added"], json!(1));

    // The decisive proof: a REAL row now exists in the actual `queue_items`
    // table, created by the host on the plugin's behalf. A stubbed host could
    // not have produced it.
    let qr = PlayQueueRepo::with_backend(state.backend.clone());
    assert_eq!(
        qr.count_all(zone_id).expect("count queue"),
        1,
        "host_queue_add must have appended exactly one queue_items row"
    );
    let entries = qr.get_ordered(zone_id).expect("ordered queue");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].track_id,
        Some(track_id),
        "the appended row is the local track the plugin forwarded to host_queue_add"
    );

    // ---- (c) GET /queue : the host queue reflects the added track ---------
    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/v1/plugins/party/queue?zone={zone_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "queue route → 200");
    // The real plugin reshapes the host `queue_get` result as
    // `{ queue: <host-response>, total }`. The host response is
    // `{ zone, length, position, tracks: [...] }`, so the queued track is at
    // `body.queue.tracks[]`. We locate our track_id anywhere in that structure
    // to stay robust to reshaping, then pin the exact host-side shape too.
    assert!(
        json_contains_track_id(&body, track_id),
        "GET /queue must reflect the track the plugin added via the host: {body}"
    );
    // The plugin unwraps the host `{…, tracks:[…]}` envelope and returns a flat
    // array of tracks decorated with the plugin's per-track `votes`. No vote
    // cast yet → `votes == 0`.
    let queue = body
        .pointer("/queue")
        .and_then(Value::as_array)
        .expect("GET /queue returns an array of annotated tracks under `queue`");
    assert_eq!(queue.len(), 1, "the single queued track is visible");
    assert_eq!(queue[0]["track_id"], json!(track_id));
    assert_eq!(queue[0]["votes"], json!(0), "no vote cast yet");

    // ---- (d) POST /vote then /queue : plugin-local vote tally changes -----
    let (status, body) = call(
        &app,
        "POST",
        "/api/v1/plugins/party/vote",
        json!({ "track_id": track_id, "vote": 5 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "vote route → 200");
    assert_eq!(
        body["total_votes"],
        json!(5),
        "the plugin accumulated the vote in its own state: {body}"
    );

    // /status now reflects the vote tally the plugin keeps locally.
    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/plugins/party/status?zone={zone_id}"),
        Value::Null,
    )
    .await;
    assert_eq!(
        body["votes"][track_id.to_string()],
        json!(5),
        "the plugin's persisted vote tally survives across dispatches: {body}"
    );

    // ---- (e) GET /queue now carries the vote annotation, driven by the real
    // host envelope (regression guard for the `tracks`-key unwrap fix) --------
    let (_, body) = call(
        &app,
        "GET",
        &format!("/api/v1/plugins/party/queue?zone={zone_id}"),
        Value::Null,
    )
    .await;
    let queue = body
        .pointer("/queue")
        .and_then(Value::as_array)
        .expect("queue array");
    assert_eq!(
        queue[0]["votes"],
        json!(5),
        "GET /queue is annotated with the plugin's vote tally: {body}"
    );
}

/// Recursively test whether `track_id` appears as a `track_id` field anywhere
/// in `v` (robust to however the plugin nests the host queue response).
fn json_contains_track_id(v: &Value, track_id: i64) -> bool {
    match v {
        Value::Object(map) => {
            if map.get("track_id").and_then(Value::as_i64) == Some(track_id) {
                return true;
            }
            map.values().any(|x| json_contains_track_id(x, track_id))
        }
        Value::Array(a) => a.iter().any(|x| json_contains_track_id(x, track_id)),
        _ => false,
    }
}
