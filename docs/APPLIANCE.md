# Tune OS — Appliance x86-64 bootable (clé USB)

Origine : demande testeur (Stéphane Villerio, 07/2026). Objectif : une image ISO
gravable avec Rufus/Balena Etcher qui transforme un PC x86-64 en appliance Tune
Server headless. Premier boot en filaire, configuration du WiFi depuis l'UI web,
puis fonctionnement 100 % WiFi. Musique sur SSD USB local et/ou partage SMB
(ex. NVMe interne d'un streamer Eversolo exposé en SMB). Lecture UPnP/DLNA
jusqu'à DSD256 en passthrough natif (pas de transcodage DSD).

## Phasage

- **Phase 1 (cette branche `feature/appliance-wifi`)** : mode appliance côté
  serveur + UI web — endpoints WiFi via nmcli, flag `appliance` dans
  `/system/config`, section WiFi dans Réglages → Réseau. Les montages SMB
  existent déjà (`/api/v1/network/*`).
- **Phase 2** : pipeline CI de build de l'ISO (Debian 12 amd64, live-build,
  image hybride bootable clé USB avec partition de persistance, firmware
  non-free inclus pour les chipsets WiFi grand public, NetworkManager, Avahi
  `tune.local`, service systemd `Restart=always`, marqueur `/etc/tune-appliance`).
  Publication comme asset de release via le webhook VPS.
- **Phase 3** : fallback hotspot `Tune-Setup` (portail de configuration quand
  aucun réseau connu n'est joignable), à la Volumio/moOde.

## Mode appliance

- Activé si `/etc/tune-appliance` existe (posé par l'image) ou `TUNE_APPLIANCE=1`
  (dev/test). Fonction : `routes::appliance::is_appliance()`.
- Hors mode appliance, tous les endpoints `/api/v1/appliance/*` renvoient 404 :
  la surface n'existe pas sur les installs desktop.
- Le client web lit le booléen `appliance` dans `GET /api/v1/system/config` pour
  afficher la section WiFi (Réglages → Réseau).

## Endpoints (`tune-server/src/routes/appliance.rs`)

| Méthode | Route | Description |
|---|---|---|
| GET | `/api/v1/appliance/status` | État des interfaces (ethernet/wifi), SSID et signal courants |
| GET | `/api/v1/appliance/wifi/scan` | Scan des réseaux (`nmcli -t -f IN-USE,SSID,SIGNAL,SECURITY device wifi list --rescan yes`), dédupliqué par SSID |
| POST | `/api/v1/appliance/wifi/connect` | `{ssid, password?}` → `nmcli device wifi connect` (timeout 60 s, mauvais mot de passe → 400 `net.wifiBadPassword`) |
| POST | `/api/v1/appliance/wifi/forget` | `{ssid}` → `nmcli connection delete id` |

- Exécution : `tokio::process::Command` + `tokio::time::timeout` (pattern de
  `routes/network.rs`). Pas de shell — aucun risque d'injection via SSID ;
  validation SSID (1-32 chars, pas de caractères de contrôle).
- Binaire surchargeable via `TUNE_NMCLI_BIN` (stub dans les tests —
  `tune-server/tests/appliance.rs`).
- Messages d'erreur localisés : clés `net.wifi*` dans `i18n_server.json`
  (9 langues).
- Parsing du format terse nmcli : `split_terse()` gère l'échappement `\:`
  et `\\` (SSID contenant des `:`).

## Prérequis image (phase 2, pour mémoire)

- NetworkManager gère toutes les interfaces (pas d'ifupdown), Ethernet DHCP.
- Le service tune-server doit pouvoir exécuter `nmcli` (tourne en root sur
  l'appliance, ou membre du groupe `netdev` + polkit).
- udisks2 + règle udev pour l'auto-mount des disques USB sous `/media`
  (ajout auto en dossier de bibliothèque : à câbler en phase 2).
- Avahi : hostname `tune`, l'utilisateur accède à `http://tune.local:8888`.
- Recette de validation : lecture DSF DSD256 → renderer DLNA Eversolo en
  passthrough natif (pas de transcodage).
