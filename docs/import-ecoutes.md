# Importer des nombres d'écoutes (#7763)

Tune compte les écoutes d'une piste dans son historique. Un import peut y
ajouter les écoutes faites ailleurs, sans fabriquer de fausses entrées
d'historique.

## Où vont les écoutes importées

Elles vont dans une table à part, `imported_plays` (migration SQLite 136,
PostgreSQL 101), qui contient une ligne par piste :

- `play_count` : la somme des écoutes importées. Un import **ajoute** :
  importer deux fois 5 écoutes en donne 10.
- `last_played_at` : la date importée la plus récente, au format de
  l'historique (`AAAA-MM-JJTHH:MM:SSZ`).

Le nombre d'écoutes affiché pour une piste (`play_count` dans les listes de
pistes, tri « # écoutes ») vaut les écoutes de l'historique **plus** les
écoutes importées. La dernière écoute affichée est la plus récente des deux
dates. Une date importée plus ancienne ne change donc rien à ce qui est
affiché.

L'Historique, lui, ne montre que les écoutes réellement faites dans Tune.

## Roon et Plex : `POST /api/v1/system/import/roon` et `/plex`

On utilise les routes d'import existantes, avec un fichier envoyé en
multipart (partie `file`). Pour chaque ligne de l'export **rapprochée d'une
piste déjà présente** dans la bibliothèque :

- le nombre d'écoutes de l'export (Roon : colonne `Play Count` ; Plex :
  `viewCount`) s'ajoute à celui de la piste ;
- la date de dernière écoute (Roon : colonne `Last Played` ; Plex :
  `lastViewedAt`) est reportée si elle est plus récente ;
- la note (1 à 5) remplace celle de la piste si elle est différente.

Une piste que l'import crée ne reçoit ni écoutes ni note. Si plusieurs lignes
renvoient à la même piste, seule la première compte.

Le rapport (réponse de l'aperçu, ou `GET /system/import/status/{task_id}`)
donne :

| Champ | Sens |
|---|---|
| `matched` / `unmatched` | lignes trouvées / non trouvées dans la bibliothèque |
| `play_counts_updated` | pistes qui ont reçu des écoutes |
| `plays_added` | total des écoutes ajoutées |
| `ratings_updated` | notes reportées |
| `history_entries_added` | toujours 0 : rien n'entre dans l'historique |

`?preview=true` calcule les mêmes chiffres et **n'écrit rien**.

## Liste m3u : `POST /api/v1/system/import/m3u-plays?plays=N`

Cette route ajoute `N` écoutes à chaque piste de la bibliothèque trouvée dans
la liste. Elle ne crée pas de liste de lecture.

- Corps : multipart, la liste dans la partie `file` (`.m3u` ou `.m3u8`, en
  UTF-8 ou en Latin-1).
- `plays` : un entier de 1 à 1000, obligatoire. En dehors de ces bornes, la
  réponse est `400` avec `"error": "plays_hors_bornes"`.
- `preview=true` : rend le rapport sans rien écrire.

Les lignes sont rapprochées comme dans l'import de liste de lecture : d'abord
par le chemin exact, puis par une recherche sur le nom de fichier, limitée à
500 recherches. Une adresse de service (http, Qobuz…) compte comme non
trouvée. Une piste citée plusieurs fois ne reçoit `N` écoutes qu'une seule
fois. La liste ne contient aucune date, donc la dernière écoute ne change
pas.

Exemple :

```sh
curl -F file=@favoris.m3u "http://<serveur>/api/v1/system/import/m3u-plays?plays=5&preview=true"
```

Réponse `200` :

```json
{
  "source": "m3u_import", "preview": true,
  "total_rows": 120, "matched": 112, "unmatched": 8, "lookup_errors": 0,
  "duplicates_skipped": 2, "plays_per_track": 5,
  "play_counts_updated": 110, "plays_added": 550, "ratings_updated": 0,
  "history_entries_added": 0,
  "not_found_paths": ["…"], "not_found_truncated": false
}
```

## Limites connues

- Une note importée est écrite dans `tracks.rating`. Si le fichier porte sa
  propre note dans ses balises, le prochain scan la remet à la place.
- L'accueil, les statistiques d'écoute, le DJ automatique, les
  recommandations et le résumé comptent encore l'historique seul.
- Les colonnes Roon acceptées sont une hypothèse : elles n'ont été vérifiées
  sur aucun export réel.
