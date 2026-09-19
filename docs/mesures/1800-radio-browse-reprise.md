# #1800 — récupérer le second commit omis de #3796

JP Robbe / OpenAI Codex / jp-robbe-20260917-p1suite3-1800.

Base : `73707a08c1658289913058a9843250622be08521`.
Lot : `batch/didl-entites-20260917`.
Branche : `fix/jp-robbe-20260917-1800-radio-browse`.
Worktree : `/srv/builds/worktrees/jp-1800-20260917-suite3`.
Target : `/srv/cache/tune/targets/jp-1800-20260917-suite3`.
Preuves : `/srv/builds/jp-evidence/jp-1800-20260917-suite3`.

## Provenance et défaut de livraison

Le correctif principal de #1800 (`5d123568`), puis les améliorations DIDL
#3771/#3796, sont déjà dans main. Cette PR ne refait ni le relais audio
ni le transcodage ni ces corrections de conformité.

La PR [#3796](https://github.com/renesenses/tune-server-rust/pull/3796)
décrit un second commit mettant en œuvre l'arbitrage de Bertrand :
`musicTrack` pour Browse, `audioBroadcast` conservé pour Search. Il n'a pas
été intégré :

- fusion de #3796 le 09/09/2026 à **17:38:00Z**, merge `529995c2` dont le
  second parent est uniquement `409eb4e8` ;
- création du second commit `e9f4395e5b3cc71a18f0d4012d672348d2ddca8c`
  à **17:42:45Z**, soit 4 min 45 après la fusion ;
- [validation du second commit](https://github.com/renesenses/tune-server-rust/pull/3796#issuecomment-5606215550)
  postée à **17:43:09Z** sur la PR déjà fusionnée ;
- ce commit demeure seul en avance sur main dans l'ancienne branche
  `fix/1800-2103-2183-conteneurs-didl-restricted-et-descriptif` ;
- la liste des commits effectivement fusionnés par #3796 ne contient pas
  `e9f4395e`, et main publie encore `RADIO_UPNP_CLASS = audioBroadcast`.

Une recherche de contenu et d'historique sur toutes les refs actualisées
n'a trouvé aucun équivalent de `RADIO_CLASSE_BROWSE` dans les lots. Il s'agit
d'une récupération du travail de **renesenses <renesenses@gmail.com>**,
auteur du commit original, avec adaptation des signatures actuelles.
La provenance du code et de ses trois témoins est conservée.

## Portée de la reprise

`BrowseDirectChildren` et `BrowseMetadata` décrivent les stations en
`musicTrack`. `Search` conserve la classe `audioBroadcast` et le critère
qui trouve les stations. Les six autres rayons gardent leur classe.

La reprise conserve les paramètres et le traitement des pochettes du code
actuel. Dans `outputs/didl.rs`, seuls les commentaires expliquant la classe
choisie sont mis à jour. Aucun changement de `network.rs`, du parcours
d'indexation ou des PR actives #4388/#4389.

L'incohérence déjà acceptée et documentée par #3796 demeure explicite :
une recherche `musicTrack` ne retourne que les pistes, alors que Browse
décrit désormais aussi les stations dans cette classe.

## Témoins repris et validation Shrek

Les trois témoins originaux mesurent les réponses SOAP/DIDL des fonctions
de production, avec des bases SQLite en mémoire :

1. `un_search_sur_le_dossier_radio_le_trouve_encore` : Search trouve
   effectivement une station et répond dans la classe demandée.
2. `le_dossier_radio_publie_musictrack_quand_on_le_parcourt` :
   les deux formes de Browse publient la même classe ; attribut restricted,
   URL audio Tune et pochette relayée restent présents.
3. `le_changement_de_classe_ne_traverse_aucun_autre_rayon` :
   les sept rayons conservent les classes attendues.

Ce sont des tests de la lib tune-core, réellement enregistrés, sans ajout
d'une cible Cargo. Ils observent les réponses produites ; ils ne lisent pas
le texte du code. Aucun réseau multicast ni appareil physique n'est utilisé. Les nouveaux
scénarios emploient Filter=* et ne prouvent pas la projection des autres
filtres. Ils ne lancent pas une lecture HTTP du relais audio.

Premier passage à 2 jobs ; contre-épreuve et vérifications finales à 1 job
pour partager Shrek sous pression IO. Les tests restent à 2 threads.

Commandes :

```sh
export TUNE_TARGET_KEY=jp-1800-20260917-suite3 CARGO_BUILD_JOBS=1
. /srv/cache/tune/env.sh
cargo test -p tune-core --lib upnp_server:: \
  --no-default-features --features oaat -- --test-threads=2
cargo fmt --all -- --check
cargo clippy -p tune-core --lib \
  --no-default-features --features oaat -- -D clippy::correctness
```

Premier passage : **96/96 tests UPnP verts** (`first-upnp-tests.log`),
dont les trois témoins récupérés. Leur corps et celui des deux helpers sont
identiques au commit original ; comparaison exacte et SHA-256 dans
`original-witnesses.sha256.log`.

Contre-épreuve fidèle au comportement initial : seule la constante de
production `RADIO_CLASSE_BROWSE` redevient `audioBroadcast`. Search et les
tests restent strictement inchangés. Même commande de test à 1 job :
**94 verts / 2 rouges d'assertion**, compilation réussie, sortie Cargo 101
(`counter-browse.log`, `counter-command.txt`).

- `le_dossier_radio_publie_musictrack_quand_on_le_parcourt` :
  « le parcours du rayon Radio ne publie pas object.item.audioItem.musicTrack »
  (`upnp_server.rs:6649`).
- `le_changement_de_classe_ne_traverse_aucun_autre_rayon` :
  « le rayon radios ne publie plus object.item.audioItem.musicTrack »
  (`upnp_server.rs:5087`).
- `un_search_sur_le_dossier_radio_le_trouve_encore` reste **vert** :
  le témoin différencie le défaut Browse du Search préservé.

Restauration avec `cp` depuis `upnp_server.fixed.rs` ; SHA-256 des deux
fichiers Rust validés (`fixed-source.sha256` et
`restored-source.sha256.log`).

Dernier passage restauré : **96/96 verts**, 7,70 s
(`restored-upnp-tests.log`). Format global et `git diff --check` réussissent.

Clippy lib réussit avec `-D clippy::correctness` en **4 min 51 s** :
434 avertissements sur tune-core, aucun sur une ligne ajoutée ou modifiée.
Les quatre diagnostics visant les deux fichiers concernés sont comparés
ligne à ligne à la base (`clippy-changed-lines.log`) ; tous portent sur du
code inchangé. Aucun nettoyage transversal n'est inclus.

## Ce que cette PR ne résout pas

Elle récupère l'arbitrage documenté ; elle ne démontre toujours pas que
`audioBroadcast` explique le dossier vide du Marantz ND8006. Une réponse
SOAP conforme au contrat testé n'est pas une validation de l'appareil.

Les métadonnées dynamiques de la radio, les essais matériels et les retours
de terrain restent à traiter séparément. Cette PR ne ferme ni #1800 ni
#2103, ne change pas leurs exigences de recette et ne sollicite aucun
testeur. Aucun bump, merge, tag ou déploiement.
