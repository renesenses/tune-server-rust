# Tune: CPAL 0.17.3, ALSA output poll recovery (#4295)

Source: crates.io CPAL 0.17.3, upstream Git commit
fd3b945bffcaa493fa7cb5ceddf9db1f9330fd30.
Crate archive SHA-256:
d8942da362c0f0d895d7cac616263f2f9424edc5687364dfd1d25ef7eba506d7.
The original Apache-2.0 licence is retained. No upstream version bump.

For #4295, the only production source changed is src/host/alsa/mod.rs.
(#5643, below, changes the ASIO host and the manifest.)
This is an output-only adaptation of the recovery logic visible at CPAL 0.18.2,
commit e1612d5d98152f8dc2a62e1b51ef7cbf4f7f26b7, not an import of the 0.18 API:

- Inspect PCM state on output POLLERR/timeout. An XRUN reaches the existing
  prepare path, a suspended PCM tries resume, ENOSYS falls back to prepare,
  EAGAIN leaves the worker cancellable through the existing drop mechanism.
- POLLHUP/POLLNVAL or Disconnected produce DeviceNotAvailable and leave the
  worker. They never trigger prepare/reopen.
- When state is not an error, POLLERR still reaches status/avail; EPIPE there
  also reaches prepare. A transient flag must not suppress valid callbacks.
- The stream retains the read end of its drop pipe (Arc shared with the worker),
  as in CPAL 0.18.2. An early disconnection exit must not make Drop write to a
  closed pipe. Wakeup assertions are retained.
- The input worker selects its existing behavior explicitly. No change to
  capture recovery, other backends, the Tune local engine or DSP.

Tests are registered in tune-core/tests/alsa_poll_recovery_4295.rs.
They build the real CPAL output stream against a private ALSA null PCM. The
test child alone receives LD_PRELOAD for a small C shim that controls ALSA
boundary results. The worker, poll syscall, callbacks, drop pipe and join are
the production implementation. The parent bounds each child to eight seconds.

This proves recovery from controlled software conditions. It does not identify
the original USB/driver fault or validate a physical DAC, Windows or macOS.

# Tune: native DSD output through ASIO (#5643, lots A and B)

Windows only, `asio` feature only. The CPAL ASIO host had no DSD: its sample
type table stopped at PCM, and asio-sys had no `ASIOFuture`. The asio-sys
side is vendor/asio-sys (see its TUNE-PATCH.md).

Changes:

- Cargo.toml: `asio-sys` comes from `path = "../asio-sys"` (version still
  0.2.6). Cargo.toml.orig is the upstream file, left as is.
- src/host/asio/dsd.rs (new, self-contained, tested on Linux by
  tune-core/tests/asio_dsd_5643.rs through `#[path]`): `DsdLayout` from the
  ASIO sample type (`ASIOSTDSDInt8MSB1` 33, `ASIOSTDSDInt8LSB1` 32,
  `ASIOSTDSDInt8NER8` 40), byte encoding (LSB1 reverses the bits of every
  byte), the 0x69 idle pattern per layout, accepted rates (2 822 400,
  5 644 800, 11 289 600), bytes per channel <-> ASIO buffer size, and the
  copy of one channel out of the interleaved `DsdU8` buffer.
- src/host/asio/device.rs: `convert_data_type` maps MSB1 and LSB1 to
  `SampleFormat::DsdU8`. NER8 is 8-bit DSD words, not a packed 1-bit stream:
  it stays unmapped and no stream is opened on it. New
  `Device::supports_dsd_output()` (`kAsioCanDoIoFormat`).
- src/host/asio/stream.rs: `build_output_stream_raw` with a DSD
  `SampleFormat` goes to a new `build_output_stream_dsd` before any PCM code
  runs (`DsdU16`/`DsdU32` are refused). That path:
  1. requires a DSD rate, no other registered callback on the driver, and
     `kAsioCanDoIoFormat` for DSD;
  2. forgets and disposes of any buffers left by an earlier stream, then
     `kAsioSetIoFormat` DSD;
  3. sets the rate, re-reads the output sample type (must be MSB1 or LSB1)
     and the channel count;
  4. takes `BufferSize::Fixed(n)` as n BYTES per channel (8n ASIO samples,
     checked against the driver range) or the driver's preferred size, which
     must be a whole number of bytes;
  5. registers a callback that hands the user `DsdU8` data (interleaved, one
     byte per channel, MSB first) and overwrites each channel buffer, bits
     reversed for LSB1. DSD is not mixed. While paused it writes the idle
     pattern instead of leaving the previous buffer to loop.
  Any failure after the switch, and dropping the stream, disposes of the
  buffers and switches the driver back to PCM. `Stream` gains a `dsd` flag,
  `false` on the two PCM constructors.

The PCM path is unchanged: same type table entries, same checks, same
callbacks. A PCM request on a driver left in DSD mode is refused as before
(before: no mapping; now: `DsdU8` differs from the requested format).

Not proven, needs a Windows host with a DSD-capable ASIO driver:

- the ASIO buffer size unit in DSD mode (taken here as DSD samples, i.e.
  bits, per channel, as asio.h describes it), hence the bytes conversion;
- which drivers report MSB1 or LSB1, and the audible result of the LSB1 bit
  reversal;
- the order switch format -> set rate -> re-read type, and whether a driver
  sends `kAsioResetRequest` on the switch (no message callback is registered
  at that moment);
- the return to PCM on drop, then a PCM stream on the same device;
- timestamps (`frames_to_duration` with the DSD rate and the sample count).


# Tune: native DSD capability probe (#5643, lots C to E)

- src/host/asio/device.rs: new `Device::dsd_output_rates()`. With no
  stream alive on the driver (`callback_count() == 0`) and
  `kAsioCanDoIoFormat` DSD, it releases leftover buffers, switches to DSD,
  asks `ASIOCanSampleRate` for each rate of `dsd::DSD_RATES`, then switches
  back to PCM. Empty on any refusal or error. Tune calls it once per device,
  under its process-wide ASIO device lock, and caches the answer
  (`tune-core/src/outputs/capacite_dsd_natif.rs`).
- src/host/asio/stream.rs: `forget_asio_buffers` becomes `pub(super)` so the
  probe can use it. No behaviour change.

Not proven: what real drivers answer to `ASIOCanSampleRate` in DSD mode, and
whether the switch there and back sends `kAsioResetRequest`.
