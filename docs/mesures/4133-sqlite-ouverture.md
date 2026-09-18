# #4133 — rendre l'échec d'ouverture SQLite exploitable

JP Robbe / OpenAI Codex / jp-robbe-20260917-p1suite-4133.

Base : 73707a08c1658289913058a9843250622be08521.
Lot prévu : batch/jp-p1-sqlite-20260917.
Worktree Shrek : /srv/builds/worktrees/jp-4133-20260917-suite.
Target : /srv/cache/tune/targets/jp-4133-20260917-suite.

## Défaut traité

Le journal de #4133 donne « sqlite open /data/tune.db: unable to open
database file », puis l'initialisation échoue. Plusieurs situations produisent
le même message : parent absent, chemin parent qui est un fichier, répertoire
à la place de la base, parent non inscriptible.

Les Dockerfile de développement et de distribution créent déjà /data sous
UID/GID 1000 et exécutent Tune sous le compte tune. Les exemples Compose
utilisent un volume nommé. Le rapport ne fournit ni le montage réel, ni ses
options, ni ses droits : aucune cause n'est attribuée à l'installation du
testeur. Shrek n'a pas Docker ; aucune image Docker n'a été exécutée.

## Changement borné

Après un échec réel de l'ouverture de la connexion SQLite principale,
conserver l'erreur originale et ajouter :

- le chemin absolu (sans canonicalisation), sans suivre ni masquer les liens symboliques ;
- une observation du parent et du type de la cible ;
- sous Linux seulement, un contrôle d'accès effectif du noyau par
  faccessat(AT_EACCESS), sans déduire les droits des seuls bits Unix ;
- une indication pour contrôler TUNE_DB_PATH, le montage et le compte du
  serveur, en rappelant que SQLite doit aussi écrire ses journaux dans le
  parent.

La branche Linux de contrôle d’un fichier existant non inscriptible n’a pas
de témoin dédié ; les tests de droits portent sur le parent. Le cas URI est
un test direct du diagnostic, pas une ouverture SQLite URI complète.

Les observations sont postérieures à l'échec : une course avec un changement
du système de fichiers reste possible. Quand les contrôles ne trouvent
rien, le message dit explicitement que la cause reste indéterminée.
Hors Linux, le diagnostic indique que les droits effectifs n'ont pas été
contrôlés. Les URI SQLite file:... ne sont jamais interprétées comme un
chemin ordinaire ; leur erreur reste inchangée.

Aucune création de dossier, ouverture supplémentaire de fichier en écriture,
modification de mode/propriétaire, nouvelle tentative ou suppression de
données. Le diagnostic ne suggère pas de passer le service en root.
Un montage incorrect doit être corrigé par son opérateur.

## Validation Shrek

Commandes sous jp (UID 1000), clé dédiée et CARGO_BUILD_JOBS=6 :

    export TUNE_TARGET_KEY=jp-4133-20260917-suite CARGO_BUILD_JOBS=6
    . /srv/cache/tune/env.sh
    cargo test -p tune-core --lib db::sqlite::open_diagnostic \
      --no-default-features --features oaat

Six tests enregistrés via un module #[cfg(test)] exécutent les vrais appels
SqliteDb::open pour quatre échecs, une ouverture normale et :memory:.
Le test URI contrôle la préservation de l'erreur. Les témoins vérifient aussi
que les données et droits des fixtures ne changent pas.
Le test des droits exige un utilisateur non-root : root contourne les
permissions DAC et invaliderait ce témoin.

Résultats : six tests ciblés verts, puis huit tests db::sqlite:: verts
après restauration. La contre-épreuve remplace uniquement le raccord
open_diagnostic::describe(path, &e) par le message précédent, sans modifier
les tests : quatre échecs comportementaux et deux témoins verts, code 101.
Exemple : « missing parent must be identified: sqlite open .../missing/tune.db:
unable to open database file ». Les témoins répertoire cible, parent fichier
et parent non inscriptible échouent aussi sur leur observation absente.
Restauration par copie, trois SHA-256 identiques, puis huit tests verts.

La première compilation est limitée à six jobs ; après montée de la charge
partagée, le Clippy à six jobs est interrompu volontairement et n'est pas
compté vert. Dernière validation après précision du libellé absolute path
avec deux jobs et deux threads de tests : huit tests verts sur le code final,
formatage et Clippy -D clippy::correctness verts. Les 434 avertissements Clippy
restants sont hors des fichiers ajoutés ; le seul avertissement stylistique
introduit a été retiré avant cette dernière validation.

    cargo test -p tune-core --lib db::sqlite:: \
      --no-default-features --features oaat -- --test-threads=2
    cargo clippy -p tune-core --lib --no-default-features --features oaat \
      -- -D clippy::correctness
    cargo fmt --all -- --check

Les preuves brutes sont conservées sur Shrek sous
/srv/builds/jp-evidence/jp-4133-20260917-suite/.

## Limites

Cette PR améliore un diagnostic ; elle ne prétend pas réparer le montage
de #4133 ni démontrer la cause de l'incident. L'erreur de démarrage reste
bloquante. Le panic du bootstrap, les échecs PRAGMA et les ouvertures du pool
de lecture restent hors périmètre. Elle ne valide pas les verrous SQLite
d'un NAS, l'image Docker distribuée ni le lancement sur Debian du testeur.

Pour identifier le cas réel, il faut encore le type/source/options du montage
/data, le compte effectif du conteneur et les propriétaires/droits du parent
et de la base existante. Ne pas demander un dump complet de l'environnement
du conteneur : il peut contenir des secrets.
