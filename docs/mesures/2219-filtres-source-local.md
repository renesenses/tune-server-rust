# Les filtres `source = 'local'`, recensés un par un

Premier livrable de la **phase 0** du chantier
[`unifier-serveurs-upnp-et-bibliotheque`](../chantiers/unifier-serveurs-upnp-et-bibliotheque.md),
qui demandait de « compter précisément les 41 filtres `source = 'local'` un par un, et dire
pour chacun s'il doit rester exclusif ou s'ouvrir ».

Mesuré sur le tag **v0.9.149**, le 14/09/2026.

## Le compte réel : 46, pas 41

```
git grep -n -E "source *(=|==) *['\"]local['\"]" v0.9.149 -- tune-core/src tune-server/src
```

**49 occurrences brutes**, dont 3 hors périmètre (2 dans `signal_path_tests.rs`, 1 dans
`migrations.rs`). Reste **46 en production**.

L'écart avec le chiffre annoncé — 41 — vient probablement d'un comptage limité aux `WHERE`
SQL. Il ne l'était pas : cinq de ces occurrences **écrivent** au lieu de filtrer.

## Ce ne sont pas 46 filtres. Ce sont quatre familles.

| famille | nombre | ce que c'est |
|---|---|---|
| **filtres SQL** | 27 | un `WHERE` qui exclut le distant d'une requête |
| **tests Rust** | 10 | `source == "local"` dans une branche de code |
| **écritures** | **5** | **assignent** `source = 'local'` à une piste |
| commentaires | 8 | de la documentation, aucun effet |

### Les cinq écritures — à traiter en premier

Ce sont elles qui décident qu'une piste **est** locale. Tant qu'elles marquent en dur, aucune
piste distante ne peut exister durablement, quel que soit le sort des 37 lectures.

| fichier | ligne | geste |
|---|---|---|
| `db/track_repo.rs` | 994 | `UPDATE tracks SET source = 'local' WHERE id = …` — l'adoption explicite |
| `library/duplicate_detector.rs` | 338 | `t.source = "local"` |
| `library/folder_playlists.rs` | 300 | `t.source = "local"` |
| `library/folder_playlists.rs` | 307 | `cue.source = "local"` — le cas CUE |
| `playback/mod.rs` | 1964 | `track.source = "local"` |

Les quatre dernières sont des **valeurs par défaut posées à la construction**, pas des
décisions. Une piste fabriquée par le détecteur de doublons ou par les playlists de dossier
naît locale parce que rien d'autre n'a jamais existé.

👉 **C'est le vrai point de départ de la phase 2** : tant que `folder_playlists` et
`duplicate_detector` marquent en dur, indexer une source UPnP produira des lignes que la
première passe de scan réécrira en `local`.

### Où se concentrent les 27 filtres SQL

| fichier | n | ce qu'il exclut |
|---|---|---|
| `db/track_repo.rs` | 6 | chemins de fichiers, empreintes audio, paires hash/album |
| `db/album_repo.rs` | 3 | listes d'albums, albums sans biographie, sans MBID |
| `routes/system/enrich.rs` | 3 | l'enrichissement de métadonnées |
| `routes/system/scan.rs` | 2 | le scan |
| `routes/library/*` | 3 | pistes, albums, statistiques |
| autres | 10 | export, config, playlists, auto-fix, playlists de dossier |

**Aucun n'est arbitraire** : ils excluent tous une opération qui suppose un fichier sur disque
— lire un chemin, calculer une empreinte, réécrire un tag. Ils resteront donc exclusifs en
grande partie.

### Les dix tests Rust — c'est là que se joue la lecture

`transport.rs` (3), `playback.rs` (3), `background.rs` (1), `zones.rs` (1), `stats.rs` (1),
`track_repo::est_locale` (1).

Ceux de `transport.rs` et `playback.rs` **décident du chemin de lecture**. Ce sont eux que la
décision **D4** — « une piste distante est-elle jouable, et par quelles sorties ? » — vient
trancher. Les toucher sans D4 serait prématuré.

## Verdict par famille

| famille | doit-elle s'ouvrir ? |
|---|---|
| les 5 **écritures** | **oui, et d'abord** — sinon rien ne tient |
| les 10 **tests Rust** | **seulement après D4** — ils portent la jouabilité |
| les 27 **filtres SQL** | **majoritairement non** : ils gardent des opérations qui exigent un fichier réel |
| les 8 commentaires | à mettre à jour quand le reste bouge |

## Ce que cette mesure ne dit pas

- **Elle ne liste pas les requêtes qui devraient filtrer et ne le font pas.** Une requête sans
  `source` traite déjà le distant comme du local, en silence. Ce recensement est celui des
  gardes posées, pas des gardes manquantes — et c'est la moitié la plus dangereuse.
- Elle ne dit rien des **41 filtres** annoncés dans le document de chantier : soit ce chiffre
  visait un autre périmètre, soit il datait d'une version antérieure.
