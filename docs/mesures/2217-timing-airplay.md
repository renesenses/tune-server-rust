# #2217 — ports annoncés et réponses timing RAOP

Identité : JP Robbe / OpenAI Codex / jp-robbe-20260917-p1suite2-2217.

Base : `73707a08c1658289913058a9843250622be08521`.
Lot : `batch/jp-p1-audio-transport-20260917`.
Branche : `fix/jp-robbe-20260917-2217-airplay-timing`.
Worktree Shrek : `/srv/builds/worktrees/jp-2217-20260917-suite2`.
Target : `/srv/cache/tune/targets/jp-2217-20260917-suite2`.
Preuves : `/srv/builds/jp-evidence/jp-2217-20260917-suite2`.

## Ce qui restait après les correctifs déjà livrés

Les PR #2629 et #3885 sont déjà intégrées dans la base. Elles conservent
le diagnostic du SETUP 403, rendent par TEARDOWN une session acceptée par
ANNOUNCE puis refusée par SETUP/RECORD, et privilégient le service mDNS RAOP.
Elles ne sont pas refaites.

Le code actuel annonce `control_port=local_port+1` et
`timing_port=local_port+2` mais ne lie que la socket audio. Ces ports peuvent
être libres, appartenir à une autre session, et l'addition dépasse u16
pour les valeurs limites. Aucun répondeur timing ne reçoit ces requêtes.

Cette modification réserve deux ports attribués par le système, annonce
leurs valeurs réelles sans addition et sert le port timing avant SETUP.
La session RTSP possède les sockets ; les refus et Stop arrêtent le
répondeur et attendent sa libération, même si TEARDOWN est refusé.
Drop annule également la tâche si la session ou une future est abandonnée.

## Référence protocolaire

Implémentation de réception primaire consultée :
[Shairport Sync, rtp.c à b69e01c](https://github.com/mikebrady/shairport-sync/blob/b69e01cdf9d4e1577c5fe4bc1738404d27b10159/rtp.c),
fonctions `rtp_timing_sender` et `rtp_timing_receiver`.
Elle émet un paquet 32 octets de type 0xd2 et attend 0xd3 ; les champs
réception/émission sont des secondes et fractions binaires NTP sur 32 bits.
La réponse est envoyée au port source UDP, filtrée sur l'IP du pair RTSP.

La [capture OpenAirPlay](https://openairplay.github.io/airplay-spec/audio/rtp_streams.html#timing-packets)
documente aussi la copie du champ émission de la requête dans le champ
origine de la réponse. Le code recopie la séquence et fournit les instants
de réception et d'émission. Aucun code de ces projets n'est copié.

Le répondeur ignore les datagrammes de longueur, version ou type différents,
ainsi que les autres IP. Le buffer accepte un datagramme UDP complet avant
contrôle de longueur afin qu'un paquet trop long ne termine pas le service
par WSAEMSGSIZE sous Windows.

## Témoins et commandes Shrek

Tous les tests sont enregistrés dans la lib tune-core, module privé
`outputs::airplay::timing_tests_2217`. Ils utilisent de vraies sockets
RTSP TCP et UDP loopback, sans appareil ni compte externe.

- Échange timing avant acquittement SETUP puis après RECORD ; vérification du
  port source, de la séquence, du champ origine, de l'époque NTP et de l'ordre
  réception/émission. Paquets courts/longs, version/type erronés, IP différente
  n'obtiennent aucune réponse. Stop libère les deux ports malgré TEARDOWN 403.
- SETUP 403 : échange timing effectué, diagnostic préservé, TEARDOWN et
  libération des deux ports.
- RECORD 403 : même libération, diagnostic RECORD préservé.
- Drop de la sortie : arrêt effectif de la tâche et libération des ports.
- Requête depuis une IP autre que celle attendue : absence de réponse UDP
  pendant une fenêtre de 100 ms, puis fermeture du service. Aucun alias
  loopback supplémentaire n’est nécessaire pour ce test.

Le fichier média du scénario est `file:///dev/null` : aucune lecture audio
n'est revendiquée. La session de contrôle est la propriété testée.

Environnement pour chaque commande :

```sh
export TUNE_TARGET_KEY=jp-2217-20260917-suite2 CARGO_BUILD_JOBS=6
. /srv/cache/tune/env.sh
cargo test -p tune-core --lib timing_tests_2217 --no-default-features --features oaat
cargo test -p tune-core --lib outputs::airplay:: --no-default-features --features oaat
cargo clippy -p tune-core --lib --tests --no-default-features --features oaat -- -D clippy::correctness
```

Résultats ciblés : **5/5 verts**, 0,10 s (`targeted-tests.log`).
Le premier passage avant la révision de portabilité contenait 4 tests,
également verts (`first-tests.log`) ; ce n'est pas la version finale du banc.

Deux contre-épreuves, avec les cinq tests strictement inchangés et
compilables, utilisent la même commande ciblée :

1. `airplay.rs` rétabli au code de production de base, en conservant les
   déclarations des modules de test et timing pour compiler le témoin IP :
   **4 rouges / 1 vert**, code Cargo 101 (`counter-baseline.log`).
   L'assertion nomme `advertised UDP port ... must belong to this session`
   (`airplay_timing_tests_2217.rs:62`). Le témoin IP appelle directement le
   service et reste vert : cette contre-épreuve concerne son intégration.
2. Après restauration, seule la condition de filtrage reçoit `true ||` :
   les sockets restent réservées mais toute réponse est neutralisée.
   **4 rouges / 1 vert**, code Cargo 101 (`counter-silent.log`).
   L'assertion nomme `announced timing port must answer a valid RAOP query`
   (`airplay_timing_tests_2217.rs:97`), après le délai maximal de 3 s.
   Les ports seuls ne satisfont donc pas le témoin.

Après chaque mutation, restauration par `cp` depuis les sauvegardes
`airplay.fixed.rs` / `airplay_timing.fixed.rs` ; les SHA-256 des deux fichiers
de production et du banc restent identiques (`fixed-source.sha256`,
`restore-baseline.sha256.log`, `restore-silent.sha256.log`).

Suite AirPlay restaurée : **23/23 verts** (5 nouveaux + 18 existants),
0,12 s (`restored-airplay-tests.log`). `cargo fmt --all -- --check`
et `git diff --check` réussissent.

Clippy réussit avec `-D clippy::correctness`, 2 min 58 s (`clippy.log`).
Il signale notamment 517 avertissements sur la lib de test, dont 423
doublons, ainsi que des avertissements sur d'autres cibles déjà présentes.
Aucun diagnostic ne vise les deux nouveaux fichiers Rust. Les cinq
diagnostics d'`airplay.rs` visent des lignes inchangées, comparées à la base
(`clippy-changed-files.log`). Les autres fichiers cités ne sont pas modifiés
par cette PR ; aucun nettoyage transversal n'est inclus.

## Limites

Ce correctif **n'explique pas le 403 initial du BeoPlay** et ne clôt pas
#2217. Le test matériel sur une version récente, notamment les contraintes
d'appairage du récepteur, reste nécessaire.

Le port control est réellement réservé mais les demandes de retransmission
et les paquets de synchronisation RTP/NTP ne sont toujours pas implémentés.
Il s'agit d'un répondeur timing, pas d'une implémentation RAOP complète.
OPTIONS, SDP, authentification, chiffrement et décodage sont inchangés.

Le refus ANNOUNCE est couvert par le témoin historique du dialogue ; le banc
n'observe pas séparément la libération des nouveaux ports dans ce cas,
puisqu'ils ne sont pas encore annoncés.

La durée de vie suit la session RTSP existante : une fin/erreur de décodage
ne ferme déjà pas cette session ; le timing reste alors actif jusqu'à Stop,
au remplacement de la lecture ou à Drop. Aucune tâche n'est détachée de la
durée de vie de la session.

Les tests Linux loopback ne valident ni Windows à l'exécution, ni les
récepteurs physiques, ni IPv6 (absent du loopback Shrek), ni la lecture,
la synchronisation ou la stabilité audio en réseau. La batterie de lot et
la recette matérielle restent distinctes de ces preuves.
