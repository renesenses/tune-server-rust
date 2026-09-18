# #2778 — conserver la date du bon favori Bandcamp

JP Robbe / OpenAI Codex / jp-robbe-20260918-parallel-2778

## Périmètre et antériorité

Base : `73707a08c1658289913058a9843250622be08521`.
Branche : `fix/jp-robbe-20260918-2778-favorite-dates`.
Lot cible : `batch/disponibilite-service-20260917`.
Worktree Shrek : `/srv/builds/worktrees/jp-2778-20260918-parallel` ;
target de même clé. Aucun compte Bandcamp ni appel au service réel.

Les correctifs de file d'album (5598d7a1, #3257), collection jouable
(71f3beb5, #3750), favoris et erreurs de liaison (bb60b683, #3872) sont
déjà dans cette base. Ils ne sont pas refaits. Les correctifs clients
tune-web-client#830 et #836 sont également fusionnés ; la dernière instruction
du ticket, datant du 11 septembre, n'est plus un inventaire actuel.

Le défaut traité ici vient de `get_user_favorites_dated` : la conversion
`albums_de_collection` écarte les articles sans URL jouable, puis le service
zippe ses albums avec les articles bruts non filtrés. Après un rejet, la date
d'un autre article est attribuée au favori. Une date absente peut même être
remplacée par une date étrangère. Le tri par date côté client reçoit donc des
données fausses.

La conversion s'effectue désormais article par article ; album et date
proviennent du même article. L'ordre des articles admis, la conversion commune
et les règles de validation des URL restent identiques. Les constructeurs
modifiés séparément par #4387 sont préservés. Le traitement reste linéaire,
avec une petite page temporaire contenant un seul article à chaque itération.

## Témoins

`cargo test -p tune-bandcamp --lib dates_favoris_2778 -- --test-threads=2`

Trois témoins comportementaux sur le convertisseur réellement utilisé :

- rejet avant le premier album et entre deux albums avec des dates différentes ;
- date absente ou illisible : aucune date empruntée à un article écarté ;
- aucun rejet : ordre et dates conservés ; entrée absente ou vide.

Un quatrième témoin est une **garde structurelle du raccord** de la méthode
du service au convertisseur. Il inspecte seulement le corps de la méthode,
avec des aiguilles assemblées qui ne se trouvent pas elles-mêmes. Il ne
constitue pas un essai HTTP ou réseau.

Résultats sur Shrek : **4/4 témoins verts** après 2 min 38 de compilation.
Après les contre-épreuves et restauration, suite complète du plugin :
`cargo test -p tune-bandcamp --lib -- --test-threads=2`, **66/66 verts**
(62 essais préexistants et 4 nouveaux), 1,09 s d'exécution.

Format : `cargo fmt --all -- --check` et `git diff --check` verts.
Clippy : `cargo clippy -p tune-bandcamp --lib --tests -- -D clippy::correctness`
vert en 1 min 32. Quatre avertissements du plugin (dupliqués pour lib test),
tous sur des lignes identiques à la base ; aucun sur les ajouts de cette PR.
Les avertissements du code dépendant tune-core restent hors périmètre.

## Contre-épreuves

Tests inchangés, production seule modifiée ; compilation requise avant toute
interprétation. Restaurations par `cp` depuis la sauvegarde du fichier corrigé,
puis contrôle SHA-256.

Commande des deux contre-épreuves :
`cargo test -p tune-bandcamp --lib dates_favoris_2778 -- --test-threads=2`.

1. Ancien `zip` réintroduit uniquement dans le convertisseur, méthode raccordée
   et tests inchangés. Compilation réussie (5,20 s), **2 rouges / 2 verts**.
   - `un_article_ecarte_ne_decale_pas_les_dates_des_favoris_suivants` :
     « le premier favori a reçu la date d'un article écarté » ;
     reçu `2026-09-01T00:00:00Z`, attendu `2026-09-11T12:34:56Z`.
   - `un_favori_sans_date_n_herite_pas_de_celle_d_un_article_ecarte` :
     « une date absente ou illisible ne doit pas être empruntée ».
   - Le témoin sans rejet et la garde de raccord restent verts.
2. Ancien corps de méthode réintroduit, convertisseur corrigé et tests
   inchangés : compilation réussie (3,21 s), **1 rouge / 3 verts**.
   `la_methode_du_service_utilise_la_conversion_eprouvee` dit :
   « la méthode de service doit employer la conversion qui conserve la date
   de chaque article ». Les trois témoins comportementaux restent verts.

Chaque rouge termine avec Cargo 101 ; aucun rouge de compilation.
Les deux restaurations `cp` et contrôles SHA-256 réussissent, puis la suite
finale donne **66/66 verts**.

## Limites

Ce correctif établit une erreur de conversion déterministe. Il ne diagnostique
pas les symptômes historiques de FabienM (liaison affichée, actions de
collection), ni une observation réelle de tri incorrect sur son compte.

Pas de test de l'API Bandcamp réelle, du transport HTTP, du navigateur, de la
lecture audio ni de l'import d'achats. La pagination, les formats de date
acceptés, la sélection des URL et le refus d'écriture de favoris restent ceux
du code existant. Aucune modification de l'authentification ou des téléchargements.

Refs #2778. L'issue reste ouverte, le verrou est conservé pendant la revue.
Aucun bump, merge, tag ou déploiement.
