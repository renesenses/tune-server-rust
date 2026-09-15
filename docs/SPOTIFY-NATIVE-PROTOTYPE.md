# Spotify natif — prototype non officiel (#4166)

Arbitrage JP : essayer sans application développeur ni Client ID à saisir.
Base : `main` 24123a4e (v0.9.150). Aucun changement de version, aucun déploiement
automatique. Le service existant reste celui des compilations ordinaires.

Depuis l'extraction du 2026-09-15, l'implémentation et ses tests vivent dans
`plugins/tune-spotify` ([frontière du plugin](plugins/SPOTIFY-NATIVE-EXTRACTION.md)).
Les comptes rendus datés ci-dessous conservent le nom de leur crate historique ;
les commandes réexécutables emploient maintenant `tune-spotify/native`.

## Isolation et limites de sécurité

**Instance de test séparée, base séparée, compte Premium uniquement.**
`librespot-core` 0.8.0 appelle `process::exit(1)` quand le compte n'est pas
Premium ; `librespot-playback` le fait aussi sur certains états internes
invalides. `catch_unwind` ne peut pas intercepter ces sorties. Le catalogue
et l'authentification tournent donc dans un sous-processus persistant, et
chaque lecture dans un sous-processus jetable. Le bootstrap les distingue
avant d'ouvrir configuration, base, plugins ou journaux du serveur.

Les messages passent par des tuyaux anonymes, avec des trames JSON limitées
à 8 Mio ; les identifiants ne passent ni dans les arguments ni dans des
fichiers de transfert. La sortie d'erreur du worker est désactivée. Le flux
audio enchaîne une réponse encadrée puis du PCM brut. Le parent supervise,
arrête et récolte les enfants, même bloqués sur un tuyau plein. Une requête
interrompue détruit son worker pour ne pas réutiliser une réponse orpheline.
Une requête suivante peut reconnecter le catalogue avec les identifiants
conservés ; aucune réouverture automatique de l'appairage.

Cette isolation de panne n'est **pas** une sandbox de sécurité ni une
validation de production. L'instance de test reste séparée de Goinfre/l'instance de production.

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
- Pistes de collections et albums enregistrés : plafond de 2 000 par résultat ;
  discographies : 300 albums. Une playlist partielle est refusée,
  pas présentée comme complète. Les playlists personnelles sont lues via la
  rootlist de la session, par pages de 100 entrées, avec un plafond de 300
  entrées (marqueurs de dossiers inclus). Tune en présente une liste plate,
  dédoublonnée dans l'ordre Spotify. Une révision qui change entre les pages,
  une page incohérente ou des métadonnées introuvables font échouer la lecture ;
  aucun résultat partiel n'est mis en cache comme une liste complète.
  Les décorations facultatives de la rootlist peuvent manquer : les fiches
  des seules playlists concernées sont alors lues, avec quatre requêtes au
  maximum en parallèle et conservation de l'ordre.
  Le rapport additif `GET /streaming/spotify/playlist-library` accepte les
  refus explicites par entrée (403, 404, 410) : `playlists` contient les fiches
  accessibles et `unavailable` les identifiants/codes refusés, sans nom ni
  nombre de titres inventés. L'onglet Playlists et le widget Mes playlists V2
  affichent ensemble les cartes et l'avertissement traduit. Le contrat ancien
  `/playlists` reste strict ; les autres écrans non migrés n'acceptent pas les
  listes partielles. Les erreurs 401/429/5xx et de pagination restent fatales.
  Titres aimés, albums enregistrés et artistes suivis sont lus nativement
  (limites détaillées ci-dessous). Écriture Spotify et synchronisation de
  playlists restent non implémentées. Les cœurs du client sont les favoris
  propres à Tune, pas une mutation de la bibliothèque Spotify.
- Lecture par le Player Rust, PCM WAV 44,1 kHz / 16 bits stéréo vers une session
  Tune. La file et les commandes de sortie restent celles de Tune. Le seek
  des sorties gérées par le serveur et du navigateur recrée le décodeur à
  l'offset demandé. Le navigateur suit `origine du flux + audio.currentTime`
  et conserve la pause. Voir les preuves du raccord navigateur ci-dessous.
- Sortie PCM bornée (64 blocs de 4096 octets), annulation même sous
  contre-pression, timeout de démarrage, EOF fini. Une seule zone productrice
  Spotify à la fois. Pas de préchargement gapless dans ce premier périmètre.
- Les zones avec DSP actif sont refusées : il ne faut pas ignorer un réglage
  silencieusement. Pas de normalisation supplémentaire, pas de VU-mètres
  Spotify ajoutés. 320 kbit/s est demandé, mais pas annoncé comme débit mesuré.
- La durée du catalogue ne sert pas à inventer un `Content-Length` PCM :
  le HTTP est de longueur inconnue et se termine sur le véritable EOF du
  worker. Le chemin du signal ne qualifie pas ce décodage de lossless ou
  bit-perfect. L'étiquette WAV 44,1/16 décrit le transport, pas la source.

## Reprises bornées des métadonnées — 15 septembre 2026

Les refus par fiche étaient ramenés à un message sans code numérique. Le code
du refus ponctuel observé dans l'unité précédente ne peut donc pas être
reconstitué ; la réussite d'une relance manuelle ne prouve pas qu'il s'agissait
d'un 503. Le parseur conserve désormais le code et le niveau (`provider` ou
`entry`) sans URI, nom, identifiant de compte ou jeton dans le diagnostic.

Les schémas épinglés exposent ces deux codes sous forme d'entiers, sans champ
Retry-After : [entity_extension_data.proto](https://github.com/librespot-org/librespot/blob/v0.8.0/protocol/proto/entity_extension_data.proto),
[extended_metadata.proto](https://github.com/librespot-org/librespot/blob/v0.8.0/protocol/proto/extended_metadata.proto).
La politique locale choisit une liste limitée : 408, 500, 502, 503, 504.
401/403, contenu absent 404/410, 429, 501 et codes inconnus ne déclenchent
aucune relance de métadonnées. Les erreurs de transport gardent la politique
existante de [SpClient](https://github.com/librespot-org/librespot/blob/v0.8.0/core/src/spclient.rs),
sans ajouter de relance fondée sur leur texte.

Chaque groupe en échec peut faire trois tentatives au total, après 250 puis
500 ms d'attente. Les groupes déjà réussis ne sont pas relus. Un budget partagé
limite toute la collection à quatre appels supplémentaires de cette couche,
hors éventuelles relances de transport internes à librespot, toujours dans
les quatre emplacements simultanés et par groupes de 50 identifiants. Une
limite de 30 s couvre la résolution entière, toutes tentatives comprises,
et annule les futures réseau restantes. La limite IPC de 45 s de l'opération
reste distincte et inchangée ; une préparation longue peut donc réduire le
temps disponible. Pas de boucle sans limite ou de relance de la bibliothèque
entière. Les 2 000 positions, leur ordre et leurs doublons restent préservés.

Le parseur examine tout le groupe avant de décider d'une reprise : un refus
permanent prime sur un transitoire, indépendamment de l'ordre. Type d'extension
inattendu, identifiant étranger/répété, contenu invalide et réponse tronquée
restent des erreurs, même si une autre fiche porte un 503. Un refus global du
fournisseur peut légitimement ne porter aucune fiche ; il n'est jamais traité
comme une collection vide. Aucun résultat partiel n'est servi ou mis en cache.

Le trait expose aussi `auth_retry_on_content_error`, vrai par défaut. Le proxy
natif le désactive : les 401/403 de contenu ne doivent pas provoquer une autre
lecture via l'heuristique HTTP de renouvellement de jeton. Le chemin Favoris
respecte cette politique, sans changer celle des autres connecteurs. Le moteur
natif et le rafraîchisseur périodique continuent de gérer leurs sessions.

Les témoins couvrent les trois types de métadonnées, le groupe seul à relire,
1 512 positions dont un doublon, les refus mixtes/permanents, les plafonds
par groupe et partagés, la limite globale et l'annulation des quatre requêtes.
Le témoin HTTP contraste le mode auto-géré natif avec la reprise OAuth
historique et vérifie qu'un refus ne devient pas un résultat vide en cache.

Contre-épreuves compilées, tests inchangés, sept rouges attendus. Depuis
l'extraction en plugin, commande native : `cargo test --locked -p tune-spotify
--lib --features native <témoin>`. Commande HTTP :
`cargo test --locked -p tune-streaming-http --features tune-core/oaat <témoin>`.

- Une seule tentative :
  `native_metadata_retries_only_the_failed_batch_and_preserves_all_positions`
  → `A transient metadata refusal must retry its batch instead of rejecting
  the whole collection`.
- Réautoriser le proxy natif à renouveler via HTTP :
  `native_proxy_poll_and_logout_do_not_spawn_a_worker`
  → `Native Spotify owns recovery; HTTP must not replay a metadata access refusal`.
- Ignorer l'option du connecteur dans le gestionnaire Favoris :
  `native_metadata_refusals_do_not_trigger_http_auth_replay_or_empty_cache`
  → `Native metadata refusals must not trigger HTTP auth replay or become
  a cached empty collection` (200 au lieu de 400).
- Budget partagé relevé à 99 :
  `native_metadata_retries_have_per_batch_and_whole_collection_limits`
  → `A collection-wide failure must stop when its shared retry budget is exhausted`.
- Limite globale relevée à 60 s :
  `native_metadata_deadline_covers_all_attempts_and_cancels_pending_fetches`
  → `The collection deadline must not restart for each retry`.
- Code effacé du diagnostic :
  `native_metadata_permanent_refusals_keep_their_code_without_retrying`
  → `Metadata errors must preserve the numeric status and scope without entity identifiers`.
- 403 classé comme transitoire :
  `native_metadata_mixed_refusals_and_invalid_responses_never_trigger_retry`
  → `A transient status must not hide denied_first or trigger retries of an
  invalid batch` (trois appels au lieu d'un).

Chaque correctif est restauré par copie après sa contre-épreuve ; les dates
des sources distantes sont actualisées avant recompilation. Les journaux
restent avec l'instance privée du Mac. Cette unité n'ajoute ni écriture Spotify,
ni nouvelle lecture audio, ni reprise de l'interface historique.

Validation après restauration : 55 tests natifs et 33 tests HTTP réussis,
puis cross-build macOS ARM. Le client web est inchangé (`b0a143df`), sa batterie
n'est pas relancée dans cette unité. Une future PR d'intégration devra demander
`ci:full` puisque le trait commun et son gestionnaire HTTP sont concernés ;
Shrek et le prototype Mac ne remplacent pas cette batterie multiplateforme.

Binaire installé sur le prototype Mac, SHA-256
`98aa1391586b1f4eae803f744d09b9fefa7526e113f76434306d5ddbdb22de87`.
Sonde worker et HTTP réels : 160 albums, 14 artistes avec portraits, 1 512
titres aimés, 18 pistes de l'album contrôlé et 520 de la grande playlist.
Chrome confirme les trois catégories et la réception/affichage des pistes
de la fiche album, zéro erreur JS. Trois arrêts du seul worker catalogue
restent récupérables à l'ouverture de Streaming en 2,7–2,8 s, sans POST
d'appairage, parent inchangé. Quatre zones arrêtées et files inchangées.
Ces lectures réelles ne constituent pas une injection de 503 dans Spotify :
la politique de reprise et ses refus sont prouvés sur les réponses contrôlées
des tests ci-dessus. Le code du refus initial reste inconnu.

## Reprise après arrêt du worker — 15 septembre 2026

Reproduction sur l'instance Mac isolée : arrêter uniquement son enfant
`--spotify-native-worker control` fait passer `/streaming/services` de
`authenticated=true` à `false`, parent Tune inchangé. L'ancien écran V2 reste
sans onglet Spotify et n'appelle pas le statut individuel. Cette reproduction
établit le chemin défaillant ; elle ne prouve pas la cause exacte de la perte
de session observée à la fin de l'unité albums/artistes.

Deux raccords complémentaires, sans modifier le contrat de la liste globale :

- `refresh_if_needed` accepte aussi un worker absent si l'appairage est
  conservé. Le rafraîchisseur existant du serveur l'appelle toutes les cinq
  minutes. Son `Status` conserve le délai de reprise du moteur ; une absence
  d'appairage, un logout ou une désactivation n'ouvre aucune connexion.
- À l'ouverture de Streaming V2, Spotify activé mais non authentifié déclenche
  un unique `GET /streaming/spotify/status`. La reprise n'est pas incluse dans
  le GET global des services et n'en rallonge donc pas le chemin de lecture.
  Les autres onglets restent affichés, la reprise ne vole pas leur sélection.
  L'onglet Spotify n'apparaît qu'après une réponse activée ET authentifiée.
  Échec réseau ou appairage conservé mais refusé : message visible et bouton
  Réessayer, sans boucle automatique ni requête d'appairage. Une réponse
  reçue après fermeture de la vue est ignorée. La requête a une limite client
  de 95 s couvrant les deux RPC bornés Init/Status, pas une attente infinie.

Le statut individuel ajoute seulement le booléen sûr `auth_details.paired`,
pour distinguer une reprise échouée d'un compte jamais appairé. Aucun
identifiant, jeton, mot de passe ou contenu de bibliothèque n'y est ajouté.
Le simple snapshot `auth_status()` reste sans réseau et ne prétend pas qu'un
worker arrêté est authentifié. L'interface historique ne reçoit pas ce
raccord d'ouverture V2 dans cette unité.

Tests : deux témoins Rust du proxy utilisent de vrais enfants supervisés
et des trames Init/Status factices, sans Spotify ni secrets. Ils couvrent
deux pertes successives, conservation de l'appairage, absence d'appairage,
logout et désactivation. Neuf nouveaux tests montent le vrai composant V2 :
réapparition, sélection Qobuz préservée, compte non appairé, services absents /
désactivés / connectés, refus réessayables et réponse tardive après démontage.

Contre-épreuves, tests inchangés :

- Remettre la seule garde `self.client.alive()` dans `refresh_if_needed`, puis
  `cargo test --locked -p tune-core --lib --no-default-features --features
  oaat,spotify-native native_refresh_recovers_a_dead_worker_using_saved_pairing`
  compile et échoue : `The periodic refresh must reconnect a stopped Spotify
  worker with saved pairing` (un rouge attendu).
- Retirer seulement l'appel de reprise à l'ouverture V2, puis
  `npx vitest run src/lib/__tests__/spotifyRecovery.test.ts -t 'fait réapparaître'`
  échoue : `La consultation Streaming doit reconnecter le worker arrêté et
  rendre son onglet` (un rouge attendu, huit tests non sélectionnés).

Sources restaurées par copie après chaque contre-épreuve. Le relevé final
d'exécution est conservé avec l'instance privée sur le Mac ; aucune écriture
Spotify, aucune publication ou modification Goinfre/l'instance de production dans cette unité.

Validation du candidat installé : 48 tests natifs, 32 tests HTTP, puis
cross-build macOS ARM. Client `b0a143dfc5f3d3111569cbbd52cbc32f0949a55f` :
413 fichiers / 4 599 tests et build verts. Binaire SHA-256
`696a2f795250c354d064b4c3c4bc88bc9bfa02d571903d4b6e8bd7a6c742a128`.
Trois arrêts contrôlés du worker suivis d'une ouverture Chrome : onglet
revenu en 2,6–2,8 s, un seul GET de reprise par essai, zéro POST d'appairage,
parent inchangé, zéro erreur JS. Les quatre zones et leurs files sont
inchangées. Ces durées ponctuelles ne garantissent pas une latence réseau.

La première sonde directe du candidat a reçu un refus sur une fiche de
métadonnées des titres aimés : aucun résultat partiel servi. Une relance
complète a réussi sans modification du binaire (160 albums / 14 artistes avec
portraits / 1 512 titres). Les HTTP du serveur installé confirment ces nombres,
18 pistes sur l'album contrôlé et 520 sur la grande playlist. Le refus initial
est conservé dans les preuves ; cette unité n'ajoute pas de reprise automatique
des lectures de collections échouées et ne détermine pas sa cause amont.

## Albums enregistrés et artistes suivis — 15 septembre 2026

Les opérations privées `UserAlbums` et `UserArtists` relient maintenant le
proxy public au moteur isolé. Les routes favoris existantes ne changent pas.

- Albums : requête de lecture `POST /collection/v2/paging`, représentation
  JSON du schéma [collection2v2.proto](https://github.com/librespot-org/librespot/blob/v0.8.0/protocol/proto/collection2v2.proto),
  ensemble `collection` du compte appairé. Parcours de toutes les pages de
  100 éléments, puis sélection des URI albums explicitement enregistrées et
  non supprimées. Aucun album n'est déduit d'un titre aimé. Tri décroissant
  par date d'ajout. Les dates entières sont acceptées comme nombres ou chaînes
  décimales ([ProtoJSON](https://protobuf.dev/programming-guides/json/)) ;
  une date absente est classée après les dates connues. La dernière page doit
  porter son jeton de fin de synchro.
  Plafonds : 100 pages / 10 000 entrées parcourues, 2 000 albums retournés.
- Artistes : méthodes natives `get_user_profile` et `get_user_following` de
  [spclient.rs](https://github.com/librespot-org/librespot/blob/v0.8.0/core/src/spclient.rs).
  Le total `following_count` doit correspondre à toutes les fiches reçues,
  utilisateurs compris, puis rester identique après lecture. Les utilisateurs
  ne deviennent pas des artistes ; une fiche non suivie ou répétée est refusée.
  Plafond : 1 000 profils suivis (artistes et utilisateurs cumulés). Pas de
  pagination des profils au-delà ; résultat tronqué ou compte supérieur à
  cette limite = erreur explicite, jamais une collection complète inventée.
- Fiches : groupes de 50 et quatre requêtes simultanées, avec les extensions
  natives `ALBUM_V4` / `ARTIST_V4`. Les contrôles de complétude, refus, type
  et identité sont partagés avec les métadonnées des pistes. Chaque type
  conserve son parseur et sa représentation Tune.
- Une erreur de page, de schéma, de réseau ou une modification détectée ne
  produit pas de résultat partiel silencieux. Les jetons/compteurs ne prouvent
  pas un instantané transactionnel face à toute modification simultanée
  (notamment un remplacement d'artiste conservant le même total).

Le test de protocole en lecture seule sur le Mac appairé a reçu 160 albums
et 1 512 titres sur 17 pages, puis 14 artistes pour un total de profil de 14.
Ce relevé précède les validations du binaire final ; il ne prouve pas à lui
seul le raccord HTTP ou l'interface. Aucun appel d'écriture Spotify ni audio.

Le premier candidat a refusé la collection réelle : les 1 672 dates étaient
toutes des chaînes numériques, contrairement aux fixtures initiales. Il n'a
pas remplacé l'instance installée. Le parseur et une fixture dédiée couvrent
désormais cette forme, les valeurs absentes et les dates numériques ; booléens,
fractions, valeurs négatives ou hors plage restent refusés. Le candidat
intermédiaire ne constitue donc pas une validation runtime réussie.

Contre-épreuves, tests inchangés et compilations réussies avant les rouges :

- Arrêt après la première page et retrait du jeton de fin : le témoin
  `native_saved_albums_read_all_pages_without_inventing_albums_from_tracks`
  échoue avec `Saved albums must read the continuation page` (1 contre 2).
  Les témoins de complétude et de plafond échouent également.
- Retrait du contrôle du total des profils :
  `native_following_rejects_truncation_mutation_duplicates_and_false_follows`
  échoue avec `Following must reject truncated`.
- Omission silencieuse de métadonnées absentes : les témoins des albums
  et des pistes échouent, dont `Saved album metadata must reject missing`.
  Bilan de cette contre-épreuve groupée : 38 verts, 6 rouges attendus.
- Retrait du parseur des dates entre guillemets :
  `native_saved_albums_accept_real_quoted_timestamps_without_relaxing_validation`
  échoue avec `Real Spotify timestamps are quoted integers; they must not
  make saved albums unreadable` ; un rouge attendu, aucun autre test exécuté.

Chaque correctif a été restauré par copie de sa sauvegarde, puis les dates
des fichiers distants actualisées pour éviter de réutiliser un binaire saboté.

Validation avant le raccord des portraits : 45 tests `spotify_native` et 32 tests HTTP réussis,
puis cross-build macOS ARM. Le worker exécuté sur le Mac reçoit réellement
160 albums, 14 artistes et 1 512 titres, avec l'appairage conservé et sans
audio. Le binaire corrigé a pour SHA-256
`064fed2ba7d867090524207266097d0b95e0893b8e3bbd3a82a6c77aea49596b`.

Le client utilise aussi l'identifiant natif `id` pour la clé des artistes
favoris, comme pour ceux de la recherche. Deux artistes homonymes restent
deux fiches : retirer ce raccord provoque un test DOM rouge, quatre verts,
et l'erreur Svelte `each_key_duplicate` sur la fixture `Artiste homonyme`.
Le témoin vérifie simultanément albums, artistes et titres sans avertissement
de catégorie indisponible. Le raccord a été restauré par copie.

Le contrôle visuel a ensuite relevé zéro portrait dans les 14 fiches artistes.
Le mapping lit désormais `portrait_group` (champ moderne), avec repli sur
`portrait` (ancien champ). Le témoin
`native_artist_portraits_use_modern_group_with_legacy_fallback` couvre les
deux champs et leur absence. Retirer le raccord provoque un rouge compilé,
`Artist portraits must prefer portrait_group, keep the legacy fallback, and
never invent an image`, puis le code est restauré par copie.

État final installé sur le Mac :

- 46 tests natifs, 32 tests HTTP ; client web 412 fichiers / 4 590 tests,
  build de production réussi. Aucun changement de version ou de CI.
- Binaire macOS ARM SHA-256
  `6865c28ae0cb34771de545f9d38823bec30d68979dd8c29a4746ba15e3467412`.
  Client web `9c057b9515d83c5c221ca2eaf87e816288965abb`.
- HTTP réel : 160 albums, 14 artistes avec 14 URL de portraits et 1 512
  titres aimés. Le premier album ouvre ses 18 pistes ; la playlist de 520
  titres reste complète. Le rapport personnel conserve 30 playlists
  accessibles / une refusée 403 et l'ancien contrat complet reste en refus.
- Chrome indépendant du profil utilisateur : 160 cartes albums, 14 artistes,
  1 512 titres, aucun avertissement de catégorie non implémentée, fiche album
  ouverte et zéro erreur JavaScript. Captures inspectées et gardées privées.
- Appairage conservé, aucune écriture Spotify ni lecture audio ; quatre zones
  arrêtées. Les cœurs restent les favoris Tune. Goinfre et l'instance de production inchangés.

Les timings des lectures locales sont des observations ponctuelles, pas des
benchmarks à froid. Ces validations ciblées ne remplacent pas la CI
multiplateforme ni une nouvelle écoute humaine.

Limite de reconnexion observée lors d'une réouverture Chrome :
`/streaming/services` a momentanément rendu Spotify activé mais non authentifié,
ce qui masque son onglet. Le `GET /streaming/spotify/auth/status` suivant a
rétabli l'état authentifié, sans appairage ; la liste des services était alors
de nouveau correcte. L'origine de cette perte transitoire n'est pas déterminée
dans cette unité. Ce constat a motivé l'unité de reprise du worker décrite
plus haut ; le succès des lectures de collections, à lui seul, ne clôturait
pas ce sujet.

## Compilation / validation

Utiliser Shrek selon `AGENTS.md` avec une clé propre à l'unité, préflight
charge/espace/AOSP, puis purge. Ne pas compiler en parallèle sur la même clé.

```sh
cargo test -p tune-spotify --locked --lib --features native spotify_native
cargo test -p tune-streaming-http --locked \
  --features tune-core/oaat
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
- Essai utilisateur effectué ensuite sur le Mac : appairage depuis Spotify,
  recherche, navigation d'album et lecture dans une zone navigateur dédiée.
  L'utilisateur a confirmé entendre le son. L'interface a été pilotée et
  inspectée dans Chrome. Ces résultats portent sur le premier binaire ; la
  version isolée fait l'objet d'une validation complémentaire ci-dessous.

Ces validations ciblées ne remplacent pas la CI multiplateforme.

### Validation complémentaire de l'isolation, le même jour

- 17 tests natifs sur Shrek, un test serveur du chemin du signal et 29 tests
  HTTP réussis. Les contre-épreuves ci-dessous ont rougi, puis les correctifs
  restaurés sont revenus au vert. Cross-build macOS ARM réussi.
- Le premier démarrage réel a révélé l'initialisation TLS manquante dans le
  worker : rustls voyait deux providers et paniquait. Tune restait disponible.
  Le worker installe maintenant explicitement son provider avant toute session.
  Le témoin runtime `verify-worker-tls.py`, avec des identifiants factices,
  échoue sur le binaire sans correctif avec
  `Spotify worker needs its own TLS provider before creating a session`, puis
  réussit inchangé avec le correctif. Aucun secret réel utilisé par ce témoin.
- Redémarrage Mac : appairage conservé, statut authentifié, recherche réelle
  de trois pistes réussie. Le parent et le worker de catalogue sont bien deux
  processus distincts. Après `SIGKILL` du worker, le PID du serveur ne change
  pas, sa santé reste `ok` et le statut suivant reconnecte un nouveau worker.
- Après `SIGKILL` d'un worker audio exactement identifié comme enfant de
  cette instance, il disparaît et Tune reste disponible avec le même PID.
  Une lecture suivante dans Chrome démarre un nouveau worker et avance.
- Worker audio réel, sans sauvegarder le PCM : les offsets 330 000 et
  335 000 ms d'une piste de 337 560 ms produisent respectivement 7,558 et
  2,541 secondes PCM, puis EOF et sortie 0.
- HTTP, navigateur écarté du flux pour éviter deux consommateurs concurrents :
  59 545 628 octets reçus, HTTP 200 `audio/wav`, chunked, fermeture normale.
  Cela valide la fin HTTP, pas encore l'enchaînement naturel dans Chrome.
- Chrome : lecture observée (`Lecture audio`, progression du compteur),
  pause, puis reprise au passage conservé après une attente. Pas de nouvelle
  confirmation d'écoute humaine sur ce binaire, contrairement au premier.

### Défauts observés avant le raccord du client web

Le seek Chrome a été reproduit en échec : demande à 219 514 ms, état serveur
mis à cette valeur, mais `audio.currentTime` continue autour de 43 secondes.
Le client ignore le seek d'un média dont `audio.duration` n'est pas finie ;
le serveur ne recrée pas le flux d'une zone navigateur sans périphérique.
Il faut un contrat explicite de rechargement du flux et d'offset côté client,
pas simplement annoncer le seek réussi après la mise à jour de l'état.

Pendant la pause, le compteur visuel retombe à zéro (position serveur obsolète)
alors que la reprise audio conserve son passage. Le badge « CD » est encore
une déduction de la résolution PCM par le client, pas une preuve de qualité
lossless Spotify. Le verdict global du chemin du signal est désormais faux
pour `lossless` et `bit_perfect`, sans inventer une seconde conversion WAV.
Le client web n'avait pas été modifié dans l'unité d'isolation.

### Raccord navigateur — 14 septembre 2026

Le flux natif porte maintenant une origine temporelle immuable, attachée à
son identifiant de session : `restart_position_ms`. Les réponses de zone
navigateur publient le contrat additif suivant :

```json
{
  "stream_url": "http://serveur:18888/stream/identifiant-unique.wav",
  "browser_stream": { "seek_mode": "restart", "start_position_ms": 219000 }
}
```

La liste des zones, la fiche, le statut et les réponses de lecture utilisent
la même URL WAV. Les autres sources gardent leur contrat ; les sorties
matérielles ne reçoivent ni URL consommable par le navigateur ni ce marqueur.

Le seek natif recrée le décodeur même sans `output_device_id`. Sa réponse
conserve `position_ms` et ajoute `zone` avec le nouveau flux. Un échec du
décodeur est un refus, pas un déplacement confirmé. La pause est conservée
si le déplacement a été demandé pendant la pause. Une position négative
est refusée ; l'extrémité est bornée avant la fin de la piste.

Le client expérimental `feat/spotify-native-browser` suit l'horloge média,
avec `position = origine du flux + audio.currentTime`. Il ne la remplace plus
par le zéro obsolète du serveur pendant la pause. Les déplacements sont
sérialisés, les gestes intermédiaires dépassés sont abandonnés, et une
notification REST puis WebSocket du même flux ne le consomme pas deux fois.
Les contrôles de seek passent par un chemin commun. La fin de file n'essaie
plus de relire le dernier flux arrêté.

Preuves automatisées :

- Shrek, `tune-core`, `--no-default-features --features oaat,spotify-native` :
  18 tests `spotify_native`, puis 30 tests `seek` réussis.
- `tune-server --lib` : 2 tests du contrat navigateur, dont les vraies routes
  GET liste/fiche/statut et le constructeur des réponses de lecture.
- Client web, Node 22.23.2 (majeure de la CI) : `npm test`, 409 fichiers et
  4 570 tests réussis ; build Vite réussi. Node 26.5.0 produisait des échecs
  `localStorage` dans le banc DOM ; aucune configuration CI n'a été modifiée.
- Contre-épreuves, tests inchangés et code compilable : sans routage vers le
  décodeur, `spotify_native_browser_seek_requires_decoder_confirmation` échoue
  avec `browser native seek must restart a decoder`; sans contrat de flux,
  `native_browser_stream_contract_is_consistent_on_every_zone_surface` échoue
  avec `PCM byte zero needs its track offset`.
- Côté web, retirer l'offset donne `2250` au lieu de `221250`; retirer la
  priorité de l'horloge audio donne `0` au lieu de `27500` pendant la pause.
  Un témoin supplémentaire exécute la vraie boucle WebSocket `v2Live` : sans
  son raccord, la pause rend `60000` au lieu de `99000`, exactement le défaut
  observé dans la nouvelle interface (qui ne monte pas `App.svelte`). Le
  chargement d'un onglet respecte aussi le volume enregistré : retirer ce
  correctif donne `1` au lieu de `0.15` dans le témoin.
  Les correctifs ont été restaurés par copie, puis les suites sont
  revenues au vert. Les fichiers Rust restaurés ont été retouchés sur Shrek
  pour ne pas réutiliser un témoin saboté à cause d'un mtime plus ancien.

Limite supplémentaire constatée pendant cette unité : après environ trente
minutes d'inactivité, le statut de la session catalogue peut devenir faux.
Une recherche réelle reconnecte la session avec l'appairage conservé et le
statut redevient vrai. L'affichage/reconnexion sur simple consultation du
statut reste à traiter séparément.

Essai réel Mac ARM64, serveur sur `18888`, appairage conservé, nouveau client
chargé explicitement (le premier onglet conservait encore l'ancienne app) :

- clic au milieu de la barre : nouveau flux à `168780` ms, Chrome avance
  ensuite à `178801` ms ;
- pause à `188493` ms (3:08), compteur inchangé après attente, alors que le
  serveur reste à `168780` ms : la priorité de l'horloge média est observée ;
- flèche gauche pendant la pause : nouveau flux à `178493` ms, zone toujours
  en pause et compteur immobile à 2:58 ;
- reprise : même identifiant de flux, compteur observé à `190352` ms ;
- seek à `330000` ms sur Instant Crush (durée `337560`) : Chrome à 5:34,
  puis un seul appel suivant automatique 7,4 s après la prise du flux ;
  la deuxième piste démarre à zéro et Chrome affiche 0:13 sur celle-ci ;
- deuxième piste, seek à `272000` ms pour une durée de `276560` : fin
  naturelle 4,5 s après la prise du flux, un seul suivant, zone arrêtée en fin
  de file. Aucun worker audio restant, santé serveur `ok`, même PID parent.

Le binaire de cet essai a pour SHA-256
`2006b1bab363e3060cff949dfe5251499452d2cf91669ecf31872ae8c35d73bb`.
Le son avait été confirmé par l'utilisateur sur le premier prototype ; cette
unité apporte des observations Chrome et serveur, pas une nouvelle attestation
d'écoute humaine. Les deux pistes d'essai restent dans la file du Mac, arrêtée.

### Reconnexion et bibliothèque personnelle — 14 septembre 2026

Le statut demandé au worker appelle maintenant `poll_status` : une session
invalide est reconnectée avec l'appairage conservé, sans recherche préalable
et sans ouvrir la découverte. Après un échec, les consultations du statut
attendent au moins 30 secondes avant une autre tentative. Une recherche
explicite peut toujours réessayer immédiatement. Une session valide ne se
reconnecte pas inutilement ; logout, désactivation et appairage en cours
interdisent cette reconnexion. Un succès efface le diagnostic précédent.
Les lectures de snapshot restent sans I/O, notamment lors de la construction
des réponses du worker. Le rafraîchissement périodique existant du serveur
(toutes les cinq minutes) emprunte lui aussi ce chemin.

La rootlist personnelle passe par `get_rootlist` de librespot 0.8.0 puis le
message protobuf `SelectedListContent`. Les routes Tune existantes
`/streaming/spotify/playlists` et `/streaming/spotify/favorites/playlists`
utilisent le même résultat. Ni favoris ajoutés, ni playlist créée/modifiée,
ni permissions supplémentaires demandées.

Preuves automatisées : 25 tests natifs et 29 tests HTTP réussis sur Shrek.
Les nouveaux tests appellent le vrai dispatch `Operation::Status` et
invalident une vraie `Session` ; seule la frontière réseau est remplacée par
un connecteur de test. L'horloge Tokio est avancée sans attente réelle pour
la temporisation. Le parseur de bibliothèque reçoit aussi une trame protobuf
encodée puis décodée, avec deux pages et des marqueurs de dossiers.

Contre-épreuves compilables, tests inchangés, puis restauration par copie et
retour au vert :

- `Status` renvoyant seulement le snapshot :
  `native_worker_status_reconnects_invalid_session_without_pairing` échoue avec
  `Spotify status must reconnect an invalid saved session without a search` ;
- contrôle du délai retiré :
  `native_status_reconnect_failure_keeps_pairing_and_backs_off` échoue avec
  `Spotify status must back off after a failed reconnect`, 4 tentatives au
  lieu de 1 ;
- contrôle de révision retiré :
  `native_library_refuses_changed_or_partial_pages` échoue avec
  `Spotify personal library must refuse revision, not return a partial collection`.
- refus réintroduit sur les décorations absentes :
  `native_library_resolves_optional_decorations_without_inventing_metadata`
  échoue avec `Spotify rootlist decorations are optional, not a missing playlist`.
  Le code est restauré par copie, puis les 25 tests natifs et 29 tests HTTP
  sont relancés et réussissent.

L'essai réel de la bibliothèque n'est **pas validé** pour ce compte. Le premier
binaire échoue sur une décoration sans nom ; celles-ci sont désormais
complétées par lecture de leur fiche (test additionnel :
`native_library_resolves_optional_decorations_without_inventing_metadata`).
La rootlist du compte contient aussi une entrée marquée 403, sans indicateur
de suppression. Une contre-vérification par lecture ordinaire de la fiche,
avec la même session authentifiée, reçoit également un 403. Ce refus n'est
pas contourné et l'entrée n'est pas supprimée du compte ni omise silencieusement.
Le code final refuse donc rapidement la liste complète sur ce statut, au
lieu de recommencer la lecture de cette fiche à chaque ouverture de l'écran.

La reconnexion du statut est traitée ; l'affichage d'une bibliothèque avec
des entrées indisponibles reste une étape distincte. Un résultat partiel devra
porter un avertissement et ne pas emprunter le contrat actuel « liste complète ».
Les tests à réseau simulé ne prouvent pas une longue veille réseau réelle.

Essai du binaire final sur le Mac, SHA-256
`62aa1c184cd97721e091ebfc964eba82ae232d60d832de606c291d61100f37df` :

- redémarrage de la seule instance de test, santé `ok`, compte authentifié
  sans appairage et recherche réelle de trois pistes réussie ;
- fiche de la playlist publique `37i9dQZF1DXcBWIGoYBM5M` : HTTP 200 ;
  ses 50 pistes sont reçues, avec identifiants et durées ;
- Chrome : lien Spotify collé dans la recherche, carte « Today’s Top Hits »,
  ouverture par le titre, puis tableau de 50 titres (2 h 43 min). Aucun
  favori ajouté, aucune playlist modifiée, aucun démarrage audio depuis cette
  fiche. Elle est laissée ouverte pour l'essai utilisateur ;
- worker audio réel, sans conserver ni faire entendre le PCM : offsets
  330 s et 335 s, respectivement 7,558 s et 2,541 s reçues, EOF et sortie 0 ;
- liste personnelle : refus explicite de la rootlist portant le statut 403,
  relayé en HTTP 502. Ce résultat ne vaut ni une liste vide ni une collection
  complète validée. Les deux pistes de la zone de test restent arrêtées.

Le client web n'a pas été modifié dans cette unité. Goinfre et l'instance de production restent
inchangés ; aucune PR, release ou publication n'est effectuée.

## Bibliothèque explicitement partielle — continuation du 14 septembre 2026

L'utilisateur autorise désormais l'affichage des playlists accessibles avec
avertissement pour les autres. Cette étape remplace le refus intégral de
l'essai précédent uniquement pour les lecteurs qui optent pour le rapport.

- Trait additif `get_playlist_library`, valeur `{playlists, unavailable}`,
  opération IPC `PlaylistLibrary` ; aucun changement du tableau JSON historique.
- Les refus 403/404/410 de la rootlist sont comptés et dédoublonnés. Aucun
  nouvel essai de fiche pour contourner un refus. Si deux occurrences se
  contredisent, le refus gagne : jamais de carte accessible et refusée à la fois.
- Révisions, pagination, limites, champs obligatoires et erreurs réseau restent
  contrôlés. Un problème global ne devient ni bibliothèque vide ni succès partiel.
- Route `/{service}/playlist-library` sans cache de contenu utilisateur et avec
  `Cache-Control: no-store` ; les avertissements restent attachés aux données.
- Le widget et l'onglet Playlists V2 affichent le nombre accessible, le nombre
  indisponible et les codes Spotify. Cas entièrement indisponible distingué du
  compte vide ; échec réseau visible ; une réponse tardive de l'onglet Playlists
  ne remplace pas les données du service suivant. Message traduit en 11 langues.
- L'ancienne interface et le hub transversal Playlists conservent pour l'instant
  le contrat complet. Les favoris et l'écriture ne sont pas ajoutés.

Preuves de cette unité : 27 tests natifs, 30 HTTP ; client web 410 fichiers /
4 575 tests et build de production. Contre-épreuves compilables, tests
inchangés, restauration par copie puis suites vertes :

- retrait de l'enregistrement des refus dans `LibraryPages::append` :
  `native_library_reports_denied_entries_without_hiding_accessible_playlists`
  échoue, `Every omitted playlist needs an explicit availability warning,
  without duplicates` (0 au lieu de 1) ;
- effacement de `unavailable` par la route :
  `playlist_library_keeps_warnings_with_data_and_never_uses_the_complete_cache`
  échoue, `HTTP dropped the partial-library warning` (null au lieu de 403) ;
- bloc d'avertissement masqué dans le vrai widget monté : deux tests échouent,
  dont `Une bibliothèque partielle doit annoncer les playlists indisponibles`.

Commandes Rust actuelles : `cargo test -p tune-spotify --locked --lib
--features native spotify_native` et `cargo test -p
tune-streaming-http --locked --features tune-core/oaat`.
Pour les contre-épreuves, le filtre est remplacé par le nom du témoin ci-dessus.

Essai réel du binaire Mac arm64 (SHA-256
`0ccb303326c3993a0bff5fcf3f10ef80726c2f9acbb62c9497e5f551ef3a687f`) :
rapport HTTP 200 avec 30 playlists accessibles et une entrée indisponible
403. L'ancien `/playlists` refuse toujours explicitement le résultat incomplet.
Une fiche personnelle et ses 15 titres sont lus ; l'authentification reste
valide et les zones restent arrêtées. Le worker audio conserve les deux seeks
de fin de piste (7,558 s / 2,541 s PCM, EOF et sortie 0, ni fichier audio ni
son émis).

Deux playlists accessibles ont un nom vide dans les métadonnées Spotify ;
leurs 16 et 27 titres ont été réellement lus. Le serveur conserve le nom vide
et les identifiants/comptages du service. Le client affiche le libellé traduit
« Playlist sans nom », explicitement une absence de nom, au lieu de supprimer
leurs cartes faute de titre et de pochette. Ce cas a son témoin DOM et sa
contre-épreuve : retour au marqueur `—` supprimant la carte, échec nommé
`Une playlist sans nom mais accessible ne doit pas disparaître`, puis
restauration par copie. Ce n'est pas un refus d'accès ajouté artificiellement.

## Titres aimés et grandes playlists (2026-09-15)

La lecture `get_user_tracks` traverse maintenant le proxy, l'opération privée
`UserTracks` et le moteur natif. Elle résout le contexte de collection du compte
appairé, sans Web API, cookie de navigateur ni nouvel appairage. Les pages en
chargement, continuations inconnues, boucles, doublons et erreurs restent des
échecs explicites. Seul le résolveur de collection Spotify est accepté pour les
continuations. L'absence de pages n'est pas assimilée à une collection vide.

Les pistes de playlists sont lues par pages de 100. Longueur, position et
révision sont vérifiées à chaque page ; une modification concurrente impose une
relance. L'ordre et les doublons intentionnels sont conservés. Fichiers locaux
et podcasts restent refusés par cette source musicale.

Les métadonnées des pistes (albums, playlists, recherche et titres aimés) sont
demandées par groupes de 50, au plus quatre groupes simultanés. La réponse est
réordonnée selon les identifiants demandés, sans perdre les doublons de file.
Une piste absente/refusée, une identité contradictoire, un doublon de réponse ou
un échec du fournisseur fait échouer la collection, sans raccourcissement caché.
Les corps protobuf sont bornés à 8 Mio ; la limite IPC de 8 Mio et son délai de
45 secondes restent inchangés.

Limite actuelle : **2 000 pistes par collection**, au-delà refus explicite.
Ce n'est pas une bibliothèque illimitée. Les discographies restent limitées à
300 albums et la rootlist à 300 entrées, dossiers compris. Le contexte des
titres aimés n'expose pas de révision de snapshot comparable à celle des
playlists : la cohérence transactionnelle d'une collection modifiée pendant
sa lecture n'est pas garantie. À la fin de cette unité, albums enregistrés et
artistes suivis restaient non implémentés ; l'étape correspondante est décrite
plus haut. Les écritures Spotify restent non implémentées.

Le client web V2 conserve les titres lisibles quand une autre catégorie de
favoris échoue ; chaque refus reste visible avec une relance. Les réponses
périmées après changement de service sont ignorées. Le cœur Tune conserve son
sens existant : il ne devient pas une écriture de favori sur Spotify.

Contre-épreuve du lot, tests inchangés, compilation réussie : arrêt après la
première page et retrait de la garde de révision, troncature des titres aimés et
des métadonnées à 300, puis retrait du refus de métadonnées par piste. La suite
`cargo test --locked -p tune-core --lib --no-default-features --features
oaat,spotify-native spotify_native` donne cinq échecs attendus (31 autres verts) :

- `native_playlist_reads_every_page_above_300_and_preserves_duplicates` :
  `Every Spotify page must be requested at its exact track offset` ;
- `native_playlist_refuses_changed_incomplete_or_unsupported_pages` :
  `Spotify playlist must reject revision, not return a truncated or mixed queue` ;
- `native_liked_tracks_reads_the_complete_context_beyond_300` :
  `Liked tracks must not be silently truncated` (300 au lieu de 1 512) ;
- `native_metadata_batches_large_collections_without_reordering_or_dropping_duplicates` :
  `Batched metadata must return every collection position` ;
- `native_metadata_refuses_missing_denied_duplicate_or_mismatched_tracks` :
  `Spotify metadata must refuse denied, never serve a shortened or incorrect collection`.

Les fichiers sont restaurés par copie, jamais par retrait des tests. La sonde
macOS de lecture seule a obtenu 1 512 titres aimés et 520 pistes de playlist,
avec toutes leurs métadonnées. Les 30 playlists accessibles et l'entrée refusée
restent distinctes. Aucun son ni écriture Spotify pendant ces vérifications.

La validation HTTP réelle a aussi révélé un écart préexistant : la route
Favoris ne passait pas par le mapping des refus typés et rendait 400 pour les
albums/artistes non implémentés. Elle conserve maintenant le verdict 501 de
`TuneError::Unsupported`, sans modifier le 400 d'un type de favori invalide.
Retirer ce seul branchement fait rougir, après compilation réussie,
`les_favoris_non_implementes_sortent_en_501_sans_devenir_un_compte_vide` :
`Les favoris non implementes doivent sortir en 501, pas en 400 ou en collection vide`.
Le correctif a ensuite été restauré par copie.

Validation finale du 2026-09-15 : 36 tests natifs, 32 tests HTTP et build macOS
ARM verts. Sur le dernier binaire réellement installé, les routes HTTP rendent
1 512 titres aimés (1,69 s) et les 520 pistes de la grande playlist (0,75 s),
avec leurs métadonnées ; albums/artistes non implémentés rendent bien 501.
Ces durées sont un essai ponctuel, pas un benchmark à froid. Le contrat ancien
de liste complète refuse toujours la rootlist partielle, tandis que le rapport
conserve 30 playlists accessibles et une indisponible 403.

Chrome headless dans un profil de test isolé affiche 1 512 cartes et les deux
catégories non implémentées, sans erreur JavaScript. Capture inspectée ; aucun
clic de lecture. L'appairage reste valide et les quatre zones sont arrêtées.

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

## Contre-épreuves exécutées

Le 2026-09-14, le témoin
`streaming::spotify_native::engine::tests::native_registry_save_and_logout_preserve_existing_web_credentials`
est vert avec la clé séparée. Remplacer **uniquement le code** de
`SpotifyNativeService::credential_key` par la clé historique
`auth_tokens_spotify` le fait rougir après compilation réussie :
`native pairing must not overwrite Web API credentials`. Le test est inchangé.
Le témoin utilise désormais le proxy public enregistré par Tune. La clé
séparée est ensuite restaurée avant la validation finale.

Commande de contre-épreuve actuelle, après extraction :

```sh
cargo test -p tune-spotify --locked --lib --features native \
  spotify_native::engine::tests::native_registry_save_and_logout_preserve_existing_web_credentials \
  -- --exact
```

La suite `spotify_native` a aussi été exécutée avec les seuls correctifs
suivants retirés, sans modifier les tests. Chaque compilation a réussi avant
les échecs attendus :

- Retrait de `child.kill().await` :
  `native_worker_drop_kills_and_reaps_a_child_ignoring_stdin` échoue avec
  `Spotify worker survived cancellation; the server must kill and reap it`.
- Remplacement de la durée PCM inconnue par celle du catalogue :
  `native_pcm_never_invents_content_length_from_catalogue_duration` échoue avec
  `Spotify metadata duration is not an exact PCM length; Chrome must receive real EOF`.
- Retrait de l'exception Spotify dans le chemin du signal :
  `spotify_native_decoded_wav_is_not_claimed_lossless_or_bit_perfect` échoue avec
  `Spotify decoded PCM is not a lossless source`.

Commandes : le filtre `spotify_native` sur `tune-core --lib` et sur
`tune-server --lib`, tous deux avec `--locked --no-default-features --features
oaat,spotify-native`. Les sources corrigées sont recopiées depuis le worktree
local. Après une copie conservant les dates anciennes, Cargo peut réutiliser
le binaire saboté : comparer les SHA-256 puis actualiser les dates des quatre
fichiers restaurés avant la relance, sans modifier leur contenu.
