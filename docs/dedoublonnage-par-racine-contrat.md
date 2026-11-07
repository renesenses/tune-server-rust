# Ne dédoublonner que sur un même support : contrat entre le serveur et le client web

Référence : https://github.com/renesenses/tune-server-rust/issues/6917 (fil 2397,
rattaché à https://github.com/renesenses/tune-web-client/issues/2354).
Arbitrage de Bertrand du 27/10/2026 : une option, éteinte par défaut.

## Le réglage

| Clé | Type | Défaut | Section |
|---|---|---|---|
| `library_dedup_same_root_only` | booléen | `false` | Réglages › Bibliothèque |

- Lecture : `GET /api/v1/system/config` publie la clé même absente de la base,
  avec `false` : la case s'affiche décochée, jamais « inconnue ».
- Écriture : `PATCH /api/v1/system/config` avec
  `{"library_dedup_same_root_only": true}` (ou `false`). Les chaînes `"true"` et
  `"false"` sont acceptées ; toute autre valeur rend 400 avec
  `library_dedup_same_root_only attend true ou false`.

Libellé proposé : « Ne dédoublonner que sur un même support ». Aide :
« Deux copies identiques rangées dans deux dossiers de musique différents
(par exemple le disque et le NAS) restent deux entrées. Aucun fichier n'est
jamais effacé. »

## Ce que l'option change

« Un même support » est un même **dossier de musique déclaré** (`music_dirs`).
Pour un fichier, c'est le dossier déclaré le plus long qui le contient.

| | Option éteinte (défaut) | Option allumée |
|---|---|---|
| Scan : copie identique, même dossier de musique | exemplaire de la piste (#4907) | exemplaire de la piste |
| Scan : copie identique, autre dossier de musique | exemplaire de la piste (#4907) | **piste à part entière** |
| « Nettoyage du serveur » : copie identique, même dossier | ligne retirée | ligne retirée |
| « Nettoyage du serveur » : copie identique, autre dossier | ligne retirée | **ligne gardée** |
| Fichier hors de tout dossier déclaré | comme avant | jamais replié sur un autre |

Dans tous les cas, **aucun fichier n'est effacé** : seules des lignes de la
bibliothèque sont en jeu.

L'option vaut pour les décisions suivantes. Les copies déjà rangées comme
exemplaires le restent jusqu'à une **analyse complète**
(`POST /api/v1/system/scan?force=true`) : celle-ci relit chaque fichier, et la
copie d'un autre dossier devient alors sa propre piste. Éteindre l'option ne
fusionne pas d'office les deux entrées : c'est « Nettoyage du serveur » qui
retire la ligne en trop, comme avant.

## Réponse de `POST /api/v1/system/cleanup`

Un champ s'ajoute, les autres ne changent pas :

| Champ | Type | Sens |
|---|---|---|
| `dedup_same_root_only` | booléen | la règle que le dédoublonnage des pistes vient d'appliquer ; `true` : les copies d'un autre dossier de musique sont restées |

## Ce qui ne change pas

Les vues de la bibliothèque replient toujours, à l'affichage, les pistes d'un
même album qui portent le même disque, le même numéro et le même titre
(`dedup_display_tracks`, #1362). Deux entrées gardées par l'option restent
deux lignes en base, deux pistes que playlists et favoris peuvent viser, mais
la liste des pistes d'un album n'en montre qu'une.

## Témoins

- `tune-core/src/library/dedoublonnage_par_racine_tests.rs` : la règle de
  racine et le nettoyage, option allumée et éteinte (SQLite).
- `tune-server/tests/dedoublonnage_par_racine_6917.rs` : le vrai scan et les
  vraies routes sur deux dossiers de musique, option allumée et éteinte, sous
  SQLite et PostgreSQL (`TUNE_TEST_PG_URL`).
