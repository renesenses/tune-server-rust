# Apports Qobuz aux albums locaux : contrat (#8077)

Quand un album de la bibliothèque locale existe aussi sur Qobuz, Tune complète
ses métadonnées depuis Qobuz, **sans jamais écraser une donnée locale**.
Chaque donnée reprise porte sa source, `"qobuz"`. Rien n'est écrit dans les
fichiers audio ni dans les colonnes de l'album : les apports vivent dans les
tables `apports_service_albums` et `apports_service` (migrations SQLite 141,
PostgreSQL 107), et les crédits repris dans `track_credits` avec
`source = 'qobuz'`. Tout se retire d'un geste.

Aucun appel sans compte Qobuz activé et connecté. Tous les appels passent par
le limiteur partagé `tune_core::http::fetch::QOBUZ` (2 requêtes/s).

## Rapprochement

1. Code-barres de l'album (UPC/EAN) ; sinon celui de la release MusicBrainz
   gardée en base pour son MBID ; sinon les ISRC de ses titres (un seul album
   Qobuz doit les porter tous). Méthodes `upc`, `mbid`, `isrc`.
2. Sinon artiste et titre normalisés égaux, même nombre de pistes, un seul
   album Qobuz : méthode `titre`. Toutes les pistes locales doivent alors se
   rapprocher.

Pistes : disque et numéro, durée égale à 3 s près (ou 2 % de la durée) ;
sinon l'ISRC.

Un rapprochement douteux n'est jamais appliqué : `statut = "a_confirmer"`, avec
un `motif` parmi `plusieurs_editions`, `nombre_de_pistes`, `titre_approche`,
`compilation`, `pistes_differentes`.

## Données reprises (seulement si absentes en local)

| Champ (`champs.<nom>.valeur`) | Absent en local quand… |
|---|---|
| `label` | `albums.label` vide |
| `date_de_sortie` (`AAAA-MM-JJ`) | pas de date exacte, et l'année locale (si connue) est la même |
| `upc` | `albums.barcode` vide |
| `phonogramme` (℗), `copyright` (©) | aucune piste ne porte de mention `copyright` |
| `genres` (liste) | ni `genre` ni `genres` |
| `livrets` (`[{name, url}]`) | aucun PDF dans le dossier de l'album |
| `recompenses` (`[{nom, publication?, date?}]`) | toujours (pas d'équivalent local) |
| `hires` (`{bit_depth, sample_rate}`) | toujours, si Qobuz l'annonce |
| `pochette_hd` (`{url, cote_min}`) | pochette absente, ou plus petite que le côté garanti par Qobuz (600 px) |
| piste : `isrc` | `tracks.isrc` vide |
| crédits (interprètes, compositeur, producteur…) | rôle absent pour la piste dans toute autre source (balises, MusicBrainz, Roon, saisie, compositeur de la piste) |

Limite connue : la taille réelle de la pochette `_max` de Qobuz n'est connue
qu'au téléchargement ; une pochette locale de 600 px ou plus n'est pas
remplacée.

## Routes

Toutes sous `/api/v1/library`.

### `GET /albums/{id}/apports-qobuz[?rafraichir=true]`

Le rapprochement et ses apports. Calculés à la demande s'ils ne l'ont jamais
été ; `rafraichir=true` les recalcule (sauf après un retrait). 404 : album
inconnu.

```json
{
  "source": "qobuz",
  "statut": "applique",            // applique | a_confirmer | aucun | retire | null (jamais examiné)
  "methode": "upc",                // upc | mbid | isrc | titre | confirme
  "service_album_id": "0123456789012",
  "service_url": "https://open.qobuz.com/album/0123456789012",
  "motif": null,
  "pistes_locales": 10,
  "pistes_rapprochees": 10,
  "verifie_le": "2026-11-07T10:00:00Z",
  "service_connecte": true,
  "champs": { "label": { "valeur": "…", "source": "qobuz" } },
  "pistes": { "42": { "isrc": { "valeur": "FRXXX2600001", "source": "qobuz" } } }
}
```

### `DELETE /albums/{id}/apports-qobuz`

Retire tous les apports Qobuz de l'album : champs et crédits. Le
rapprochement reste au statut `retire` : ni l'ouverture de la fiche ni la
passe de fond ne les remettent. Réponse :
`{"statut": "retire", "champs_retires": n, "credits_retires": m}`.

### `POST /albums/{id}/apports-qobuz/confirmer`

Corps facultatif `{"service_album_id": "…"}` (sinon l'album Qobuz du
rapprochement rangé). Applique ce rapprochement (`methode = "confirme"`), y
compris après un retrait. 400 : rien à confirmer ; 409 : Qobuz non connecté.

### `POST /apports-qobuz/passe`

Lance la passe de fond (albums jamais examinés, 500 au plus) : `{"lancee":
true|false}`. Elle tourne aussi seule, 15 minutes après le démarrage puis
toutes les six heures. Inscrite au registre des tâches sous `apports_qobuz`,
elle cède à la pause « Enrichissement » et à la lecture.

## Champs ajoutés aux réponses existantes

- `GET /albums/{id}` : `apports_qobuz` (même forme que ci-dessus, sans
  `service_connecte`), absent si l'album n'a jamais été examiné. L'ouverture
  lance le calcul en arrière-plan s'il n'a jamais eu lieu.
- `GET /albums/{id}/tracks` : `apports_qobuz` sur chaque piste qui en a
  (`{"isrc": {"valeur", "source"}}`).
- `GET /tracks/{id}/credits`, `/albums/{id}/credits`, `/artists/{id}/credits` :
  une ligne reprise de Qobuz porte `"source": "qobuz"` ; elle n'est pas rendue
  quand une autre source porte le même crédit.
