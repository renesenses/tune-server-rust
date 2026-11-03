# Audit d'intégrité de la bibliothèque — contrat serveur pour le client web

Refs https://github.com/renesenses/tune-server-rust/issues/6286 (suite de
https://github.com/renesenses/tune-server-rust/pull/6828).

Ce document décrit ce que le serveur sert depuis cette suite ; l'écran
Réglages › Bibliothèque du client web s'y branchera. Tout est **additif** :
les champs que la carte `docs/contrat-web.json` exige déjà
(`GET`/`POST /library/integrity`, type `EtatIntegrite`) restent servis
à l'identique.

## Périmètre

L'audit suit le périmètre des analyses (#5593) en entier :

- `background_analysis_excluded_roots` : les racines exclues ne sont ni lues
  ni comptées (déjà le cas depuis #6828) ;
- `background_analysis_included_dirs` : **nouveau pour l'audit** — réglés, seuls
  les fichiers sous l'un de ces dossiers sont lus et comptés.

Un fichier doit être DANS un dossier inclus (s'il y en a) et HORS des racines
exclues. La comparaison est celle des passes qui décodent, sur le chemin
ouvrable (le support d'une image CUE compte). Une modification du périmètre
pendant l'audit prend effet au fichier suivant, comme avant.

## Lancer un audit

`POST /library/integrity` accepte, en paramètres de requête :

| Paramètre      | Défaut  | Effet |
|----------------|---------|-------|
| `resume`       | `false` | reprend l'audit en pause ou en erreur (inchangé) |
| `changed_only` | `false` | **nouveau** — ne relit que les fichiers changés depuis l'audit précédent |

Avec `changed_only=true`, un fichier est **relu** s'il est nouveau, absent, si
son chemin, sa taille ou sa date de modification diffère de ceux de l'audit
précédent, ou si l'audit précédent ne les avait pas notés (audit d'avant cette
option). Sinon son résultat précédent est **repris sans être décodé**. Sans
audit précédent, tout est relu.

Limite assumée : un fichier modifié en gardant sa taille ET sa date n'est pas
vu comme changé ; seul un audit complet le relit.

## `GET` et `POST /library/integrity` — champs ajoutés

| Champ                    | Type                 | Sens |
|--------------------------|----------------------|------|
| `modifies_seulement`     | booléen              | l'audit a été lancé avec `changed_only` |
| `reference`              | chaîne ou `null`     | l'audit précédent dont les résultats sont repris |
| `reutilises`             | entier               | fichiers inchangés repris sans être relus ; comptés aussi dans `traites` et dans leur verdict |
| `debut`                  | RFC 3339 ou `null`   | début de cet audit |
| `dernier_audit_termine`  | RFC 3339 ou `null`   | fin du dernier audit terminé, conservée d'un audit à l'autre |
| `rappel`                 | objet                | voir ci-dessous |

`rappel` :

| Champ      | Type               | Sens |
|------------|--------------------|------|
| `jours`    | entier             | intervalle réglé, 0 = éteint |
| `du`       | booléen            | un audit est dû : rappel allumé et dernier audit terminé plus ancien que l'intervalle (ou jamais terminé) ; toujours faux pendant un audit en cours |
| `prochain` | RFC 3339 ou `null` | date du prochain rappel |

Le serveur ne lance rien de lui-même : le client affiche le rappel et propose
de lancer l'audit (complet ou fichiers changés seulement).

## `GET /library/integrity/report` — champs ajoutés aux lignes

| Champ         | Type             | Sens |
|---------------|------------------|------|
| `date`        | nombre ou `null` | date de modification du fichier quand il a été lu, en secondes |
| `verifie_par` | chaîne ou `null` | l'audit qui a réellement décodé le fichier ; différent de l'audit courant pour un résultat repris |

## Réglage du rappel

`GET /system/config` publie, `PATCH /system/config` écrit :

| Clé                                | Valeur                         | Défaut |
|------------------------------------|--------------------------------|--------|
| `library_integrity_reminder_days`  | entier de 0 (éteint) à 365, en jours | `0` |

Toute autre valeur est refusée (400) en nommant les bornes.

## Pour le client web

- Réglages › Bibliothèque : une case « Seulement les fichiers changés depuis le
  dernier audit » qui ajoute `changed_only=true` au lancement ; le compteur
  `reutilises` à côté de l'avancement. Près de la case, rappeler la limite
  ci-dessus : un fichier réécrit avec la même taille et la même date n'est
  relu que par un audit complet.
- Un champ « Me rappeler de relancer l'audit tous les … jours » (0 / vide =
  jamais) sur `library_integrity_reminder_days`, et un bandeau quand
  `rappel.du` est vrai.
- `docs/contrat-web.json` est régénéré depuis le client : ces champs n'y
  figureront qu'une fois que le client les lira.

## Stockage

Aucune migration : l'état, le rapport et le réglage vivent dans `settings`,
identiques sous SQLite et PostgreSQL. Le rapport de l'audit de référence est
gardé tant que l'audit partiel n'est pas terminé, puis supprimé.
