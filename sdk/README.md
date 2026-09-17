# Tune plugin SDK — experimental 0.1

This workspace builds independently of `tune-core` and `tune-server`. It is the
first deliverable of #4363, not the stable SDK or an installable premium plugin.
The released server does not load these interfaces yet.

Implemented here:

- typed PCM blocks, source/output context, processing lifecycle, explicit bypass;
- requested versus effective configuration states;
- version and required/optional capability negotiation;
- spectrum provenance, true frequency resolution and stale-generation filtering;
- file-job service traits with scoped identifiers, streaming reads, seek,
  codec discovery, progress, cancellation and artifact publication;
- versioned UI message types (no UI runtime);
- an offline PCM capture host and an in-memory batch host;
- a CLI generating two external projects with executable conformance tests.

Not implemented: production host adapters, hot parameter transitions, the four
premium algorithms/plugins, legacy API adapters, license integration, native
ABI/loading, signing, installation, UI panels or codec/hardware acceptance.
Required fields are not a promise that the production host implements them.

## Build and generate a plugin

Rust 1.98 is the reference toolchain (matching the server build host). The
reference validation uses the project's Shrek environment. Follow the parent
`AGENTS.md` for `TUNE_TARGET_KEY`, capacity checks and remote target cleanup.

```sh
cargo test --manifest-path sdk/Cargo.toml --workspace --locked
cargo install --path sdk/cargo-tune-plugin --locked
cargo tune-plugin new my-gain --template dsp --sdk-path /path/to/tune/sdk --output /path/to/my-gain
cargo tune-plugin check /path/to/my-gain
cargo tune-plugin test /path/to/my-gain
```

The output directory must not exist. Existing files, empty directories and
symlinks are refused. A failed filesystem write can leave a partial *new*
directory; inspect it before removing it. The generator never overwrites a
project. Paths containing spaces are supported; no shell interpolation is used.

Use `--template batch` for a PCM copy tool. The batch example requests a codec
by name; tests use `pcm-test`, a capture sink rather than a real encoder. Both
projects declare their own `[workspace]` and use explicit SDK paths. No package
is published yet. Pin this repository's revision when sharing a scaffold.

There is deliberately no `pack`, `install` or `dsp-with-ui` command yet. The
manifest only accepts `distribution: "source"`. Native binaries need a separate
C-compatible ABI; these Rust traits must never cross a dynamic library boundary.

## Read the contracts

```sh
cargo doc --manifest-path sdk/Cargo.toml --workspace --no-deps --locked
```

The API reference is generated from the actual public types and their Rustdoc,
not a second manually transcribed signature list. Protocol 0.x requires an exact
minor match; additive compatibility is not presumed during experimentation.

| Module | Author's obligations |
|---|---|
| `audio` | Complete interleaved frames; actual format; one stateful processor per stream. No I/O, allocation, blocking or license check in `process`. Declare unsupported formats. Preparation happens off the audio thread. |
| `observation` | Declare measurement point and provenance. A source probe cannot claim post-DSP measurement. Frequencies come from the analyzer; resolution uses real signal frames, not padded FFT length. |
| `batch` | Use host-scoped handles; preserve sources; check cancellation between blocks; abort unpublished writers on failure. Codecs and output permissions come from the host. |
| `manifest` | Request only required capabilities plus explicitly optional ones. Negotiation fails before setup on any missing required capability. |
| `ui` | Typed messages only. Session matching is not authorization: the future host must validate the sender, zone access and capability grants. |

`AudioBlock` supports signed 16/24/32-bit PCM, f32 and f64 without conversion.
The generated gain supports f32 only and returns `UnsupportedFormat` otherwise.
Discrete channels are not implicitly L/R pairs. `PcmReader` exposes interleaved
right-justified integers at the reported bit depth; it reports **frames**, not
samples. Float input must be rejected by that reader interface.

Stateful DSP must survive arbitrary chunk boundaries. The host calls `reset`
on a discontinuity, not each block. Gapless continuity versus reset and hot
configuration transfer remain pending per-processor contracts; `AppliedLive`
must never be reported just because settings were saved.

Drain returns the count of valid output frames; zero capacity or infinite
draining cannot silently pass conformance. The capture host limits drain calls
and checks finite samples. It does not simulate a hardware deadline or guarantee
that native code cannot panic, abort or corrupt memory.

## Verify scaffolding, not just template syntax

```sh
cargo build --manifest-path sdk/Cargo.toml -p cargo-tune-plugin --locked
python3 sdk/scripts/verify_scaffolding.py --binary sdk/target/debug/cargo-tune-plugin
python3 sdk/scripts/verify_matrix.py
```

If `CARGO_TARGET_DIR` is configured, pass its actual binary path (append `.exe`
on Windows). The script generates both projects in a temporary directory outside
the workspace, checks dependencies, compiles and **runs** their tests, then
exercises overwrite/path/version refusals. The temporary projects are removed.

CI runs this workspace and the external projects on Linux, macOS and Windows in
`plugin-sdk.yml`. The server's `cargo test` alone does not execute this separate
workspace. None of these jobs is a hardware/audio-device acceptance test.

The [migration plan](../docs/plugins/premium-sdk.md) and
[parity matrix](../docs/plugins/premium-sdk-matrix.json) track the remaining
production work. A passing SDK contract is not a passing row for Tune runtime.
