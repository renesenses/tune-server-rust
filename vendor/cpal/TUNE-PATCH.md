# Tune: CPAL 0.17.3, ALSA output poll recovery (#4295)

Source: crates.io CPAL 0.17.3, upstream Git commit
fd3b945bffcaa493fa7cb5ceddf9db1f9330fd30.
Crate archive SHA-256:
d8942da362c0f0d895d7cac616263f2f9424edc5687364dfd1d25ef7eba506d7.
The original Apache-2.0 licence is retained. No upstream version bump.

The only production source changed is src/host/alsa/mod.rs.
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
