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

## Global shortcuts

`Cmd/Ctrl+Shift+Space` play/pause · `Right` next · `Left` prev ·
`Up`/`Down` volume ±5. On macOS they may require Accessibility/Input
Monitoring permission; registration failures are logged, not fatal.
