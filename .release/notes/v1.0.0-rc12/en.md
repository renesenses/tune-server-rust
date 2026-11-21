Tune 1.0.0-rc12, the twelfth release candidate of Tune 1.0. Several screens arrive in a new design by Levente, a designer from the Tune community.

## New features

- **New interface**: Tune Circle redesigned (Discover, Messages, Shared playlists, My circle); album page with description, credits, details and a Versions tab showing the catalog number; artist page with a large portrait, figures and tabs; Settings in a more readable column; Server status and the listening dashboard, with a period selector.
- **Home**: a new "Listening overview (new layout)" widget, to add from Home › Edit, with a Zones panel; larger tiles with year and genre.
- **Library**: pin the filters you want with the Filters button; classical music with Conductor, Ensemble / orchestra and Work; channel count and a Stereo filter; a map and a filter by artist country; "Replace path" for a music folder; fake Hi-Res detection, off by default.
- **Track technical sheet**: spectrogram and per-channel waveform, also for the Qobuz and TIDAL tracks in your library.
- **End-of-queue radio**: when the queue ends, Tune carries on with similar tracks, marked "Radio"; adjustable zone by zone.
- **Sleep timer**: the music stops at the chosen time; on Tune OS, the machine can also shut down.
- **Playlists**: play count and last played on every row; a Remove cross and "Remove from this playlist"; remove a track from your own Qobuz playlist.
- **Radio**: Lossless, High quality and Hi-Res filters on one line; a directory station can be edited just for you; a radio can be added to a playlist.
- **Playback**: native DSD on the Linux local output (ALSA); fixed PCM rate per zone; exclusive mode set zone by zone; deleted zones can be restored; real AccurateRip verification of CDs.
- **DLNA**: continuous stream between tracks for renderers that join two tracks badly, off by default; a group's master volume can be set from any member.
- **Streaming**: choose the TIDAL catalog country; Qobuz new releases match the Qobuz player; Spotify gets a connection assistant, and Spotify Connect plays in the chosen zone.
- **Signal path**: clicking a processing step opens its settings; grouped playback shows every output.
- **Settings and interface**: search by setting, text size, waveform progress bar, a single search field on every screen, voice input in the assistant.

## Improvements

- **Speed**: smart collections and alphabet jumps are much faster on a large library; large lists no longer slow the server down.
- **Memory**: Tune gives back freed memory during long listening sessions; transcoding memory is bounded.
- **Faster start of sound**: YouTube starts sooner; TIDAL no longer waits for the whole track to download on a local or OAAT output.
- **Statistics**: one total of tracks and albums on every screen, one entry per codec, search across the whole ranking.
- **Tune Circle**: emojis in Messages; when listening at a contact's is closed, Tune looks for the same track in your own sources.
- **Mobile apps**: CarPlay plays directly on the phone, with Collections and a Radio tab; Android gains the TIDAL catalog, radio and podcast history, and a Favorite radios widget.
- **Windows**: a banner warns when the firewall filters Tune; a WASAPI or ASIO badge on local zones.
- **Docker**: the SMB share assistant works inside the image; the CD drive can be used in the container.
- **Translations**: correct plurals in all 15 languages.
- **Dependencies** updated, including bundled SQLite 3.54.

## Fixes

- **Web interface**: no more black screen after an interface update.
- **Mute**: a muted zone stays silent when a new track starts; an alarm or a volume limit no longer unmutes it.
- **Playback**: a damaged file no longer freezes the zone, Tune moves on; a finished podcast episode no longer reopens at its end.
- **AirPlay 2**: playback resumes where it stopped after a pause, and starts at the zone volume, never at full power.
- **DLNA and groups**: playback resumes after a 701 error; the next track is prepared in advance; an unresponsive device no longer blocks other zones; Sonos zones take their model name.
- **HQPlayer, Devialet and Diretta**: position and state tracked, automatic reconnection; the Devialet amplifier responds again; no more decorative volume slider on Diretta.
- **OAAT**: native DSD shown and bit-perfect; DSD1024 recognized, with PCM fallback on a limited receiver.
- **Radios**: no more duplicate stations; a radio refused in your country moves to its next stream.
- **Spotify, Qobuz and TIDAL**: Spotify playlists and favorites follow the new API; a Qobuz refusal is no longer shown as wrong credentials; TIDAL preloading no longer delays playback.
- **Library**: plays are counted per track; a hyphen no longer creates a fake artist; discs of a box set are recognized better; a track without a year takes its album's year.
- **Windows**: a missing DAC no longer sends sound to the PC speakers; folder watching also covers network shares.
- **PostgreSQL 18**: UPnP favorites work again.

## Security

- Administering Tune from another machine on the network now requires signing in (Tune OS: until the new image).
- Webhooks can no longer target the Tune machine itself.
- Home Assistant / Hue configuration now requires admin rights or the Tune machine itself; Home Assistant installed on the same machine as Tune is no longer supported.

## Known issues

- **DSD over DoP on the local output**: it may play at full scale after attenuated PCM, ignoring the volume. Keep your amplifier volume low when going from a PCM track to a DSD over DoP track (next release).
- **Integrity audit**: many FLAC files may be wrongly reported as corrupt; do not delete any file on this verdict alone.
- **Windows ASIO driven by another server**: no sound after Pause then Play; skip to the next track.
- **After the update**: if the interface does not change, force a reload (Cmd+Shift+R or Ctrl+Shift+R). The first start takes longer while two indexes are created.
