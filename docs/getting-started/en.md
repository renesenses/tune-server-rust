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

---

## 9. Lost administrator password

Authentication is on and the administrator password (`default`, or `admin`)
is lost: recovery happens **on the server machine**, with a command that does
not start the server and never prints a password. It must see the same
database as the service: same user, same `TUNE_DATA_DIR` / `TUNE_DB_PATH`.
With no database there, it says so and creates nothing.

Two ways to finish:

- **with a browser on the machine**: `tune-server --reset-admin` (or
  `--reinitialiser-admin`) writes a one-time code (15 min, 5 attempts) to the
  console and to `tune-server.log`. Open `http://localhost:8888` **on that
  machine**, click "Forgot password?", enter the code and the new password.
  From another device, behind a proxy or through the remote relay, it is
  refused;
- **with no browser on the machine**: add `--password-stdin` (or
  `--mot-de-passe-stdin`), type the new password and press Enter.

Either way, existing sessions are revoked: sign in again everywhere.

| System | Command |
|---|---|
| Windows | Start menu → *Tune Server* → **Mot de passe administrateur oublie** (runs `reinitialiser-admin.bat` next to `tune-server.exe`) |
| macOS | in Terminal, as your own user: `tune-server --reset-admin` (Homebrew) |
| Debian / Ubuntu | `sudo -u tune sh -c 'cd /var/lib/tune && set -a && . /etc/default/tune-server && exec /opt/tune/tune-server --reset-admin'` |
| Docker | `docker exec -it -u tune tune /app/tune-server --reset-admin --password-stdin` (the host's browser is not "this machine" for the container) |
| Tune OS | over SSH: `sudo sh -c 'cd /opt/tune && TUNE_DATA_DIR=/opt/tune/data ./tune-server --reset-admin --password-stdin'` |

> The password of the Tune OS **system account** `tune` (SSH, console) is a
> different matter: see the image's console helper.

Enjoy the music!
