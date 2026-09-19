# #3234 / web #1186 — Disponibilité de l’extracteur ISO SACD

JP Robbe / OpenAI Codex / jp-robbe-20260918-suite7-3234

## Constat et périmètre

Bases : serveur 5bef77710657f26a28abe89358a4bba8e40c447c,
web 4476ce7daf505c117c2add58f31ad446115cb80c.
Lots coordonnés batch/jp-p2-sacd-status-20260918 dans les deux dépôts.

L’issue demande de rendre visible la dépendance externe, sans embarquer
sacd_extract ni ajouter de moteur. Le scan (scanner/walker.rs) extrait les
ISO SACD avant de les ajouter à la bibliothèque. Aucun état de l’outil n’était
affiché dans le client. L’ancien /sacd-rip/status décrit le rip physique et
utilise which, contrairement à la recherche réelle de l’extracteur ISO :
il n’est donc pas réutilisé pour ce diagnostic.

Inventaire initial : pas de verrou pour #3234, aucun miroir web existant ;
miroir #1186 créé et les deux verrous acquis atomiquement. Les PR web
1077/1083/1084 modifient des clés distinctes dans les mêmes locales ; la
PR1084 ajoute un raccord Pont Roon dans une autre section de SettingsV2.
Aucun changement de api.ts, de Cargo, de scanner ou de lecteur. Les refs
hors main recherchées pour SACD/#3234 ne contenaient pas ce correctif.

## Changement

- Nouveau GET /api/v1/sacd-rip/iso-status : available booléen et tool
  (sacd_extract ou none). Aucun état du lecteur physique.
- La sonde partage la liste de candidats et le critère --help de l’extracteur :
  code zéro OU sortie stdout/stderr non vide. La politique de l’extraction
  existante reste identique.
- Budget global de deux secondes, processus tué à l’abandon/expiration et une
  seule sonde HTTP admise à la fois, sans file d’attente. Refus concurrent ou
  délai dépassé : HTTP 503, aucune affirmation d’absence.
- Encart dans Bibliothèque des réglages V2, près du scan. Outil détecté ne
  garantit pas l’extraction ; outil non détecté donne la consigne
  d’installation séparée sur le serveur, de redémarrage si PATH change et de
  nouveau scan. Réponse invalide, serveur ancien/404, refus 503 ou réseau
  indisponible restent « impossible de vérifier », avec bouton de nouvelle
  vérification. Onze traductions.
- Un montage déclenche une requête ; le bouton reste désactivé tant qu’elle
  attend. Une réponse après démontage ne modifie plus l’état du composant.

## Preuves et commandes

Toutes les commandes ont été exécutées sur Shrek, sans compte de service ou
matériel. Cargo : clé jp-3234-20260918-suite7, env.sh explicite, six jobs.
Web : runtime officiel Node22.23.2 propre à l’unité, npm-cache/node_modules
isolés, deux workers.

    export TUNE_TARGET_KEY=jp-3234-20260918-suite7 CARGO_BUILD_JOBS=6
    . /srv/cache/tune/env.sh
    cargo test -p tune-core -p tune-server --lib iso_status --no-default-features --features oaat -- --test-threads=2

    export PATH=/srv/builds/jp-web-3234/runtime/node-v22.23.2-linux-x64/bin:$PATH
    npx vitest run src/lib/__tests__/sacdIso1186.test.ts --maxWorkers=2
    npm test -- --maxWorkers=2
    npm run build

Les huit tests web exécutent les vrais composants, client HTTP et traductions ;
un cas monte l’écran SettingsV2 réel. Seuls fetch et les observateurs de
géométrie du navigateur sont simulés. Les autres appels de l’écran reçoivent
des réponses minimales. Les cas couvrent absent, présent, HTTP 404/503 suivi
d’un retry réussi, deux réponses invalides, réseau refusé, requête en attente
et démontage.

Contre-épreuve web : seuls le raccord de SettingsV2 et la publication d’un
résultat valide sont retirés ; les tests compilent et restent inchangés.
Résultat : quatre assertions rouges / quatre témoins verts. Restauration
par cp, trois empreintes vérifiées (deux fichiers source et le test), puis
huit tests verts.

Les tests Rust utilisent de vrais processus Python temporaires pour les
résultats --help, l’absence et le délai dépassé. Le témoin Linux vérifie la
disparition du PID après expiration. Le router réel est invoqué pour réponse
normale et refus concurrent. L’erreur de sonde est ensuite injectée dans le
même handler pour vérifier son passage en 503 et la libération du permis ;
ce dernier témoin n’est pas une extraction réelle ni un second processus.

Résultats enregistrés :

- Rust : 4/4 témoins verts après restauration (3 core, 1 serveur).
  Format cargo fmt --all --check vert.
- Contre-épreuve Rust compilée : dans probe_candidates, le retour positif
  devient false ; le raccord /iso-status est retiré. Aucun test n’est changé
  (empreintes des deux modules vérifiées). Deux assertions de disponibilité
  échouent et le test HTTP reçoit 404 au lieu de 200 ; le test de timeout reste
  vert. Les messages réels figurent dans server-counterproof.log :
  « assertion failed: probe_candidates(...) » et « left: 404 / right: 200 ».
  Sources restaurées par cp et deux empreintes complètes vérifiées.
- Web : suite officielle (six gardes inclus) : 483 fichiers / 5082 tests verts,
  244,32 secondes. Puis seule précision de libellé dans les onze locales :
  « non détecté » au lieu d’« inaccessible ». Sur ce texte final : 8/8 témoins
  verts, garde i18n vert, build final vert en 1 min 05. Les avertissements Svelte
  et de taille des chunks restent présents ; le garde Svelte officiel indique
  « aucune nouvelle erreur », pas absence de dette.
- Clippy : vert en 2 min 37 avec -D clippy::correctness. Les avertissements
  préexistants du graphe restent présents ; aucun n’a été corrigé hors périmètre.

Commandes de contre-épreuve/restauration et contrôle Rust :

    cargo test -p tune-core -p tune-server --lib iso_status --no-fail-fast --no-default-features --features oaat -- --test-threads=2
    cargo test -p tune-core -p tune-server --lib iso_status --no-default-features --features oaat -- --test-threads=2
    cargo fmt --all --check
    cargo clippy -p tune-core -p tune-server --lib --no-default-features --features oaat -- -D clippy::correctness

Les deux main distants ont été revérifiés après les tests : mêmes SHA.
Aucun batch en avance sur main ne touche les fichiers serveur concernés ni
SettingsV2/module API. Les branches des autres sessions restent intactes.

## Limites et livraison

Le binaire sacd_extract réel et une ISO SACD ne sont pas nécessaires à ces
témoins et n’ont pas été exécutés. Pas de preuve d’extraction, de décodage,
de son ou de runtime Windows/macOS. Le scanner conserve sa politique
d’extraction et sa propre gestion des processus ; le budget de deux secondes concerne
uniquement le diagnostic HTTP.

Le client requiert le nouveau serveur pour afficher présent/absent ; un
serveur ancien est explicitement inconnu. L’interface historique et le rip
physique restent hors périmètre. Les onze traductions sont contrôlées pour
leurs clés, sans recette linguistique humaine. Aucune conclusion sur la
cause exacte du cas terrain ni clôture anticipée par cet encart.

Preuves hors worktree : /srv/builds/jp-evidence/jp-3234-20260918-suite7/.
Verrous conservés pendant revue ; aucune fusion, version, étiquette de release
ou publication effectuée.
