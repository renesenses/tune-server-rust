Tune 1.0.0-rc14 includes everything in the rc13.90 and rc13.91 pre-releases; these notes describe what has changed since rc13.91. Thank you to the testers: many of these points come from your feedback.

## New features

- **TESS, Save and Reload presets**: as soon as a setting differs from the chosen preset, TESS shows "modified". **Save** overwrites the original preset, **Reload** goes back to it. A preset shipped with Tune, such as "Pure", cannot be overwritten: "Save as…" makes a copy.
- **TESS, DSD preset applied right away**: a preset that changes the DSD target (for example DSD128 → DSD256), loaded in "apply immediately" mode, restarts the track at its position instead of waiting for the next track. Upsampling is a Premium feature.
- **Headroom**: Automatic, or by hand from −12 to 0 dB in 0.5 dB steps. The panel shows the value actually applied, or says why it is not.
- **Headroom when upsampling to DSD**: the manual headroom is no longer applied twice (−12 dB used to give −24 dB; it is now −15 dB, including −3 dB for the DSD modulator, shown on its own line).
- **Room measurement guided to 75 dB(C)**: with a calibrated microphone, the screen shows the measured level, its difference from 75 dB(C) and a volume instruction. The rule is cautious, as the conversion has not yet been checked against a calibrator: Tune never advises going above an estimated 80 dB(C) and cuts the signal at 90 dB SPL. Correction measured with a microphone is Premium; correction calculated without one remains free.
- **Hearing safety**: unchecking an equalizer, switching to PURE or Bypass all, or removing a convolution, a crossfeed or a mono fold-down no longer makes the level jump up: it returns to the previous level with a fade of about half a second. If you raised the volume in the meantime, the "level reduced" indicator lights up and "Restore level" releases the extra. This also applies to network outputs (UPnP, OAAT).
- **A faithful signal path**:
  - on a network output, it follows the real order of the stream, up to the player's volume;
  - the stage of an impulse response filter is called "Convolution";
  - the filter keeps the name of the imported .wav or .zip file;
  - after a DSD target change, it shows the DSD rate actually playing, and announces the next one;
  - the window turns green: orange is kept for real alerts.
- **Convolution**: the panel shows the curve of the loaded filter, channel by channel, and the phase on request.
- **Bypass all**: the TESS icon in the playback bar is dimmed when TESS is bypassed, and the TESS page shows "TESS bypassed".
- **Harmonized TESS page**: TESS green accent, current zone shown next to the badge, Headroom panel greyed out on a network output, and "Without microphone (estimate)" or "With microphone (recommended)" measurement.
- **Network output**: the Upsampling card says that upsampling does not apply there.
- **Harmonized interface**: embedded fonts, also offline; Library, Live Radio, Media servers and Concerts follow the new design.

## Fixes

- **Radio to a Sonos** (and any network player with a rate ceiling): a 192 kHz radio station is served at the rate the player accepts. Before, the player stopped straight away, over and over.
- **768 kHz**: a zone's "Max frequency" list offers 768 kHz and 1,536 kHz.
- **Library**: the album grid no longer freezes while scrolling through a large collection.
- **Playback in the browser tab**: seeking really moves playback, even from another device.
- **Help**: the priority support banner no longer appears for a Premium account.
- **License**: the expiration date is read correctly, and Settings › License says why the tier is free.
- **Convolution**: a filter that boosts some frequencies no longer clips.
- **Long podcasts**: an episode whose host regularly drops the connection is no longer interrupted.
- **Debian package in an LXC container** (Proxmox): the service starts even without private mount namespaces.
- **First launch**: the wizard no longer calls services with no account "enabled", and it saves the chosen music folder. An empty "My radios" opens the directory.

## Security and accounts

- **Renewed session**: with "Stay signed in" (unchecked by default), the browser session renews itself as long as it is used, up to 90 days. Otherwise, 24 h as before. The avatar menu offers "Sign out of this server".
- **Recovering administrator access**: run `tune-server --reset-admin` (or `--reinitialiser-admin`) **on the server machine**. The one-time code it gives, valid for 15 minutes, is entered in "Forgot password?" from that machine; without a browser, add `--password-stdin`. Open sessions are closed. The installation guide (section 9) gives the command for each system.
- **`admin` profile**: the administrator profile created by default is now called `admin`; a name you chose is kept, and `default` is still accepted.
- **Administrator password from the first setup**: on a fresh installation, the wizard has you create it.
- **Server without authentication**: administration is accepted from the local network, no longer only from the server machine. **Warning**: without authentication, devices on the local network can administer the server; turn it on if your network is not trusted. The "Administration from this machine only" setting restores the previous rule.

## Remotes

- On a protected server, apps that are not yet up to date ask for the password again every 24 h, as before.

## Please check at home

- **"Bypass all" in a preset**: a preset that changes both Bypass all and the DSD target, in "apply immediately" mode, restarts the track at its position. Only checked on the test bench: tell us whether the restart is clean.
