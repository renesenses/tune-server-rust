# #4233 — instructions dans les archives Linux

JP Robbe / OpenAI Codex / jp-robbe-20260916-messages-installation

Base : 3a2b710a151a257e217143b2900eeb53076ea6ef.
Branche : fix/jp-robbe-20260916-4233-archive-instructions.
Lot : batch/jp-messages-installation-20260916.
Worktree Shrek : /srv/builds/worktrees/jp-robbe-20260916-messages-installation.

## Changement

L'étape Package (unix) de release.yml copie packaging/linux/README.txt à
côté du binaire et l'ajoute explicitement au tarball sur Linux. Le README
explique en français et anglais le lancement manuel, la conservation des
données, l'installation systemd via le .deb de la même release et
l'inspection d'une ancienne unité Python avec systemctl cat tune-server.
Les autres distributions sont distinguées des systèmes utilisant apt.

Aucun installateur, unité générique ou mécanisme de migration Python n'est
inventé. Aucun numéro de version n'est figé dans les instructions.

## Preuve exécutée sur Shrek, 2026-09-16

Commande : python3 scripts/test-archive-instructions.py

Le test extrait le corps réel de Package (unix) du workflow, substitue les
trois expressions de matrice et l'exécute dans un répertoire temporaire
avec de petits fichiers de substitution. Il ouvre ensuite le tar.gz
PRODUIT et compare tous les fichiers octet par octet.

**Deux tests, dix cas d'archive réussis** :
- Linux x86_64 GNU, aarch64 GNU et aarch64 musl, avec et sans les binaires
  facultatifs airplay-daemon/ffmpeg : six archives contiennent le README exact ;
- macOS x86_64 et aarch64, avec et sans ces binaires : quatre archives gardent
  leur contenu précédent, sans instructions Linux.

Le binaire, le web, le greffon Party et, quand présents, le démon, FFmpeg et
sa licence sont conservés dans chaque comparaison.
Le YAML est également relu par PyYAML sur Shrek ; git diff --check réussit.

## Contre-épreuve

Sauvegarde du workflow par cp. Retrait du bloc copiant le README et de son
argument dans tar, en conservant le script de test intact (SHA-256 vérifié).
Même commande : **les six cas Linux échouent**, avec le message
« Linux tarball omits installation instructions (#4233) ».
L'emballage réussit : c'est le contenu de l'archive qui échoue au contrôle.
Les quatre cas macOS restent verts.

Restauration du workflow par cp, relance : **dix cas verts**.
Journaux dans /srv/builds/jp-evidence/jp-robbe-20260916-messages-installation/.

## Limites

Ce contrôle prouve l'inclusion et la conservation des fichiers par l'étape
d'emballage, avec des fixtures. Il ne compile ni n'exécute les binaires
Linux/macOS, ne signe pas et ne produit aucune release publiée.
Le script est rejouable explicitement ; il n'est pas ajouté à la batterie CI.
Les checks GitHub sont suivis séparément avec ci:full pour ce changement de
release. Aucune installation apt ni opération systemd n'a été effectuée.

Le support systemd générique hors Debian reste une décision produit ouverte.
La réponse au forum et la vérification d'une éventuelle ancienne unité chez
le testeur ne sont pas effectuées. Refs #4233, sans clôture globale du ticket.
Verrou conservé pendant la revue ; pas de merge, tag, bump ou déploiement.
