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

### Debian / Ubuntu (.deb package)

Download `tune-server_<version>_amd64.deb` (or `_arm64.deb` for a Raspberry Pi)
from [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases), then:

```bash
sudo apt install ./tune-server_<version>_amd64.deb
```

The service starts on its own and comes back after a reboot. Point it at your
music in `/etc/default/tune-server`:

```bash
sudo nano /etc/default/tune-server     # TUNE_MUSIC_DIRS=/srv/music
sudo systemctl restart tune-server
journalctl -u tune-server -f
```

> Requires Debian 12 or Ubuntu 22.04 at minimum (glibc 2.35). On an older base
> `apt` refuses the install with a clear message rather than leaving you with a
> half-working server.

Details (paths, updates, removal): `/usr/share/doc/tune-server/README.Debian`.

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

## 2b. Administration without a password

As long as authentication is off, administration actions (restart, update, plugins…) are accepted from this machine and from devices on the local network. Never considered local: remote access (relay, reverse proxy), public addresses, the shared address space `100.64.0.0/10`, the `fc00::/8` and `fd7a:115c:a1e0::/48` ranges, and the machine's default gateway (a virtual machine behind NAT, or a router that rewrites the source address, makes every connection appear to come from it).

The account, the licence and the remote-access token stay reserved to this machine (or to the administrator).

Turn on **Settings → Administration from this machine only** (or enable authentication) if:

- a TCP relay (port forwarding) runs on another machine of the network in front of Tune: it cannot be detected;
- an overlay network uses private addresses that Tune cannot tell apart from the home network. You can also remove its ranges with `TUNE_ADMIN_LAN_EXCLUDED_RANGES=fd12:3456:789a::/48,10.8.0.0/24`.

In a container, administration from the local network is only enabled with `TUNE_ADMIN_LAN_IN_CONTAINER=1`, to be set only with the host network (`--network host`). With rootless Podman, every connection seems to come from `10.0.2.100`: do not set this variable.

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

Only Tune endpoints grouped through **OAAT** share a clock and receive presentation timestamps. Groups of DLNA, AirPlay, Chromecast, or other independent outputs provide shared control, but Tune does not present them as a synchronized multi-room system. The per-zone delay is a manual correction, not an automatic measurement of acoustic latency.

---

## 8. Going further

- **Test plan**: [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 tests to validate
- **API documentation**: `GET /api/v1/system/api-docs` or via the browser on your server
- **Community forum**: https://mozaiklabs.fr/forum
- **GitHub**: https://github.com/renesenses/tune-server-rust
- **CLI**: `cargo install tune-cli` to control from the terminal

Enjoy the music!
