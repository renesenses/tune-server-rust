# Tune audio plugin SDK 0.1 — native ABI 1

Independent Cargo workspace. Plugins import SDK crates, never `tune-core` or `tune-server`. The four reference implementations are equalizer, crossfeed, converter and Dé-ploc. Tune's host adapters preserve existing HTTP screens, profiles, presets, audio producers and file codecs. The source-composed providers preserve upgrades; installing a signed native package overrides the corresponding provider at the next startup. This SDK does not replace Tune's WASM plugin system.

## Author workflow

```sh
cargo install --path sdk/cargo-tune-plugin --locked
cargo tune-plugin new equalizer --template equalizer --sdk-path /absolute/tune/sdk --output /new/plugin-directory
cargo tune-plugin check /new/plugin-directory
cargo tune-plugin test /new/plugin-directory
cargo tune-plugin dev /new/plugin-directory --input input.wav --output new-output.wav --settings settings.json
cargo tune-plugin pack /new/plugin-directory --target x86_64-unknown-linux-gnu --output equalizer.tuneplugin
minisign -S -s /operator/private-key -m equalizer.tuneplugin
```

Templates: `dsp` (minimal gain), `batch` (minimal PCM copy), `equalizer`, `crossfeed`, `converter`, `declick` (real implementation plus its conformance tests). Generated projects own their workspace and include native exports, schemas generated from Rust configuration/command/event/job types, a sandbox UI example, TypeScript declarations, translation resources, conformance fixtures, documentation and CI commands. `verify_schemas.py` rejects drift from compiled types. Schema documents describe structural JSON; factories and host services additionally enforce semantic ranges/permissions. Paths with spaces work. Existing projects/output captures/packages are never overwritten. For Tune's four slots use their canonical IDs. Other IDs are useful for development; the server rejects installation into an unrelated slot.

`dev` runs the **actual locally built native library** on integer WAV 16/24/32. Batch development only advertises equal-format WAV; set `format: "wav"` for converter or `output_format: "wav"` for Dé-ploc. This host does not certify production resampling, tags or artwork. Production uses the original Tune codecs and metadata adapter. The independent testkit's `pcm-test` is a PCM capture sink, not an encoder.

Build a separate artifact for each exact target. No Rust ABI crosses the library boundary. See [ABI ownership and lifetime](tune-plugin-abi/SAFETY.md). A signature is mandatory for installation; development does not create a production key or change server trust.

## Host services and contracts

Run `cargo doc --manifest-path sdk/Cargo.toml --workspace --no-deps --locked` for the compiled public API. Source manifest SDK 0.x requires the exact minor version; the enclosing native package independently declares ABI 1 and target. Required capabilities are negotiated before activation.

| Service | Contract |
|---|---|
| `DspFactory::assess` | Resolve PCM requirement/applicability before choosing passthrough. PURE, DSD/DoP and entitlement bypass are host policy. |
| `prepare` | Actual encoding/rate/channels, bounded max frames, validated finite settings; allocate off the device callback. |
| `Processor` | Complete interleaved frames; persistent filter/dither/ring history; process, update, inherit, reset, drain, latency and diagnostics. One instance per stream/format. |
| DSP formats | SDK represents S16, packed little endian S24, S32, F32, F64. Reference DSP supports the first four, refuses F64; crossfeed requires stereo. No implicit integer bypass conversion. |
| Observation | Host-owned source PCM/spectrum/VU, independent of premium installation. Authorized zones, bounded queue, generation/position filtering, drop counts, real frequency axes and FFT resolution. Unsupported post-DSP observation is refused. |
| `BatchHost` | Authorized source resolution, source format, readers/seek, discovered codecs, renderer/resampler, temporary writers, metadata, progress/cancellation, atomic publication and abort. Handles belong to one job. |
| Batch lifecycle | Preserve source and existing destination; cancel between blocks and before publication. External encoder calls may finish before cancellation is observed. Partial failures are explicit. Restarted jobs are reported interrupted and are not resumed. |
| `ui/client.mjs` | Feature-scoped configuration, EQ presets/AutoEq/bands, prepared coefficient response, batch jobs and spectrum. Host injects authenticated requests and event subscription. No server credentials enter plugin UI. |
| `ui/bridge.mjs` | Opaque sandbox iframe, dedicated MessagePort, allowlisted commands, source checks, 16 in-flight request cap, timeout and subscription cleanup. Context includes zone, theme and locale. Existing Tune screens continue to work. |

EQ settings are the existing `EqProfile` (all macro fields plus optional bands), crossfeed uses `enabled/amount/delay_ms`. Converter and Dé-ploc settings are their public `Options` types. The factory validates before constructing state. Diagnostics expose the actual prepared coefficients/headroom and scalar clipping counters; a preview curve is explicitly distinct from a live spectrum or measured hardware response.

Source-level `UiRequest` describes semantic version/session checks; the JS transport uses `{id, method, args}` on a private port established by protocol-1 handshake. It exposes no arbitrary route, SQL, filesystem, shell or HTML injection command. `client.d.ts` documents the JS surface. Browser routing/navigation remains owned by Tune; a plugin cannot choose a privileged parent route.

## Installation and lifecycle

Set `TUNE_AUDIO_PLUGINS_DIR`, or use `$TUNE_PLUGINS_DIR/audio` (default `plugins/audio`). Trust comes from `TUNE_AUDIO_PLUGIN_TRUST` (JSON array of minisign public-key base64 strings) or `TUNE_AUDIO_PLUGIN_PUBLIC_KEY`. There is no default key or unsigned installation switch.

Authenticated admin API:

- `GET /api/v1/audio-plugins/`: target, ABI, trust configuration, loaded providers and activation failures.
- `POST /api/v1/audio-plugins/{id}/install`: ZIP body, `X-Tune-Plugin-Signature` carrying the detached signature with newlines encoded as literal `\n`. Requires the plugin entitlement: equalizer is Free; crossfeed, converter and Dé-ploc require Premium.
- `POST /api/v1/audio-plugins/{id}/rollback`: verify the retained previous version, atomically switch next-startup activation.
- `POST /api/v1/audio-plugins/{id}/uninstall` with `{"remove_native":true}`: disable the feature, deactivate the native version, preserve profiles/presets and retained versions.
- `GET /api/v1/audio-plugins/{id}/assets/{name}`: only signed inventory-listed HTML/JS/CSS, with sandbox CSP. The host must use its authenticated asset delivery when mounting a UI; the existing Tune screens need no iframe.

Installation verifies signature, target, ABI, SDK/capabilities, portable paths, file inventory and SHA-256 before extraction or execution. Libraries are verified again at startup. A corrupt installed provider is unavailable and does not silently fall back. Live instances pin their library until destruction. Replacing files cannot unload code in use. Activation is at restart; existing playback/jobs finish. The next activation checks installed/enabled state and current licence, with no network/licence checks per sample.

The idempotent migration preserves `zone_*_eq_profile`, crossfeed settings and presets. The equalizer source-composed provider is enabled for Free and Premium accounts, preserving explicit user disable/uninstall choices. The `dsp_eq` capability remains advertised as enabled for backward compatibility; crossfeed uses its own `crossfeed` entitlement. Existing Premium accounts retain all source-composed providers. The migration policy for previously used crossfeed on Free accounts remains a product decision before extraction release; the draft currently preserves configuration but disables that provider for Free. A migration marker prevents resurrection after explicit uninstall. This transition intentionally keeps reference implementations in the server build: removal of that compatibility build is a separate release policy, not a promised binary secrecy boundary.

## One plugin inventory for builds and templates

[`plugins.json`](plugins.json) is the canonical inventory of in-tree and native plugins. CI, release (including macOS) and Docker (amd64/arm64) run `python scripts/plugin-catalog.py --check` before their builds. Their explicit Cargo commands are generated from this inventory, so independent historical CI guards can still inspect real package and feature lists.

To add an in-tree plugin, create its Cargo workspace member, optional server dependency and feature, then add one `in_tree` entry with that feature and its `fast_test` policy. Run `python scripts/plugin-catalog.py --write` and commit the generated workflow diff. Platform markers hold only non-plugin features and the `all`/`fast` policy. All shipped builds include every in-tree plugin; the fast tests include the declared subset. New native plugins need a SDK member with a manifest, `native`/`schemas` features, a cdylib, schema generator, config schema and conformance test, then one `native` entry. The CLI discovers concrete templates there; schema and external-scaffold verification iterate the same inventory. Generic `dsp`/`batch` templates remain available.

`--check` rejects missing inventory entries, stale expansions and unmanaged server build commands. `python -m unittest discover -s scripts/tests -p test_plugin_catalog.py` adds a fixture plugin through the real Cargo declarations and proves propagation to all workflows; removing a feature from any generated list is rejected. Native workspace tests and package checks still validate actual code: registration alone does not implement a plugin or authorize a new production slot.

## Qualification

```sh
cargo test --manifest-path sdk/Cargo.toml --workspace --locked
python sdk/scripts/verify_native.py
python sdk/scripts/verify_dsp_parity.py
python sdk/scripts/verify_scaffolding.py --binary /target/debug/cargo-tune-plugin
node --test sdk/ui/client.test.mjs sdk/ui/bridge.test.mjs
python sdk/scripts/verify_matrix.py
```

Run native verification before parity (it produces the four libraries). CI runs on Linux, macOS and Windows. Server tests additionally exercise actual FLAC/WAV, no-clobber publication, source preservation, handle isolation, migration combinations and spectrum delivery. The matrix separates code/contract coverage from runtime/hardware acceptance; neither a capture host nor cross compilation proves CoreAudio/WASAPI/network device behavior. SDK 0.1 remains experimental until that acceptance is recorded.
