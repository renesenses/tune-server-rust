---
title: "Tune Server"
subtitle: "Architecture logicielle"
author: "Bertrand Clech — Mozaik Labs"
date: "Juillet 2026"
---

# Tune Server — Architecture logicielle

## Qu'est-ce que Tune ?

Tune est un **serveur audio multiroom haute-fidélité** entièrement écrit en Rust. Il gère la lecture de musique locale et streaming (Qobuz, Tidal, Spotify...) vers n'importe quel appareil audio du réseau (enceintes DLNA, AirPlay, Chromecast, Sonos...).

### Le projet en chiffres

| | |
|---|---|
| **Langage** | Rust (100%) |
| **Modules** | 6 composants principaux |
| **Formats audio** | FLAC, WAV, DSD, AIFF, MP3, AAC, ALAC, APE, WavPack, Opus |
| **Sorties supportées** | 15 types (DLNA, AirPlay, Chromecast, Sonos, ASIO...) |
| **Services streaming** | 7 plateformes (Qobuz, Tidal, Spotify, Deezer...) |

---

\newpage

## Vue d'ensemble

L'architecture suit un modèle en couches. Chaque couche a une responsabilité claire.

```mermaid
graph TB
    subgraph "INTERFACES UTILISATEUR"
        WEB["<b>Client Web</b><br/>Svelte 5"]
        IOS["<b>App iOS / iPad</b><br/>Swift + FFI"]
        FLUTTER["<b>App Flutter</b><br/>Dart + FFI"]
        TERMINAL["<b>Ligne de commande</b><br/>tune-cli"]
    end

    subgraph "SERVEUR API"
        API["<b>Serveur HTTP</b><br/>67 endpoints REST<br/>+ WebSocket temps réel"]
    end

    subgraph "MOTEUR AUDIO"
        CORE["<b>Moteur Tune</b><br/>Lecture, décodage, transcodage<br/>Gestion des zones et appareils"]
    end

    subgraph "CLOUD"
        BRIDGE["<b>Relais Cloud</b><br/>Accès distant via WebSocket"]
    end

    WEB -->|HTTP + WebSocket| API
    IOS -->|Embarqué| API
    FLUTTER -->|Embarqué| API
    TERMINAL -->|HTTP| API
    API --> CORE
    CORE <-->|WebSocket| BRIDGE

    style API fill:#1a237e,color:#fff
    style CORE fill:#0d47a1,color:#fff
    style WEB fill:#e3f2fd
    style IOS fill:#e3f2fd
    style FLUTTER fill:#e3f2fd
    style TERMINAL fill:#e3f2fd
    style BRIDGE fill:#e8eaf6
```

---

\newpage

## Les 6 modules du projet

```mermaid
graph LR
    subgraph " "
        A["<b>tune-core</b><br/>Moteur audio"]
        B["<b>tune-server</b><br/>API REST / WebSocket"]
        C["<b>tune-cli</b><br/>Outil terminal"]
        D["<b>tune-bridge</b><br/>Relais cloud"]
        E["<b>tune-ffi</b><br/>Interface mobile"]
        F["<b>tune-pyo3</b><br/>Extension Python"]
    end

    B --> A
    C --> A
    E --> A
    F --> A

    style A fill:#1565c0,color:#fff
    style B fill:#1976d2,color:#fff
    style C fill:#64b5f6
    style D fill:#64b5f6
    style E fill:#64b5f6
    style F fill:#64b5f6
```

| Module | Rôle |
|--------|------|
| **tune-core** | Le cerveau : traitement audio, base de données, appareils, streaming, plugins |
| **tune-server** | La facade : API HTTP, authentification, WebSocket, routes |
| **tune-cli** | Outil en ligne de commande pour administrer le serveur |
| **tune-bridge** | Passerelle cloud pour l'accès distant au serveur |
| **tune-ffi** | Interface C pour embarquer Tune dans les apps mobiles (Flutter, iOS) |
| **tune-pyo3** | Extension Python pour compatibilité avec l'ancien serveur |

---

\newpage

## Le moteur audio (tune-core)

Le coeur du système, découpé en sous-systèmes indépendants :

```mermaid
graph TB
    subgraph "CHEF D'ORCHESTRE"
        ORCH["<b>Orchestrator</b><br/>Décide comment lire chaque piste :<br/>quel format, quel décodeur,<br/>quel appareil de sortie"]
    end

    subgraph "TRAITEMENT AUDIO"
        DEC["<b>Décodeurs</b><br/>FLAC, WAV, AIFF, DSD<br/>MP3, AAC, APE, WavPack"]
        DSP["<b>Traitement du signal</b><br/>Resampling, égalisation<br/>Correction acoustique (FIR)"]
    end

    subgraph "SORTIES AUDIO — 15 types"
        LOCAL["<b>Sortie locale</b><br/>USB, ASIO, WASAPI<br/>CoreAudio (macOS)"]
        RESEAU["<b>Réseau</b><br/>DLNA, AirPlay, Chromecast<br/>Sonos, BluOS, OpenHome<br/>Squeezebox, HQPlayer"]
        MULTI["<b>Multiroom</b><br/>OAAT (Open Advanced<br/>Audio Transport)<br/>Groupes de zones"]
    end

    subgraph "SERVICES DE STREAMING"
        STR["Qobuz · Tidal · Spotify<br/>Deezer · Amazon Music<br/>YouTube Music · Podcasts"]
    end

    subgraph "BIBLIOTHÈQUE"
        LIB["<b>Scanner</b><br/>Parcours des dossiers<br/>Extraction métadonnées"]
        META["<b>Enrichissement</b><br/>MusicBrainz, Last.fm<br/>Pochettes, biographies"]
        DB["<b>Base de données</b><br/>SQLite ou PostgreSQL<br/>14 tables principales"]
    end

    subgraph "EXTENSIBILITÉ"
        PLG["<b>Système de plugins</b><br/>SDK, chargement dynamique<br/>Marketplace intégré"]
    end

    ORCH --> DEC
    ORCH --> DSP
    ORCH --> LOCAL
    ORCH --> RESEAU
    ORCH --> MULTI
    ORCH --> STR
    LIB --> DB
    META --> DB
    ORCH --> DB
    PLG --> ORCH

    style ORCH fill:#e65100,color:#fff
    style DEC fill:#fff3e0
    style DSP fill:#fff3e0
    style LOCAL fill:#e8f5e9
    style RESEAU fill:#e8f5e9
    style MULTI fill:#e8f5e9
    style STR fill:#f3e5f5
    style LIB fill:#e3f2fd
    style META fill:#e3f2fd
    style DB fill:#fce4ec
    style PLG fill:#fff8e1
```

---

\newpage

## Architecture modulaire et plugins

Tune est concu pour être extensible. Le système de plugins permet d'ajouter des fonctionnalités sans modifier le coeur du serveur.

### Principes d'extensibilité

| Mécanisme | Description |
|-----------|-------------|
| **Plugin SDK** | Kit de développement permettant de créer des plugins en Rust |
| **Chargement dynamique** | Les plugins sont chargés au démarrage sans recompilation |
| **Points d'extension** | 8 points d'accroche : décodeurs, sorties, services, métadonnées, DSP, UI, événements, commandes |
| **Marketplace** | Catalogue intégré pour découvrir et installer des plugins |
| **Isolation** | Chaque plugin tourne dans son propre contexte avec permissions contrôlées |

### Points d'extension du plugin SDK

```mermaid
graph TB
    subgraph "PLUGIN SDK"
        SDK["<b>Plugin Context</b><br/>API d'accès aux services<br/>du serveur"]
    end

    subgraph "POINTS D'EXTENSION"
        PE1["<b>Décodeurs</b><br/>Ajouter le support<br/>de nouveaux formats"]
        PE2["<b>Sorties audio</b><br/>Nouveaux protocoles<br/>de diffusion"]
        PE3["<b>Services streaming</b><br/>Intégrer de nouvelles<br/>plateformes"]
        PE4["<b>Métadonnées</b><br/>Sources d'enrichissement<br/>additionnelles"]
        PE5["<b>Traitement DSP</b><br/>Effets audio,<br/>analyse spectrale"]
        PE6["<b>Événements</b><br/>Réagir aux actions<br/>(scrobbling, stats...)"]
    end

    SDK --> PE1
    SDK --> PE2
    SDK --> PE3
    SDK --> PE4
    SDK --> PE5
    SDK --> PE6

    style SDK fill:#f57f17,color:#fff
    style PE1 fill:#fff8e1
    style PE2 fill:#fff8e1
    style PE3 fill:#fff8e1
    style PE4 fill:#fff8e1
    style PE5 fill:#fff8e1
    style PE6 fill:#fff8e1
```

### Exemples de plugins existants

| Plugin | Fonction | Statut |
|--------|----------|--------|
| **Last.fm Scrobbler** | Envoie l'historique d'écoute à Last.fm | Intégré |
| **Correction acoustique** | Convolution FIR pour correction de salle | Intégré |
| **Égaliseur paramétrique** | EQ pro avec presets par zone | Intégré |
| **Radio metadata** | Récupération des métadonnées ICY/Shoutcast | Intégré |
| **Sleep timer** | Arrêt programmé de la lecture | Intégré |
| **Auto DJ** | Génération de playlists par IA | Intégré |

---

\newpage

## Comment une piste est jouée

Ce diagramme montre le parcours d'une piste, du clic utilisateur jusqu'au son dans les enceintes.

```mermaid
sequenceDiagram
    participant U as Utilisateur
    participant W as Client Web
    participant S as Serveur API
    participant O as Orchestrator
    participant D as Décodeur
    participant A as Appareil audio

    U->>W: Clic "Lecture"
    W->>S: POST /playback/play
    S->>O: Lancer la lecture

    alt Fichier local (NAS, disque)
        O->>D: Décoder le fichier
        D-->>O: Flux audio PCM
    else Streaming (Qobuz, Tidal...)
        O->>O: Obtenir l'URL du flux
    end

    O->>A: Envoyer le flux audio

    alt Sortie locale (USB, ASIO)
        A->>A: Resampling si nécessaire
        A->>A: Correction acoustique
        A->>A: Envoi au DAC
    else DLNA (enceinte réseau)
        A->>A: Négociation du format
        A->>A: Envoi HTTP au renderer
    end

    loop Toutes les secondes
        A-->>S: Position, état, volume
        S-->>W: Mise à jour en temps réel
    end
```

---

\newpage

## Les appareils de sortie supportés

Tune peut envoyer le son vers 15 types d'appareils différents :

### Sortie locale (connexion directe au DAC)

| Type | Plateforme | Bit-perfect | DSD natif |
|------|-----------|-------------|-----------|
| **Audio standard** | macOS, Windows, Linux | Non | Via DoP |
| **CoreAudio exclusif** | macOS | Oui | Via DoP |
| **ASIO exclusif** | Windows | Oui | Via DoP |
| **WASAPI exclusif** | Windows | Oui | Via DoP |

### Sortie réseau (enceintes et streamers)

| Protocole | Exemples d'appareils | Gapless | Volume | DSD |
|-----------|---------------------|---------|--------|-----|
| **DLNA/UPnP** | Denon, Marantz, HiFi Rose, Micromega | Oui | Oui | Passthrough |
| **OpenHome** | Linn, Naim, dCS | Oui | Oui | Passthrough |
| **Sonos** | Toute la gamme Sonos | Oui | Oui | Non |
| **AirPlay** | Apple TV, HomePod, enceintes AirPlay | Non | Oui | Non |
| **Chromecast** | Google Nest, enceintes Cast | Non | Oui | Non |
| **BluOS** | Bluesound Node, NAD | Non | Oui | Non |
| **Squeezebox** | Logitech Squeezebox, Squeezelite | Oui | Oui | Non |
| **HQPlayer** | Signalyst HQPlayer | Non | Oui | Natif |

### Multiroom

| Protocole | Description |
|-----------|-------------|
| **OAAT** | Open Advanced Audio Transport — protocole de synchronisation multiroom développé pour Tune. Synchronisation sub-milliseconde entre les zones via UDP multicast. |
| **Groupes de zones** | Regroupement logique de plusieurs appareils pour lecture synchronisée |

---

\newpage

## Les services de streaming

| Service | Qualité maximale | Authentification |
|---------|-----------------|------------------|
| **Qobuz** | FLAC 24 bits / 192 kHz | Identifiant + mot de passe |
| **Tidal** | FLAC Hi-Res | OAuth (PKCE) |
| **Spotify** | 320 kbps | OAuth + Spotify Connect |
| **Deezer** | FLAC CD | OAuth |
| **Amazon Music** | Ultra HD (24/192) | OAuth |
| **YouTube Music** | AAC 256 kbps | OAuth (PKCE) |
| **Podcasts** | Variable | Flux RSS (pas d'auth) |

---

## La base de données

Tune supporte deux moteurs de base de données via une couche d'abstraction commune :

- **SQLite** : utilisé par défaut (desktop, Docker). Fichier unique, aucune installation.
- **PostgreSQL** : pour les serveurs de production. Meilleure gestion des accès concurrents.

### Schéma des tables principales

```mermaid
erDiagram
    ARTISTS ||--o{ ALBUMS : "a produit"
    ARTISTS ||--o{ TRACKS : "interprète"
    ALBUMS ||--o{ TRACKS : "contient"
    PLAYLISTS ||--o{ PLAYLIST_TRACKS : "contient"
    TRACKS ||--o{ PLAYLIST_TRACKS : "référencé par"
    ZONES ||--o{ PLAY_QUEUE : "file d'attente"
    ZONES ||--o{ HISTORY : "historique"
    TRACKS ||--o{ HISTORY : "écouté"
    TRACKS ||--o{ RATINGS : "noté"
    PROFILES ||--o{ RATINGS : "par utilisateur"

    ARTISTS {
        int id PK
        string name
        string bio
        string image_url
    }

    ALBUMS {
        int id PK
        string title
        int artist_id FK
        int year
        string genre
        string cover_path
    }

    TRACKS {
        int id PK
        string title
        int album_id FK
        int artist_id FK
        string file_path
        string format
        int sample_rate
        int bit_depth
        int duration_ms
    }

    ZONES {
        int id PK
        string name
        string output_type
        string output_device_id
        int max_sample_rate
        string dsd_mode
    }

    PLAYLISTS {
        int id PK
        string name
        string description
        bool is_smart
    }

    SETTINGS {
        string key PK
        string value
    }

    RADIOS {
        int id PK
        string name
        string url
        string artwork_url
    }
```

### Les 14 tables

| Table | Rôle |
|-------|------|
| **artists** | Artistes avec biographie et image |
| **albums** | Albums avec pochette, année, genre |
| **tracks** | Pistes avec chemin fichier, format, qualité audio |
| **zones** | Zones de lecture (chaque appareil = une zone) |
| **playlists** | Playlists manuelles et intelligentes |
| **play_queue** | File d'attente par zone |
| **history** | Historique d'écoute |
| **ratings** | Notes utilisateur (1 à 5 étoiles) |
| **radios** | Stations de radio internet |
| **settings** | Paramètres clé-valeur |
| **profiles** | Profils multi-utilisateurs |
| **tags** | Tags personnalisés |
| **source_links** | Liens vers les services de streaming (Qobuz ID, Tidal ID...) |
| **track_metadata** | Métadonnées enrichies (MusicBrainz, Last.fm) |

---

\newpage

## L'API — Principaux endpoints

Le serveur expose une API REST avec plus de 67 groupes d'endpoints.

### Lecture et contrôle

| Endpoint | Action |
|----------|--------|
| `POST /playback/play` | Lancer la lecture |
| `POST /playback/pause` | Mettre en pause |
| `POST /playback/stop` | Arrêter |
| `POST /playback/next` | Piste suivante |
| `POST /playback/seek` | Avancer/reculer dans la piste |
| `POST /playback/volume` | Régler le volume |

### Bibliothèque

| Endpoint | Action |
|----------|--------|
| `GET /library/albums` | Liste des albums |
| `GET /library/artists` | Liste des artistes |
| `GET /library/tracks` | Liste des pistes |
| `GET /library/search` | Recherche plein texte |
| `POST /system/scan` | Lancer un scan de la bibliothèque |

### Zones et appareils

| Endpoint | Action |
|----------|--------|
| `GET /zones` | Liste des zones de lecture |
| `POST /zones` | Créer une zone |
| `DELETE /zones/:id` | Supprimer une zone |
| `GET /devices` | Appareils découverts sur le réseau |

### Streaming

| Endpoint | Action |
|----------|--------|
| `GET /streaming/:service/search` | Rechercher sur un service |
| `GET /streaming/:service/albums/:id` | Détail d'un album |
| `POST /streaming/:service/auth` | Authentification |

---

\newpage

## Compilation conditionnelle

Le même code source produit des binaires différents selon les plateformes, grâce à des **drapeaux de compilation** :

| Drapeau | Par défaut | Description |
|---------|-----------|-------------|
| `local-audio` | Oui | Active la sortie audio locale (USB, casque) |
| `asio` | Non | Active le support ASIO (Windows, audio pro) |
| `oaat` | Oui | Active le protocole multiroom OAAT |
| `cloud-relay` | Oui | Active la connexion au cloud Mozaik Labs |
| `postgres` | Non | Active le support PostgreSQL |

---

## Points d'attention pour l'architecte

1. **L'Orchestrator est central** — Point unique de décision pour le routage audio. Sa complexité croît avec chaque nouveau type de sortie ou format. Candidat pour un refactoring en pattern Strategy.

2. **Le Poller (boucle de surveillance)** — Boucle à 1 Hz qui surveille l'état de toutes les zones, gère le gapless, les transitions, le volume. Code dense. Candidat pour une refonte en machine à états.

3. **Double base de données** — SQLite et PostgreSQL coexistent via une couche d'abstraction. Les requêtes SQL utilisent des placeholders dynamiques. Risque de divergence entre les deux moteurs.

4. **15 implémentations de sortie** — Chaque protocole (DLNA, AirPlay, Chromecast...) a sa propre implémentation derrière un trait commun `OutputTarget`. La couverture de tests est inégale selon les protocoles.

5. **Système de plugins** — Le SDK est fonctionnel avec 6 points d'extension. L'isolation des plugins et la gestion des dépendances restent à renforcer pour un usage tiers.

6. **Interface mobile (FFI)** — L'API C pour Flutter/iOS est minimale (4 fonctions exposées). Elle mériterait un enrichissement pour exploiter les capacités natives des plateformes.
