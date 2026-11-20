# Tune Client (`tune-widget/`)

**Tune Client** is Tune's desktop app — a Tauri 2 app (macOS/Windows) that
shows the server's web interface in its own window, either as a mini-player
or as the full interface. It talks to a running `tune-server` over its
HTTP/WS API; the server stays the single source of truth. Install **Tune
Server** (the music server) and **Tune Client** (this app) to use Tune on a
single Mac; Tune Client alone drives a server running elsewhere on the LAN.

Named **Tune Widget** until rc12 (renamed on 20 Nov 2026, forum thread 2968,
#9255). What changed with the name, and what deliberately did not:

- displayed name, `.app` (`Tune Client.app`), window titles, menus, tray
  tooltip, and the release files (`tune-client-<tag>-…`) are **Tune Client**;
- the bundle identifier stays **`fr.mozaiklabs.tune-widget`**: macOS keys the
  *Local Network* permission, the WebKit data (the web client's
  `localStorage`: chosen profile, zone) and the web cache on it — changing it
  would make every existing user grant the permission again and lose that
  state. The executable (`tune-widget`), the Cargo crate, this folder, the
  config folder (`<config dir>/tune-widget/`) and `widget.yml` keep their
  names for the same reason (and so that the Windows uninstaller hooks keep
  finding `tune-widget.exe.WebView2`);
- a Mac that had *Tune Widget.app* keeps it next to *Tune Client.app*: the
  old one can be dragged to the Trash (its settings are shared).

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

A `--debug` bundle (`cargo tauri build --debug --bundles app`) is enough to
check `Info.plist` and the menus. On macOS 27 a local **release** build fails
on proc-macro dylibs (« mis-aligned LINKEDIT string pool », the same linker
issue as sqlx in the server): release bundles come from `widget.yml`.

## Finding the server (#9255)

Servers announce themselves over mDNS as `_tune-server._tcp` (see
`tune-core/src/discovery/mdns.rs`); `src-tauri/src/decouverte.rs` browses for
them (3 s) — the same service the iOS/tvOS apps use. At launch:

1. the remembered address answers → kept (a Tune Server on this Mac wins);
2. the server picked from the list last time shows up at another address
   (new DHCP lease) → followed, by its mDNS name (`server_cle`);
3. nothing was ever chosen and exactly ONE server answers on the LAN →
   connected to it, no question asked;
4. otherwise (none, several, or the chosen one is gone) → the list opens,
   with a field for typing an address by hand.

The list is also under *Tune Client › Choisir un serveur…* (menu bar) and in
the tray menu. The choice is kept in `<config dir>/tune-widget/config.json`
(`server_url`, `server_cle`). Default: `http://localhost:8888`.

`cargo test -- --ignored --nocapture le_reseau_reel` prints what the app
would find on the machine's real network; the regular tests use a fake
announcer on the loopback only.

## Permissions (#9255)

The app asks for **one** permission: *Local Network*
(`src-tauri/Info.plist`: `NSLocalNetworkUsageDescription` +
`NSBonjourServices` = `_tune-server._tcp`, merged into the bundle by Tauri).
Without those keys macOS 15+ cannot even grant it, and every connection to
another machine of the LAN fails — the same trap as Tune Server's #4949.

Up to rc12 it also triggered the *Accessibility* prompt (« control this
computer »): `tauri-plugin-global-shortcut` captured the keyboard's media keys
through a `CGEventTap`. Global shortcuts are gone **on macOS** (decision of
20 Nov 2026). They stay on **Windows only** (#8531), where they need no
permission: `tauri-plugin-global-shortcut` is a
`[target.'cfg(windows)'.dependencies]` entry and `raccourcis_globaux.rs` is
`#[cfg(windows)]` — `Ctrl+Shift+Space / → / ← / ↑ / ↓ / M` and the keyboard's
Play/Pause, Next, Previous media keys. A test keeps the plugin out of every
other section of `Cargo.toml` and out of the capabilities.

## Release (signed + notarized)

Recipe: [`.github/workflows/widget.yml`](../.github/workflows/widget.yml) —
macOS arm64 + Intel (signed with the Developer ID certificate, notarized and
stapled) and Windows x64 (NSIS installer, **not signed** until the
Authenticode certificate exists; the signing step is already in place and
stays inert while `WIN_SIGN_CERT_BASE64` is empty).

Three ways in:

- **With every server release (since rc10).** `release.yml` calls
  `widget.yml` (`workflow_call`, input `release_tag`). The files are named
  after the server tag — `tune-client-<tag>-macos-aarch64.dmg`,
  `tune-client-<tag>-macos-x86_64.dmg`,
  `tune-client-<tag>-windows-x86_64-setup.exe` (`tune-widget-…` until
  rc12) — and land in the same draft release, inside `SHA256SUMS`. The `tune-client-` prefix keeps them out of the
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

## Keyboard shortcuts and target zone (#9255, #8531)

On macOS, in-app shortcuts only — menu-bar key equivalents, active while the
app is frontmost, no permission needed (`src-tauri/src/raccourcis.rs`,
following Roon where it does not clash with macOS):

| Keys | Action |
|------|--------|
| Space | play / pause (handled by the page: web client `keyboard.ts`, bundled mini `app.js`) |
| ⌘J / ⌘K | previous / next track |
| ⌘↑ / ⌘↓ | volume + / − |
| ⌘[ / ⌘] | previous / next screen (`history.back/forward`) |
| ⌘F | search |
| ⌘⇧M | mini-player ↔ full window |
| ⌃⌘F | full screen |

The web client also has its own keys (Space, ←/→, ⇧←/⇧→, ↑/↓, N/P/M/S) and,
from rc13, back/forward arrows in its header (web#3275). When the page
handles a key itself, WebKit does not pass it on to the menu, so nothing runs
twice.

Playback actions drive the zone shown in the window: the page reports it by
navigating to `/__widget/zone/<id>` (intercepted, never loaded; the full
window is loaded with `?hote=widget` so it reports too). Until a zone is
reported, the target is the zone that is playing, else a paused one, else the
first (`zone_cible.rs`).

## macOS: Dock, menu bar icon, full screen (#9255)

A regular app: a Dock icon (clicking it brings the window back) and a menu
bar. The tray icon is a template image (`icons/tray-template.png`, black +
alpha) that macOS paints for light or dark mode; Windows keeps the colour
`tray-icon.png`. The full window declares `NSWindowCollectionBehavior
FullScreenPrimary`, so the green button gives the real macOS full screen; the
mini-player declares `FullScreenNone`.

## Server contract

`src/contrat.js` holds everything the interface reads from or sends to the
server, away from the DOM, and `tests/contrat.test.js` pins it
(`node --test tune-widget/tests/*.test.js`, also run by `widget-ci.yml`).
Rules worth knowing before touching it:

- `volume` is always a linear gain `0..1`, both ways. The server reads any
  number `> 1` as a percentage — sending the slider's `1` used to mean 100 %.
  A zone whose `GET /zones` entry says `volume_curve: "db60"` (software
  outputs) has a dB slider: same formulas as `tune_core::audio::volume_scale`
  (also ported to `src-tauri/src/volume.rs` for the menu volume shortcuts).
- `GET /search` returns library hits under `local` and each streaming
  service under `services.<name>` (items keyed by `source_id`, a string).
  The widget sends one request for the library (`sources=local`, shown as
  soon as it answers) and one per service (`sources=qobuz`, `sources=tidal`)
  in parallel, debounced and aborted when the query changes. Play bodies are
  the web client's: `{track_id}`, `{album_id}`, `{artist_id}` (up to 50 random
  tracks, #6467), `{source, source_id, title, …}` for a service track and
  `{source, streaming_album_id}` for a service album. Service artists are not
  listed: `POST /zones/{id}/play` has no field to play one. Thumbnails go
  through `/library/artwork/{hash}?size=80` or `/library/artwork/proxy?url=`.
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
