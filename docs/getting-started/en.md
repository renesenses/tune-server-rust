# Getting Started with Tune

**Tune** is an open-source multi-room music server that unifies your local library and streaming services (Tidal, Qobuz, Spotify, Deezer) in a web and iPad interface. It streams to your DLNA, AirPlay, Chromecast, BluOS, and Squeezebox devices without any cloud dependency.

---

## 1. Installation

### Docker (recommended)

```bash
docker run -d \
  --name tune \
  --network host \
  -v /path/to/music:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **Important**: `--network host` is required for DLNA/mDNS discovery on your local network.

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

Download the `.exe` installer from [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) and run it.

### iPad (TestFlight)

Request the TestFlight invitation on the [mozaiklabs forum](https://mozaiklabs.fr/forum).

---

## 2. First launch

Open a browser at `http://localhost:8888` (or your server's address).

The onboarding wizard guides you through the first steps:

1. Set the folders containing your music
2. Run a first scan
3. Choose an output zone (your DAC, your DLNA speaker, etc.)

---

## 3. Add your library

**Settings → Music folders → Add**

Tune supports all common audio formats:

- **Lossless**: FLAC, WAV, AIFF, ALAC, APE, WavPack
- **DSD**: DSF, DFF, DST
- **Lossy**: MP3, AAC, OGG, Opus, WMA

The scan is progressive: tracks appear in the library as they're indexed. For a 100,000-track library, expect about 30 minutes.

---

## 4. First zone

A **zone** represents an audio output. Tune automatically detects:

- **DLNA/UPnP**: Hi-Fi streamers (Eversolo, Lindemann, Cocktail Audio, Hifi Rose, Sonos)
- **AirPlay**: Apple speakers, compatible AVRs
- **Chromecast**: Google speakers, some TVs
- **BluOS**: Bluesound, NAD
- **OAAT**: open-source bit-perfect protocol (RPi + USB DAC)
- **Local output**: USB DAC connected to the server

**Settings → Devices** lists everything detected.

To create a zone: **Settings → Zones → New**, pick a name and link a device.

---

## 5. First playback

In the top bar, select the target zone. Then in the library:

- Click on a track → immediate playback
- Click on **Play album** → entire album in the queue
- Click on a playlist's arrow → playlist playback

Playback controls (play/pause/next/volume) are at the bottom of the web client.

---

## 6. Streaming services

**Settings → Streaming services** → Connect

| Service | Authentication | Max quality |
|---------|----------------|-------------|
| Tidal | OAuth (HiFi account) | FLAC 24/192 |
| Qobuz | Login/password (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | ARL token | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

Once connected, the service appears in the **Streaming** menu.

---

## 7. Multi-room

To play **the same track on multiple zones simultaneously**:

**Settings → Zone groups → Create a group**

The server synchronizes outputs via NTP. Latency is adjustable per zone (**Settings → Zones → Sync delay**).

---

## 8. Going further

- **Test plan**: [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 tests to validate
- **API documentation**: `GET /api/v1/system/api-docs` or via the browser on your server
- **Community forum**: https://mozaiklabs.fr/forum
- **GitHub**: https://github.com/renesenses/tune-server-rust
- **CLI**: `cargo install tune-cli` to control from the terminal

Enjoy the music!
