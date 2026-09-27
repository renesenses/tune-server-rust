# Pochettes des albums FLAC + CUE (#5222)

Bertrand / OpenAI Codex / support-20260927-5222, 27 septembre 2026.
Base : `b06eb4db72700655a13051f998ea211bcbad7f52`, lot
`batch/recherche-chemins-20260926` (scan serveur déjà porté par ce lot).
Prochaine RC à assigner ; aucun bump de version ni déploiement.

## Défaut

Le parcours CUE écrit les tranches, puis écarte leur fichier image de l'import
ordinaire. Celui-ci ne peut donc plus appliquer la priorité de pochette à
ces albums. Le rattrapage des pochettes ne traite que les albums sans image ;
le suivi des fichiers sources saute un `cover.jpg` inchangé. Une jaquette
intégrée peut ainsi rester absente, ou ne jamais remplacer l'image du dossier.

## Correction

Le bilan CUE retient les identifiants des albums dont il a effectivement écrit
des pistes. Une fois les tranches visibles en base, le scan manuel, le scan
de démarrage et le surveillant appellent la règle commune de réévaluation
pour ces seuls albums. Le fichier de l'image se retrouve par `cue_media_path`.
Les supports partagés par plusieurs tranches sont déjà dédupliqués par la
règle commune, et chaque album n'est réévalué qu'une fois par bilan.

La priorité jaquette intégrée > image de dossier et la protection des
pochettes téléversées restent celles du reste de la bibliothèque. Seule
l'analyse complète reçoit le mode forcé ; les passes rapides conservent
leur politique prudente pour les images de fournisseur ou de source inconnue.

## Banc

`tune-server/src/pochettes_cue_tests_5222.rs` crée un véritable FLAC avec deux
tranches CUE et une base SQLite **sur disque**. Les deux images portent des
octets distincts, comparés par leur hash de contenu et leur provenance SQL.
Les octets JPEG sont des marqueurs de métadonnées, pas un test de rendu.

Les entrées sont celles de production : événements notify et attente de
stabilité du surveillant ; `spawn_library_scan` rapide, ciblé ou complet ;
`spawn_auto_scan` pour le démarrage. Aucun dépôt SQL ni extracteur d'image
n'est remplacé par un simulacre.

Le cas initial présente la jaquette dès le premier lot, sans événement de
modification de `cover.jpg`. Le cas de reprise fait d'abord poser `cover.jpg`
par le surveillant sur un FLAC sans jaquette, puis ajoute celle-ci : chaque
passe doit la préférer. Les étapes suivantes remplacent la jaquette puis la
retirent, et vérifient le repli vers l'image du dossier. Un témoin distinct
vérifie qu'une pochette téléversée reste protégée.

Commande sur Shrek, worktree `/srv/builds/bertrand/worktrees/codex-5222-20260927`,
cible Cargo `bertrand-codex-5222-20260927`, six jobs :

```sh
cargo test -p tune-server --lib --no-default-features --features oaat 5222 \
  -- --nocapture --test-threads=1
```

## Reproduction exécutée

Sur la base non corrigée, la commande compile puis échoue sur six témoins :
`cue_import_surveillant_pose_la_jaquette_5222` rend `cover_path = NULL` ;
les cinq tests `cue_*_reprend_cover_5222` gardent le hash de `cover.jpg`,
y compris en analyse complète. Le témoin de protection de la pochette
téléversée passe (1 réussite, 6 échecs, 62,74 s).

Messages : « #5222 : le nouvel album CUE reste sans jaquette intégrée » et
« #5222 : [Passe] garde cover.jpg malgré la jaquette CUE ».
Journal : `/srv/builds/bertrand/codex-5222-rouge.log`.

## Limites

Le calendrier exact de copie des fichiers et les fichiers de Didier ne sont
pas disponibles. Le banc reproduit des séquences contrôlées compatibles avec
les deux symptômes ; il n'identifie pas l'ordre des notifications de son essai.
Pas d'essai sur son PC Windows. Les hashes de contenu et la provenance des pochettes en
base sont vérifiés ; aucun rendu dans son navigateur n'est affirmé.
