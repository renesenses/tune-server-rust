# Spotify natif — prototype non officiel (#4166)

Arbitrage JP : essayer sans application développeur ni Client ID à saisir.
Base : `main` 24123a4e (v0.9.150). Aucun changement de version, aucun déploiement
automatique. Le service existant reste celui des compilations ordinaires.

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
- Collections limitées à 300 objets. Une playlist partielle est refusée,
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
  Titres aimés, albums enregistrés, artistes suivis, écriture et synchronisation
  de playlists restent non implémentés. Les titres aimés rendent désormais un
  refus explicite, au lieu de la liste vide héritée du trait.
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

Commandes Rust : `cargo test -p tune-core --locked --lib --no-default-features
--features oaat,spotify-native spotify_native` et `cargo test -p
tune-streaming-http --locked --features tune-core/spotify-native,tune-core/oaat`.
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

Commande de contre-épreuve :

```sh
cargo test -p tune-core --locked --lib --no-default-features \
  --features oaat,spotify-native \
  streaming::spotify_native::engine::tests::native_registry_save_and_logout_preserve_existing_web_credentials \
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
