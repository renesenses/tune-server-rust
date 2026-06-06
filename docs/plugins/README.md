# Tune Plugin Developer Guide

## Overview

Tune supports plugins that react to server events, read/write configuration,
and extend behaviour without modifying core code.  Plugins are implemented as
Rust types that satisfy the `TunePlugin` trait.

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
  "entry_point": "main.wasm",
  "permissions": ["playback", "library"],
  "min_server_version": "0.8.0"
}
```

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
        match event.event_type.as_str() {
            "playback.started" => { /* react */ }
            "library.scan.complete" => { /* react */ }
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
| `api_base_url` | `String`         | Base URL of the Tune HTTP API              |
| `data_dir`     | `PathBuf`        | Plugin-specific writable data directory    |
| `event_bus`    | `Option<EventBus>` | Direct access to the event bus (if set) |

## Event Types

Events use a dotted namespace convention.  The full list of typed events is
defined in `tune-core/src/event_types.rs`:

### Playback

| Event                | Data fields                                      |
|----------------------|--------------------------------------------------|
| `playback_started`   | `zone_id`, `track_id`, `title`, `artist_name`    |
| `playback_stopped`   | `zone_id`                                        |
| `playback_paused`    | (generic)                                        |
| `playback_resumed`   | (generic)                                        |
| `track_changed`      | `zone_id`, `track_id`, `title`, `artist_name`, `album_title`, `cover_url` |
| `volume_changed`     | `zone_id`, `volume`, `muted`                     |
| `seek_changed`       | (generic)                                        |
| `shuffle_changed`    | (generic)                                        |
| `repeat_changed`     | (generic)                                        |

### Queue

| Event           | Data fields |
|-----------------|-------------|
| `queue_changed` | (generic)   |

### Library

| Event                  | Data fields                           |
|------------------------|---------------------------------------|
| `scan_started`         | (generic)                             |
| `scan_progress`        | `scanned`, `total`, `current_path`    |
| `scan_complete`        | (generic)                             |
| `library_track_added`  | (generic)                             |
| `library_track_removed`| (generic)                             |
| `library_track_updated`| (generic)                             |

### Devices

| Event               | Data fields                               |
|----------------------|-------------------------------------------|
| `device_discovered`  | `device_id`, `name`, `device_type`, `host`|
| `device_lost`        | (generic)                                 |

### Zones

| Event           | Data fields |
|-----------------|-------------|
| `zone_created`  | (generic)   |
| `zone_deleted`  | (generic)   |
| `zone_updated`  | (generic)   |

### Services

| Event                  | Data fields |
|------------------------|-------------|
| `service_connected`    | (generic)   |
| `service_disconnected` | (generic)   |

### Social / Party

| Event              | Data fields |
|--------------------|-------------|
| `party_track_added`| (generic)   |
| `party_vote`       | (generic)   |

### System

| Event              | Data fields |
|--------------------|-------------|
| `profile_switched` | (generic)   |
| `error`            | (generic)   |

The `EventBus` also supports free-form dotted events such as
`library.scan.started`, `zone.created`, `system.restart`, etc.

## Permission Scopes

Declared in `manifest.json` under `permissions`:

- `playback` -- control and observe playback state
- `library` -- read/write library data
- `settings` -- read/write server settings
- `network` -- discover and interact with network devices

## Installation

### From the REST API

```bash
# Install a plugin
POST /api/plugins/{name}/install

# Enable / disable
POST /api/plugins/{name}/enable
POST /api/plugins/{name}/disable

# Uninstall
DELETE /api/plugins/{name}

# List all plugins
GET /api/plugins
```

### Manual installation

1. Create a directory under `plugins/` named after your plugin ID.
2. Place `manifest.json` and your entry point file inside.
3. Restart the server or call `POST /api/plugins/{name}/install`.

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
