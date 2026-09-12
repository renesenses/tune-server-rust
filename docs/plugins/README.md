# Tune Plugin Developer Guide

## Overview

Tune supports plugins that react to server events, read/write configuration,
and extend behaviour without modifying core code.  Plugins are implemented as
Rust types that satisfy the `TunePlugin` trait.

### Loading model

Plugins are **compiled into the server** behind cargo features.  There is no
`libloading` and no wasm runtime — `docs/ARCHITECTURE-CIBLE-v0.9.md` lists
dynamic loading as a target, not the current state.  Adding a plugin is three
lines in `tune-server`: a feature, an optional dependency, and an arm in
`register_builtin_plugins` (`tune-server/src/plugins.rs`).

One cargo constraint to know before you reference an out-of-tree plugin by
path: **cargo resolves optional path dependencies while writing the lockfile**,
so a `path` dependency pointing at a directory that is not in the clone breaks
`cargo check` for everyone, feature enabled or not.  That is why no concrete
plugin is referenced in this repository.

### Plugins that live outside this repository

Which is the case for anything closed-source.  Rather than referencing it from
here, invert the dependency: `tune-server` is a library whose `run` *is* the
whole server startup, so a binary in its own workspace composes the two.

```toml
# ~/tune-dist/Cargo.toml — its own workspace, not a member of this one
[workspace]

[dependencies]
tune-server = { path = "../tune-server-rust/tune-server" }
tune-core   = { path = "../tune-server-rust/tune-core", default-features = false, features = ["plugin-http"] }
tokio       = { version = "1", features = ["full"] }
```

```rust,ignore
#[tokio::main]
async fn main() {
    tune_server::run(Some(Box::new(|state: &AppState| {
        vec![Box::new(MyPlugin::new(state.backend.clone())) as Box<dyn TunePlugin>]
    })))
    .await;
}
```

The closure receives `&AppState` because host services — `backend`,
`services`, `http_client` — only exist once state is built.  It runs after
local outputs are registered and before the router is built, the same point as
`register_builtin_plugins`, and what it returns is registered on **equal
terms**: same protocol gate, same `plugin_{name}_enabled` switch, same
registration draining, same `/api/v1/ext/{name}` mount.

This repository never learns the plugin's name, so a plain clone keeps
building, and there is no cargo feature to add here.

## Creating a Plugin

### 1. manifest.json

Every plugin lives in its own directory under `plugins/`.  The directory must
contain a `manifest.json`:

```json
{
  "id": "my-plugin",
  "name": "My Plugin",
  "version": "1.0.0",
  "description": "Short description of what the plugin does",
  "author": "Your Name",
  "entry_point": "my-plugin",
  "permissions": ["playback", "library"],
  "min_server_version": "0.9.13"
}
```

> **The server does not read this file yet.** Plugins are compiled in (see
> *Loading model* below), so the loader never scans manifests: `permissions` is
> not enforced and `min_server_version` is not compared to anything. Ship a
> manifest anyway — it is the forward-compatible shape — but do not rely on it
> for anything today. The version check that *is* enforced is
> `protocol_version` on the trait.

| Field                | Required | Description                                        |
|----------------------|----------|----------------------------------------------------|
| `id`                 | yes      | Unique slug (lowercase, hyphens)                   |
| `name`               | yes      | Human-readable name                                |
| `version`            | yes      | SemVer version string                              |
| `description`        | yes      | One-line description                               |
| `author`             | yes      | Author name or organisation                        |
| `entry_point`        | yes      | Relative path to the plugin binary/module          |
| `permissions`        | yes      | List of permission scopes (see below)              |
| `min_server_version` | no       | Minimum Tune server version required               |

### 2. Implement `TunePlugin`

```rust
use async_trait::async_trait;
use tune_core::plugin_sdk::{TunePlugin, PluginContext};
use tune_core::event_bus::TuneEvent;

pub struct MyPlugin;

#[async_trait]
impl TunePlugin for MyPlugin {
    fn name(&self) -> &str { "my-plugin" }
    fn version(&self) -> &str { "1.0.0" }
    fn description(&self) -> &str { "Does something useful" }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        // Called once when the plugin is loaded.
        // Use ctx to read config, store state, etc.
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        // Called when the plugin is unloaded or the server shuts down.
        Ok(())
    }

    async fn on_event(&mut self, event: &TuneEvent) {
        // Called for every event emitted on the event bus.
        //
        // Must return promptly: dispatch is sequential and holds the loader
        // lock, so blocking here delays every other plugin. Queue the work
        // and hand it to a task you own.
        match event.event_type.as_str() {
            "playback.started" => { /* react */ }
            "library.scan.completed" => { /* react */ }
            _ => {}
        }
    }
}
```

### 3. Register with the PluginLoader

```rust
use tune_core::plugin_sdk::PluginLoader;

let mut loader = PluginLoader::new(data_root)
    .with_event_bus(event_bus.clone())
    .with_db(db_backend);

loader.register(Box::new(MyPlugin)).await;
loader.setup_all("http://localhost:8888").await;
loader.start_event_dispatch(); // wires EventBus -> on_event
```

## Available Hooks

| Hook          | Signature                                                 | When called                          |
|---------------|-----------------------------------------------------------|--------------------------------------|
| `setup`       | `async fn setup(&mut self, ctx: &PluginContext)`          | Once, when the plugin is loaded      |
| `teardown`    | `async fn teardown(&mut self)`                            | Once, on unload or server shutdown   |
| `on_event`    | `async fn on_event(&mut self, event: &TuneEvent)`        | For every event on the event bus     |

## Optional declarations

| Method             | Default            | Purpose                                     |
|--------------------|--------------------|---------------------------------------------|
| `config_schema`    | `{}`               | JSON schema surfaced by `GET /api/v1/plugins` |
| `default_enabled`  | `true`             | `false` = opt-in: dormant until installed    |
| `catalogued`       | `true`             | `false` = never offered by the plugin manager |
| `protocol_version` | the SDK's constant | ABI generation this plugin was built against |

### `default_enabled` vs `catalogued`

These two answer different questions, and confusing them is what produced
[#2090](https://github.com/renesenses/tune-server-rust/issues/2090).

* `default_enabled() == false` makes a plugin **opt-in**: `setup_all` leaves it
  dormant until `plugin_{name}_installed == "true"`.  That is exactly what makes
  it **visible** in the manager, as an "Install" button.
* `catalogued() == false` takes it **out of the catalogue**: the manager never
  offers it.  The plugin is still compiled, still tested, and still loads if the
  install setting is written by hand — it simply stops promising.

A plugin whose routes answer but that no client screen can reach needs the
second, not the first.  Offering "Install" on a feature nothing exposes spends
the user's trust and returns nothing: they install, they restart as asked, and
nothing appears.

The filter applies to the *offer* only.  A plugin that is actually **running**
stays listed whatever `catalogued` says: hiding it would make it impossible to
uninstall, and would misreport what the machine is doing.

**Uncatalogued today** (all three are compiled into every published binary — see
the `--features` lines in `release.yml` — and all three keep their tests):

| Plugin    | Why it is not offered |
|-----------|-----------------------|
| `dj`      | **Not ready.** `HostServices` carries only the DB backend — no `PlaybackManager`, no output registry — so the plugin has no access to the audio path at all. 11 of its 13 routes change nothing: 7 echo their argument without writing anything, `/sync-tempo` answers `"tempo sync not yet implemented"`, and `/enable` `/disable` `/status` only write and re-read `dj_enabled_{zone}` — a setting those three handlers are the sole readers of. `/status` reports decks permanently `loaded: false`, contradicting `/load` one call earlier. Only `/waveform` and `/analyze` do real work. |
| `karaoke` | **Ready, but redundant — and narrower.** All three routes work. The product already ships karaoke and it is already reachable: the lyrics panel offers a "Karaoke" toggle and highlights the current line itself, from the same core `/lyrics/{id}` data this plugin reuses. Worse, `/now/{zone_id}` gives up when the current track has no library id, while the client falls back to `/lyrics/by-meta` and so keeps karaoke working on streaming. Installing it would buy a second, smaller door — at the cost of an install and a restart. `/now/{zone_id}` keeps a use of its own for a client with no position loop; re-catalogue it when such a client exists. |
| `concerts` | **Ready, but there is nothing to show yet — twice over.** The routes work; what they relay does not. The cloud table `concert_events` is empty and stays empty: its only writer is an endpoint nobody calls, and the one source wired behind it is MusicBrainz, whose `event` entity is an archive — 0 future dates across Coldplay, Taylor Swift and Metallica combined (measured 2026-08-27). And no client screen reaches `/api/v1/ext/concerts/upcoming`: `git grep -i concert` in `tune-web-client` returns nothing of the feature. Catalogue it when both are true — a source that knows the future, and a screen. See #2363. |

`PLUGIN_PROTOCOL_VERSION` is **enforced**: `setup_all` refuses a plugin whose
major differs, or whose minor is newer than the server's.  With plugins
compiled in the default can never disagree — there is one `tune-core` in the
graph — so the gate only fires on a deliberate override.  It exists for the day
`libloading` lets two generations coexist.

## Registration surface

Everything below is **collected** during `setup` and applied by the host once
`setup` returns.  A plugin therefore never holds the output-registry lock, and a
plugin whose `setup` returns `Err` has its registrations dropped rather than
installed — a broken plugin leaves no half-built device behind.

```rust
async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
    // 1. An audio output. Keyed by `device_id()`; refused if something
    //    already owns that id. Call once per output.
    ctx.register_output(Box::new(MyOutput::new("myplugin:1", "My Output")));

    // 2. HTTP routes. Mounted at /api/v1/ext/{plugin_name} — you do not
    //    choose the prefix. Requires the `plugin-http` feature on tune-core.
    ctx.register_router(axum::Router::new().route("/status", get(status)));

    // 3. A zone bound to one of your outputs, so it is selectable in the UI.
    ctx.register_zone("My Output", "myplugin", "myplugin:1");
    Ok(())
}
```

| Method            | Applied to                          | Notes |
|-------------------|-------------------------------------|-------|
| `register_output` | `OutputRegistry`, keyed on `device_id()` | Refused on id conflict, with a warning |
| `register_router` | `/api/v1/ext/{plugin_name}`         | `Router<()>`; the host derives the prefix |
| `register_zone`   | `zones` table, via `get_or_create`  | Only if one of *your* outputs claimed that `device_id` |

Two deliberate constraints:

- **You do not choose your mount prefix.**  Letting a plugin mount anywhere
  lets it shadow a core route, or another plugin's, with no diagnostic.  The
  namespace comes from `name()`.
- **Plugin routes sit inside the `/api/v1` tree**, so they inherit its auth,
  analytics and body-limit layers.  A plugin endpoint is *not* public.

The router is `Router<()>`: capture your own state in closures rather than
sharing the host's `AppState`, which keeps `tune-core` independent of
`tune-server`'s types.

## Database access

```rust
// The same backend the server uses — SQLite or PostgreSQL.
if let Some(db) = ctx.db() {
    let tracks = tune_core::db::track_repo::TrackRepo::with_backend(db);
    // ...
}
```

There is no sandbox: a plugin gets the host's `Arc<dyn DbBackend>` and can read
or write anything, including creating its own tables.  There is also **no
per-plugin migration framework** — if you need a table, issue a
`CREATE TABLE IF NOT EXISTS` in `setup` and version it yourself, e.g. under a
`ctx.get_config("schema_version")` key.

Prefer `ctx.db()` over calling your own server's REST API, and note the
`api_base_url` caveat below.

## PluginContext Methods

### Configuration (database-backed)

```rust
// Read a plugin-specific setting.
// Stored as `plugin_{name}_{key}` in the settings table.
ctx.get_config("volume") -> Option<String>

// Write a plugin-specific setting.
ctx.set_config("volume", "80") -> Result<(), String>
```

### File-based configuration

```rust
// Read config.json from the plugin's data directory.
let cfg: serde_json::Value = self.read_config(ctx);

// Write config.json to the plugin's data directory.
self.write_config(ctx, &cfg)?;
```

### Event emission

```rust
// Emit a custom event through the event bus.
ctx.emit_event("my_plugin.something_happened", serde_json::json!({
    "detail": "value"
}));
```

### Other fields

| Field          | Type             | Description                                |
|----------------|------------------|--------------------------------------------|
| `api_base_url` | `String`         | Base URL of the Tune HTTP API — see caveat |
| `data_dir`     | `PathBuf`        | Plugin-specific writable data directory    |
| `event_bus`    | `Option<EventBus>` | Direct access to the event bus (if set) |

> **`api_base_url` is unusable during `setup`.**  Plugins are set up while the
> HTTP listener is bound but not yet accepting, so a request to this URL from
> `setup` sits in the accept backlog until it times out.  Read the library
> through `ctx.db()` during setup, and keep `api_base_url` for later.

## Event Types

Events use a dotted namespace convention.  The full list of typed events is
defined in `tune-core/src/event_types.rs`:

These are the wire names — the strings `EventType::as_str` produces and that
`event.event_type` carries.  They are part of the client contract; match on them
verbatim.

### Playback

| Event                     | Data fields                                      |
|---------------------------|--------------------------------------------------|
| `playback.started`        | `zone_id`, `track_id`, `title`, `artist_name`    |
| `playback.stopped`        | `zone_id`                                        |
| `playback.paused`         | (generic)                                        |
| `playback.resumed`        | (generic)                                        |
| `playback.track_changed`  | `zone_id`, `track_id`, `title`, `artist_name`, `album_title`, `cover_url` |
| `playback.volume`         | `zone_id`, `volume`, `muted`                     |
| `playback.seek`           | (generic)                                        |
| `playback.shuffle`        | (generic)                                        |
| `playback.repeat`         | (generic)                                        |
| `playback.queue.changed`  | (generic)                                        |

### Library

| Event                     | Data fields                           |
|---------------------------|---------------------------------------|
| `library.scan.started`    | (generic)                             |
| `library.scan.progress`   | `scanned`, `total`, `current_path`    |
| `library.scan.completed`  | (generic)                             |
| `library.track.added`     | (generic)                             |
| `library.track.removed`   | (generic)                             |
| `library.track.updated`   | (generic)                             |

### Devices

| Event                | Data fields                               |
|----------------------|-------------------------------------------|
| `device.discovered`  | `device_id`, `name`, `device_type`, `host`|
| `device.lost`        | (generic)                                 |

### Zones and groups

| Event            | Data fields |
|------------------|-------------|
| `zone.created`   | (generic)   |
| `zone.deleted`   | (generic)   |
| `zone.updated`   | (generic)   |
| `group.created`  | (generic)   |
| `group.updated`  | (generic)   |
| `group.deleted`  | (generic)   |

### Services

| Event                   | Data fields |
|-------------------------|-------------|
| `service.connected`     | (generic)   |
| `service.disconnected`  | (generic)   |

### Social / Party

| Event                | Data fields |
|----------------------|-------------|
| `party.track_added`  | (generic)   |
| `party.vote`         | (generic)   |

### System

| Event              | Data fields |
|--------------------|-------------|
| `profile.switched` | (generic)   |
| `error`            | (generic)   |

The `EventBus` also carries free-form dotted events that have no `EventType`
variant, such as `system.restart` — and anything a plugin emits itself through
`ctx.emit_event`.

## Permission Scopes

Declared in `manifest.json` under `permissions`:

- `playback` -- control and observe playback state
- `library` -- read/write library data
- `settings` -- read/write server settings
- `network` -- discover and interact with network devices

**Not enforced.**  The loader never reads the manifest, so these are
documentation of intent, not a sandbox — a plugin runs in-process with the
server's own database handle.  Use the four scopes above rather than inventing
finer-grained names, so declarations stay comparable when enforcement lands
(`docs/ARCHITECTURE-CIBLE-v0.9.md` puts sandboxing after v1).

## Installation

A plugin is installed by **compiling it in**; there is no runtime install step.

1. Add a cargo feature and an optional dependency in `tune-server/Cargo.toml`.
2. Add an arm to `register_builtin_plugins` in `tune-server/src/plugins.rs`.
3. Rebuild with `--features your-plugin`.

### REST endpoints

```bash
# List plugins. SDK-backed entries carry "type": "sdk" and are the only ones
# reflecting running code — the rest is settings-table bookkeeping.
GET /api/v1/plugins

# Details for one plugin.
GET /api/v1/plugins/{name}

# Enable / disable / install / uninstall.
POST   /api/v1/plugins/{name}/enable
POST   /api/v1/plugins/{name}/disable
POST   /api/v1/plugins/{name}/install
DELETE /api/v1/plugins/{name}
```

`install` and `DELETE` only flip keys in the `settings` table — they cannot load
or unload a compiled-in plugin.  `disable` is different: it writes
`plugin_{name}_enabled = false`, which `setup_all` honours, so a compiled-in
plugin can be kept out **at the next start** without recompiling.  It is a boot
switch, not a hot unload.

## Architecture

```
EventBus ──emit──> broadcast channel
                        │
            PluginLoader.start_event_dispatch()
                        │
                        ▼
               ┌─────────────────┐
               │  for each plugin │
               │   on_event(ev)   │
               └─────────────────┘
```

The `PluginLoader` subscribes to the `EventBus` broadcast channel and
dispatches every event to all loaded plugins sequentially.  If a plugin
blocks for too long it will delay delivery to subsequent plugins, so
`on_event` implementations should be non-blocking or spawn their own tasks.
