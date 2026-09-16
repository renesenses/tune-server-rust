# #3326 — Appairage Sendspin S2-b, travail en cours

Identité : **JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing**.

Base : `3a2b710a151a257e217143b2900eeb53076ea6ef`.
Branche : `fix/jp-robbe-20260916-3326-pairing`.
Lot prévu : `batch/jp-sendspin-pairing-20260916`.
Worktree Shrek : `/srv/builds/worktrees/jp-robbe-20260916-3326-pairing`.
Target propre : `jp-robbe-20260916-3326-pairing`, six jobs.

## Contrat épinglé et portée

- Spécification : `Sendspin/spec@8a8b1cbd6764ea116dcaa07e41544a97bc13080c`.
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

Aucune dépendance de production ajoutée à ce stade. Le banc séparé
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
