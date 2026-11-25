Tune 1.0.0-rc13.90: **pre-release of rc14**, for testers only. This is not a published version: automatic update does not offer it. Thank you to everyone testing it.

## New features

- **TESS, the 64-bit processing chain**: all processing runs in 64-bit floating point end to end, with a single quantization at the very end.
- **The same path for every output**: local output, UPnP/DLNA, AirPlay, Devialet AIR and OAAT all go through TESS; an output that cannot receive a processing stage says so clearly.
- **Parametric equalizer** with free bands (frequency, gain, Q), next to the graphic equalizer and the headphone equalizer (AutoEq profiles).
- **Convolution**: filters as .wav, .zip archives or Convolver configurations (.cfg), one filter per sample rate, gapless switching and announced latency.
- **Measured room correction**: a mixed-phase FIR filter computed from several listening positions.
- **Upsampling to DSD** (DSD64, DSD128, DSD256) with a sigma-delta modulator.
- **Chain presets**: save a complete chain and recall it, immediately or at the next playback.
- **Signal path**: from the playback bar, stage by stage, what is really applied, and why a stage is not.
- **Native plugins** in the chain: hot activation, 64-bit format, channel order and count respected.
- **Interface in 20 languages**: Dutch, Traditional Chinese, Russian and Ukrainian arrive.
- **Authentication**: a "Disable authentication" button in Settings › System › Authentication.

## Improvements

- **SACD ISO**: native DSD is served to network players as virtual DSF; DSD carried over DoP is announced as DSD.
- **Remote listening**: on 5G, the first track is sent as FLAC.
- **Absent devices**: forgotten automatically after 30 days (adjustable), never deleting a zone.
- **Cover+**: better artwork, including for web radios (ICY metadata).
- **Group playback**: per-zone offset; "Previous" on the leading zone also restarts the following zones.
- **Library and streaming**: background passes no longer block the interface, alphabetical album sorting is kept, similar artists from a service's artist page.
- **Refusal messages** are shorter and point to the right setting.

## Fixes

- **The rc13.1 and rc13.2 fixes** are included: administrator login, linking the mozaiklabs.fr account from the local network, artwork with authentication enabled, updating on Windows.
- **UPnP**: Tune no longer indexes itself; an indexed track plays from its server's current address.
- **Playback**: 30 s preloading without discarded buffers; a seek refused by the source no longer shifts the stream.
- **Chromecast** reconnects; AirPlay republishing no longer interrupts playback.
- **Podcasts**: unavailable episodes are reported cleanly.
- **Forum reports**: history, Qobuz, Spotify, "Back" button, search and iPad.

## Behavior changes

- **Parametric equalizer**: free up to rc13, it is now part of Tune Premium, like convolution, room correction, real-time declick and third-party plugins. The graphic equalizer, the headphone equalizer and crossfeed stay free.

## Please check at home

- **A complete TESS chain** (equalizer, crossfeed, convolution or room correction): does the signal path match what you hear?
- **Importing a convolution set** and switching filters during playback.
- **Upsampling to DSD** on a compatible DAC: stability, level, no clicks.
- **Chain presets**, in both application modes.
- **A network output** (UPnP, AirPlay or OAAT) with active processing.
- **Updating from rc13.x** and the "Disable authentication" button.

## Known issues

- **DST-compressed SACDs**: these ISO images are not yet played natively.
- **VU meters** on a relayed MP3, AAC or Ogg track: after a seek they switch off until the end of the track. The sound is not affected.
- **Remote listening on CarPlay**: the relay fix is postponed to rc15.
- **Headless server with authentication off**: to change the linked mozaiklabs.fr account, enable authentication or use an SSH tunnel to localhost:8888.
