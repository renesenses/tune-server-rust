# Erste Schritte mit Tune

**Tune** ist ein Open-Source-Multi-Room-Musikserver, der Ihre lokale Bibliothek und Streaming-Dienste (Tidal, Qobuz, Spotify, Deezer) in einer Web- und iPad-Oberfläche vereint. Er streamt zu Ihren DLNA-, AirPlay-, Chromecast-, BluOS- und Squeezebox-Geräten ohne Cloud-Abhängigkeit.

---

## 1. Installation

### Docker (empfohlen)

```bash
docker run -d \
  --name tune \
  --network host \
  -v /pfad/zur/musik:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **Wichtig**: `--network host` ist für die DLNA/mDNS-Erkennung im lokalen Netzwerk erforderlich.

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

Laden Sie das `.exe`-Installationsprogramm von [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) herunter und starten Sie es.

### iPad (TestFlight)

Fordern Sie die TestFlight-Einladung im [Mozaiklabs-Forum](https://mozaiklabs.fr/forum) an.

---

## 2. Erster Start

Öffnen Sie einen Browser unter `http://localhost:8888` (oder der Adresse Ihres Servers).

Der Onboarding-Assistent führt Sie durch die ersten Schritte:

1. Geben Sie die Ordner mit Ihrer Musik an
2. Starten Sie einen ersten Scan
3. Wählen Sie eine Ausgabezone (Ihr DAC, Ihr DLNA-Lautsprecher usw.)

---

## 3. Bibliothek hinzufügen

**Einstellungen → Musikordner → Hinzufügen**

Tune unterstützt alle gängigen Audioformate:

- **Lossless**: FLAC, WAV, AIFF, ALAC, APE, WavPack
- **DSD**: DSF, DFF, DST
- **Verlustbehaftet**: MP3, AAC, OGG, Opus, WMA

Der Scan ist progressiv: Titel erscheinen in der Bibliothek, sobald sie indiziert werden. Für eine Bibliothek mit 100.000 Titeln rechnen Sie mit etwa 30 Minuten.

---

## 4. Erste Zone

Eine **Zone** stellt eine Audioausgabe dar. Tune erkennt automatisch:

- **DLNA/UPnP**: Hi-Fi-Streamer (Eversolo, Lindemann, Cocktail Audio, Hifi Rose, Sonos)
- **AirPlay**: Apple-Lautsprecher, kompatible AVRs
- **Chromecast**: Google-Lautsprecher, einige TVs
- **BluOS**: Bluesound, NAD
- **OAAT**: Open-Source bit-perfect Protokoll (RPi + USB-DAC)
- **Lokale Ausgabe**: USB-DAC am Server

**Einstellungen → Geräte** listet alles Erkannte auf.

Zum Erstellen einer Zone: **Einstellungen → Zonen → Neu**, Name wählen und Gerät verknüpfen.

---

## 5. Erste Wiedergabe

In der oberen Leiste die Ziel-Zone auswählen. Dann in der Bibliothek:

- Auf einen Titel klicken → sofortige Wiedergabe
- Auf **Album abspielen** klicken → ganzes Album in die Warteschlange
- Auf den Pfeil einer Playlist klicken → Playlist abspielen

Wiedergabesteuerungen (Play/Pause/Weiter/Lautstärke) sind unten im Webclient.

---

## 6. Streaming-Dienste

**Einstellungen → Streaming-Dienste** → Verbinden

| Dienst | Authentifizierung | Max. Qualität |
|--------|-------------------|---------------|
| Tidal | OAuth (HiFi-Konto) | FLAC 24/192 |
| Qobuz | Login/Passwort (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | ARL-Token | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

Nach Verbindung erscheint der Dienst im Menü **Streaming**.

---

## 7. Multi-Room

Um **denselben Titel gleichzeitig auf mehreren Zonen** abzuspielen:

**Einstellungen → Zonengruppen → Gruppe erstellen**

Der Server synchronisiert die Ausgaben über NTP. Die Latenz ist pro Zone einstellbar (**Einstellungen → Zonen → Sync-Verzögerung**).

---

## 8. Weiterführend

- **Testplan**: [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 zu validierende Tests
- **API-Dokumentation**: `GET /api/v1/system/api-docs` oder im Browser auf Ihrem Server
- **Community-Forum**: https://mozaiklabs.fr/forum
- **GitHub**: https://github.com/renesenses/tune-server-rust
- **CLI**: `cargo install tune-cli` zur Steuerung vom Terminal

Viel Spaß beim Hören!
