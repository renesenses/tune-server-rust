# Empezar con Tune

**Tune** es un servidor de música multi-sala de código abierto que unifica su biblioteca local y sus servicios de streaming (Tidal, Qobuz, Spotify, Deezer) en una interfaz web e iPad. Transmite a sus dispositivos DLNA, AirPlay, Chromecast, BluOS y Squeezebox sin dependencia de la nube.

---

## 1. Instalación

### Docker (recomendado)

```bash
docker run -d \
  --name tune \
  --network host \
  -v /ruta/a/musica:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **Importante**: `--network host` es necesario para el descubrimiento DLNA/mDNS en su red local.

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

Descargue el instalador `.exe` desde [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) y ejecútelo.

### iPad (TestFlight)

Solicite la invitación TestFlight en el [foro mozaiklabs](https://mozaiklabs.fr/forum).

---

## 2. Primer inicio

Abra un navegador en `http://localhost:8888` (o la dirección de su servidor).

El asistente de inicio le guía por los primeros pasos:

1. Indique las carpetas con su música
2. Lance un primer escaneo
3. Elija una zona de salida (su DAC, su altavoz DLNA, etc.)

---

## 3. Añadir su biblioteca

**Ajustes → Carpetas de música → Añadir**

Tune admite todos los formatos de audio comunes:

- **Lossless**: FLAC, WAV, AIFF, ALAC, APE, WavPack
- **DSD**: DSF, DFF, DST
- **Con pérdida**: MP3, AAC, OGG, Opus, WMA

El escaneo es progresivo: las pistas aparecen en la biblioteca a medida que se indexan. Para una biblioteca de 100.000 pistas, cuente unos 30 minutos.

---

## 4. Primera zona

Una **zona** representa una salida de audio. Tune detecta automáticamente:

- **DLNA/UPnP**: streamers Hi-Fi (Eversolo, Lindemann, Cocktail Audio, Hifi Rose, Sonos)
- **AirPlay**: altavoces Apple, AVRs compatibles
- **Chromecast**: altavoces Google, algunos TVs
- **BluOS**: Bluesound, NAD
- **OAAT**: protocolo open-source bit-perfect (RPi + DAC USB)
- **Salida local**: DAC USB conectado al servidor

**Ajustes → Dispositivos** lista todo lo detectado.

Para crear una zona: **Ajustes → Zonas → Nueva**, elija un nombre y asocie un dispositivo.

---

## 5. Primera reproducción

En la barra superior, seleccione la zona objetivo. Luego en la biblioteca:

- Haga clic en una pista → reproducción inmediata
- Haga clic en **Reproducir álbum** → álbum entero en la cola
- Haga clic en la flecha de una playlist → reproducción de la playlist

Los controles de reproducción (play/pausa/siguiente/volumen) están en la parte inferior del cliente web.

---

## 6. Servicios de streaming

**Ajustes → Servicios de streaming** → Conectar

| Servicio | Autenticación | Calidad máx. |
|----------|---------------|--------------|
| Tidal | OAuth (cuenta HiFi) | FLAC 24/192 |
| Qobuz | Login/contraseña (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | Token ARL | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

Una vez conectado, el servicio aparece en el menú **Streaming**.

---

## 7. Multi-sala

Para reproducir **la misma pista en varias zonas simultáneamente**:

**Ajustes → Grupos de zonas → Crear un grupo**

El servidor sincroniza las salidas mediante NTP. La latencia es ajustable por zona (**Ajustes → Zonas → Retardo de sync**).

---

## 8. Ir más allá

- **Plan de pruebas**: [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 pruebas a validar
- **Documentación API**: `GET /api/v1/system/api-docs` o vía navegador en su servidor
- **Foro comunidad**: https://mozaiklabs.fr/forum
- **GitHub**: https://github.com/renesenses/tune-server-rust
- **CLI**: `cargo install tune-cli` para controlar desde la terminal

¡Buena escucha!
