# Iniziare con Tune

**Tune** è un server musicale multi-room open-source che unifica la tua libreria locale e i servizi di streaming (Tidal, Qobuz, Spotify, Deezer) in un'interfaccia web e iPad. Trasmette ai tuoi dispositivi DLNA, AirPlay, Chromecast, BluOS e Squeezebox senza dipendenza dal cloud.

---

## 1. Installazione

### Docker (consigliato)

```bash
docker run -d \
  --name tune \
  --network host \
  -v /percorso/musica:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **Importante**: `--network host` è necessario per il rilevamento DLNA/mDNS sulla rete locale.

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

Scarica l'installer `.exe` da [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) ed eseguilo.

### iPad (TestFlight)

Richiedi l'invito TestFlight sul [forum mozaiklabs](https://mozaiklabs.fr/forum).

---

## 2. Primo avvio

Apri un browser su `http://localhost:8888` (o l'indirizzo del tuo server).

L'assistente di benvenuto ti guida nei primi passi:

1. Indica le cartelle con la tua musica
2. Avvia una prima scansione
3. Scegli una zona di uscita (il tuo DAC, il tuo diffusore DLNA, ecc.)

---

## 3. Aggiungere la libreria

**Impostazioni → Cartelle musicali → Aggiungi**

Tune supporta tutti i formati audio comuni:

- **Lossless**: FLAC, WAV, AIFF, ALAC, APE, WavPack
- **DSD**: DSF, DFF, DST
- **Lossy**: MP3, AAC, OGG, Opus, WMA

La scansione è progressiva: le tracce appaiono nella libreria man mano che vengono indicizzate. Per una libreria da 100.000 tracce, conta circa 30 minuti.

---

## 4. Prima zona

Una **zona** rappresenta un'uscita audio. Tune rileva automaticamente:

- **DLNA/UPnP**: streamer Hi-Fi (Eversolo, Lindemann, Cocktail Audio, Hifi Rose, Sonos)
- **AirPlay**: diffusori Apple, AVR compatibili
- **Chromecast**: diffusori Google, alcune TV
- **BluOS**: Bluesound, NAD
- **OAAT**: protocollo open-source bit-perfect (RPi + DAC USB)
- **Uscita locale**: DAC USB collegato al server

**Impostazioni → Dispositivi** elenca tutto ciò che è stato rilevato.

Per creare una zona: **Impostazioni → Zone → Nuova**, scegli un nome e associa un dispositivo.

---

## 5. Prima riproduzione

Nella barra in alto, seleziona la zona di destinazione. Poi nella libreria:

- Clicca su una traccia → riproduzione immediata
- Clicca su **Riproduci album** → intero album in coda
- Clicca sulla freccia di una playlist → riproduzione della playlist

I controlli di riproduzione (play/pausa/successivo/volume) sono in fondo al client web.

---

## 6. Servizi di streaming

**Impostazioni → Servizi di streaming** → Connetti

| Servizio | Autenticazione | Qualità max |
|----------|----------------|-------------|
| Tidal | OAuth (account HiFi) | FLAC 24/192 |
| Qobuz | Login/password (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | Token ARL | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

Una volta connesso, il servizio appare nel menu **Streaming**.

---

## 7. Multi-room

Per riprodurre **la stessa traccia su più zone simultaneamente**:

**Impostazioni → Gruppi di zone → Crea un gruppo**

Il server sincronizza le uscite tramite NTP. La latenza è regolabile per zona (**Impostazioni → Zone → Ritardo di sync**).

---

## 8. Approfondire

- **Piano di test**: [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 test da validare
- **Documentazione API**: `GET /api/v1/system/api-docs` o tramite browser sul tuo server
- **Forum community**: https://mozaiklabs.fr/forum
- **GitHub**: https://github.com/renesenses/tune-server-rust
- **CLI**: `cargo install tune-cli` per controllare dal terminale

Buon ascolto!
