# Faux hi-res : contrat entre le serveur et les clients (deuxième étape)

Référence : #6285. La première étape a livré la mesure, la passe de fond et le
verdict de chaque piste (`hires_verifications`, `GET /library/albums/{id}/hires`).
Ce document fixe ce que la deuxième étape publie : le verdict dans la liste des
albums, son filtre et le critère des règles intelligentes.

## Les verdicts publiés

| Valeur | Sens | Badge |
|---|---|---|
| `verifie` | se dit hi-res et l'est | « hi-res vérifié » |
| `conforme` | qualité CD déclarée, rien de suspect | aucun |
| `douteux` | une signature contredit ce que le fichier déclare (`raison`) | « douteux », **seulement si le réglage le montre** |
| `non_analyse` | aucune mesure, rien de concluant, ou un « douteux » masqué | aucun |

Le verdict d'un album suit celui de ses pistes mesurées (`verdict_d_album`) :
une piste douteuse le rend douteux ; sinon, toutes les pistes mesurées
vérifiées le rendent vérifié ; une conforme le rend conforme ; rien de mesuré
ou rien que du non concluant le laisse non analysé. Les pistes pas encore
mesurées ne comptent pas.

Raisons d'un « douteux » : `bits_de_remplissage`, `bande_de_cd`,
`coupure_de_codec`.

## « Douteux » masqué par défaut

Le taux de faux positifs doit être mesuré sur une vraie bibliothèque avant de
montrer un badge « douteux ». Tant que le réglage `faux_hires_douteux_visible`
ne vaut pas `true`, un album ou une piste douteux se publie `non_analyse`, sans
raison, **partout** : liste des albums, filtre, règles intelligentes et fiche
`GET /library/albums/{id}/hires`. Rien ne le distingue d'un album pas encore
mesuré. Le verdict reste en base ; poser le réglage le montre partout.

| Route | Corps | Réponse |
|---|---|---|
| `GET /library/hires-check` | — | `{"active", "douteux_visible", "restantes", "rangees"}` |
| `POST /library/hires-check` | `{"active"?, "douteux_visible"?}` | `{"active", "douteux_visible"}` |

Les deux champs du `POST` sont facultatifs : un champ absent garde sa valeur.
`{"active": true}` seul se comporte comme avant.

## Liste des albums : `GET /library/albums`

Chaque ligne de `items` porte deux champs de plus :

| Champ | Type | Valeur |
|---|---|---|
| `hires_verdict` | chaîne | `verifie`, `conforme`, `douteux` ou `non_analyse` |
| `hires_raison` | chaîne ou `null` | la raison d'un `douteux` montré, `null` sinon |

Paramètre de filtre `?hires_verdict=<verdict>` : seuls les albums dont le
`hires_verdict` publié est celui-là. Il se combine aux autres facettes, au tri,
à la pagination et à la recherche `q`. Avec ce filtre, `total` est le nombre
d'albums filtrés (comme pour une recherche). Tant que « douteux » est masqué,
`?hires_verdict=douteux` ne rend rien et `?hires_verdict=non_analyse` rend
aussi les albums douteux. Un mot inconnu ne rend rien, jamais toute la
bibliothèque. Sans le paramètre, la réponse est celle d'avant, plus les deux
champs.

## Règles intelligentes : critère `hires_verdict`

Même nom aux deux niveaux, famille `hires_verdict`, opérateurs `=` et `!=`,
valeur `verifie`, `conforme`, `douteux` ou `non_analyse`.

- Collection (albums) : le verdict publié de l'album, celui de son badge dans
  la liste.
- Playlist (pistes) : le verdict de la piste ; une piste sans mesure ou non
  concluante est `non_analyse`.

« Douteux » suit le même réglage. Un verdict ou un opérateur inconnu rend
FAUX ; dans l'aperçu d'une playlist, la règle figure parmi les refusées.

Requête texte : `hires:verifie`, `hires!=douteux`.

## Pour le client web

- `smartRegles.ts` : ajouter le critère `hires_verdict` (famille
  `hires_verdict`, opérateurs `=` / `!=`, liste fermée des quatre valeurs). Le
  compte des critères du serveur passe à 35, dont 33 aux collections
  (`criteres::tests::le_tableau_a_la_taille_de_celui_du_client`).
- La grille des albums peut lire `hires_verdict` / `hires_raison` comme
  `badgeHiRes.ts` lit la fiche, et proposer le filtre `?hires_verdict=`.
- `docs/contrat-web.json` est régénéré depuis le client : il n'y figurera
  qu'une fois que le client lira ces champs. L'entrée existante
  `GET /library/albums/{}/hires` (`VerdictHiRes` : `verdict`, `album_id`,
  `raison`) reste exacte.
