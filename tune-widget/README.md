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

Point it at a server from the ⚙ panel (`host:8888`); the address is kept in
`<config dir>/tune-widget/config.json` (`server_url`) and shared with the
native side (global shortcuts, full window). Default: `http://localhost:8888`.

## Release (signed + notarized)

Recipe: [`.github/workflows/widget.yml`](../.github/workflows/widget.yml) —
macOS arm64 + Intel (signed with the Developer ID certificate, notarized and
stapled) and Windows x64 (NSIS installer, **not signed** until the
Authenticode certificate exists; the signing step is already in place and
stays inert while `WIN_SIGN_CERT_BASE64` is empty).

Three ways in:

- **With every server release (since rc10).** `release.yml` calls
  `widget.yml` (`workflow_call`, input `release_tag`). The files are named
  after the server tag — `tune-widget-<tag>-macos-aarch64.dmg`,
  `tune-widget-<tag>-macos-x86_64.dmg`,
  `tune-widget-<tag>-windows-x86_64-setup.exe` — and land in the same draft
  release, inside `SHA256SUMS`. The `tune-widget-` prefix keeps them out of the
  self-updater (`tune-server-` archives only) and of the /download page.
- **Manually** (`workflow_dispatch`): with `release_tag` set, the run produces
  exactly the files a release would carry, without any tag — the way to check
  the chain before a train. Without it, Tauri's own file names.
- **On its own cadence** with a `widget-v*` tag, which must match
  `tauri.conf.json`'s version and publishes a separate `widget-v*` release.

Bump the widget version (`tauri.conf.json`, `Cargo.toml`, `Cargo.lock`)
whenever its interface changes: the web-cache purge below keys on it.

## Mini view = the web client's phone layout (rc10)

When the server's web client announces it (`GET /vue-mini.json` →
`{"vue_mini":1}`), the widget shows ONE window that loads
`http://<server>/?vue=mini&hote=widget`: the phone layout of the web client
(mockup A "remote", approved 31 Oct 2026). Older servers keep the bundled
interface below. Sizes: 360 × 560, resizable up to 760 px wide; collapsed
360 × 140 (mockup C).

That window loads the configured server and nothing else
(`vue_mini::decider_navigation`): other http(s) links open in the system
browser, other schemes are refused. The page drives the window only through
four same-origin addresses that are intercepted and never loaded:
`/__widget/epingle` (always on top), `/__widget/replier`,
`/__widget/deplier`, `/__widget/complet`. No Tauri command is exposed to the
remote page. Pin and collapse states persist in `config.json`
(`mini_epingle`, `mini_replie`).

## Server contract

`src/contrat.js` holds everything the interface reads from or sends to the
server, away from the DOM, and `tests/contrat.test.js` pins it
(`node --test tune-widget/tests/*.test.js`, also run by `widget-ci.yml`).
Rules worth knowing before touching it:

- `volume` is always a linear gain `0..1`, both ways. The server reads any
  number `> 1` as a percentage — sending the slider's `1` used to mean 100 %.
  A zone whose `GET /zones` entry says `volume_curve: "db60"` (software
  outputs) has a dB slider: same formulas as `tune_core::audio::volume_scale`
  (also ported to `src-tauri/src/volume.rs` for the global shortcuts).
- `GET /search` returns library hits under `local`; the widget asks for
  `sources=local` only, since it cannot play streaming results.
- WebSocket messages are matched on their exact `type` and on
  `data.zone_id`.
- No authentication: the widget works against a server with
  `auth_enabled = false` (the default). With authentication on, the mini
  player gets `401`s; the full window (the server's own web UI) still works.

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
