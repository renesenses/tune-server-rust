# tune-widget

Tune's desktop **mini-player** — a Tauri 2 tray app (macOS/Windows) with a
compact now-playing UI, transport controls, per-zone volume, search, and
global media-control shortcuts. It talks to a running `tune-server` over its
HTTP/WS API; the server stays the single source of truth.

## Isolated from the server workspace

This crate is **not** a member of the root `tune-server-rust` workspace
(root `Cargo.toml` has `exclude = ["tune-widget"]`, and `src-tauri/Cargo.toml`
declares its own `[workspace]`). That keeps its GUI/webkit dependencies out of
the server CI (`cargo fmt --all`, tests, clippy) and out of the server build
matrix. Run Cargo commands from `tune-widget/src-tauri/`.

## Build locally

```bash
cargo install tauri-cli --version "^2"     # once
cd tune-widget/src-tauri
cargo tauri build                          # → target/release/bundle/{macos,dmg}
```

Point it at a server with `TUNE_SERVER_URL=http://<host>:8888` (default
`http://localhost:8888`).

## Release (signed + notarized)

Ships on its own cadence via [`.github/workflows/widget.yml`](../.github/workflows/widget.yml),
**not** with the server's `v*` tags. Trigger it with a `widget-v*` tag or
run it manually (`workflow_dispatch`). The workflow builds arm64 + Intel,
signs with the Developer ID certificate, and — because the notarization
secrets are set — notarizes and staples the DMG automatically, then publishes
it as a GitHub release asset.

## Stale web cache (#1704)

The UI is a page rendered by the system web engine, which keeps a disk copy of
what it loaded — **outside** the program folder on macOS, and on Windows inside
the install folder under `tune-widget.exe.WebView2\` (WebView2's documented
default when no `dataDirectory` is declared). Neither an uninstall nor a
reinstall used to clear it, so a UI fix could stay invisible forever: nothing
distinguished "the fix was not shipped" from "the fix is hidden by a cache".

Two nets now:

- **At startup**, `purge_webview_cache_on_version_change()` drops the HTTP
  cache whenever the running version differs from the stamp in
  `<config dir>/tune-widget/webview-cache-version`. Only the caches — the
  neighbouring `Local Storage` holds the server address the user typed in.
- **At uninstall**, `src-tauri/installer-hooks.nsh` clears the same caches, and
  removes the whole profile plus `%APPDATA%\tune-widget` when the user ticks
  *delete application data*.

If you ever need to reproduce a user's stale UI, delete that stamp file rather
than the cache: the next launch will purge it the same way theirs does.

## Global shortcuts

`Cmd/Ctrl+Shift+Space` play/pause · `Right` next · `Left` prev ·
`Up`/`Down` volume ±5. On macOS they may require Accessibility/Input
Monitoring permission; registration failures are logged, not fatal.
