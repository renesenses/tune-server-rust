# #1897 — extraire les URL à gabarits imbriqués

JP Robbe / OpenAI Codex / jp-robbe-20260916-1897-library

## Périmètre et reproduction

Base serveur : `3a2b710a151a257e217143b2900eeb53076ea6ef`.
Branche : `fix/jp-robbe-20260916-1897-map`.
Lot : `batch/jp-library-contracts-20260916`.
Client épinglé, inchangé : `4d812e8c491c40beadf4cd3f9ae1c7e015caa50e`.

Le client appelle `getAlbumTracks` avec un gabarit imbriqué dans le suffixe
de requête. L'extracteur s'arrêtait au premier accent grave intérieur et
classait une route valide parmi les non-résolutions. Le script de la base a
reproduit la carte commitée octet pour octet avant modification.

Le lecteur lexical trouve désormais la fin réelle de la chaîne en tenant
compte des interpolations, chaînes, commentaires et délimiteurs échappés.
Il retire les suffixes conditionnels dont les deux branches sont
manifestement une requête ou une chaîne vide. Il n'évalue aucun JavaScript.

La carte régénérée au même SHA passe de **260 routes / 144 non-résolutions**
à **261 / 143** :

- `GET /library/albums/{}/tracks` retrouve son contrat `Track[]` ;
- `GET /radios{}` devient `GET /radios` ; le test de réponse persistée utilise
  la nouvelle clé, et le test du lecteur de l'ancienne notation reste présent ;
- `/home/recently-added` retrouve son URL, mais demeure non résolue à cause du
  type `any[]`. Aucun contrat de réponse n'est inventé.

Les suffixes de variables opaques gardent la notation historique `{}`.
Les expressions régulières et divisions dans une interpolation, les URL
tronquées, les suffixes conditionnels pouvant changer le chemin et les
gabarits excessivement imbriqués restent signalés, sans contrat supposé.

## Preuves sur Shrek

Worktree : `/srv/builds/worktrees/jp-robbe-20260916-1897-map`.
Target propre : `TUNE_TARGET_KEY=jp-robbe-20260916-1897-map`.
Environnement : `/srv/cache/tune/env.sh`, Cargo à 6 jobs.

- `python3 scripts/web-contract-map.py --self-test` : 17 gardes existantes
  et 14 nouveaux tests réussis.
- `python3 scripts/verifier-carte-web.py --self-test` : 14 gardes réussies.
- Régénération au SHA client épinglé : comparaison exacte avec
  `docs/contrat-web.json` réussie.
- `python3 scripts/verifier-carte-web.py --web <client-épinglé> --exiger-complet` :
  réussi. Ce contrôle de fraîcheur ne prétend pas résoudre les 143 exclusions.
- `cargo test --locked -j6 -p tune-server --no-default-features --features oaat
  --test web_response_contracts` : **14 réussis** après restauration.
  La première suite complète a correctement signalé l'ancienne clé des radios ;
  la clé a été mise en cohérence avec la carte avant la relance verte.

- `cargo test --locked -j6 -p tune-server --no-default-features --features oaat
  --test server_contracts i1897_pistes_album` : **1 réussi, 715 filtrés**.
- `cargo fmt --all -- --check` et `git diff --check` : réussis.

Le nouveau test Rust monte le vrai routeur Axum et une base SQLite en mémoire,
avec deux albums et quatre pistes. Il vérifie le contrat, l'ordre, l'appartenance
à l'album et la sélection FLAC, avec et sans requête de filtre.

### Contre-épreuves, tests inchangés

1. Retour temporaire à la fin d'URL donnée par l'expression régulière dans
   `url_complete`, en conservant le lancement des nouveaux tests :
   `--self-test` échoue, **5 des 14 nouveaux tests rouges**, dont
   `test_url_pistes_album_du_client_epingle` avec `URL valide perdue`.
   Restauration par `cp`, puis 17 + 14 gardes vertes.
2. Retrait temporaire de `title` dans la réponse de production de
   `album_tracks`. La compilation réussit ; le test
   `i1897_pistes_album_extraites_du_client_passent_par_le_routeur` échoue :
   `GET /library/albums/{}/tracks -> Track, element 0: champ obligatoire absent: title`.
   Restauration du fichier par `cp`, puis suite complète verte.

Les SHA-256 des deux fichiers de nouveaux tests restent identiques avant et
après les contre-épreuves. Aucun changement de production Rust ne subsiste.
Journaux et sauvegardes :
`/srv/builds/jp-evidence/jp-robbe-20260916-1897-map`.

## CI, dépendances et limites

Le workflow séparé `web-contract-extractor.yml` exécute les gardes Python,
récupère le client au SHA de la carte, régénère la carte et impose une
comparaison exacte ainsi que le contrôle de fraîcheur. Il ne modifie pas
`ci.yml`, déjà touché par d'autres lots. La PR porte `ci:full`.

Le module Rust est inclus par `web_response_contracts`, cible explicitement
enregistrée dans Cargo, elle-même incluse par `server_contracts`.

Cette PR ne dépend pas de #4322 : les deux partent de la même base et ciblent
le même lot. #4322 ajoute ses tests en tête du fichier ; cette PR ajoute son
module en fin. Le changement DR des lots actifs est dans une autre section.

Ce travail ne ferme pas #1897 : il ajoute un contrat récupéré et sa preuve
d'exécution, sans résoudre tous les types du client ni les autres parcours.
Les essais locaux portent sur SQLite et le profil `oaat` ; ils ne constituent
pas une recette navigateur, matérielle, ou une preuve de publication.
Le verrou #1897 est conservé pendant la revue.
