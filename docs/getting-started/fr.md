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

### Debian / Ubuntu (paquet .deb)

Télécharger `tune-server_<version>_amd64.deb` (ou `_arm64.deb` pour un
Raspberry Pi) depuis [GitHub Releases](https://github.com/renesenses/tune-server-rust/releases), puis :

```bash
sudo apt install ./tune-server_<version>_amd64.deb
```

Le service démarre tout seul et se relance au redémarrage de la machine.
Indiquer ensuite les dossiers de musique dans `/etc/default/tune-server` :

```bash
sudo nano /etc/default/tune-server     # TUNE_MUSIC_DIRS=/srv/musique
sudo systemctl restart tune-server
journalctl -u tune-server -f
```

> Requiert Debian 12 ou Ubuntu 22.04 au minimum (glibc 2.35). Sur une base plus
> ancienne, `apt` refuse l'installation avec un message clair plutôt que de
> laisser un serveur à moitié fonctionnel.

Détails (emplacements, mise à jour, désinstallation) :
`/usr/share/doc/tune-server/README.Debian`.

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

Seuls les points de diffusion Tune regroupés par **OAAT** partagent une horloge et reçoivent des timestamps de présentation. Les groupes de sorties DLNA, AirPlay, Chromecast ou autres permettent un pilotage commun, mais Tune ne les présente pas comme un système multiroom synchronisé. Le délai réglable par zone est une correction manuelle, pas une mesure automatique de la latence acoustique.

---

## 8. Aller plus loin

- **Cahier de recette** : [docs/cahier-recette-v0.8.20.md](../cahier-recette-v0.8.20.md) — 58 tests à valider
- **Documentation API** : `GET /api/v1/system/api-docs` ou via le navigateur sur votre serveur
- **Forum communauté** : https://mozaiklabs.fr/forum
- **GitHub** : https://github.com/renesenses/tune-server-rust
- **CLI** : `cargo install tune-cli` pour piloter depuis le terminal

---

## 9. Mot de passe administrateur perdu

Authentification active et mot de passe du compte administrateur (`default`,
ou `admin`) perdu : la récupération se fait **sur la machine du serveur**, par
une commande qui ne démarre pas le serveur et n'affiche jamais de mot de passe.
Elle doit voir la même base que le service : même compte, mêmes
`TUNE_DATA_DIR` / `TUNE_DB_PATH`. Sans base à cet endroit, elle le dit et ne
crée rien.

Deux façons de finir :

- **avec un navigateur sur la machine** : `tune-server --reinitialiser-admin`
  (ou `--reset-admin`) écrit un code à usage unique (15 min, 5 essais) dans la
  console et dans `tune-server.log`. Ouvrir `http://localhost:8888` **sur cette
  machine**, cliquer « Mot de passe oublié ? », saisir le code et le nouveau
  mot de passe. Depuis un autre appareil, derrière un mandataire ou par le
  relais distant, c'est refusé ;
- **sans navigateur sur la machine** : ajouter `--mot-de-passe-stdin` (ou
  `--password-stdin`) et taper le nouveau mot de passe, deux fois, sans qu'il
  s'affiche (ou le passer par un tube : la première ligne est lue).

Dans les deux cas, en une seule opération : les sessions ouvertes sont
révoquées (il faut se reconnecter partout, et les WebSocket du serveur en
marche se ferment dans les secondes qui suivent) et la **clé d'API est
effacée** (à régénérer pour les scripts qui s'en servaient). Le **jeton du
pont** (écoute à distance) ne change pas : les apps appairées le restent.
Si l'écriture échoue, rien n'est changé et la commande le dit.

| Système | Commande |
|---|---|
| Windows | Menu Démarrer → *Tune Server* → **Mot de passe administrateur oublié** (lance `reinitialiser-admin.bat` à côté de `tune-server.exe`) |
| macOS | dans le Terminal, sous votre compte : `tune-server --reinitialiser-admin` (Homebrew) |
| Debian / Ubuntu | `sudo -u tune sh -c 'cd /var/lib/tune && set -a && . /etc/default/tune-server && exec /opt/tune/tune-server --reinitialiser-admin'` |
| Docker (conteneur `tune-server` du docker-compose) | `docker exec -it -u tune tune-server /app/tune-server --reinitialiser-admin --mot-de-passe-stdin` (le navigateur de l'hôte n'est pas « cette machine » pour le conteneur) |
| Tune OS | en SSH : `sudo sh -c 'cd /opt/tune && TUNE_DATA_DIR=/opt/tune/data ./tune-server --reinitialiser-admin --mot-de-passe-stdin'` |

> Le mot de passe du **compte système** `tune` de Tune OS (SSH, console) est
> une autre affaire : voir l'assistant de la console de l'image.

Bonne écoute !
