# #4420 — exposer les refus d'écriture des métadonnées étendues

JP Robbe / OpenAI Codex / jp-robbe-20260918-suite4-3816.

Related #3816, clôturée pour le rattrapage déjà livré. #4420 suit le défaut
distinct du bilan découvert pendant cette unité. L'identité, les chemins,
le nom du module de tests et la branche conservent 3816, l'unité ayant
commencé avant cette séparation.

## Défaut et changement

La relecture de métadonnées remplit déjà le magasin étendu depuis #3886.
Lorsqu'un lot d'upserts échoue, elle journalisait le refus puis rendait un bilan
qui ne le mentionnait pas. La ligne tracks pouvait être mise à jour alors que
les champs lus par l'éditeur restaient absents.

Le résultat de GET /api/v1/library/rescan-metadata/status et l'événement
library.rescan_metadata.completed partagent maintenant le même objet, enrichi de :

- extended_metadata_failed_batches : nombre de lots dont l'écriture a renvoyé une erreur ;
- has_errors : errors > 0 OU extended_metadata_failed_batches > 0.

Le journal rescan_metadata_complete expose les mêmes compteurs. errors reste
le nombre d'erreurs de pistes déjà comptées par la passe ; updated garde son
sens de mises à jour de la ligne tracks. La progression ne change pas.

Un lot n'est pas une transaction : set_batch_multi effectue des upserts
successifs et s'arrête au premier refus. Le compteur vaut donc un par lot
refusé, jamais le nombre de champs ou de pistes supposés perdus. Les écritures
déjà faites sont conservées. Pas de nouvelle tentative ni d'atomicité ajoutée.
Un succès sans balises étendues ne crée aucun refus artificiel.

## Validation sur Shrek

Base : 73707a08c1658289913058a9843250622be08521.
Branche : fix/jp-robbe-20260918-3816-rescan-errors.
Lot : batch/jp-p2-metadata-20260918.
Worktree et clé target : jp-3816-20260918-suite4.
Six jobs Cargo, deux threads de tests, environnement /srv/cache/tune/env.sh.

Les quatre témoins passent par le vrai routeur HTTP et une base SQLite isolée.
Un FLAC du dépôt est copié et étiqueté ; les 501 chemins du cas volumétrique
sont des liens physiques vers cette seule copie.

| Cas | Propriété attendue |
| --- | --- |
| Succès, un fichier | Champs présents, zéro refus de lot, has_errors=false |
| Refus SQL sur 501 pistes | Deux lots refusés : 500 puis reliquat ; errors=0, updated=501, has_errors=true |
| Refus après le premier champ écrit | Un refus de lot, champ déjà écrit conservé indépendamment de l'ordre HashMap |
| Refus SQL de mise à jour de la piste | errors=1, aucun refus de lot étendu, has_errors=true |

Chaque témoin compare le bilan reçu par événement au résultat relu par HTTP.
Les triggers de test produisent de vrais refus SQLite, sans mock de la route.

Résultats réels :

- Les quatre nouveaux témoins passent (6,92 s d'exécution).
- Contre-épreuve : seul le retour du bras Err du helper de lot passe de 1 à 0.
  Même commande et mêmes tests : compilation réussie, deux assertions rouges
  (0 au lieu de 1 pour le refus partiel, 0 au lieu de 2 pour 500 + reliquat),
  deux contrôles verts.
- Restauration par copie du fichier sauvegardé ; SHA-256 de la production et
  des tests identiques à ceux d'avant contre-épreuve.
- Suite finale du module tracks : 40 tests verts en 9,79 s, dont les quatre
  nouveaux et le témoin existant de rattrapage #3816. Pas de double comptage.
- Formatage vérifié ; Clippy passe avec -D clippy::correctness (2 min 30 s).
  Les avertissements non bloquants sont conservés dans le journal.

Commandes exactes après export de TUNE_TARGET_KEY=jp-3816-20260918-suite4,
CARGO_BUILD_JOBS=6 et chargement de /srv/cache/tune/env.sh :

    cargo test -p tune-server --lib rescan_metadata_errors_3816 --no-default-features --features oaat -- --test-threads=2
    # Même commande pendant la contre-épreuve.
    cargo test -p tune-server --lib routes::library::tracks:: --no-default-features --features oaat -- --test-threads=2
    cargo fmt --all --check
    cargo clippy -p tune-server --lib --no-default-features --features oaat -- -D clippy::correctness

Le premier essai comportait une fixture inadéquate pour l'erreur de piste :
un faux FLAC bénéficie du repli de read_metadata sur le nom de fichier.
Trois tests passaient, ce quatrième échouait. Il a été remplacé AVANT la
contre-épreuve par un vrai FLAC et un trigger refusant UPDATE tracks.
Les tests sont restés inchangés entre le vert corrigé, la contre-épreuve
et le vert final.

Preuves : /srv/builds/jp-evidence/jp-3816-20260918-suite4/ ;
tests-fixture-corrected.log, counterproof.log, restoration-hashes.log,
tests-restored-tracks.log, fmt.log, clippy.log. Relecture indépendante du
diff et des preuves par un second agent, sans défaut bloquant identifié.

## Limites

Ce changement rend les refus comptés observables ; il ne répare pas les
écritures refusées et ne démontre pas leur présence chez Pierre M.
La relecture doit toujours être déclenchée pour rattraper une bibliothèque
ancienne. Cette validation ne constitue pas une recette terrain de l'ancien signalement #3816.

has_errors agrège les deux catégories comptées. Il ne certifie pas toutes les
opérations de maintenance : échecs de rafraîchissement des albums, persistance
des réglages et erreurs silencieuses de read_extended_metadata restent hors
périmètre. La requête de liste initiale et un panic de la tâche ne sont pas
couverts par ce correctif.

Le client web main 098837be définit rescanMetadataStatus sans appelant dans src
et n'écoute pas cet événement ; aucune réparation de toast ou de formulaire
n'est annoncée. Aucun compte réel, appareil, appel MusicBrainz ou Qobuz utilisé.
Aucune validation PostgreSQL ni mesure de performances d'une bibliothèque réelle.

