# Langage de requête des règles intelligentes (#6287)

Décision de Bertrand du 13/10, rattachée au chantier artistes multiples
(#6216). L'idée vient d'Aède, qui fait des requêtes relationnelles. Le but :
écrire en une ligne ce que l'éditeur visuel compose règle par règle, et aller
au-delà avec des combinaisons booléennes.

## Principe

Une requête textuelle se **traduit vers les règles de l'éditeur**. Chaque
critère devient une règle `{field, op, value}`, que le moteur des playlists
(`build_smart_query_rapport`) traduit comme il traduit celles de l'éditeur.
Le langage n'écrit aucun SQL de critère. Il combine seulement les
conditions que le moteur lui rend, avec ET, OU et NON.

Code :
- `tune-smart-http/src/requete_texte.rs` : la grammaire et le moteur ;
- `POST /api/v1/library/smart-playlists/requete` : la route.

## Grammaire

```text
requete   := ou
ou        := et ( OU et )*                    OU  : « ou », « or », « | »
et        := unaire ( [ET] unaire )*          ET  : « et », « and », « & », ou rien
unaire    := NON unaire | atome               NON : « non », « not », « ! », « - »
atome     := "(" ou ")" | credit | motcle | critere
credit    := credit "(" cle ":" valeur ("," cle ":" valeur)* ")"
             cle : artiste | role | instrument     (sur la MÊME ligne de crédit)
motcle    := favori | jamais_ecoute
critere   := champ op valeur
op        := ":" | "=" | "!=" | ">" | ">=" | "<" | "<="
valeur    := mot | "chaîne" | bas..haut
```

ET lie plus fort que OU : `a b ou c` se lit `(a ET b) OU c`.

### Ce que `:` veut dire

| Famille | `:` | Autres |
|---|---|---|
| texte (genre, compositeur…) | contient | `=` égal, `!=` différent, `:vide`, `!=vide` |
| nombre (année, durée…) | égal ; `a..b` entre | `>` `>=` `<` `<=` `!=` ; la durée accepte `90s`, `5min`, `1h`, `4:30` |
| écoutes | égal ; `a..b` entre | `>` `>=` `<` `<=` (« moins de » devient « entre 0 et … » : une piste jamais écoutée a zéro écoute) |
| date (ajout, dernière écoute) | `:30j` (30 derniers jours), `:jamais` ; `a..b` entre | `>` après, `<` avant (strictes : `>=` et `<=` sont refusés) |
| étiquette | par identifiant | `!=` |
| favori | `favori`, `favori:album`, `favori:artiste` | `!=` |

Les noms de champ s'écrivent en français, avec ou sans accent (`annee`,
`année`, `compositeur`, `derniere_ecoute`, `duree`, `ecoutes`, `etiquette`,
`interprete`…). Ils s'écrivent aussi sous leur nom de règle (`year`,
`composer`, `track_artist`…).

### La négation

`non a` est le complément EXACT de `a`. Une piste que `a` ne retient pas passe
`non a`, y compris quand c'est parce que sa valeur est absente. Le SQL d'une
règle peut valoir NULL, d'où `NOT COALESCE((a), 1 = 0)` : c'est la même
leçon que « ne contient pas » (#6250).

## Exemples

```text
credit(artiste: "Bill Evans", instrument: piano)
favori jamais_ecoute
genre:jazz annee:1955..1965 non credit(role: producer)
(compositeur:Bach ou compositeur:Haendel) duree>10min
ecoutes>=5 derniere_ecoute<2025-01-01
```

### Les bornes

Une requête fait au plus 2 000 caractères et 32 niveaux imbriqués
(parenthèses et négations cumulées). Au-delà, la route rend `400
requete_invalide`.

Sous une négation, un critère que le moteur refuse (une plage de dates
illisible, par exemple) vaudrait FAUX, et sa négation VRAI : la requête
rendrait toute la bibliothèque. Elle est donc refusée entière, en `400
requete_invalide` avec `position: null`. Hors négation, le refus ne retient
rien et reste signalé dans `regles_refusees`, comme à l'aperçu.

## Vers l'éditeur visuel

Une requête plate a une forme d'éditeur : la route rend `rules` et
`match_mode`. C'est le cas d'une suite de critères joints par ET (`all`), ou
seulement par OU (`any`). Une requête imbriquée ou niée n'en a pas, car
l'éditeur n'a qu'un niveau : `rules` et `match_mode` valent `null`, et la
requête reste exécutable.

## Enregistrer une requête (étape 4)

Une requête **plate** s'enregistre comme aujourd'hui : ses règles d'éditeur,
dans `rules`, avec son `match_mode`.

Une requête **imbriquée ou niée** s'enregistre en UNE règle, dont la valeur
est le texte de la requête :

```json
{"field": "requete", "op": "is", "value": "genre:jazz (annee:1959 ou non credit(instrument: piano))"}
```

La route `/requete` rend ce qu'il faut enregistrer dans `enregistrement`
(`{rules, match_mode}`, jamais `null`) : les règles d'éditeur d'une requête
plate, la règle `requete` (en `all`) d'une imbriquée. L'éditeur le passe tel
quel à `POST /library/smart-playlists` (ou `PUT /{id}`).

- **Recharger.** `GET /library/smart-playlists/{id}` rend `rules` tel
  qu'enregistré. Une seule règle de champ `requete` : l'éditeur ouvre
  « Requête avancée » avec sa valeur.
- **Évaluer.** Le moteur des playlists traduit la règle `requete` comme la
  route : les pistes, les albums, l'aperçu et une playlist qui référence
  celle-ci la lisent de la même façon, sur SQLite comme sur PostgreSQL. Elle
  peut côtoyer d'autres règles, comme une règle ordinaire.
- **Refuser.** À l'enregistrement, une requête qui ne se lit pas, ou qui
  refuse un critère sous une négation, rend `400` avec le corps de la route
  `/requete` (`requete_invalide`, `message`, `position`). Rien n'est écrit.
- **Échouer fermé.** Une règle `requete` qui ne se lit plus (un serveur plus
  ancien, une grammaire changée) ne retient rien et se nomme dans
  `regles_refusees` (`requete is`), jamais toute la bibliothèque.
- **Cycles.** Une référence écrite dans le texte (`playlist:"smart:7"`)
  compte pour la détection des références circulaires.

Pourquoi une règle plutôt qu'une colonne ou un arbre JSON : la colonne
`rules` est déjà du texte sur les deux bases (aucune migration), tous ses
lecteurs voient la règle, et l'éditeur relit le texte que l'utilisateur a
écrit. Code : `tune-smart-http/src/requete_enregistree.rs`.

`docs/contrat-web.json` est régénéré depuis le client web
(`scripts/web-contract-map.py`) : il citera `enregistrement` quand l'éditeur
le lira. Jusque-là, c'est cette section qui décrit le contrat.

## Étapes suivantes

1. **Crédits par piste du chantier artistes multiples** (`track_artists`).
   Ils ne sont pas dans le lot rc7. Quand ils y entreront, la règle `artist`
   les lira déjà (deuxième étape du chantier), sans rien changer ici.
2. **Étiquette par nom** (`etiquette:"J'adore"`), résolue en base. Elle se
   donne aujourd'hui par identifiant.
3. **Taille de fichier** (« sans perte au-delà de N Mo »). Ce n'est pas un
   critère du moteur aujourd'hui : il faut une colonne de taille, puis un
   critère dans `criteres.rs`.
4. ~~**Enregistrer une requête imbriquée** comme playlist intelligente.~~
   Fait : la règle `requete`, voir « Enregistrer une requête (étape 4) ».
5. **Interface** : un champ « requête avancée » dans l'éditeur, qui affiche
   la position de l'erreur rendue par la route.
6. Les favoris de service et les pistes de catalogue (règles `source`,
   `catalogue:`), que l'aperçu ajoute, ne sont pas ajoutés par la route
   textuelle.
