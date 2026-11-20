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
| `douteux` | **« Source altérée »** (#9067) : une signature contredit ce que le fichier déclare (`raison`) | « Source altérée », **seulement si le réglage le montre** |
| `non_analyse` | aucune mesure, rien de concluant, ou un « douteux » masqué | aucun |

Le verdict d'un album suit celui de ses pistes mesurées (`verdict_d_album`) :
**au moins la moitié** de ses pistes mesurées douteuses le rendent douteux
(#9035, décision du 18/11/2026, fil 2902 : deux bonus douteux sur dix pistes
ne font pas un album douteux) ; sinon, les pistes douteuses mises de côté,
toutes les autres pistes mesurées vérifiées le rendent vérifié, une conforme
le rend conforme, rien de mesuré ou rien que du non concluant le laisse non
analysé. Les pistes pas encore mesurées et les non concluantes ne comptent
pas. Un album qui n'est pas douteux mais a des pistes douteuses le dit
(`pistes_douteuses`, `raison_pistes`) ; chaque piste douteuse garde son
verdict.

**« Source altérée » couvre tous les formats, CD compris** (#9067, décision
de Bertrand du 19/11, fil 2915 ; elle remplace celle du 18/11, « un CD n'est
jamais Hi-Res douteux »). Le mot rangé et publié reste `douteux` : seuls les
libellés changent (« Source altérée »). Un CD (16 bits, 48 kHz au plus) n'est
altéré que sur la signature d'un codec avec perte : une coupure **en mur**
(au moins 40 dB de chute entre les 500 Hz sous le bord et tout ce qui suit
1 kHz au-dessus, jusqu'à Nyquist ; dans au moins la moitié des fenêtres en
cause) entre **15 kHz et 20,5 kHz** (passe-bas d'un encodeur : ~16 kHz à
128 kbit/s, 19-20 kHz à 320). Jamais sur une pente douce avant 20 kHz (vieille
bande, master sombre), un filtre qui n'est pas un mur, ni un mur sous 15 kHz.
Raison : `mur_de_codec`. Les règles des fichiers qui se disent hi-res ne
changent pas.

Lignes déjà rangées : un CD rangé `douteux` sous une autre raison que
`mur_de_codec` (jugé sur la seule coupure, avant #9067) se LIT `conforme`
partout et la passe le REMESURE une fois ; un CD rangé `conforme` avec une
coupure sous 20,5 kHz et sans raison (jugé du 18 au 19/11) est remesuré aussi.
Après remesure, la ligne porte `mur_de_codec` (altérée) ou, conforme, la
raison interne `coupure_sans_mur` (jamais publiée) : elle n'est plus
candidate. La règle de la moitié se compte sur **toutes** les pistes mesurées
concluantes (vérifiées, conformes ou altérées) : une piste altérée sur dix ne
fait pas un album altéré.

« Douteux » masqué : un album qui a la moindre piste douteuse (même sous la
moitié) se publie `non_analyse`, jamais `verifie` ni `conforme` — dans la
liste, son filtre, les règles intelligentes et le catalogue.

Raisons d'un « douteux » (#9035) :

| Raison | Ce qui a été mesuré |
|---|---|
| `bits_de_remplissage` | les bits de poids faible sont toujours nuls |
| `bande_de_cd` | un hi-res sans rien au-dessus de 20 à 24 kHz |
| `coupure_spectrale` | le spectre coupé net sous 20 kHz, SANS signature de codec : origine possible, codec avec perte ou filtrage au mastering |
| `mur_de_codec` | #9067 — un CD coupé en mur entre 15 et 20,5 kHz (`coupure_hz`) : la signature d'un encodeur avec perte |
| `signature_de_codec` | la coupure, ET des bandes de l'aigu vidées par intermittence sous elle : des trous **compatibles** avec un codec avec perte (calibré sur un encodeur simulé ; le libellé reste prudent tant qu'il n'est pas calibré sur de vrais fichiers) |

`coupure_de_codec`, rangé avant #9035 sur la seule fréquence de coupure, n'est
plus publié : une ligne qui le porte se publie `coupure_spectrale`. Un client
qui le reçoit d'un serveur plus ancien le lit comme `coupure_spectrale`. La
passe de fond REMESURE une fois ces pistes-là (après les pistes jamais
mesurées) ; les autres mesures ne sont pas touchées.

**Publier le serveur et le client web ensemble** : un client antérieur à #9035
ne connaît ni `coupure_spectrale` ni `signature_de_codec` et montre, sur un
badge « douteux », l'infobulle de « vérifié ».

Ce que la fiche `GET /library/albums/{id}/hires` rend en plus (#9035), `null`
sans piste douteuse ou tant que « douteux » est masqué — l'album lui-même
douteux ou non :

| Champ | Valeur |
|---|---|
| `pistes_douteuses` | combien de pistes de l'album sont douteuses |
| `pistes_mesurees` | sur combien de pistes mesurées concluantes (vérifiées, conformes ou altérées, #9067) |
| `raison_pistes` | la raison la plus fréquente des pistes douteuses (celle de l'album quand il est douteux) |
| `coupure_hz` | médiane des coupures des pistes douteuses qui portent `raison_pistes` |

## « Douteux » masqué par défaut

Le taux de faux positifs doit être mesuré sur une vraie bibliothèque avant de
montrer un badge « douteux ». Tant que le réglage `faux_hires_douteux_visible`
ne vaut pas `true`, un album ou une piste douteux se publie `non_analyse`, sans
raison, **partout** : liste des albums, filtre, règles intelligentes et fiche
`GET /library/albums/{id}/hires`. Rien ne le distingue d'un album pas encore
mesuré. Le verdict reste en base ; poser le réglage le montre partout.

| Route | Corps | Réponse |
|---|---|---|
| `GET /library/hires-check` | — | `{"active", "douteux_visible", "restantes", "rangees", "catalogue_hires_verdict", "source_alteree"}` |
| `POST` ou `PUT /library/hires-check` | `{"active"?, "douteux_visible"?}` | `{"active", "douteux_visible"}` |

Les deux champs sont facultatifs : un champ absent garde sa valeur, sauf un
cas (rc12, décision du 13/11/2026). `{"active": true}` seul — ce qu'envoie
l'interrupteur « Détecter les faux Hi-Res » de Réglages › Bibliothèque —
arme la passe ET montre « douteux » ; `{"active": false}` seul la désarme et
le masque. Un `douteux_visible` fourni dans le même corps l'emporte. Sans geste
de l'utilisateur, rien ne change : passe éteinte, « douteux » masqué.

## Compatibilité « Source altérée » (#9067)

Aucun champ ne change de nom ni de valeur : `douteux`, `douteux_visible`,
`hires_verdict`, `faux_hires_douteux_visible` restent. En plus :

- `GET /library/hires-check` rend `"source_alteree": true` : le client web
  relibelle « Hi-Res douteux » en « Source altérée » devant un serveur qui le
  dit ;
- `altere` (et `alteree`) est accepté comme ALIAS d'entrée de `douteux` :
  `?hires_verdict=altere`, le catalogue, le critère `hires_verdict` des règles
  et la requête texte (`hires:altere`). Le mot publié reste `douteux` ;
- une raison de plus, `mur_de_codec`.

## Liste des albums : `GET /library/albums`

Chaque ligne de `items` porte deux champs de plus :

| Champ | Type | Valeur |
|---|---|---|
| `hires_verdict` | chaîne | `verifie`, `conforme`, `douteux` ou `non_analyse` |
| `hires_raison` | chaîne ou `null` | la raison d'un `douteux` montré, `null` sinon |
| `hires_pistes_douteuses` | entier ou `null` | #9035 — pistes douteuses de l'album (douteux ou non), `null` sans piste douteuse ou masqué |
| `hires_pistes_mesurees` | entier ou `null` | #9035 — sur combien de pistes mesurées |
| `hires_raison_pistes` | chaîne ou `null` | #9035 — la raison la plus fréquente des pistes douteuses |

Paramètre de filtre `?hires_verdict=<verdict>` : seuls les albums dont le
`hires_verdict` publié est celui-là. Il se combine aux autres facettes, au tri,
à la pagination et à la recherche `q`. Avec ce filtre, `total` est le nombre
d'albums filtrés (comme pour une recherche). Tant que « douteux » est masqué,
`?hires_verdict=douteux` ne rend rien et `?hires_verdict=non_analyse` rend
aussi les albums douteux. Un mot inconnu ne rend rien, jamais toute la
bibliothèque. Sans le paramètre, la réponse est celle d'avant, plus les deux
champs.

## Catalogue imprimable : `GET /export/catalogue.html` (rc12)

`?hires_verdict=verifie,douteux` (un ou plusieurs mots, séparés par des
virgules) : les albums de ces verdicts publiés, mêmes prédicats que la liste
(« douteux » masqué ne rend rien, un mot inconnu non plus). C'est une valeur
de plus de la facette Qualité : OU avec `quality`, comme les pastilles de la
Bibliothèque. `GET /library/hires-check` rend `"catalogue_hires_verdict": true`
sur un serveur qui le sait ; le client web n'active l'export d'un filtre
« Hi-Res vérifié / douteux » que s'il le lit.

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
