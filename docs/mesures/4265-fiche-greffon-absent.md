# #4265 — une fiche absente ne promet plus une installation possible

JP Robbe / OpenAI Codex / jp-robbe-20260916-4265-plugin-card.

Base : 59ff9529b4db3806c0257680daef6bd2d9de813c, lot batch/bugs-14.
Développement, compilation et tests sur Shrek, target dédié, six jobs.

## Contrat corrigé

La fiche d’un greffon absent consulte maintenant la même autorité que
install/update : le registre des noms chargeables et, lorsque la fonctionnalité
WASM est compilée, les manifestes présents sur disque.

GET /api/v1/plugins/{nom} garde HTTP 200, mais rend compatible=false et
reason=not_compiled_into_this_server. Le statut reste not_installed sans
anciens réglages, unavailable si installed ou enabled était vrai.
Les deux indicateurs rendus sont faux ; les réglages conservés en base ne sont
ni réécrits ni supprimés par la consultation.

Install/update gardent HTTP 404 et error=plugin_inconnu, avec le même champ
reason ajouté. Ce motif distingue l’absence d’une incompatibilité de version.
La liste, le chargement des plugins et les règles d’installation ne changent pas.

Un plugin SDK effectivement chargé reste compatible. Un plugin enregistré
mais dormant garde sa fiche installable. Pour un WASM présent, le manifeste
garde son verdict de version : absence de min_server_version ne vaut pas
absence du plugin. Une erreur de scan ne doit pas promettre de compatibilité
pour un nom non enregistré.

## Tests natifs et contre-épreuve

Commande du premier passage :

    cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test plugin_contracts plugin_routes::

Résultat : 30 réussites, aucun ignoré. Les trois témoins i4265 couvrent :
nom inconnu et Bandcamp non compilé, trois combinaisons de réglages hérités,
et les plugins chargés/dormants. Les requêtes passent par le vrai routeur
Axum en processus ; ce n’est pas une mesure d’interface web.

Contre-épreuve : restauration du seul fichier de production plugins.rs à la
base, sans changer les tests, puis même commande avec le filtre i4265.

La compilation réussit. Résultat : 1 vert / 2 rouges, exactement :

- i4265_fiche_absente_et_installation_portent_le_meme_motif :
  « la fiche d'un greffon absent ne doit pas promettre sa compatibilite » ;
- i4265_les_reglages_herites_ne_rendent_pas_un_absent_compatible :
  « un reglage herite ne doit pas rendre un greffon absent compatible ».

Le témoin des plugins chargés/dormants reste vert. Les SHA-256 du fichier
de tests sont identiques avant et après. Restauration par cp, puis la cible
plugin_contracts complète passe : 32 verts, aucun ignoré.

Journaux Shrek : native-first.log, counter-native.log, native-restored.log,
tests-before.sha256 et tests-restored.log, sous
/srv/builds/jp-evidence/jp-robbe-20260916-4265-plugin-card/.

## WASM, contre-épreuve et contrôles finaux

    cargo test --locked -j6 -p tune-server --no-default-features \
      --features oaat,plugins-wasm --test plugin_wasm_contracts

Résultat : 8 réussites, aucun ignoré. Le témoin i4265 confronte un manifeste
présent sans version minimale, un manifeste exigeant 999.0.0, un nom absent
du dossier et un chemin de scan inaccessible. Les fixtures sont isolées ;
la variable du dossier WASM est restaurée à la sortie.

Contre-épreuve : seul plugins.rs est remplacé par celui de la base ; le
fichier de tests garde le même SHA-256. Le filtre i4265 compile puis donne
0 vert / 1 rouge : « un dossier WASM ne rend pas un nom absent compatible ».
Restauration par cp, SHA-256 des tests vérifié, puis la suite complète
plugin_wasm_contracts repasse : 8 verts, aucun ignoré.

    cargo clippy --locked -j6 -p tune-server --no-default-features \
      --features oaat,plugins-wasm --test plugin_contracts \
      --test plugin_wasm_contracts -- -D clippy::correctness
    cargo fmt --all -- --check
    git diff --check

Ces trois contrôles réussissent. Clippy conserve des avertissements ; cette
validation ne signifie pas une compilation sans avertissement. Journaux :
wasm-first.log, counter-wasm.log, wasm-restored.log, clippy.log,
wasm-tests-before.sha256 et wasm-tests-restored.log, dans le même répertoire
de preuves Shrek.

## Limites

Aucune installation distante, aucun chargement réel de Bandcamp et aucune
validation de l’affichage de l’écran Extensions ne sont revendiqués.
Le motif commun ne distingue pas un nom absent d’un scan inaccessible ;
il reprend le verdict de l’autorité déjà utilisée par install/update.
Aucune migration, aucun bump, merge, tag ou déploiement.
