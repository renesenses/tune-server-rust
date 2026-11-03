Tune 1.0.0-rc9, the ninth release candidate of Tune 1.0.

## New features

- **Tune Circle**: private messaging between contacts, with music cards playable in one click, deleting your own messages and reporting. Messages are not end-to-end encrypted and may be read by Mozaiklabs after a report.
- **Tune Circle**: "Write" from the contact list opens the conversation directly; Enter sends the message; phone-friendly layout.
- **Tune Circle**: find the circles you have been invited to and share a playlist with them; a badge and a list gather notifications, configurable by type.
- **Tune Circle**: the World view shows the community anonymously, by country and by areas of about 110 km. You can remove your presence from the public map.
- **Radio**: global search finds your stations and directory stations, with several words in any order. A station page shows its genres, website and streams, and you can choose your preferred stream.
- **Maps and podcasts**: explore stations by genre, country and network, with a list view; browse podcasts from several countries.
- **Resume playback**: resume returns to the original album, playlist or queue, shuffle included. For podcasts it steps back a few seconds, adjustable from 0 to 300 s.
- **Collections**: random sort in collections and tags, with "Reshuffle"; a service's favourite albums can be sorted by year and quality.
- **Now Playing**: a "Share" button copies the link to the current track.
- **Oxygen**: add or remove a tag on an entire filtered result, after a preview and confirmation.
- **Appearance**: new "Mozaiklabs" theme; the Mozaiklabs sign-in page follows the interface language and theme.
- **Languages**: Norwegian Bokmål, Polish and Greek arrive in Tune.
- **Android Auto**: Favourites, Playlists, Radio, Queue and Now Playing, with controls for the selected zone (not yet tested in a car).

## Improvements

- **In the car**: with Android Auto as with CarPlay, Tune Remote plays on "This phone"; the home zone is left untouched.
- **Radio**: artwork published by stations is shown, with the logo as a fallback; the directory refreshes without restarting Tune; stream selection respects country restrictions.
- **TIDAL**: the search for a Hi-Res version continues when the first response is CD quality only, within the limits of your subscription.
- **HQPlayer**: track changes are more reliable and a restart that fails to begin playback is reported.
- **Devialet**: seeking in a Qobuz Hi-Res track resumes without losing sound; for Devialet AIR, the volume observed on the device is shown separately.
- **Volume**: volume and mute follow each output's capabilities; the AI assistant only raises the volume in small steps.
- **Outputs**: a device offering both AirPlay and UPnP/DLNA is listed in both forms; ASIO and CoreAudio follow the channel mapping grid more closely.
- **Library**: compact device list; the Folders filter also applies to recently added items; history groups plays of the same station.
- **Remote listening**: account-based access can be set up without a QR code, and devices linked to the account can be removed.

## Fixes

- **Playback**: an outdated position report no longer ends a track after seeking; very short WAV files no longer break the playback flow; playback resumes after a temporary sample-rate change refusal.
- **DLNA and zones**: DLNA players receive the announced multichannel FLAC; changing the device of a playing zone stops the previous one; a refused transfer leaves the original playback intact.
- **TIDAL**: start-up errors caused by certain audio links are fixed.
- **Miscellaneous**: CD ripping after Stop, alarms in order, new playlists shown immediately, update banner that disappears after installation, ListenBrainz and Last.fm keys with PostgreSQL, backups without HTTP caches.
- **Security**: fix for the remote-listening relay; details will be published in a separate advisory.

Spotify ships without a verified real playback test: please tell us on the forum whether playback works with your account.
