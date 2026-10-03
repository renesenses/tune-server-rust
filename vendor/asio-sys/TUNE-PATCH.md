# Tune: asio-sys 0.2.6, DSD I/O format switching (#5643)

Source: crates.io asio-sys 0.2.6, upstream Git commit
fd3b945bffcaa493fa7cb5ceddf9db1f9330fd30 (the same commit as vendor/cpal,
CPAL 0.17.3), path `asio-sys` in the CPAL repository.
Crate archive SHA-256:
826194e1612938c9be09b78b58323fbb2e326de3d491b4230186cf6e832d8ded.
Licence Apache-2.0 (`license` field of the manifest). The archive ships no
licence file: LICENSE is the repository's own, copied from vendor/cpal (same
upstream commit). No upstream version bump. The archive's Cargo.lock is
ignored by the upstream .gitignore and is not committed; the workspace lock
owns dependency resolution.

The first commit of #5643 is the unmodified archive. Every later difference is
listed here.

How it is wired: vendor/cpal/Cargo.toml reaches it by `path = "../asio-sys"`,
not by `[patch.crates-io]`. A `[patch]` is only honoured by the root manifest
of the workspace being built; an external consumer of tune-core (see the
rust_cast note in the root Cargo.toml) would silently get crates.io's
asio-sys, without these calls, and fail to build with `asio`. The directory is
in the workspace `exclude` list, like the other vendored crates.

## Differences

- `src/bindings/io_format.rs` (new): the ASIO SDK DSD extension, from
  `asio.h`. `ASIOFuture` selectors `kAsioSetIoFormat` (0x23111961),
  `kAsioGetIoFormat` (0x23111983), `kAsioCanDoIoFormat` (0x23112004);
  `AsioIoFormatType` (PCM / DSD); `AsioIoFormat`, a `repr(C)` mirror of the
  512-byte `ASIOIoFormat`; `can_do_from_code`, which reads the answer to
  `kAsioCanDoIoFormat` (`ASE_SUCCESS`/`ASE_OK` yes, `ASE_NotPresent`/
  `ASE_InvalidParameter` no, anything else an error). The file is
  self-contained so that Tune tests it on Linux.
- `src/bindings/mod.rs`: declares and re-exports the module, and adds four
  `Driver` methods:
  - `can_io_format(format)` (`kAsioCanDoIoFormat`);
  - `set_io_format(format)` (`kAsioSetIoFormat`), refused with `BadMode`
    unless the driver is `Initialized`: buffers belong to one format, and the
    caller may still hold their pointers, so the caller disposes of them first;
  - `io_format()` (`kAsioGetIoFormat`), preset to `kASIOFormatInvalid` so a
    driver that writes nothing is not read as PCM;
  - `callback_count()`, the number of buffer callbacks registered, so CPAL
    can refuse native DSD while another stream of the driver is alive.
  `check_type_sizes` also compares `AsioIoFormat` with the generated
  `ASIOIoFormat` (512 bytes). It runs only where the real bindings exist.
- `build.rs`: bindgen allowlists `ASIOFuture` and `ASIOIoFormat`.
- `asio_stub_bindings.rs` (DOCS_RS stubs): `ASIOIoFormatType`, `ASIOIoFormat`
  and an `ASIOFuture` stub that answers `ASE_InvalidParameter` (a stub driver
  is PCM-only).

No existing function, type or behaviour is changed. `AsioSampleType` already
listed `ASIOSTDSDInt8LSB1` (32), `ASIOSTDSDInt8MSB1` (33) and
`ASIOSTDSDInt8NER8` (40) upstream.

## Proof and limits

- Linux: `tune-core/tests/asio_dsd_5643.rs` compiles `io_format.rs` itself
  (`#[path]`) and checks selectors, structure size, format values and the
  reading of `kAsioCanDoIoFormat` codes.
- Windows target: `DOCS_RS=1 cargo check -p tune-core --target
  x86_64-pc-windows-msvc --features local-audio,asio` compiles the crate
  against the stubs above, not against the SDK.

Not proven (no Windows host, no ASIO SDK, no DSD driver on Shrek): that bindgen
emits `ASIOFuture` and `ASIOIoFormat` under these names from the current SDK
(DSD support appeared in ASIO SDK 2.2); that the link resolves `ASIOFuture`
from the SDK's `common/asio.cpp`; the answers of real drivers to the three
selectors; the `check_type_sizes` test on Windows.
