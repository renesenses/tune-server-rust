# #1897 — contrats de bibliothèque sur des objets persistés

Intervention : **JP Robbe / OpenAI Codex / jp-robbe-20260916-1897-library**.
Base serveur : **3a2b710a151a257e217143b2900eeb53076ea6ef**.
Lot : **batch/jp-library-contracts-20260916**.
Branche : **test/jp-robbe-20260916-1897-library**.

## Ce que cette vague prouve

Le banc appelle six réponses du vrai routeur Axum, puis charge leurs exigences
depuis la carte commitée, sans recopier une seconde liste de champs dans les
tests. Deux artistes, trois albums et trois pistes sont créés dans une base
SQLite isolée par test. Les listes doivent être non vides et chaque élément
doit satisfaire la carte.

| Route GET | Objet vérifié |
| --- | --- |
| /library/artists/{id} | Artiste demandé, identité et nom persistés |
| /library/albums/{id} | Album demandé, identité et titre persistés |
| /library/tracks/{id} | Piste demandée, identité et titre persistés |
| /library/artists/{id}/albums | Les deux albums de cet artiste, sans celui de l'autre |
| /library/artists/{id}/tracks | Ses deux pistes, avec leur titre propre |
| /library/albums/recent?limit=2 | Deux albums distincts, chaque titre confronté à sa fiche |

Les trois nouveaux témoins sont dans
[tune-server/tests/web_contracts/library_1897.rs](../../tune-server/tests/web_contracts/library_1897.rs).
Le module parent est déjà enregistré comme cible autonome et dans
l'agrégateur server_contracts ; aucun manifeste Cargo ne change.

Les six contrats (type, forme liste/objet, champs obligatoires) ont aussi été
comparés à api.ts et types.ts du client web au SHA
**b62a8ea4ad18d05d0e6925152b4b88683f0b02fd** : ils sont identiques à ceux de
la carte serveur, produite depuis **4d812e8c491c40beadf4cd3f9ae1c7e015caa50e**.
Cela ne rafraîchit ni ne valide le reste de cette carte.

## Exécution sur Shrek

Worktree : /srv/builds/worktrees/jp-robbe-20260916-1897-library.
Preuves : /srv/builds/jp-evidence/jp-robbe-20260916-1897-library.

Environnement chargé depuis /srv/cache/tune/env.sh avec la clé dédiée
jp-robbe-20260916-1897-library. Six jobs, priorité nice 15. Au lancement :
charge 0,11, 240 Gio de mémoire disponibles, environ 596 Gio de disque libres.

    cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test web_response_contracts i1897_

Résultat initial : **3 réussis**, 13 filtrés, 0,51 s (initial-green.log).

Après les deux contre-épreuves et restauration de la production :

    cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test web_response_contracts

Résultat : **16 réussis**, aucun échec ni test ignoré, 2,10 s
(restored-all.log). Format et git diff --check réussis.
Des avertissements préexistants restent présents. Le premier essai de compilation
a corrigé deux types erronés dans la fixture ; cet échec de compilation n'est
pas une contre-épreuve.

Vérification du raccordement à l'agrégateur :

    cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test server_contracts i1897_

Résultat : **3 réussis**, 715 filtrés, 0,49 s (aggregator.log).
Cette commande ne revendique pas l'exécution des 715 tests filtrés.

## Deux contre-épreuves comportementales

Script conservé : counterproofs.sh. Chaque altération porte sur la production,
compile, puis fait échouer le témoin attendu. Les fichiers de test conservent
leur SHA-256 ; les deux sources sont restaurées par copie de sauvegarde, et
leur diff final est vide.

1. Retirer title de la réponse de get_album, dans routes/library/albums.rs.
   Même commande ciblée avec le filtre i1897_fiches :
   **0 réussi / 1 échec**, témoin
   i1897_fiches_bibliotheque_respectent_la_carte_et_l_identite_demandee.
   Message : **GET /library/albums/{} -> Album: champ obligatoire absent: title**.
   Journal : counter-field.log.
2. Faire rendre un tableau vide par artist_albums, dans routes/library/artists.rs.
   Même commande avec le filtre i1897_listes :
   **0 réussi / 1 échec**, témoin
   i1897_listes_artiste_respectent_la_carte_sur_chaque_element_persiste.
   Message : **GET /library/artists/{}/albums -> Album: tableau vide, impossible
   de prouver les champs de l'element**.
   Journal : counter-empty.log.

tests-before.sha256 et tests-restored.log attestent les empreintes avant/après.
Le retour vert des 16 tests a lieu après restauration.

## Limites et suites constatées

- Aucun comportement de production n'est changé et aucune panne utilisateur
  n'est déclarée corrigée par cette vague. Le chantier #1897 reste ouvert.
- Les fixtures exécutent SQLite ; une compilation avec la fonctionnalité
  PostgreSQL ne constitue pas une exécution de ces fixtures sur PostgreSQL.
- Pas de matériel, de service musical externe, ni d'acceptation dans le navigateur.
  Les tests n'exigent pas tous les champs optionnels du client.
- La forme GET /library/albums reste déclarée Album[] par getAlbums dans le
  client épinglé, alors que la route serveur rend une page {items,total,limit,offset}.
  getAllAlbumsSeeded lit séparément cette enveloppe via un appel any. Ce cas
  nécessite de vérifier les consommateurs avant de modifier le contrat.
- Le contrat des pistes d'album manque à la carte : le gabarit TypeScript
  comportant le suffixe conditionnel de requête de getAlbumTracks est tronqué
  au gabarit imbriqué. L'extracteur le signale dans non_resolus :
  **interpolation non fermée — route non fiable**. Ce cas n'est pas présenté
  comme couvert par la présente vague.
- Pas de migration, bump, fusion, tag ou déploiement. La PR vise son propre
  lot. La modification DR des PR #4312/#4316 se situe dans un autre témoin
  du fichier parent ; le nouveau module n'en modifie aucune ligne.
