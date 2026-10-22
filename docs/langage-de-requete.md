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

## Étapes suivantes

1. **Crédits par piste du chantier artistes multiples** (`track_artists`).
   Ils ne sont pas dans le lot rc7. Quand ils y entreront, la règle `artist`
   les lira déjà (deuxième étape du chantier), sans rien changer ici.
2. **Étiquette par nom** (`etiquette:"J'adore"`), résolue en base. Elle se
   donne aujourd'hui par identifiant.
3. **Taille de fichier** (« sans perte au-delà de N Mo »). Ce n'est pas un
   critère du moteur aujourd'hui : il faut une colonne de taille, puis un
   critère dans `criteres.rs`.
4. **Enregistrer une requête imbriquée** comme playlist intelligente. Le
   format des règles est plat : il faut un format d'arbre, ou un champ
   `requete` texte à côté de `rules`.
5. **Interface** : un champ « requête avancée » dans l'éditeur, qui affiche
   la position de l'erreur rendue par la route.
6. Les favoris de service et les pistes de catalogue (règles `source`,
   `catalogue:`), que l'aperçu ajoute, ne sont pas ajoutés par la route
   textuelle.
