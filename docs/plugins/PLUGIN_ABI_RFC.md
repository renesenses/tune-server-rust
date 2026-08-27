# RFC — Tune Plugin ABI (host ↔ WASM)

Status: **Draft**. Base for the DJ mode / Party mode plugin work.
Author: Bertrand + Claude. Date: 2026-07-27.

## 1. Goal & scope

Today `tune-core/src/plugins.rs` is a **registry only**: it scans a plugin dir,
reads `manifest.json`, and `enable()` merely checks that the `entry_point`
(`main.wasm`) file exists. Nothing is ever executed.

This RFC specifies the missing piece: a **binary contract** so a plugin compiled
to `wasm32` can be *loaded, executed, and given controlled access to the server*
(register HTTP routes, read/modify the play queue, drive playback, receive
events) — inside a sandbox gated by the manifest's `permissions`.

**Non-goals:** replacing the native out-of-tree composition model (§2), a public
package registry, hot-reload, multi-language SDKs beyond the Rust reference.

## 2. Two composition models — pick per plugin

Tune already has **two** ways to extend the server; they are complementary, not
competitors:

| | **A. WASM plugin (this RFC)** | **B. Native compose (#917 / #878)** |
|---|---|---|
| Mechanism | `main.wasm` loaded by wasmtime, sandboxed | downstream Rust binary links tune-server as a lib, injects modules at compile time |
| Isolation | strong sandbox, capability-gated | none (native, full access) |
| Language | any → wasm | Rust only |
| Latency/access | mediated by host-functions | direct, real-time |
| Distribution | one file, `pip`/download | ship a binary |
| Used by | Party mode, most future 3rd-party plugins | tune-diretta (OutputProvider), private outputs |

**Decision for the two plugins we are about to build:**
- **Party mode → model A (WASM).** It only manipulates a collaborative queue and
  serves a few endpoints — a perfect first client to validate the ABI.
- **DJ mode → model B (native) or dedicated host-audio functions.** Real-time
  crossfade / decode / waveform / tempo-sync need the audio pipeline and the
  native decoder; they map badly to a sandbox. Its non-ported routes
  (`load/volume/sync-tempo/waveform/analyze`) are exactly the host-bound ones.

The rest of this RFC specifies **model A**.

## 3. WASM ABI

### 3.1 Runtime
- Isolate **`wasmtime`** in `tune-plugin-runtime-wasm`, pulled only by the
  `plugins-wasm` feature of `tune-server`.
- One `Engine` per process; one `Store` + `Instance` per active plugin.
- Enforce **resource limits**: max linear memory (e.g. 64 MiB), execution
  **fuel** or **epoch interruption** (kill a call that runs > N ms), no WASI
  filesystem/net by default (capabilities are the only I/O).

### 3.2 Memory & marshalling
WASM only passes integers, so all structured data crosses as **UTF-8 JSON in
linear memory**, addressed by `(ptr: u32, len: u32)`.
- Plugin exports `alloc(len)->ptr` and `dealloc(ptr,len)` (already stubbed in the
  DJ/Party crates).
- Convention: a returned buffer is `[len: u32 LE][json bytes]`, or a packed
  `u64 = (ptr<<32)|len` (as the crates already do). Host reads it, then calls
  `dealloc`.
- Host→plugin: host `alloc`s in the plugin's memory, writes JSON, passes
  `(ptr,len)`.

### 3.3 Plugin exports (required)
```
abi_version() -> u32                    // must equal HOST_ABI_VERSION
plugin_manifest() -> (ptr,len)          // JSON: id, routes[], event_subscriptions[]
plugin_init(ctx_json_ptr,len) -> i32    // 0 = ok; receives server version, config
plugin_dispatch(req_ptr,len) -> u64     // JSON request -> JSON response (packed ptr/len)
plugin_on_event(evt_ptr,len)            // optional; fired for subscribed events
alloc(len)->ptr ; dealloc(ptr,len)
```

### 3.4 Host imports (capabilities = the real surface)
Grouped by the `permissions` that unlock them. All take/return `(ptr,len)` JSON.
Unlisted permission ⇒ the import traps (deny by default).

| Permission | Host functions |
|---|---|
| *(always)* | `host_log(level,msg)` |
| `queue` | `host_queue_get(zone)`, `host_queue_add(zone,tracks)`, `host_queue_set(zone,tracks,pos)`, `host_queue_remove/move` |
| `playback` | `host_now_playing(zone)`, `host_play(zone,req)`, `host_pause/stop/next/prev(zone)`, `host_seek(zone,ms)`, `host_set_volume(zone,v)` |
| `library` | `host_search(q)`, `host_track_get(id)` |
| `events` | `host_emit(event,payload)` (subscription is declared in the manifest) |
| `kv` | `host_kv_get/set(key)` — per-plugin settings namespace |
| `net` | `host_fetch(req)` — gated outbound HTTP, host-executed (no raw sockets) |

Host functions run **async on the host side**; the wasm call is suspended via a
host-provided await shim (or the call returns a future-id the plugin polls —
decide in impl; simplest first cut = synchronous host calls executed on a
blocking thread).

### 3.5 HTTP route mounting
- The manifest declares `routes: [{ method, path }]` under `/api/v1/plugins/{id}/…`.
- The host mounts a single axum handler that packages `{method, path, query,
  headers, body}` as JSON and calls `plugin_dispatch`; the plugin's JSON response
  `{status, headers, body}` is returned to the client.
- Auth/premium gating stays host-side (the host checks the license before
  dispatching, reusing `premium_guard`).

### 3.6 Events
- Manifest `event_subscriptions: ["playback.*","zone.*"]`.
- Host forwards matching `event_bus` events to `plugin_on_event` (fire-and-forget,
  timeout-bounded).

### 3.7 Lifecycle & errors
- `enable` → instantiate, check `abi_version`, call `plugin_init`; on trap/limit
  → mark `PluginState::Error`, keep server running.
- Every plugin call is timeout/fuel-bounded; a misbehaving plugin can never hang
  or crash the host.

## 4. Versioning
`HOST_ABI_VERSION: u32`. `enable` rejects a plugin whose `abi_version()` differs
(or is outside a supported range). Manifest `min_server_version` already exists.

## 5. Party mode mapped onto the ABI (worked example)
- Permissions: `["queue","playback","events"]`.
- Manifest routes → the existing dispatch actions: `status, enable, disable, add,
  queue, vote, vote_reset` under `/api/v1/plugins/party/…`.
- `party_add` uses `host_search` + `host_queue_add`; `party_vote` reorders via
  `host_queue_set`; `status` uses `host_now_playing`. The current in-memory
  `PartyState` (votes) stays inside the plugin; the actual queue lives in the host
  via the queue host-functions. → **no core code needed beyond the ABI.**

## 6. DJ mode (native path, sketch)
Because crossfade/decoding are real-time and host-bound, DJ mode ships as a
**native** module (model B) or gets **dedicated host-audio functions**
(`host_dj_crossfade`, `host_dj_load_deck`, …) if we still want it as WASM. The
crate already implements the state machine (crossfader position, auto-crossfade,
decks); only the audio-touching calls need the host. Decide once the ABI exists.

## 7. Implementation plan (phased)
- **P0** — wasmtime load + `abi_version`/`plugin_init`/`plugin_dispatch` + JSON
  marshalling + limits. No host-functions yet (echo plugin test).
- **P1** — host-functions: `log`, `queue`, `playback`, `now_playing` + permission
  gating.
- **P2** — route mounting under `/api/v1/plugins/{id}` + license gating.
- **P3** — event forwarding (`plugin_on_event`).
- **P4** — **Party mode end-to-end** on the ABI (validates the whole thing).
- **P5** — decide DJ mode (native vs host-audio functions) and implement.

## 8. Open questions
- Sync vs async host-call bridge (fuel/epoch interaction with blocking host I/O).
- Do plugins get their own DB tables, or only the `kv` namespace?
- Packaging: bundle the reference Rust SDK crate (`tune-plugin-sdk`) that wraps
  the raw ABI so plugin authors write `impl TunePlugin` instead of `extern "C"`.
- Signature/trust for 3rd-party wasm (out of scope for first cut — our own
  plugins only).
