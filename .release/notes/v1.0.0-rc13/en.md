Tune 1.0.0-rc13, the thirteenth release candidate of Tune 1.0. Thanks to Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor and Régis for their reports and tests, to alec_eiffel for the Smart Upsampler, to cometdom for Qobuz and to Levente for the design.

## New features

- **Upsampler**: the Smart Upsampler by Rémi (alec_eiffel) plays at the same level as without it, and a setting change is heard right away.
- **Playback and outputs**: Sendspin and Devialet AIR receive the processed sound (equalizer, plugins, upsampler). The sleep timer can shut the computer down; desktop media controls on Linux.
- **Crossfeed**: free, with its settings and presets.
- **Convolution**: one impulse response file per sample rate.
- **DSD**: DSD from a UPnP server plays natively on demanding network players, and two DSD tracks follow each other without a gap.
- **Library**: your personal tags become Tune tags; select several albums for a grouped action; filter by instrument.
- **Altered source** replaces "Doubtful Hi-Res", for every format, with the measured cutoff.
- **Waveform** is finer, also for CUE, Qobuz and TIDAL tracks (as an option).
- **Queue**: albums can be collapsed and moved in one gesture.
- **Home**: 5 "Because you like…" playlists and the Top artists, Frequently played, Listen later and Resume listening widgets.
- **Services**: TIDAL's "For you" page; 200 Qobuz new releases per genre.
- **Tune Circle**: members of a circle can see each other if they agree to it, and the World view shows every circle.
- **Web interface**: Turkish, authentication can be turned on from the interface, and history per track or per play.
- **Tune Client (desktop app)**: finds network servers by itself and drives the local server.
- **Windows**: start Tune with Windows, offered by the installer.
- **iOS app**: fuller CarPlay on 5G, the sidebar is back on iPad.

## Improvements

- **Transitions**: ReplayGain "Auto" keeps album gain between two tracks of the same album; AAC, Opus and CUE tracks play gapless.
- **Playback and outputs**: same volume curve for AirPlay 1 and 2; a radio starts faster on a network player.
- **Windows**: in WASAPI exclusive mode, Tune falls back to the other rate family instead of stopping the track.
- **Devialet AIR**: volume in 1 dB steps, up to 192 kHz.
- **DSD**: native DSD detection is more cautious and is re-evaluated.
- **Large libraries**: Folders opens without waiting, and background tasks no longer block everything else.
- **Services**: Tune's own Google sign-in client for YouTube; Spotify and TIDAL are sturdier during playback.
- **Web interface**: more precise parametric equalizer and a confirmation before deleting.
- **iOS app**: the lock screen drives the zone even when stopped; without a DAC, the slider sets the iOS volume.
- **Updates and diagnostics**: the update also replaces the programs shipped with Tune; bug reports hide names.

## Fixes

- **Playback and outputs**: resume works after a pause that had released the DAC; "Repeat all" no longer mistakes a brief stop for the end of a track; on Windows, a DAC switched on after Tune is found.
- **Zones**: an already linked device names its zone; with OAAT, resuming reopens the session instead of staying silent.
- **DLNA, UPnP and OpenHome**: no more stray Stop on Musicbook Source, mute stays put, Linn Klimax DS/2 recognized.
- **AirPlay**: the AirPlay 1 output no longer starts at maximum after a restart.
- **Signal path**: it no longer reports processing that does not happen.
- **Library**: the DSD and CD editions of the same album are no longer merged; burning the DR no longer freezes the server.
- **Services**: Qobuz and TIDAL favorites show their duration; Spotify sections are no longer duplicated.
- **Radios**: FIP stations get their logo back.
- **Plugins and crossfeed**: an installed plugin works without a restart; crossfeed is no longer locked on OAAT or in the browser.
- **Web interface**: the grid scrolls to the end; the "server is not responding" banner goes away as soon as it responds.

## Security

- Several security hardenings.
- Logs and bug reports now hide the credentials carried by some stream addresses.
- Replace path (Settings › music folders) now checks the folders and can no longer be reached from another device without authentication.

## Behavior changes

- **Tune Circle**: with authentication, restricted to administrators; read-only from remote listening.
- **YouTube**: accounts connected before this version must be reconnected once.
- **End-of-queue radio**: off by default again; tick it again in the zone if you had chosen it.
- **Equalizer**: turning the equalizer off keeps its preamp until "Restore level".
- **Multiroom**: a group is free up to 3 zones; beyond that, Premium. Stereo pairs stay free.
- **CUE with INDEX 02**: track starts are corrected at the next scan, history is kept.
- **Tune OS**: the temporary password is shown on the machine's screen or on the boot card, no longer on the sign-in page; remote access stays closed until it is changed.
- **Profiles and history**: with authentication, only the administrator manages other profiles.
- **Replace path**: without authentication, from the machine Tune runs on.

## Please check at home

These changes passed the automated tests without a try on real hardware. Tell us what you see:

- **Sendspin and Devialet AIR** with processing active.
- **Windows**: mini player, WASAPI exclusive mode, starting with Windows.
- **AirPlay 1**: level at start and between tracks.
- **DSD to a network player** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** as OpenHome, **Tune Client on macOS** and **CarPlay on 5G**.

## Known issues

- **DSD over DoP on the local output**: after attenuated PCM, it may play at full scale. Keep your amplifier volume low.
- **UPnP after an address change**: a track may not play; re-index that server.
- **Integrity audit**: a few files may be wrongly reported; do not delete anything on this verdict alone.
- **Windows ASIO driven by another server**: no sound after Pause then Play; skip to the next track.
- **After the update**: if the interface does not change, force a reload (Cmd+Shift+R or Ctrl+Shift+R).
