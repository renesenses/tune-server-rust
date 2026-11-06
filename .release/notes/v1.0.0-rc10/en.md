Tune 1.0.0-rc10, the tenth release candidate of Tune 1.0.

## New features

- **My physical collection**: a new plugin compares your Discogs collection with your library and the streaming services, in a grid of covers cached on the server. A dedicated entry appears in the sidebar, right after Tune Circle.
- **My theme**: choose every colour of the interface, per profile. A plugin can also provide a theme, and you can send yours to a Tune Circle contact, who applies it in one click.
- **Crosstalk**: the plugin that reduces crosstalk between speakers joins the extensions catalogue, with its settings screen; on a given zone it turns crossfeed off and says so.
- **Folders**: play a folder with all its subfolders, or add it to the queue, in order; the Library's Directory filter goes down into subfolders and offers to return to the whole library.
- **Radio**: a "More filters" bar in My radios and the Directory lets you search by criteria; four new genres arrive in the directory.
- **Radio playback**: a station's streams are sorted by quality; Now Playing shows the station logo and a "My radios" button; the listening country is set in Settings › General.
- **Labels**: record label logos are displayed, shared by mozaiklabs.fr; the server status page follows the progress of album covers and label logos.
- **Volume**: the slider is graduated in dB, the comfort and safety limits are shown in colour, and a comfort limit above the safety limit is refused.
- **Oxygen and metadata**: edit a field across a whole filtered result; exclude folders from automatic metadata enrichment.
- **Tune Circle**: zoom into an area of the globe in the World view; Tune Circle playlists get their own chip in the Playlists filters; a service playlist can be shared from its page.
- **Desktop widget**: the mini window uses the mini view of the web interface and gains a search for artists, Qobuz and TIDAL, with thumbnails and keyboard navigation.
- **AES67 output**: an experimental output to AES67-compatible Dante and Ravenna audio networks, off by default, is available in Settings › Experimental.

## Improvements

- **This browser**: the next track is prepared before the end for smoother transitions, and the server's "This browser" zone plays without any prior setup.
- **Resume listening**: the card names the album, playlist or episode being resumed, and a finished listen leaves the list.
- **History and top tracks**: the tracks of an open album or playlist run from most recent to oldest; an artist's top tracks add the ones you listen to most, from every source.
- **Library**: filters by DR and by original year are much faster on large libraries; track list columns can be resized and each screen remembers their width; acoustic analysis explains its pace and shows its progress.
- **Equalizer**: free entry of frequency and Q, validated when you finish typing; deleting imported presets asks for confirmation; room correction and DAC calibration apply the full equalizer profile.
- **Playlists**: .m3u, .m3u8 and .dpl import lives in the Playlist manager and reports the real number of tracks; Import, Transfer and Compare say why they fail.
- **Covers**: embedded artwork is read from every tag, front cover first; the online search follows the disc re-read; the rectangular artwork of a Qobuz playlist is shown in full.
- **Messages**: the last error messages still in French are translated into the interface language; an AirPlay 2 speaker that refuses the connection reports missing pairing; an application ID refused by Qobuz is no longer presented as a wrong password.
- **Licence**: the screen states that Premium comes from your Mozaiklabs account, without asking for a key.
- **Home**: "Other versions" no longer hangs while waiting for the streaming services.
- **ASIO**: when DSD support cannot be checked, Tune says so and explains what the output can do.

## Fixes

- **Playback**: switching between PURE and the equalizer no longer restarts a stopped zone; unchecking "Repeat by default" really turns repeat off for the zone; transitions between compatible tracks stay continuous.
- **OpenHome**: pause and stop remove the prepared next track from the player, and the end position is kept while moving to the next track.
- **Radio**: with an unknown listening country, the chosen stream is no longer replaced by a 96 kbit/s stream; a station added from the console keeps its link to its page; a single failure no longer suspends radio metadata; "Recently played" shows the right date; the Qobuz playlist created from radio favourites appears at once.
- **Library and profiles**: the favourites and ratings filters follow the active profile; "Most played" no longer counts a listen several times; enrichment no longer replaces an existing genre; UPnP tracks keep their year and genres; a MusicBrainz identifier already in use is no longer assigned a second time.
- **Collections and playlists**: a refused save is reported, a faulty folder no longer empties the list, and every page of playlists is read.
- **Plugins and account**: uninstalling an active native plugin disables it for good; the first link to a Mozaiklabs account attaches to the single profile even without an email address.
- **Miscellaneous**: playlist import during a scan says why it does not import; a failure to read the settings is reported instead of falling back to defaults; putting the computer to sleep is no longer reported as a freeze; the old interface no longer survives a server update; the VU meter bar no longer overflows between 1280 and 1800 px; WASAPI mode stays visible with the Windows default setting; audio is no longer compressed when sent.
- **Security**: hardening fixes.
