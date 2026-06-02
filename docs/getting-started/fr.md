# Démarrer avec Tune

**Tune** est un serveur de musique multi-room open-source qui unifie votre bibliothèque locale et vos services de streaming (Tidal, Qobuz, Spotify, Deezer) dans une interface web et iPad. Il diffuse vers vos appareils DLNA, AirPlay, Chromecast, BluOS, et Squeezebox sans dépendance cloud.

---

## 1. Installation

### Docker (recommandé)

```bash
docker run -d \
  --name tune \
  --network host \
  -v /chemin/vers/musique:/music \
  -v ~/.tune:/data \
  renesenses/tune:latest
```

> **Important** : `--network host` est nécessaire pour la découverte DLNA/mDNS sur votre réseau local.

### macOS (Homebrew)

```bash
brew tap renesenses/tap
brew install tune-server
brew services start tune-server
```

### Windows

Télécharger l'installeur `.exe` depuis [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases) et le lancer.

### iPad (TestFlight)

Demander l'invitation TestFlight sur le [forum mozaiklabs](https://mozaiklabs.fr/forum).

---

## 2. Premier démarrage

Ouvrir un navigateur sur `http://localhost:8888` (ou l'adresse de votre serveur).

L'assistant d'accueil vous guide à travers les premières étapes :

1. Renseigner les dossiers contenant votre musique
2. Lancer un premier scan
3. Choisir une zone de sortie (votre DAC, votre enceinte DLNA, etc.)

---

## 3. Ajouter votre bibliothèque

**Réglages → Dossiers musicaux → Ajouter**

Tune supporte tous les formats audio courants :

- **Lossless** : FLAC, WAV, AIFF, ALAC, APE, WavPack
- **DSD** : DSF, DFF, DST
- **Lossy** : MP3, AAC, OGG, Opus, WMA

Le scan est progressif : les pistes apparaissent dans la bibliothèque au fur et à mesure. Pour une bibliothèque de 100 000 pistes, comptez environ 30 minutes.

---

## 4. Première zone

Une **zone** représente une sortie audio. Tune détecte automatiquement :

- **DLNA/UPnP** : streamers Hi-Fi (Eversolo, Lindemann, Cocktail Audio, Hifi Rose, Sonos)
- **AirPlay** : enceintes Apple, AVR compatibles
- **Chromecast** : enceintes Google, certains TV
- **BluOS** : Bluesound, NAD
- **OAAT** : protocole open-source bit-perfect (RPi + DAC USB)
- **Sortie locale** : DAC USB connecté au serveur

**Réglages → Appareils** liste tout ce qui a été détecté.

Pour créer une zone : **Réglages → Zones → Nouvelle**, choisir un nom et associer un appareil.

---

## 5. Première lecture

Dans la barre du haut, sélectionner la zone cible. Puis dans la bibliothèque :

- Cliquer sur une piste → lecture immédiate
- Cliquer sur **Lire l'album** → l'album entier dans la file d'attente
- Cliquer sur la flèche d'une playlist → lecture de la playlist

Les contrôles de lecture (play/pause/suivant/volume) sont en bas du client web.

---

## 6. Services de streaming

**Réglages → Services de streaming** → Connecter

| Service | Authentification | Qualité max |
|---------|------------------|-------------|
| Tidal | OAuth (compte HiFi) | FLAC 24/192 |
| Qobuz | Login/password (Studio) | FLAC 24/192 |
| Spotify | OAuth (Premium) | 320 kbps |
| Deezer | Token ARL | FLAC 16/44 |
| YouTube Music | OAuth | ~256 kbps |

Une fois connecté, le service apparaît dans le menu **Streaming**.

---

## 7. Multi-room

Pour jouer **la même piste sur plusieurs zones simultanément** :

**Réglages → Groupes de zones → Créer un groupe**

Le serveur synchronise les sorties via NTP. La latence est ajustable par zone (**Réglages → Zones → Délai de sync**).

---

## 8. Aller plus loin

- **Cahier de recette** : [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 tests à valider
- **Documentation API** : `GET /api/v1/system/api-docs` ou via le navigateur sur votre serveur
- **Forum communauté** : https://mozaiklabs.fr/forum
- **GitHub** : https://github.com/renesenses/tune-server-rust
- **CLI** : `cargo install tune-cli` pour piloter depuis le terminal

Bonne écoute !
