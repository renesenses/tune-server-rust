# #4298 — préserver la file lors d'une relance locale

JP Robbe / OpenAI Codex / jp-robbe-20260917-p1-audit-1609.

Base : `batch/p1-20260917`, `792b1aaeb671ee3a3036f7e25a139d539e6054af`.
Branche : `fix/jp-robbe-20260917-4298-local-queue`.
Développement et validations : Shrek, worktree/target `jp-4298-20260917-1609`,
six jobs, environnement `/srv/cache/tune/env.sh`.

## Défaut et contrat

La route `POST /zones/{id}/play` calculait déjà `demande_nue` pour la reprise
de position, mais remplaçait systématiquement la file par la liste résolue.
Pour `{"track_id": 2}` cette liste ne contient qu'un titre. Le garde-fou
existant de #2569 ne couvre que la branche streaming.

La demande nue cherche désormais le `track_id` local dans la file unifiée :

- piste présente : conserver les lignes, leur identité et leur ordre ;
- occurrences répétées : conserver l'occurrence courante si elle correspond,
  sinon choisir la première occurrence ;
- conserver la position dans la file entière, distincte de l'index zéro
  dans la demande d'un titre ; déplacer le marqueur courant de façon
  transactionnelle, y compris si l'entrée précédente était de streaming ;
- lecture de base en erreur : erreur HTTP 500 nommée, aucune suppression ;
- piste absente ou contenant explicite (album, playlist, track_ids,
  start_index) : conserver le comportement de remplacement.

Le test de file mixte inclut volontairement une entrée de service dont le
`source_id` vaut `"2"` : elle ne doit pas être confondue avec la piste locale 2.

## Tests exécutés sur Shrek

Préfixe commun :

```sh
export TUNE_TARGET_KEY=jp-4298-20260917-1609 CARGO_BUILD_JOBS=6
. /srv/cache/tune/env.sh
```

```sh
cargo test -p tune-server --no-default-features --features oaat,cloud-relay,bandcamp \
  --test server_contracts --test playback_panne_de_base_4261 relance_locale -- --nocapture
cargo test -p tune-server --no-default-features --features oaat,cloud-relay,bandcamp \
  --test playback_panne_de_base_4261
cargo test -p tune-server --no-default-features --features oaat,cloud-relay,bandcamp \
  --test server_contracts reprise_position_au_demarrage
cargo test -p tune-server --no-default-features --features oaat,cloud-relay,bandcamp \
  --lib routes::playback::
cargo fmt --all -- --check
cargo clippy -p tune-server --lib --test server_contracts \
  --test playback_panne_de_base_4261 --no-default-features \
  --features oaat,cloud-relay,bandcamp -- -D clippy::correctness
```

Résultats : 6 nouveaux tests verts ; 13 tests du binaire de panne de base
(dont un des six nouveaux), 5 de reprise au démarrage, 95 unitaires playback.
**118 tests distincts réussis**, sans compter les relances.
Les cinq témoins HTTP du nouveau module sont inscrits dans `server_contracts` ;
le sixième est ajouté au binaire existant `playback_panne_de_base_4261`.
`autotests = false` ne peut donc pas les laisser hors compilation.

Formatage : réussi. Clippy : réussi avec -D clippy::correctness ; avertissements non bloquants consignés dans clippy.log.

## Contre-épreuve réellement exécutée

Sauvegarde de `playback.rs`, puis remplacement uniquement de la condition de
production `if demande_nue` par `if demande_nue && false` dans
`file_conservee`. Aucun test modifié.

La commande filtrée ci-dessus compile, puis le premier binaire échoue :
`relance_locale_file_illisible_4298_ne_l_efface_pas` reçoit 409 au lieu de
l'erreur de base 500 attendue. Cargo s'arrête au premier binaire rouge.
Le binaire `server_contracts` compilé dans cette même passe est conservé
sous `server-contracts-counter` et exécuté directement :

```sh
/srv/builds/jp-evidence/jp-4298-20260917-1609/server-contracts-counter \
  relance_locale_file_4298 --nocapture
```

Résultat : **3 rouges comportementaux, 2 verts** :

- `relance_locale_garde_les_lignes_et_le_rang` :
  « la relance a remplacé l'album par un seul titre » ;
- `relance_locale_choisit_l_occurrence_courante_du_doublon` :
  « les doublons intentionnels ne doivent pas disparaître » ;
- `relance_locale_preserve_une_file_mixte_et_deplace_le_courant` :
  « la file mixte a été effacée » ;
- piste absente et contenant explicite restent verts.

Le témoin principal attend les trois lignes originales et observe seulement
`[(4, Some(2), 0)]` après sabotage : le défaut de #4298 est reproduit.

Restauration par `cp`, vérification SHA-256 du fichier de production ET des
deux fichiers de tests, puis relance Cargo : **6/6 verts**. Le rouge initial
de compilation de la fixture (type de `track_number`) a été corrigé avant
ces essais ; il n'est pas compté comme contre-épreuve.

## Preuves et limites

Journaux, copie de production restaurée, empreintes et binaire de
contre-épreuve : `/srv/builds/jp-evidence/jp-4298-20260917-1609`.

La base est SQLite réelle et les pistes sont des WAV temporaires. La sortie
audio est factice ; aucun DAC, compte musical, service réseau ou écoute
matérielle n'est testé. PostgreSQL n'a pas été exécuté sur Shrek pour ce
correctif ; aucune requête SQL du dépôt n'est modifiée.

Le journal terrain ne prouve toujours pas le geste exact du testeur. Le
contrat choisi rejoint celui des pistes de service : une demande nue visant
un titre déjà chargé conserve la file ; un remplacement volontaire peut
être exprimé par `track_ids` ou un contenant explicite.

Les coupures ALSA (#3318/#4295), la reprise Qobuz (#4220), la concurrence
générale entre modifications de file et lectures, et l'acceptation sur la
machine du testeur restent hors de cette correction. Aucun bump, merge,
tag ni déploiement effectué. La CI de PR et la batterie RC restent séparées
des validations locales ci-dessus.
