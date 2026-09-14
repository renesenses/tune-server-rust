# Spotify natif — prototype non officiel (#4166)

Arbitrage JP : essayer sans application développeur ni Client ID à saisir.
Base : `main` 24123a4e (v0.9.150). Aucun changement de version, aucun déploiement
automatique. Le service existant reste celui des compilations ordinaires.

## Limite de sécurité avant toute utilisation

**Instance de test séparée, base séparée, compte Premium uniquement.**
`librespot-core` 0.8.0 appelle `process::exit(1)` quand le compte n'est pas
Premium ; `librespot-playback` le fait aussi sur certains états internes
invalides. `catch_unwind` ne peut pas intercepter ces sorties. Le prototype
ne doit donc pas être activé dans une instance Tune en production. Une
isolation en sous-processus avec supervision est requise avant intégration.

Sources examinées : [session.rs](https://github.com/librespot-org/librespot/blob/v0.8.0/core/src/session.rs),
[player.rs](https://github.com/librespot-org/librespot/blob/v0.8.0/playback/src/player.rs).
Cette voie n'est ni une API partenaire Spotify ni une promesse de stabilité.

## Périmètre implémenté

- Compilation `--features spotify-native` **et** `TUNE_SPOTIFY_NATIVE=1`.
- Appairage local sur demande, fenêtre de 180 secondes ; aucun mot de passe,
  cookie de navigateur, Client Secret ou jeton à copier dans un formulaire.
- Page `/api/v1/streaming/spotify/native-pairing`, également renvoyée comme
  `verification_url` au client. La page ne démarre pas l'appairage à sa lecture.
- Identifiants de session dans `auth_tokens_spotify_native`, distincts de
  `auth_tokens_spotify`. Ils sont sensibles : protéger la base et ses sauvegardes.
  Ce changement n'ajoute pas de chiffrement au stockage existant.
- Catalogue via les interfaces de session de librespot : métadonnées de
  pistes, albums, artistes et playlists ; navigation depuis leurs identifiants.
- Recherche textuelle expérimentale via `get_context("spotify:search:…")`,
  limitée à 30 pistes. Ce n'est pas la recherche multi-catégorie du Web Player.
  Coller une URI ou un lien `https://open.spotify.com/…` ouvre aussi l'entité
  correspondante (piste, album, artiste ou playlist).
- Collections limitées à 300 objets. Une playlist partielle est refusée,
  pas présentée comme complète. Favoris personnels, écriture et synchronisation
  de playlists non implémentés et signalés comme tels.
- Lecture par le Player Rust, PCM WAV 44,1 kHz / 16 bits stéréo vers une session
  Tune. La file et les commandes de sortie restent celles de Tune. Le seek
  recrée le décodeur à l'offset demandé, sans tenter un Range dans un tuyau PCM.
- Sortie PCM bornée (64 blocs de 4096 octets), annulation même sous
  contre-pression, timeout de démarrage, EOF fini. Une seule zone productrice
  Spotify à la fois. Pas de préchargement gapless dans ce premier périmètre.
- Les zones avec DSP actif sont refusées : il ne faut pas ignorer un réglage
  silencieusement. Pas de normalisation supplémentaire, pas de VU-mètres
  Spotify ajoutés. 320 kbit/s est demandé, mais pas annoncé comme débit mesuré.

## Compilation / validation

Utiliser Shrek selon `AGENTS.md` avec une clé propre à l'unité, préflight
charge/espace/AOSP, puis purge. Ne pas compiler en parallèle sur la même clé.

```sh
cargo test -p tune-core --locked --lib --no-default-features \
  --features oaat,spotify-native spotify_native
cargo test -p tune-streaming-http --locked \
  --features tune-core/spotify-native,tune-core/oaat
cargo build -p tune-server --locked --no-default-features \
  --features oaat,spotify-native
```

La feature épingle `vergen=9.0.6` : `librespot` 0.8.0 ne compile pas avec
`vergen` 9.1.0 / `vergen-gitcl` 1.0.8
([défaut amont #1681](https://github.com/librespot-org/librespot/issues/1681)).
Le verrou provient de la release amont, pas d'une modification du cache Cargo.

### Vérifications du 2026-09-14

- Linux sur Shrek : 11 tests filtrés `spotify_native` réussis, dont le passage
  de l'orchestrateur par la session native, sans URL audio publique.
- Routes `tune-streaming-http` : 29 tests réussis avec la feature native.
- Ancien connecteur : 28 tests `streaming::spotify::tests` réussis sans la
  feature native (`--no-default-features --features oaat`).
- Cross-compilation `aarch64-apple-darwin` réussie ; le Mach-O a ensuite été
  exécuté sur le Mac, avec une base neuve et un port distinct. `--version`
  répond `0.9.150`, `/api/v1/system/health` répond `ok`, zéro piste et album.
- Page et JavaScript : HTTP 200 avec les types MIME attendus et `no-store` ;
  page sous CSP restrictive. Lecture de la page et du statut : aucun appairage.
  JSON mal formé à l'authentification : HTTP 400, toujours aucun appairage.
- Recherche sans compte : refus explicite demandant l'appairage, aucun résultat
  inventé. Formatage Rust, `git diff --check` et `node --check` réussis.
- Aucun navigateur connecté à l'outil : rendu visuel et interactions JavaScript
  non vérifiés dans un navigateur. Les contrôles HTTP ne les remplacent pas.

Ces validations ciblées ne remplacent pas la CI multiplateforme.

## Essai utilisateur requis

1. Démarrer le binaire dans une instance de test avec un nouveau `TUNE_DB_PATH`,
   un `TUNE_DATA_DIR` séparé, un port libre, `TUNE_AUTO_SCAN=false`,
   `TUNE_AUTO_UPDATE=false`, `TUNE_SPOTIFY_NATIVE=1` et aucune bibliothèque locale.
2. Ouvrir la page d'appairage. Cliquer pour ouvrir la fenêtre ; dans Spotify,
   sur le même réseau, choisir **Tune — Spotify pairing**. Attendre la confirmation
   dans Tune. Ne pas coller d'identifiant de session dans une issue ou un journal.
3. Chercher une piste, puis essayer aussi un lien d'album connu. Vérifier noms,
   durée, ordre des pistes et pochette. Vérifier le comportement si la recherche
   de contexte n'est plus acceptée par Spotify.
4. Jouer vers une zone de test sans DSP. Vérifier écoute, pause/reprise,
   seek, piste suivante, fin naturelle et stop. Ne pas utiliser une zone
   actuellement pilotée par un autre serveur pour éviter la concurrence.
5. Redémarrer cette instance et vérifier la reconnexion. Déconnecter Spotify,
   redémarrer et vérifier que le compte ne revient pas. Vérifier l'expiration
   de la fenêtre et l'absence de publicité mDNS hors appairage.

Les tests hors connexion ne prouvent **ni** l'acceptation de l'appairage par
Spotify **ni** la recherche réelle **ni** l'écoute sur un renderer. Aucun de
ces trois résultats ne doit être annoncé sans essai avec le compte utilisateur.

## Contre-épreuve exécutée

Le 2026-09-14, le témoin
`streaming::spotify_native::tests::native_registry_save_and_logout_preserve_existing_web_credentials`
est vert avec la clé séparée. Remplacer **uniquement le code** de
`SpotifyNativeService::credential_key` par la clé historique
`auth_tokens_spotify` le fait rougir après compilation réussie :
`native pairing must not overwrite Web API credentials`. Le test est inchangé.
La clé séparée est ensuite restaurée avant la validation finale.

Commande de contre-épreuve :

```sh
cargo test -p tune-core --locked --lib --no-default-features \
  --features oaat,spotify-native \
  streaming::spotify_native::tests::native_registry_save_and_logout_preserve_existing_web_credentials \
  -- --exact
```
