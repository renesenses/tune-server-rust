# PCM audio tap — contract

A minimal, generic core primitive that exposes the **decoded PCM signal** to more
than one consumer, so signal analysis (spectrum, loudness, waveform, milkdrop,
OAAT…) can live in **plugins** instead of being hard-coded in the core.

## Why it must be in core

A plugin (WASM sandbox) can receive bus events, query the DB, expose routes and
register outputs — but it **cannot access decoded PCM**. Spectrum/FFT, loudness
and waveform all compute on the raw signal. So the core exposes a per-zone
broadcast of PCM windows; the analysis itself is a plugin consuming it.

This reconciles "spectrum in core" (JP) and "make it a plugin" (Bertrand): what
goes in core is the **PCM pipe**, not the spectrum. The spectrum is a plugin on
top.

## Contract

- **Transport:** a dedicated per-zone `tokio::sync::broadcast::Sender<PcmTapFrame>`,
  **separate from the JSON event bus** (PCM never transits as JSON/base64).
  `broadcast` → N fan-out consumers; the bounded ring makes a slow consumer
  *lag/drop* rather than back-pressure the audio path.
- **Payload (`PcmTapFrame`, see `tune-core/src/audio/tap.rs`):**
  - `zone_id: i64`
  - `pcm: Arc<[u8]>` — raw interleaved window (Arc → O(1) per-subscriber clone)
  - `format: PcmFormat { sample_rate, channels, bit_depth, sample_format }` —
    `sample_format` is explicit (`SignedInt`/`Float`); do not infer from
    `bit_depth` alone
  - `track_position: Duration` — start-of-window offset in the track (align to the
    renderer position; buffer-depth correction). **In the contract from v1** to
    avoid a later breaking change.
  - `window: Duration` — window length (≈40 ms, matches #1105)
  - `play_seq: u64` — supersession counter; consumers drop stale frames
- **Granularity:** ~40 ms windows (25–32 Hz), reusing #1105's `send_windowed_levels`
  sizing. A consumer needing a fixed FFT size rebuffers on its side.
- **Opt-in / zero-cost when idle:** with no subscriber, publishing is skipped
  (`receiver_count() == 0`) → behaviour identical to today.

## Wiring (on top of #1105)

1. **Producer** — the decoder publishes at the **four** sites in
   `decode_to_pcm_streaming_inner` where #1105 calls
   `levels::send_windowed_levels(ltx, pcm, bd, ch, sr)`. Same `pcm: &[u8]` +
   format already in hand; swap the call for `publisher.send_windowed(pcm)`.
2. **State** — `Playback` holds one `ZoneTap` per active zone (next to
   `current_play_seq(zone_id)`) and hands a `PcmPublisher` to the decoder for each
   track playthrough.
3. **Core consumer** — the `playback.audio_levels` forwarder (#1105) becomes a
   **subscriber**: it `compute_levels`/`compute_spectrum` off the decode hot path,
   keeping its pacing + `play_seq` supersession + tests unchanged. (Analysis moves
   one hop downstream; #1105's work stays valid.)
4. **Plugin SDK** — opted-in plugins (`wants_pcm: true`) receive frames through a
   host callback: `on_pcm(zone_id, ptr, len, sample_rate, channels, bit_depth,
   fmt, position_ms, play_seq)`. The host subscribes on their behalf and copies
   each frame once into WASM memory. (Second half; not part of the core tap PR.)

## Scaffold

`tune-core/src/audio/tap.rs` provides the compiling primitives (`PcmTapFrame`,
`PcmFormat`, `SampleFormat`, `ZoneTap`, `PcmPublisher`) + unit tests for the
windowing. The producer sites, the `Playback` per-zone registry, and the
levels-forwarder-as-consumer refactor are the follow-up integration (marked
`TODO` / `#![allow(dead_code)]` for now).
