Tune 1.0.0-rc11, the eleventh release candidate of Tune 1.0.

## New features

- **Room correction**: a plugin, off by default, measures your room with a microphone (sweep, impulse response, RT60) or simulates its bass modes without one, then proposes a target and an automatic equalizer that only cuts, with one profile per zone; without Premium, impulse-response convolution fades out.
- **Parental controls**: rules per profile in Settings › Profiles, hiding of explicit content, time windows and a listening budget.
- **Upsampling**: a Premium high-quality upsampling plugin plugs into the local output, with its own per-zone setting.
- **Advanced query**: query the library with an expression, the same one that defines smart collections.
- **One page per album**: the versions of an album are grouped and chosen on its page; new DSD quality and Multichannel filters and the channel count appear.
- **Notes and statistics**: personal notes in Markdown and library statistics by breakdown.
- **Catalog and spectrogram**: export the library as a printable HTML catalog, with the current filters; show a track's spectrogram from its menu; a button stops DR analysis between two tracks.
- **World maps**: a map of countries and a country filter in the Library; the globe draws borders, coasts and major rivers, and names the country on hover.
- **Podcasts**: add a podcast from the address of its RSS feed.
- **TIDAL mixes**: the "Your mixes" row in Streaming › TIDAL shows and plays the account's personal mixes.
- **Equalizer import**: import an equalizer file (AutoEq, Equalizer APO, OPRA) with a preview of the bands before applying it.
- **Biographies**: links to Tune artists and albums and to the source; biographies and album notes completed on demand from Qobuz, without overwriting anything and reversibly.
- **Playlists**: replace an unavailable service track in place and repair the playlist.
- **Tune Circle**: write to a whole circle in Messages; general circles and thread consent go through the relay.
- **Physical collection**: cover for each disc, search on streaming services, browsing by folders, genres and styles.

## Improvements

- **Radios**: filter by minimum stream quality in "More filters", "My radios" separated from "Recently played", default stream always playable in the listening country.
- **Tune tested**: Tune offers to add a device after an hour of listening without errors, from a dedicated button; a device already in the catalog adds a household.
- **Library**: "Library extended to service favorites" setting, on by default; faster search by word start; grid search requires all words; From / To period on the years timeline; smoother scrolling of tracks.
- **Track lists**: columns can be reordered and each screen remembers it; a playlist screen gains covers, thumbnail size and a compact header; source badges in the Playlist Manager.
- **Home and resume**: choose the panels of the listening overview; "Resume listening" shows one subtitle per kind, resumes a podcast with title and cover (resuming at the saved position on a DLNA zone comes in the next version), and without duplicates.
- **History**: a listen counts only after 30 s or half the track; "Play" after Stop keeps the current album or playlist; podcast episodes and radios carry the station name.
- **Listening statistics**: imported listens (Roon, Plex, m3u) count in the most played; smart lists and History count like the Library; a Qobuz listen no longer counts for the local album of the same name.
- **Crosstalk and equalizer**: separation / coloration curve and speaker response in the settings screen, separation target from 1 dB; parametric graph frozen by default (Edit / Done) and a color per channel.
- **Extensions**: a "Plugin status" panel for the administrator; a catalog entry and its local plugin are now one extension.
- **Notifications**: errors persist, pause on hover and are grouped; the last hard-coded error messages are translated; the raw exception text is folded into "Details".
- **Background analyses**: progress is shown, priority is low and the default cautious after the scan; enrichment resumes where it left off and retries after a 503; "Run pass" for label logos works even when paused.
- **Support and remote access**: the bug report attaches logs; the link, QR code and copy button return in Settings › Remote access.
- **Playback**: transcoding reasons are named and FLAC is served as is when nothing applies; Next and Previous skip tracks refused by the service.

## Fixes

- **AirPlay 2**: the "Pair" button pairs through the AirPlay 2 component or names its cause; waiting for the PIN no longer blocks playback and closing its window cancels it; the session reopens after a pause; the position advances and a pause no longer loops; the component exists for 64-bit Raspberry Pi.
- **DLNA and OpenHome**: gapless DSD tracks, faster "next", a pause made on the device is respected, the machine's own output no longer appears twice, OpenHome volume without waiting, the list loaded by Kazoo is kept, faster Play for TIDAL CD.
- **Volume**: a new local output no longer starts at full scale; after unmuting, the slider returns to the previous volume; each slider position gives a distinct level; WASAPI and ASIO volume is heard immediately; Tune follows the device below the safety ceiling.
- **Windows**: the shortcut opens without a console window; a DoP the output cannot carry goes out as PCM; network discovery passes the firewall; an unreadable temperature no longer throttles analyses; the update starts its switch script at exit.
- **Updates and license**: on Unix, the new binary and web interface are validated before replacing the old one, and a failure is explicit; removing the license also frees the machine on the site.
- **Library**: photos and videos of a UPnP server no longer come in; a FLAC + CUE box set forms a single album; DSD and CD editions stay two albums; a hand-made compilation is no longer split; a deleted playlist is no longer recreated at scan; a rescan without an artist tag no longer moves the album to "Unknown Artist"; adding a folder scans only that folder.
- **Covers and analyses**: as a last resort, the largest image in the folder; fake Hi-Res analysis over the whole track; gapless MP3; the integrity audit no longer declares intact FLAC files truncated.
- **Streaming services**: Spotify search returns results, a refusal is no longer silent and favorites go past 50 tracks; TIDAL › Favorites no longer freezes Tune; a Qobuz account without favorites quickly returns its empty list; a converted track starts in Safari.
- **Network and storage**: discovery resumes when the network arrives after Tune; a failing SMB mount says why, including under Docker; backup succeeds on PostgreSQL; CD extraction refuses a full or non-writable destination; Tune leaves the Squeezebox ports to a Lyrion server on the same machine.
- **Screens**: the Concerts screen opens without a license; "Ungroup all" reappears; Now playing follows the album change; an expired streaming session keeps its "Reconnect" tab; New alarm stays visible.
- **Raspberry Pi 5**: the Active Cooler fan spins again (new Tune OS image).
- **Security**: hardening fixes; on a server without authentication, exporting the configuration with secrets in clear text is refused (use the backup encrypted with a passphrase).

## Known issues

- **Mute on a DLNA zone**: a muted zone may restart with sound when a new track is started. Lower the volume instead of muting until the fix (next version).
- **Podcast episode resume**: on a DLNA zone, a resumed episode restarts from the beginning (next version).
- **Tune Circle**: circles you are a member of, or invited to, do not yet appear in the list like the ones you created (next version).
