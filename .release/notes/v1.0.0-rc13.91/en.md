Tune 1.0.0-rc13.91: **second pre-release of rc14**, for testers only. This is not a published version: automatic update does not offer it. It includes everything in rc13.90. Thank you to everyone testing it.

## New features

- **Equalizers that combine in series**: graphic, parametric and headphone equalizers and room correction follow one another in the same TESS chain; applying a room correction no longer overwrites the other equalizers. The parametric equalizer's gain range is wider.
- **Complete Bypass all**: a single action takes the whole TESS chain out of circuit, and restores it exactly as it was.
- **Headphones plugin** drawn from the OPRA database: your headphones' profile is recognized, with preamp and level.
- **Speaker Setup plugin** (speaker adjustment): distance, gain and phase, channel by channel.
- **Upsampling to DSD on the local output**: a dedicated sigma-delta modulator, and the output's DSD capabilities are published.
- **Choice of upsampling target**: in TESS, a screen lets you choose what to upsample to, PCM or DSD.
- **Microphone level test** before measuring the room.
- **Measurement at the microphone's native sample rate**, from 44.1 to 192 kHz.
- **On-the-fly TESS settings**: disabling a stage takes effect immediately, and the preamp stays bounded.

## Improvements

- **Room correction**: the verification measurement hears the zone's correction; uncorrected deviations are explained, and the upper limit can be set up to 1 kHz.
- **Room correction**: your input, the last measurement and the applied correction are restored when you return to the screen; microphone placement instructions and a "Before measuring" box guide the measurement.
- **Measurement in PURE mode**: volume is forced to 100% for the duration of the sweep, then restored to what it was.
- **Signal path**: a stage that is off (upsampling, convolution) says why, in TESS and on the plugin's page; on Windows in shared mode, upsampling targets the mixer's sample rate.
- **Credits**: the instrument rather than "Musician", the author distinct from the composer, one name per role.
- **Scan**: the barcode present in the tags completes the album if it has none.
- **Server status**: a warning is shown if the artwork cache cannot be created.

## Fixes

- **Calibration**: the sweep no longer stops before the announced length and no longer gets stuck in the queue; a zone without an output is clearly refused.
- **Calibration**: a peak at the ceiling no longer blocks the following bands.
- **RT60**: "not measurable (noise)" rather than a wrong value; error messages stay readable for at least 10 s.
- **Sample rate warning**: it now only appears outside the accepted sample rates.
- **Radio**: changing station is no longer overridden by the previous station.
- **Artwork**: on a fresh installation, the cache is created under the data folder.
- **TESS**: a single "next track" notice, and the help for the "Apply" button is corrected.

## Behavior changes

- **Changing the TESS chain during playback**: a disabled stage is cut immediately; an added or re-enabled stage is heard from the next playback.

## Please check at home

These points have not yet been tested on real hardware:

- **Upsampling to DSD on a real DSD DAC**: stability, level, no clicks.
- **Room measurement at 44.1 kHz and at 96 kHz** on a real audio interface, with the microphone level test.
- **On Windows, in shared WASAPI**: playback and upsampling.
- **Speaker Setup on a network output** (UPnP, AirPlay or OAAT): distance, gain and phase.

And also:

- **Several equalizers in series** with Bypass all: does the signal path match what you hear?
- **The Headphones plugin** with your own headphones.
- **Updating from rc13.90.**

## Known issues

- **DST-compressed SACDs**: these ISO images are not yet played natively.
- **VU meters** on a relayed MP3, AAC or Ogg track: after a seek they switch off until the end of the track. The sound is not affected.
- **Remote listening on CarPlay**: the relay fix is postponed to rc15.
- **Headless server with authentication off**: to change the linked mozaiklabs.fr account, enable authentication or use an SSH tunnel to localhost:8888.
