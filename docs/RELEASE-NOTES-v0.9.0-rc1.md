# Tune Server v0.9.0-rc1 — Release Candidate 1

> **v0.9.0 = train de stabilisation.** Cette RC gèle les fonctionnalités et se
> concentre sur la fiabilité : refonte structurelle interne (sans changement de
> comportement), consolidation de la file d'attente, et la vague de corrections
> accumulées depuis la dernière v0.8 stable.
>
> **Canal beta** — build réservé aux testeurs qualifiés, à ne pas diffuser
> publiquement. Merci de remonter tout écart avec vos logs.

## ✨ Refonte structurelle (Axe 3 — invisible côté utilisateur)

- **Poller** : toute la logique de décision (fin de piste, gapless, transitions)
  extraite en prédicats purs et testés, puis en un moteur d'états (**FSM**)
  `classify_stopped` / `classify_playing`. Un mode *shadow-compare* (drapeau
  `TUNE_POLLER_FSM_SHADOW`, désactivé par défaut) compare le nouveau moteur au
  code historique sans rien changer au comportement — première étape avant la
  bascule progressive par zone.
- **File d'attente** : les deux tables `play_queue` (local) et `streaming_queue`
  fusionnées en une seule table `queue_items`, un seul dépôt, migration
  automatique une-fois. Sémantique identique, base saine pour la suite.
- **Zéro warning** de compilation sur l'ensemble du workspace.

## 🎧 Lecture, gapless & poller

- **Tidal 24/192 (DASH)** : lecture corrigée sur sorties locales / ASIO (3 bugs
  du rapport DEvir).
- **Repeat sur sortie ASIO / exclusive** : ne reste plus bloqué en attente d'une
  transition DLNA qui n'arrive jamais (rapport DEvir).
- **Gapless** : nouvelle tentative de résolution de la piste suivante en cas
  d'échec transitoire ; métadonnées « Now Playing » conservées sur repeat, seek
  et enchaînement gapless ; fin naturelle reconnue pour les pistes < 30 s.
- **Position** bornée à la durée de la piste (plus de dépassement dans l'UI).
- **Volume DLNA** : ne revient plus à sa valeur par défaut périmée.

## 🗂️ Zones (stabilité)

- Les zones supprimées ne **réapparaissent plus** après re-découverte
  SSDP / mDNS (plusieurs chemins corrigés, y compris au scan de démarrage).
- Le **comptage de zones** exclut désormais les zones masquées.

## 📚 Bibliothèque, scan & genres

- **Dédup d'albums** par `(titre, artiste)` au lieu du titre seul ; `?force=true`
  pour re-résoudre un `album_id` et réparer une fusion corrompue.
- **Genres** : dédup insensible à la casse (Classique / classique = un seul).
- Le **scan manuel** respecte `quality_split` comme le file-watcher.
- Nouvel endpoint `POST /library/albums` (créer un album par titre).

## 🔊 Streaming & badges qualité

- Badge qualité correct pour les **flux proxifiés** (mime du codec servi ;
  lossless Qobuz) et **infos techniques** dans l'événement `started` pour un
  badge instantané.
- **Préchargement** : profondeur de bits PCM élargie côté sortie locale (fin du
  bruit blanc) ; le buffer de prefetch n'est plus servi sur un seek.

## 🔒 Sécurité

- Les **secrets sont masqués** de `GET /system/config` et de l'export de config
  par défaut (`?include_secrets=true` pour les inclure explicitement).

## 🛠️ Divers

- **Radios** : import des playlists `.pls` (plus de « 0 station importée »).
- **Crossfade par zone** persistant + endpoint `GET` (fix 405).
- **SSO** : page « Cloud bientôt disponible » propre si non provisionné.
- **Découverte** : n'annonce jamais une IP de tunnel VPN (NordVPN) comme IP
  serveur.
- **YouTube** : flux OAuth *device* pointé sur le client contrôlé (corrige les
  connexions).

## 📦 Chaîne de release (RC-aware)

- Les tags `-rc` sont publiés en **prerelease** et **exclus des canaux stable**
  (GitHub « latest », Homebrew, image Docker `:latest`, page de téléchargement
  publique). Une RC n'auto-update jamais un utilisateur stable.

---

*Testeurs de cette RC : DEvir (Séoul), Silviu, …* — merci ! Signalez les
régressions avec la version exacte (`/api/v1/system/version`) et vos logs.
