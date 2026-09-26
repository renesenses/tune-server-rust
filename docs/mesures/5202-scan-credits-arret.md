# #5202 — crédits hors transaction et arrêt du scan manuel

Base : `f136a8f2003b7a79d33158cff8c60c2e2404683b` (v0.9.166).
Session : Bertrand / OpenAI Codex / support-nuit-20260926-5202.
Worktree Shrek : `/srv/builds/bertrand/worktrees/codex-5202-20260926`.
Cible Cargo : `bertrand-codex-5202-20260926`, six jobs.

## Défauts reproduits

Le scan manuel relisait les crédits de chaque fichier **dans** la transaction
SQLite du lot. Une lecture lente du partage prolongeait donc cette transaction.
L'arrêt n'était examiné qu'avant le lot ; même les lots suivants étaient lus
avant que leur import ne soit sauté.

Les témoins serveur exécutent le vrai scan manuel, sur trois WAV et une base
SQLite **de fichier**. Le lecteur de crédits est injectable : il mesure
`is_autocommit()`, simule une lecture lente de 2,1 s et rend des crédits connus.
Le second témoin demande l'arrêt pendant la première lecture. Une piste absente
du disque mais déjà en base sert de témoin contre une purge après annulation.

Le témoin du parcours présente deux lots : le premier demande l'arrêt, le
second ne doit être ni lu ni importé.

## Correctif

- Les crédits sont préchargés avant le `BEGIN`, puis écrits avec les identifiants
  lus sur la connexion d'écriture après insertion des pistes. La sélection des
  fichiers à relire (#5043) reste identique.
- La prélecture s'arrête entre fichiers et avant l'ouverture de la transaction.
  Le parcours manuel ne commence plus les fichiers/lots suivants après l'arrêt.
- La prélecture émet le dossier/fichier courant et son compteur, au départ puis
  au plus toutes les deux secondes entre lectures. Les compteurs d'import
  restent ceux des lots terminés. Le client publié fusionne ces événements et
  affiche `current_dir` ; les champs `metadata_read` et `metadata_total` sont
  disponibles dans l'événement, sans prétendre qu'ils sont déjà affichés.
- Le bilan final conserve `cancelled: true`. L'enrichissement n'est pas lancé
  après annulation ; les nouvelles passes de pochettes et de playlists sont
  évitées.

## Contre-épreuve

Commande, avec l'environnement Shrek chargé :

```sh
cargo test -p tune-server -p tune-core --lib --no-default-features \
  --features oaat --no-fail-fast 5202 -- --nocapture --test-threads=2
```

Sans modifier les tests : la prélecture est déplacée après `BEGIN`, les gardes
d'arrêt de la prélecture et du parcours sont neutralisées. Le code compile.
Les trois témoins échouent, sortie Cargo 101 :

- `les_lectures_de_credits_ne_tiennent_pas_la_transaction_sqlite_5202` :
  « #5202 : la lecture réseau des crédits retient la transaction SQLite »,
  **3 lectures sous transaction au lieu de 0**.
- `arreter_pendant_les_credits_ne_lit_pas_le_reste_du_lot_et_ne_purge_pas_5202` :
  « #5202 : Arrêter doit interrompre les crédits entre deux fichiers, pas après
  le lot entier », **3 lectures au lieu de 1**.
- `arret_apres_un_lot_ne_lit_pas_le_suivant_5202` :
  « #5202 : aucun lot après l'arrêt », **2 lots au lieu de 1**.

Restauration des trois sources par `cp` depuis leurs sauvegardes, puis même
sélection : **3 tests réussis**. Les SHA-256 des quatre sources, tests compris,
sont identiques entre Shrek et le worktree local après restauration.

Journaux : `/srv/builds/bertrand/codex-5202-preuves/contre.log` et
`restaure.log`. La compilation initiale et les huit tests de la porte du scan
sont dans `/srv/builds/bertrand/codex-5202-baseline.log`.

## Vérifications complémentaires

- Formatage : réussi.
- Parcours `scanner::walker::` : **49 tests réussis**.
- Régressions serveur (`scan_`, dont les métadonnées étendues #5043) : **112 tests réussis**.
- Clippy, commande complète de la CI avec `-D clippy::correctness` : réussi
  (246,85 s, avertissements non bloquants conservés).

## Limites

Le montage SMB Windows du testeur n'est pas reproduit : cette note ne clôt pas
son gel à 97 %. Un appel système déjà bloqué doit revenir avant que l'arrêt
soit observé ; aucun fil d'E/S n'est abandonné. Les lectures de pochettes de
l'importeur et les requêtes SQLite restent synchrones. Le coffret Bernstein,
le scan automatique de démarrage et l'isolation générale des autres écrivains
SQLite (#5120) ne sont pas corrigés ici. Aucun binaire n'est déployé.
