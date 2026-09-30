# Cahier de recette — Tune v1.0.0-rc1

> ⚠️ **BROUILLON à relire par Bertrand.** Claude l'a rédigé le 30/09/2026 à partir de deux
> sources : les PR fusionnées dans les lots `batch/*` depuis la v0.9.168, et les PR du client
> web versées depuis. **La liste n'a pas encore été confrontée au manifeste**
> `.release/v1.0.0-rc1.json` : une ligne dont la PR n'est pas dans la RC doit être retirée
> avant le tag.

**Version** : v1.0.0-rc1. Les fichiers sont en `1.0.0` ; le suffixe ne vit que sur le tag.

**Canal** : tout le monde, soit :
- la release « Latest » ;
- Docker `:latest` ;
- Homebrew ;
- le paquet `.deb` ;
- les images Tune OS.

**Plateformes** :
- macOS (DMG) ;
- Windows (zip ou installeur) ;
- Linux (tar.gz, `.deb`, Docker) ;
- Tune OS (RPi, x86) ;
- iPad et iPhone (TestFlight, app en `1.0.0`) ;
- Android (Firebase).

> La rc1 est la version qui aurait été la 0.9.170. Comme elle part chez tout le monde, la
> **non-régression** et la **mise à jour** passent avant les nouveautés.

## Instructions

Ouvrir le client web à l'adresse `http://<adresse-serveur>:8888`. Pour chaque test, noter
**OK** ou **KO**. Un KO se commente avec :
- le comportement observé ;
- la zone et la sortie audio ;
- la version exacte, lue via `/api/v1/system/version` ;
- les journaux si possible.

---

## 1. Version, mise à jour, paquets (propre à la 1.0.0-rc1)

| # | Test | Résultat |
|---|------|----------|
| 1.1 | `/api/v1/system/version` et `tune-server --version` disent **`1.0.0-rc1`**, et non `1.0.0` | |
| 1.2 | Idem sur Raspberry Pi et Linux ARM, dont le binaire est construit par `cross` | |
| 1.3 | Un serveur **0.9.169** se voit proposer la 1.0.0-rc1 et se met à jour par le bouton | |
| 1.4 | Réglages : **pas** d'alerte « client périmé », alors que le serveur dit `1.0.0-rc1` et le client `1.0.0` | |
| 1.5 | Panneau « Quoi de neuf » : les notes de la 1.0.0-rc1 apparaissent | |
| 1.6 | Docker : `renesenses/tune:latest` pointe sur `v1.0.0-rc1`, et le conteneur démarre | |
| 1.7 | Homebrew : `brew upgrade tune-server` installe la rc1, sommes de contrôle justes | |
| 1.8 | `.deb` : `dpkg -s tune-server` affiche `Version: 1.0.0~rc1`, et l'installation est propre | |
| 1.9 | Tune OS, premier démarrage d'une image rc1 : le serveur s'installe, l'accueil de la console affiche « v1.0.0-rc1 » | |
| 1.10 | Mise à jour Unix : c'est le binaire installé qui est relancé, jamais `<exe>.old` (#5461) | |

## 2. Lecture et sortie locale

| # | Test | Résultat |
|---|------|----------|
| 2.1 | Radio en sortie locale : le PCM 24 bits est préservé (#5217) | |
| 2.2 | ASIO exclusif : enchaînement gapless à format égal, sans rouvrir le pilote (#5204) | |
| 2.3 | CoreAudio exclusif : enchaînement à format égal, sans rouvrir le périphérique (#5451) | |
| 2.4 | WASAPI : l'exclusif armé par ASIO ne s'applique plus, et le chemin du signal n'affiche plus « ASIO (exclusive) » (#5353) | |
| 2.5 | WASAPI exclusif : le fil de rendu passe en MMCSS « Pro Audio », sans craquements (#4357) | |
| 2.6 | FLAC ou MP3 depuis un serveur multimédia : ouverture à la cadence de la source, son dès le téléchargement (#5439) | |
| 2.7 | Bascule PURE : le message dit quand elle s'entend, et une relance réseau n'est pas prise pour la piste suivante (#4680) | |
| 2.8 | Flux interne : une reprise Range repart au bon octet (#5426). Une commande arrivée après la reprise annule le Seek de reprise (#5476) | |
| 2.9 | La position après un Seek est publiée pendant la grâce de déplacement (#5498) | |
| 2.10 | Chromecast : le premier Play est réessayé une fois si le budget de 2 s est épuisé (#5323) | |
| 2.11 | UPnP renderer : NextURI est publié, et `Next` est accepté face à un contrôleur Tune (#5304) | |
| 2.12 | DLNA : le HEAD mandataire est honnête sur la longueur, et l'erreur 701 sur Pause est diagnostiquée (#5050) | |
| 2.13 | Niveaux : les barres retombent quand les trames cessent (web #1791), et le journal dit pourquoi (#5104) | |

## 3. Zones

| # | Test | Résultat |
|---|------|----------|
| 3.1 | Une zone supprimée en pause ou en lecture s'arrête, puis ne revient plus dans `GET /zones` (#5322) | |
| 3.2 | Le rebond de zone ne viole plus l'index unique des sorties, sous SQLite comme sous PostgreSQL (#5464) | |

## 4. Bibliothèque, scan, métadonnées

| # | Test | Résultat |
|---|------|----------|
| 4.1 | Surveillant : un fichier renommé garde sa ligne, et une retouche de balises garde son identifiant (#4896, #5341, #5346) | |
| 4.2 | Le genre d'un album descend sur ses pistes, et « Écrire dans les fichiers » écrit la balise GENRE (#5314) | |
| 4.3 | Scan de démarrage : les fichiers inchangés sont comptés au total (#5371) | |
| 4.4 | Fichier sans balise utile : repli sur le chemin, sans « Musique » ni « 750GB » comme artiste (#4412) | |
| 4.5 | Fichier lent : la lecture des crédits et les pochettes de l'importeur sont bornées, puis sautées et journalisées (#5202) | |
| 4.6 | La pochette d'un album est celle que porte la majorité de ses pistes (#5454) | |
| 4.7 | Coffrets automatiques : feuille CUE (#5317), marqueur de disque (#5357), chemins Windows (#5318) | |
| 4.8 | Coffret composé à la main : il tient après un scan, son titre aussi, et « Défaire le coffret » fonctionne (#5319) | |
| 4.9 | CUE : les balises de l'image complètent la feuille, y compris ISRC, SONGWRITER, CATALOG et REM COMMENT (#5463) | |
| 4.10 | Image ISO de données : AIFF, DSF, DFF, APE, WavPack, Opus et Matroska se lisent (#5299) | |
| 4.11 | Les listes et les rayons du serveur média suivent l'ordre alphabétique naturel (#4956, web #1772) | |
| 4.12 | Les filtres de qualité des albums suivent la règle du badge (#5413) ; la facette dossier gère les accents (#5354) | |
| 4.13 | Onglet Doublons : il ne tourne plus sans fin, abandonne après une minute et propose Réessayer (#5455, web #1788) | |
| 4.14 | Collections intelligentes : liste rapide, avec ses pochettes (#5438) | |
| 4.15 | Étiquettes : `/tags/{id}/…` rend la date du dépôt (#5478) | |
| 4.16 | Enrichissement : une passe coupée repart, et « Reprendre » la relance (#5469) | |
| 4.17 | PostgreSQL : la bascule depuis SQLite copie la date d'ajout (#5389) | |

## 5. Streaming, radio, recherche

| # | Test | Résultat |
|---|------|----------|
| 5.1 | **Radio artiste** : à la demande, multi-sources, sans fin, par lots de 50 titres (#5395) | |
| 5.2 | La recherche pagine sur TIDAL et Deezer ; un service sans pagination garde sa limite (#4803) | |
| 5.3 | Deezer : l'ARL saisie authentifie, et `TUNE_DEEZER_ARL` ne rouvre pas une session fermée volontairement (#5427) | |
| 5.4 | Podcasts : la vignette est mise en cache dès l'abonnement (#5214) | |
| 5.5 | Concerts : la page et le périmètre sont relayés (#5368, #5369) ; le tri « Par date » est le défaut (web #1718) | |

## 6. Convertisseur

| # | Test | Résultat |
|---|------|----------|
| 6.1 | DSF vers MP3 et AAC aux fréquences MPEG (44,1 ou 48 kHz), sans exit -22 (#5480) | |
| 6.2 | DSD vers Hi-Res en 24/176,4 kHz, fréquence au choix, format relu (#5481) | |
| 6.3 | Choix des pistes ; archive au nom de l'album, nom valide sous Windows, macOS et Linux (#5482, #5483) | |
| 6.4 | Les tâches sont retrouvées au retour et conservées après une nouvelle conversion (web #1804) | |

## 7. Greffons, démarrage, Tune Circle

| # | Test | Résultat |
|---|------|----------|
| 7.1 | La page d'attente du démarrage nomme l'étape et le greffon en cours (#5370) | |
| 7.2 | Un greffon trop long ou en échec reste visible, en erreur, avec un message expurgé et **Réessayer** (#5403, web #1799) | |
| 7.3 | Un greffon audio natif s'installe depuis le catalogue | |
| 7.4 | Tune Circle : catalogue d'un contact, rayons partagés, écoute par le relais, « Lire l'album », playlists collaboratives (#5325-#5328) | |
| 7.5 | Copie en ligne : le format de synchro est versionné, et la copie repart une seule fois quand il s'enrichit (#5358) | |
| 7.6 | Un rapport de bug envoyé au forum porte l'identité du compte (#5428) | |

## 8. Client web

| # | Test | Résultat |
|---|------|----------|
| 8.1 | Écouter plus tard : vues liste, petite et grande vignette ; tris par date, titre, artiste et type (web #1802) | |
| 8.2 | Album : sélection multiple des pistes, actions et édition groupées (web #1683) | |
| 8.3 | Précédent : depuis une playlist, une collection, les favoris ou une étiquette, retour au bon onglet en une entrée (web #1790, #1661) | |
| 8.4 | Le réglage « Lecture en cours » ouvre l'album ou la playlist qui joue (web #1784) | |
| 8.5 | La molette n'ouvre plus la file par défaut, un réglage le permet (web #1762) ; la Recherche est juste sous Accueil (web #1759) | |
| 8.6 | EQ et crossfeed : « Enregistrer » et « Enregistrer sous » pour mes préréglages (web #1750) | |
| 8.7 | Playlist Qobuz ou TIDAL : le bouton lecture lance la lecture (web #1760) ; une playlist introuvable affiche « Indisponible » (web #1789) | |
| 8.8 | Favoris : la radio affiche son logo, les playlists s'ouvrent en grille (web #1650) ; Bandcamp ouvre l'album au clic (web #1761) | |
| 8.9 | Tableau de bord : un bloc vide dit ce qui manque (web #1781) | |

## Régressions bloquantes (P0)

Tout KO dans la section 1 (version, mise à jour, paquets) bloque la promotion : la rc1 part
chez tout le monde, sur tous les canaux à la fois.
