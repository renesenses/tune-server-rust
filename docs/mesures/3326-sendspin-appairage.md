# #3326 — Appairage Sendspin S2-b, travail en cours

Identité : **JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing**.

Base : `3a2b710a151a257e217143b2900eeb53076ea6ef`.
Branche : `fix/jp-robbe-20260916-3326-pairing`.
Lot prévu : `batch/jp-sendspin-pairing-20260916`.
Worktree Shrek : `/srv/builds/worktrees/jp-robbe-20260916-3326-pairing`.
Target propre : `jp-robbe-20260916-3326-pairing`, six jobs.

## Contrat épinglé et portée

- Spécification : `Sendspin/spec@8a8b1cbd6764ea116dcaa07e41544a97bc13080c`.
  Cette révision porte un fichier LICENSE.md avec l’identifiant Community-Spec-1.0 ;
  les anciens constats d’absence de licence sont historiques.
- Référence tierce : `Sendspin/aiosendspin@b6f8564d07b212d77bfb026b80baa23435d9e591`.
- Ce travail continue S2-b. Il ne livre pas S2-c/S2-d et ne clôt pas #3326.
  La lecture reste indisponible ; aucun `OutputTarget` n'est ajouté.
- La persistance et la reconnexion avec une clé longue durée sont implantées
  et éprouvées ci-dessous. Les trois méthodes, l'interaction opérateur et leur
  orchestration WebSocket restent à réaliser. La porte matérielle n'est pas
  franchie.

La révision courante impose une catégorie avec le PSK ID, un second message
Noise contenant les deux octets `{}`, puis un rééchange après appairage.
Le condensat de la poignée précédente lie le rééchange au canal existant.
Le signal de perte de clé utilise la sentinelle uniquement lors d'une connexion
initiale ; il ne supprime pas la clé enregistrée et n'autorise pas la lecture.

## Première étape : transport des clés

`PskPair` lie une catégorie et un secret à la clé publique du client.
La constante sentinelle ne peut pas devenir une clé privée d'appairage.
`PoigneeServeur` annonce la catégorie, vérifie Noise, rend le condensat et
signale le repli authentifié par la clé statique. Son rééchange conserve les
identités et la suite, et refuse le repli.

Au commit de cette première étape (`4a527ffb`), le serveur HTTP employait
encore la sentinelle. La seconde étape ci-dessous branche les clés longue
durée. Les routes permettant un nouvel appairage restent à réaliser.

### Validations exécutées sur Shrek

```sh
export TUNE_TARGET_KEY=jp-robbe-20260916-3326-pairing
. /srv/cache/tune/env.sh
cargo test -j 6 -p tune-core --no-default-features --features oaat \
  --test sendspin_poignee_s2a
```

16 tests réussis, dont neuf nouveaux ; un test d'interopérabilité explicitement
ignoré par cette commande. Le nouveau module est inclus dans une cible déjà
déclarée, malgré `autotests = false`.

Interopérabilité exécutée séparément :

```sh
export SENDSPIN_REFERENCE_PYTHON=/srv/builds/jp-research/jp-robbe-20260916-3326-pairing/venv/bin/python3
timeout 60 cargo test -j 6 -p tune-core --no-default-features --features oaat \
  --test sendspin_poignee_s2a i3326_reference_aiosendspin -- --ignored --nocapture
```

Un test réussi couvrant dix scénarios : chaque suite traverse les catégories
sentinelle, appairage et longue durée, avec deux pertes de clé supplémentaires.
Chaque scénario vérifie le transport bidirectionnel puis le rééchange vers une
clé longue durée. Le client utilise `NoiseSession` et les modèles de messages
d'aiosendspin, dans un sous-processus isolé. Cette preuve porte sur ces objets
de protocole ; elle ne couvre ni le SDK client complet, ni un WebSocket réel,
ni une enceinte physique.

Les journaux, révisions et contre-épreuves restent sous
`/srv/builds/jp-evidence/jp-robbe-20260916-3326-pairing/`.
Aucune batterie CI n'a encore été lancée pour ce travail en cours.

### Refus des clés X25519 de faible ordre

Le résolveur par défaut de snow 0.10 retourne un résultat DH nul sans erreur
pour certaines clés publiques. Avant de bâtir Noise ou de créer un record,
Tune vérifie donc que la clé contribue à un échange X25519, via
`x25519-dalek`. La même vérification s'applique à l'éphémère du second message.
Le témoin des identités couvre zéro, un, p-1, p, p+1 et leurs variantes avec
le bit haut ignoré par X25519.

### Contre-épreuves

Les commandes de base sont celles des tests cœur ci-dessus, avec le filtre
indiqué. Chaque sabotage conserve les tests ; leur SHA-256 est vérifié avant
et après l'essai. Chaque rouge vient d'une assertion après compilation réussie.

| Filtre du témoin | Sabotage du code de production | Défaut nommé par le rouge |
|---|---|---|
| `i3326_les_trois_categories` | Annoncer toujours `sn` dans le message 1 | « le message Noise 1 doit lier l'identifiant ET la categorie de PSK » |
| `i3326_le_re_echange_refuse_le_repli` | Retirer la restriction du repli aux connexions initiales | « un re-echange vers LT ne doit jamais retomber en sentinelle » |
| `i3326_une_identite_x25519_de_faible_ordre` | Désactiver le refus des identités de faible ordre | « une cle X25519 donnant un DH nul ne prouve pas une identite » |

Chaque essai : code de sortie 101, un test échoué. Restauration par `cp`
depuis la sauvegarde correspondante, puis retour au vert sur le code complet.
Les deux premiers essais précèdent l'ajout du témoin de faible ordre ;
chaque série conserve son propre relevé des SHA-256.

### Routes serveur

```sh
cargo test -j 6 -p tune-server --no-default-features --features oaat \
  --test sendspin_point_d_acces_s2a --test sendspin_mode_transition
```

12 tests réussis : sept de transition et cinq du point d'accès, dont des
WebSocket locaux réels dans les deux suites. Le banc demandant un lecteur
matériel reste explicitement ignoré. Les fixtures chiffrées envoient désormais
le corps Noise 2 `{}` prévu par la révision épinglée.

### Analyse statique

```sh
cargo clippy -j 6 -p tune-core -p tune-server --no-default-features \
  --features oaat --lib --test sendspin_poignee_s2a \
  --test sendspin_point_d_acces_s2a --test sendspin_mode_transition \
  -- -D clippy::correctness
cargo fmt --all --check
git diff --check
```

Les trois commandes réussissent. Des avertissements du dépôt subsistent ;
le résultat Clippy porte sur la catégorie `correctness`.

## Recherche pour la suite CPace

Au stade du banc initial, aucune dépendance de production n'était ajoutée.
Le banc séparé
`/srv/builds/jp-research/jp-robbe-20260916-3326-pairing/cpace-map-check`
a comparé le mapping `Legacy` de `curve25519-elligator2 0.1.0-alpha.2`
(feature `digest`) : un vecteur G_25519 du brouillon CPace et 128 générateurs
produits par `cpace 0.1.0` concordent, soit 129 cas. Les archives sources ont
été vérifiées avec les SHA-256 publiés par crates.io et PyPI.

Ce résultat porte uniquement sur le générateur. Il ne prouve pas encore
l'échange CPace, sa confirmation mutuelle, le wrapping, la machine à états
d'appairage ni les erreurs de code. Il reste à vérifier ces couches avant
d'utiliser cette bibliothèque dans Tune.

Attention au choix d'API : `RFC9380::from_representative` et `map_to_point`
masquent deux bits hauts pour l'encodage de transport Elligator ; CPace n'en
masque qu'un. Le mode `Legacy` donne accès au mapping du champ nécessaire au
banc. Les caisses Rust CPace/Ristretto255 ne sont pas interchangeables avec
CPACE-X25519-SHA512.

## Deuxième étape : magasin et reconnexion

Le magasin autonome `tune-core/src/sendspin/magasin.rs` conserve l'identité
du serveur et les clés longue durée, liées au client et aux méthodes ayant
servi à l'appairer. La rotation remplace la clé ; la révocation est durable.
Les vues publiques et Debug ne contiennent aucun secret.

Le serveur choisit `<TuneConfig.db_path>.sendspin`, sans migration musicale
ni dépendance ajoutée. Le même contexte alimente le WebSocket et l'API
`/api/v1/devices/sendspin`. Les I/O sont exécutées par `spawn_blocking`.
L'ouverture est différée au premier usage : construire un routeur ne crée
pas de fichiers. Une erreur rend le contexte indisponible jusqu'au
redémarrage ; le WebSocket répond 503 avant upgrade.

Le verrou exclusif reste détenu tant que le magasin est ouvert. Le document
est borné à 8 Mio, écrit dans un temporaire privé puis synchronisé et renommé.
Le dossier est synchronisé après publication ; son parent l'est à l'ouverture.
Le marqueur d'initialisation empêche de considérer un JSON perdu comme une
première installation. Une corruption ou une écriture ratée ne régénère pas
l'identité et n'annonce jamais une clé comme enregistrée à tort.

Sur Unix, permissions privées et refus des liens protègent le magasin.
Le parent du dossier est supposé contrôlé par l'opérateur. Les ACL Windows
et la tenue lors d'une coupure de courant réelle ne sont pas éprouvées ici.
Les tests de panne injectent un échec de destination et des documents perdus
ou corrompus ; ils ne constituent pas une simulation exhaustive du stockage.

La route choisit LT pour un pair enregistré, SN sinon, puis vérifie que le
record LT n'a pas changé pendant Noise. La perte de clé reste observable,
sans destruction du record ni activation audio. Le retour en clair consulte
également le magasin durable : la protection fonctionne avant la première
observation d'un pair dans le processus.

### Preuves locales exécutées

Commandes, avec la même clé et le même environnement Shrek que ci-dessus :

```sh
cargo test -j 6 -p tune-core --no-default-features --features oaat \
  --test sendspin_poignee_s2a
cargo test -j 6 -p tune-core --no-default-features --features oaat \
  --lib sendspin::
cargo test -j 6 -p tune-server --no-default-features --features oaat \
  --test sendspin_point_d_acces_s2a --test sendspin_mode_transition
```

- Cœur intégration : **24 réussis**, dont huit témoins de stockage.
  Deux entrées ignorées : l'interopérabilité séparée, et l'auxiliaire
  que le témoin multiprocessus exécute explicitement dans deux enfants.
- Cœur interne : **36 réussis**, filtre `sendspin::` (4 509 hors filtre).
- Serveur : **17 réussis**, dont cinq nouveaux témoins de persistance :
  rechargement LT dans les deux suites, perte de clé sans effacement,
  refus du clair pour un pair persisté jamais observé, corruption → 503,
  et routeur complet partageant le même magasin entre HTTP et WebSocket.
  Un banc matériel reste ignoré.

Ces tests utilisent des records provisionnés dans des fixtures temporaires.
Ils ne prouvent pas encore la création d'un appairage par le protocole
complet, ni la lecture sur une enceinte.

### Contre-épreuves de stockage et de routes

Les commandes d'intégration précédentes sont exécutées avec les filtres
ci-dessous. Les cinq fichiers de tests ont les mêmes SHA-256 avant et après
chaque sabotage. Tous les rouges présentés sont des assertions après
compilation réussie, code de sortie 101, un témoin échoué.

| Filtre | Retrait temporaire du correctif | Message du témoin |
|---|---|---|
| `i3326_identite_et_pairs_survivent_au_redemarrage_du_magasin` | Publier le document sans ses pairs, en les gardant seulement en mémoire | « le pair doit survivre au redemarrage » |
| `i3326_le_point_d_acces_recharge_l_identite_et_la_psk_longue_duree` | Choisir SN même quand le magasin contient LT | « le point d'acces doit annoncer la cle choisie dans son magasin » |
| `i3326_un_pair_appaire_jamais_vu_dans_ce_processus_ne_peut_pas_revenir_en_clair` | Retirer la consultation du magasin dans le refus du clair | « un record persiste doit interdire le clair avant toute observation dans ce processus » |

Restauration par `cp` après chaque essai. Retour au vert : 24 tests cœur,
puis le témoin de sélection seul, puis les 17 tests serveur complets.
Le premier essai de sabotage du magasin publiait l'ancien document et
échouait plus tard sur la révocation ; conservé dans `counter-store-first.log`,
il n'est pas le rouge retenu dans le tableau.

Journaux : `counter-store-durable.log`, `counter-store-selection.log`,
`counter-store-clear.log`, `store-core-restored.log` et
`store-server-restored.log`, dans le dossier de preuves indiqué plus haut.
Les sauvegardes du code de production et le relevé des SHA-256 sont archivés.

### Analyse statique de cette étape

La commande Clippy indiquée dans la première étape est repassée sur les
bibliothèques et les trois cibles de tests, avec `-D clippy::correctness` :
succès. Des avertissements du dépôt restent présents. `cargo fmt --all --check`
et `git diff --check` réussissent également. Journaux : `clippy-store.log` et
`fmt-store.log`. Aucun check CI de PR n'est encore revendiqué pour S2-b.

## Troisième étape : cryptographie des codes

`tune-core/src/sendspin/pake.rs` implémente le rôle A de CPACE-X25519-SHA512
avec confirmation mutuelle, selon
[draft-irtf-cfrg-cpace-21](https://datatracker.ietf.org/doc/html/draft-irtf-cfrg-cpace-21)
et la révision Sendspin épinglée. Le SID contient le condensat Noise,
le compteur d'appairage et le numéro du tour en big endian ; CI est vide,
les AD sont `server` et `client`.

Le code statique a huit chiffres ASCII, le code dynamique six chiffres,
et le format QR vingt-quatre octets bruts. Ce sont trois formats de code,
pas les trois méthodes d'appairage : la méthode `pairing_psk` ne fait pas
appel à CPace et reste à orchestrer avec les deux méthodes par code.

Les états publics se consomment. Le scalaire vient du CSPRNG et ne peut pas
être réemployé par le même objet. Le partage doit contribuer à X25519.
L'objet donnant accès au champ PSK n'est constructible qu'après vérification
de la confirmation client et, en dynamique, déchiffrement du nonce B,
vérification de son engagement puis recalcul du code. Une confirmation
incorrecte et une erreur de protocole restent deux erreurs distinctes, pour
que la route puisse appliquer le comportement prévu.

Les deux champs chiffrés ont des clés dérivées dans des domaines distincts.
Le déchiffrement utilise l'AEAD négocié (AES-256-GCM ou ChaCha20-Poly1305),
le nonce nul de douze octets et les données associées vides prévus par le
protocole. La valeur finale est une PSK longue durée liée à l'identité du
client. Aucun secret n'est exposé par Debug. Les champs de code et de clé
possédés par ces objets utilisent Zeroizing ; ceci ne prétend pas effacer
toutes les copies internes des primitives ou de la pile.

### Dépendances et limites de la revue

Deux paquets entrent dans le lockfile : `curve25519-elligator2 0.1.0-alpha.2`
et `hmac 0.12.1` (compatible avec le graphe digest 0.10 de SHA-512).
AES-GCM, subtle et zeroize étaient déjà transitifs et sont maintenant
déclarés directement. Aucun autre paquet existant n'est mis à jour.
Le choix accidentel de getrandom 0.4 pour tempfile lors de la résolution a
été retiré ; `cargo check --locked` accepte le graphe précédent conservé.

La caisse Elligator, sous BSD-3-Clause, est épinglée exactement. Elle dérive
de Dalek ; Tune utilise son mapping, et conserve x25519-dalek pour les
multiplications. Le chemin Legacy observé utilise les sélections
conditionnelles du champ et la conversion Edwards → Montgomery ; il garde
le bit 254 nécessaire à CPace. Cette inspection de code et les vecteurs ne
constituent pas un audit cryptographique indépendant ni une mesure complète
des canaux auxiliaires. La comparaison Python sert de référence numérique,
pas d'implémentation de production ni de garantie de temps constant.

### Preuves initiales

`cargo test -j6 -p tune-core --no-default-features --features oaat --lib sendspin::pake::`

Seize témoins passent : vecteur complet du brouillon (g, Ya, K, ISK),
129 générateurs, 64 échanges/confirmations indépendants, trois formats
dans les deux AEAD, erreurs de code/SID/tag, réflexion, engagement,
liaison du code, domaines distincts, tailles et authenticité des champs,
points de faible ordre et variantes valides RFC7748, fraîcheur et Debug.
Les données sont incluses dans les tests internes réellement compilés.

`tests/sendspin/generer_vecteurs_pake.py` reproduit les deux jeux de vecteurs
dérivés avec les bibliothèques tierces, sans charger Tune. Une régénération
dans un autre dossier est identique octet pour octet (journal
`pake-regenerated.log`). La provenance et la licence de référence se trouvent
dans `tune-core/src/sendspin/pake/VECTORS.md`.

L'interopérabilité explicite appelle les API publiques de Tune depuis le
constructeur qui tire son scalaire aléatoire. Un processus Python distinct
exécute CPace 0.1.0, les helpers de code aiosendspin et les AEAD de cryptography.
Un témoin réussit **36 scénarios** : six cas valides (3 formats × 2 suites)
et trente refus attendus, avec confirmation côté client avant livraison
de sa clé finale.

```sh
export SENDSPIN_REFERENCE_PYTHON=/srv/builds/jp-research/jp-robbe-20260916-3326-pairing/venv/bin/python3
cargo test --locked -j6 -p tune-core --no-default-features --features oaat --test sendspin_poignee_s2a i3326_pake_reference_vivante -- --ignored --nocapture
```

**Limite précise de cette interopérabilité :** le parcours d'appairage du SDK
aiosendspin épinglé, dans `noise/pairing.py::_pake_sid`, omet encore le tour.
Le banc assemble ce champ selon la spécification épinglée et compare les
objets CPace, les codes et le chiffrement des valeurs. Il ne prétend pas
exécuter le parcours SDK complet. Tune ne retire pas le tour pour satisfaire
une référence en retard sur ce point.

L'orchestration des messages WebSocket et les commandes opérateur restent
à réaliser. Aucun parcours de création d'appairage n'est encore offert par
les routes et aucune activité audio n'est activée. La porte matérielle de
#3326 reste ouverte.

### Contre-épreuves natives

Les neuf fichiers de tests, vecteurs et bancs Python sont identiques par
SHA-256 pendant les deux séries de sabotage. Le code de production est
restauré par copie entre les séries, puis avant le retour au vert.

1. Filtre `i3326_pake_un_` : retrait de la vérification de confirmation,
   du refus d'un commitment différent et du refus d'un code non lié.
   Compilation réussie, puis **trois témoins échoués**, code 101 :
   « un mauvais code ne doit pas donner acces a la cle finale »,
   « le commitment doit etre verifie avant de lire une PSK » et
   « le code saisi doit etre derive des deux nonces et de Noise avant toute PSK ».
   Les témoins d'engagement et de liaison emploient un tag client valide :
   le retrait simultané de la confirmation ne cause pas leur rouge.
2. Filtre `i3326_pake_concorde_avec_129_generateurs` : effacement du bit 254
   en plus du bit 255. Compilation réussie, puis **un témoin échoué**,
   code 101 : « generateur de reference 0 : CPace ne masque que le bit 255 ».

Commandes : `cargo test --locked -j6 -p tune-core --no-default-features --features oaat --lib <filtre>`.
Journaux : `counter-pake-confirmations.log`, `counter-pake-generator.log`,
sauvegarde `pake.rs.before-counter` et relevé `pake-tests-before-counter.sha256`.

Après restauration, `--lib sendspin::` passe **52 tests** (36 existants et
16 nouveaux ; 4 509 hors filtre) sur le lockfile conservant getrandom 0.3
pour tempfile.

### Vérification finale de la brique native

Sur le code restauré, puis complété par la copie des valeurs publiques de
liaison pour les futurs tours d'un même essai (les objets CPace restent
non clonables), les commandes avec `--locked` donnent :

- `--lib sendspin::` : **52 réussis**, aucun ignoré ;
- `--test sendspin_poignee_s2a` : **24 réussis**, trois ignorés ;
- les deux cibles serveur : **17 réussis**, un banc matériel ignoré ;
- filtre `reference -- --ignored --nocapture` : **deux témoins réussis**,
  dix scénarios Noise et trente-six scénarios CPace.

Les trois entrées ignorées de la cible cœur sont les deux bancs tierces
exécutés séparément et l'auxiliaire du témoin multiprocessus.
La dernière relance des 24 tests utilise deux jobs, après la hausse de charge
de Shrek ; la validation en cours a également reçu une priorité CPU réduite.
Aucun processus d'une autre session n'a été modifié.

Clippy, sur les bibliothèques et les trois cibles d'intégration, passe avec
`-D clippy::correctness`. Des avertissements du dépôt subsistent.
`cargo fmt --all --check` et `git diff --check` passent.
Les journaux finaux portent les préfixes `pake-core-`, `pake-server-`,
`pake-interop-`, `clippy-pake.log` et `fmt-pake.log`.
La batterie CI de la PR brouillon est une porte distincte, sans résultat
revendiqué à cette étape.

## Etape suivante : entrees operateur des jetons et codes

JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing.

Le module jeton lit les versions SP:0 (cle publique du client puis PSK
provisoire) et SP:1 (24 octets de code QR). La PSK provisoire n'est obtenue
qu'en fournissant l'identite correspondante de la connexion Noise.
Les autres versions, les encodages malformes, les charges tronquees et
les identites X25519 de faible ordre sont refuses. La Sentinelle ne peut
pas etre promue en PSK d'appairage.

Les saisies tolerent la casse, l'absence de prefixe, les espaces autour
du jeton et la translitteration 9/2. Les octets d'extension sont ignores
apres validation de l'encodage entier. Les codes chiffres acceptent les
groupements par espaces et tirets, en conservant les zeros initiaux.
Le QR fournit ses octets bruts a CPace, jamais le texte du jeton.

data-encoding 2.11.0 devient une dependance directe ; cette version etait
deja verrouillee. Aucun nouveau paquet ni mise a jour de paquet existant
dans cette etape.

Les deux vecteurs normatifs et 24 cas produits par les decodeurs du SDK
epingle sont conserves dans jeton/ ; les 16 cas valides couvrent notamment
les extensions, les 8 autres sont tronques. Un temoin du module PAKE
inspecte les octets effectivement produits par la saisie normalisee.
La reference, son script de regeneration et le SHA-256 sont documentes dans
tune-core/src/sendspin/jeton/VECTORS.md.

Le premier passage sur Shrek passe 59 tests internes (52 precedents et
7 nouveaux), avec deux jobs et une priorite CPU basse sous forte charge.
Cette entree operateur reste a brancher aux commandes authentifiees et
a l'orchestration WebSocket avant de sortir la PR du brouillon.

### Contre-epreuves des entrees operateur

Commande : cargo test --locked -j2 -p tune-core --no-default-features
--features oaat --lib i3326_jeton_ (priorite CPU 19).

Deux gardes de production sont retirees dans une meme compilation :
la comparaison entre identite saisie et identite de la connexion, et la
troncature de la charge aux octets definis par la version du jeton.
Compilation reussie, puis 4 tests verts et 2 rouges (code 101) :

- i3326_jeton_identite_et_psk_sont_liees_au_transport :
  « le secret du jeton ne doit jamais etre utilisable pour un autre client » ;
- i3326_jeton_extensions_et_troncatures_concordent_avec_python :
  « les octets d'extension sont reserves ».

Les fichiers jeton/tests.rs, jeton/reference.json, pake/tests.rs et le
generateur Python sont inchanges par SHA-256. Le code est restaure avec cp,
puis --lib sendspin:: repasse 59 tests, aucun echec ni ignore.
Le journal rouge est counter-jeton.log ; la sauvegarde est
jeton.rs.before-counter ; le retour au vert est jeton-unit-final.log.

Les 24 tests d'integration du coeur, les 17 tests serveur et les 46 scenarios
d'interoperabilite restent les mesures de la tete a1f74d34, avant cette
brique d'entree. Ils ne sont pas recomptes comme une nouvelle execution.

Clippy de tune-core --lib passe sur la brique de saisie avec
--locked --no-default-features --features oaat -D clippy::correctness.
Le formatage du workspace et git diff --check passent egalement.
Ces controles ne sont pas annonces sans avertissements : le depot en emet.

### CI de la premiere tete du brouillon

La tete a1f74d349b28f881758564f55dffe2ca52598a2c de la PR #4263
passe la CI 35100091300 et PostgreSQL 35100091389.
Les journaux du job Test montrent les 16 temoins CPace, les temoins de
transport, de stockage et les cinq temoins de persistance serveur #3326.
Les deux bancs Python restent ignores en CI et sont executes explicitement
sur Shrek ; l'auxiliaire multiprocessus est appele par son test parent.
Les resultats sont archives dans ci-a1f74d34 avec SHA256SUMS.

Ces runs precedent le commit des jetons et ne valent pas resultat CI pour
une tete ulterieure. La PR reste un brouillon, sans appairage complet ni son.

## Orchestration en cours — point de sauvegarde avant renouvellement du cache

JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing.

La machine appairage.rs conduit les messages des trois methodes et emet des
actions ordonnees : envoi chiffre, attente de geste, demande de code,
persistance puis acquittement et promotion. Elle garde les nonces et le delai
sur une reprise, borne les tours, rejette les champs interdits et les erreurs
de sequence, et ignore les messages en vol apres annulation. Les entrees et
sorties ne sont pas encore raccordees au WebSocket ni aux routes operateur.

Une premiere version passe 71 tests internes sur Shrek. La garde du nouvel
essai apres annulation et son treizieme test ont ete ajoutes ensuite ; ils
restent a valider. Le nouveau banc de dix scenarios sur les messages de la
machine, avec les primitives du client Python, reste aussi a executer.
Il ne represente pas encore un parcours WebSocket ou SDK complet.
Les contre-epreuves et Clippy de cette machine restent a faire.

Ce point sauvegarde le code avant purge du seul target de cette session
(22 Gio mesures), parce que l'espace libre partage est passe sous 120 Gio.
La tete publiee de la PR reste 32a3198e jusqu'aux validations de cette etape.

## Raccordement HTTP et WebSocket — validation locale en cours

JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing.

La machine est maintenant appelee par le pilote WebSocket. Les commandes
operateur sont montees sur la vraie API devices, avec RequireAdmin, une limite
de corps de 16 Kio et une file bornee a huit commandes par connexion. Chaque
instance de routeur possede son magasin et son registre de connexions. Les
trois methodes ont leur commande de demarrage ; aucune activation audio ne
fait partie de cette etape.

Le premier passage complet du nouveau banc, sur Shrek avec six jobs et une
priorite CPU 15, donne 15 tests verts et un banc materiel explicitement ignore.
Commande : cargo test --locked -j6 -p tune-server --no-default-features
--features oaat --test sendspin_point_d_acces_s2a.
Journal : runtime-tests-second.log (compilation 3 min 17 s ; tests 23,19 s).

Les cinq nouveaux temoins executent :

- PSK : deux suites Noise, passage SN -> PR -> LT sur la meme connexion,
  repetition des hello apres chaque re-echange, fichier persiste avant
  acquittement, reconnexion LT puis revocation active ;
- entrees HTTP invalides, jeton lie a un autre client et corps trop grand
  refuses, puis echange d'horloge toujours utilisable ;
- cinq commandes avec authentification active : 401 sans identite, 403
  pour le role user et acces administrateur ;
- revocation d'un client qui ne repond plus pendant le re-echange ;
- ecriture atomique rendue impossible sur une fixture, sans acquittement
  ni promotion LT.

Le client de ce banc utilise snow directement : ce sont de vrais parcours
HTTP/WebSocket, mais pas une preuve d'interoperabilite avec un lecteur tiers.
Les Ping/Pong et un message chiffre deja en vol sont exerces pendant les
poignees. Les contre-epreuves des nouvelles gardes serveur sont encore a
executer avant de revendiquer leur couverture.

La machine du coeur avait auparavant donne 72 tests verts et 56 scenarios
d'interoperabilite en memoire (3 bancs explicitement selectionnes). Ses trois
contre-epreuves avaient rougi sur : acquittement avant persistance, partage
statique avant geste client et renouvellement indu du delai a la reprise.
Les tests etaient inchanges et les sources restaurees par cp. La quatorzieme
regression de machine, sur les champs inconnus de client/pair-retry, fait
l'objet d'une validation separee.

Restent notamment les parcours CPace complets via WebSocket et client tiers,
les controles finaux du raccordement, et l'alignement des erreurs init avec
server/error de la specification epinglee. Le banc historique de fermeture
silencieuse sur un init malforme n'est pas une validation du nouveau contrat.
La PR reste en brouillon et #3326 reste ouverte ; aucune zone ni lecture
d'album synchronisee n'est revendiquee.

### Champs inconnus a la reprise : contre-epreuve terminee

Commande de depart et de retour au vert : cargo test --locked -j6
-p tune-core --no-default-features --features oaat --lib sendspin::.
Resultat : 73 tests verts, aucun ignore, avant et apres restauration.

La mutation de production retablit le refus de tout champ dans
client/pair-retry. Le test
i3326_appairage_reprise_ignore_les_champs_du_futur compile puis echoue seul
(code 101) : « un champ payload inconnu doit etre ignore sans fermer le tour ».
Son fichier est inchange par SHA-256 ; restauration du code par cp.
Journaux : appairage-73-initial.log, counter-compat-future.log,
compat-tests-after.log et appairage-73-restored.log.

### Contre-epreuve serveur interrompue par la garde d'espace

Apres acquisition des 15 tests serveur verts et des 73 tests coeur verts,
l'espace partage est descendu de 136 Gio a 119 Gio, puis 116 Gio. La tentative
de contre-epreuve des trois gardes serveur a ete arretee avant execution des
tests, uniquement sur les PID Cargo/rustc de cette unite. Ce journal ne vaut
ni rouge de propriete ni regression. Les deux fichiers de production ont ete
restaures par cp et compares a leurs sauvegardes ; les deux fichiers de
tests sont inchanges par SHA-256.

Aucune contre-epreuve serveur reussie n'est donc revendiquee a ce point.
Le script verify-runtime-guards.sh et le journal counter-runtime-guards.log
sont conserves dans les preuves Shrek pour reprise apres liberation du cache.
Les trois tests internes du registre de sessions et Clippy serveur restent
a executer ; les resultats verts ci-dessus ne les englobent pas.


## Reprise après nettoyage : contre-épreuves serveur et parcours CPace

JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing.

Cette section remplace les états « interrompu » et « à brancher » ci-dessus.
Les mesures historiques restent datées par leurs étapes ; elles ne sont pas
additionnées comme de nouvelles exécutions.

### Trois gardes serveur éprouvées

Commande : `cargo test --locked -j6 -p tune-server --no-default-features
--features oaat --test sendspin_point_d_acces_s2a runtime_3326::`.

La mutation retire les contrôles administrateur, ignore le résultat de
persistance et suspend l’écoute de révocation pendant le rééchange.
La compilation réussit ; **2 tests passent et les 3 témoins attendus échouent** :

- `i3326_runtime_commandes_exigent_un_administrateur_quand_auth_active` :
  `GET pair, role=Some("user")`, résultat 404 au lieu de 403 ;
- `i3326_runtime_ecriture_impossible_ne_confirme_pas_l_appairage` :
  « une ecriture impossible ne doit emettre ni acquittement ni re-echange LT » ;
- `i3326_runtime_revocation_interrompt_un_reechange_sans_reponse` :
  « la revocation doit interrompre le re-echange sans attendre le pair ».

Les tests sont inchangés par SHA-256 pendant la contre-épreuve.
Restauration des deux fichiers de production par `cp`, puis **15 tests
passent et un banc matériel reste ignoré**. Journaux :
`counter-runtime-guards.log`, `runtime-tests-restored.log`,
`runtime-tests-after-counter.log`.

### Init : émission et ordre du refus

Commande de contre-épreuve : même commande Cargo, filtre `init_3326::`.

- Retirer seulement l’envoi de `server/error` compile puis donne **1 vert /
  3 rouges**. Les trois témoins nomment « un echec init exige server/error
  avant fermeture ».
- Vérifier `client_id` avant la version et la suite compile puis donne
  **2 verts / 2 rouges**. Les témoins de priorité version/suite nomment
  « l'ordre enveloppe/version/suite/identite doit determiner le refus ».

Le témoin d’erreur Noise reste vert dans les deux mutations. Les fichiers de
tests restent inchangés par SHA-256 ; restauration par `cp`, puis **19 verts /
1 ignoré**. Journaux : `counter-init-emission.log`, `counter-init-ordre.log`,
`init-tests-restored.log` et `init-tests-after-counter.log`.

La matrice couvre JSON/enveloppe invalides, version absente ou non entière,
versions futures et négatives jusqu’aux limites i64/u64, suite inconnue,
identité invalide, fermeture après un unique refus, Noise invalide, erreur
AEAD et texte après activation. Les échanges nominaux gardent un champ futur
et des espaces dans le prologue brut, dans les deux suites.

### CPace sur le vrai HTTP/WebSocket

Commande : `SENDSPIN_REFERENCE_PYTHON=<venv épinglé>/bin/python3
cargo test --locked -j6 -p tune-server --no-default-features --features oaat
--test sendspin_point_d_acces_s2a i3326_cpace_websocket -- --ignored --nocapture`.

**Un test exécute dix scénarios, tous verts** (23,64 s hors compilation) :

- code statique dans chacune des deux suites ;
- chiffres dynamiques dans chacune des suites, avec succès direct ou reprise ;
- QR dans chacune des suites, avec succès direct ou reprise.

Chaque scénario passe par attente de geste, échange d’horloge sans partage
prématuré, annulation et nouveau compteur d’essai, appairage, fichier durable
avant acquittement, rééchange LT, reconnexion LT puis révocation active.
Une reprise conserve les nonces, change l’éphémère et le SID du tour, et
accepte une extension future. Un mauvais code ou une annulation laisse le
magasin initial inchangé.

Le premier essai du banc supposait à tort que `pairing.json` n’existait pas
avant le premier appairage : le magasin crée déjà le fichier avec l’identité.
Le témoin compare maintenant les octets au magasin initial ; ce premier
échec est une correction de fixture, pas une contre-épreuve de production.

Journal : `cpace-runtime-second.log`. Le processus Python est une fixture
bornée, utilisant CPace 0.1.0 et les helpers aiosendspin épinglés. Le transport
client repose sur Snow. L’omission de `round` dans le SDK épinglé impose
toujours le SID normatif dans le banc : **aucune interopérabilité du client
SDK complet ou d’une enceinte réelle n’est annoncée**. Les secrets émis par
l’auxiliaire Python sont générés uniquement pour ces fixtures éphémères.

Le banc CPace est ignoré par défaut et exécuté explicitement sur Shrek.
L’autre test ignoré est le banc matériel. Ils ne sont jamais comptés parmi
les succès ordinaires de la CI.


### Contrôles finaux de cette étape

Sur le même worktree et le même graphe oaat, six jobs :

- tune-server --test sendspin_point_d_acces_s2a : **19 verts / 2 ignorés** ;
- tune-server --lib i3326_sessions : **3 verts**, aucun ignoré ;
- tune-core --lib sendspin:: : **73 verts**, aucun ignoré ;
- tune-core --test sendspin_poignee_s2a : **24 verts / 4 ignorés** ;
- même cible cœur, filtre interop_ -- --ignored --nocapture :
  **3 verts / 0 ignoré**, 56 scénarios tiers (43,77 s hors compilation) ;
- cargo clippy --locked -j6 -p tune-server --no-default-features --features oaat
  --all-targets -- -D clippy::correctness : **succès**, avertissements conservés ;
- cargo fmt --all et git diff --check : **succès**.

Journaux : final-server-tests.log, final-sessions-tests.log,
final-core-tests.log, final-core-integration.log, final-interop-selected.log
et final-clippy.log.

Une première sélection --include-ignored a également lancé directement
l’auxiliaire i3326_processus_magasin, sans les variables de sa fixture :
27 verts / 1 rouge. Ce lancement est conservé dans final-core-interop.log
comme erreur de sélection. Le test parent multiprocessus passe dans la suite
normale ; les trois bancs tiers passent dans la sélection explicite corrigée.
Aucun test n’a été désactivé pour masquer cet échec.

La CI doit porter sur le commit publié de cette étape ; les succès de la
tête précédente ef22a846 ne valent pas validation de cette nouvelle tête.

### Rangement du banc après la CI de e472b93d

La CI principale 35126457635 et PostgreSQL 35126457649 ont échoué sur le
même témoin tests_orphelins. Le banc CPace était exécuté par le module runtime,
mais son fichier à la racine de tests/ était invisible au recensement des
agrégateurs directs. Il est déplacé dans tests/sendspin/cpace_runtime_3326.rs,
le sous-dossier destiné aux modules, et son attribut path est mis à jour.

Le code de tests_orphelins.rs et le contenu du banc CPace sont inchangés par
SHA-256. Aucun test n’est exclu, aucun workflow ni garde n’est modifié.

Sur Shrek, compilation directe du même test standard avec
CARGO_MANIFEST_DIR fixé au paquet tune-server :

    rustc --edition=2024 --test tune-server/tests/tests_orphelins.rs \
      -o "$CARGO_TARGET_DIR/tests-orphelins"

Avant déplacement : un rouge nommant sendspin_cpace_runtime_3326.rs.
Après déplacement : un vert. Puis la cible réellement utilisée en CI passe :

    cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test server_contracts tests_orphelins::

Résultat : un vert. La cible sendspin_point_d_acces_s2a repasse 19 tests
(2 ignorés), et le filtre CPace exécuté explicitement repasse ses dix
scénarios depuis le nouvel emplacement (107,51 s hors compilation).

Un timeout de trois secondes a été observé sur le premier passage PSK de
cette relance, pendant une forte attente disque (pression IO ~33 %, charge
~34). Sa cause n’est pas isolée. Sans modifier le code ni les délais, le test
isolé repasse, puis la suite complète repasse. Ce premier échec est conservé ;
il n’est pas présenté comme une contre-épreuve ni effacé des mesures.

Preuves : layout-before.log, layout-after.log, layout-hashes.json,
layout-server-tests.log (timeout), layout-timeout-resources.log,
layout-psk-isolated.log, layout-server-recheck.log, layout-cpace-tests.log et
layout-orphan-cargo.log. Les journaux CI rouges sont sous ci-e472b93d/.
