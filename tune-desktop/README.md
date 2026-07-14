# tune-desktop

Native desktop controller for Tune (Tauri 2). A **thin client**: it hosts the
existing web UI in a native window and talks to a running `tune-server` over its
HTTP/WS API. The server stays the single source of truth — the desktop app adds
only what the browser sandbox can't do (native dialogs, OS integration, and
later media keys and local audio devices).

## Status: scaffold

What works today:

- Compiles as a workspace member (`cargo check -p tune-desktop`).
- Tauri 2 shell wrapping the built `../xtune-web` frontend.
- `tauri-plugin-dialog` wired (native file/folder picker).
- One bridge command, `app_info`, returning name/version and the configured
  server URL (`TUNE_SERVER_URL`, default `http://localhost:8888`).

## Run (dev)

Requires the Tauri CLI:

```bash
cargo install tauri-cli --version "^2"     # once
cd tune-desktop
cargo tauri dev                            # or: cargo run -p tune-desktop
```

Point it at a server with `TUNE_SERVER_URL=http://<host>:8888`.

## Real-time model

The frontend connects to `GET /ws`, receives the `type: "snapshot"` message for
the full current state, then applies the typed delta events (zone online/offline,
volume, track, queue, groups, devices …). No polling. See `tune-core`'s
`event_types::EventType` for the contract.

## Next steps

- **Frontend server base URL.** The bundled `xtune-web` makes same-origin API
  calls; under the `tauri://` origin those won't reach the server. Wire the
  frontend to use `invoke("app_info").server_url` as its REST/WS base (or load
  the server-hosted UI directly during early bring-up).
- **Media keys** — add `tauri-plugin-global-shortcut`, register play/pause/next
  and forward them to the server (emit a Tauri event the frontend handles).
- **Local audio output** — optionally embed `tune-core` (`local-audio`) so this
  machine can itself be a render target / zone.
- **Tray + Now Playing**, native notifications, packaged icons/bundles.
