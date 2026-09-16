# Bibliothèque UPnP — lot 3 : pochettes hors connexion

Code préparé pour #4201, dans la PR #4205. Aucun déploiement.

## Comportement

L'indexation conserve les octets des pochettes dans le cache d'illustrations
de Tune. `albums.cover_path` et `tracks.cover_path` portent le condensat du
contenu, servi par la route d'illustrations existante. L'URL annoncée reste
dans l'instantané de métadonnées UPnP.

Les URL identiques sont traitées une fois par passe. Un petit index local
associe chaque URL à son condensat pendant 24 heures ; une actualisation
ratée conserve la copie antérieure. Une URL qui change peut acquérir une
nouvelle image, sans changer l'identité de la piste. La nouvelle pochette
est propagée aux pistes existantes ; une piste sans image propre continue
de suivre celle de son album.

Bornes : quatre téléchargements simultanés, huit Mio par image (y compris
sans Content-Length), six secondes par téléchargement et deux minutes pour
la phase de pochettes. Une image absente ou invalide ne rend pas incomplet
le parcours audio. Le bilan indique `pochettes_en_cache`, nombre d'URL dont
la copie a été retrouvée ou obtenue pendant la passe.

Un nouvel album sans copie utilisable affiche un emplacement vide. Les
anciens albums UPnP qui portaient une URL distante sont convertis lors de
leur prochaine indexation ; une copie locale déjà enregistrée est conservée
en cas d'échec. Il faut donc synchroniser les sources pour remplir le cache.

## Preuves exécutées

Trois tests `cargo test -p tune-server --lib pochettes_upnp
--no-default-features --features oaat` passent :

- vraie réponse HTTP, trois pistes dont la première sans image, une seule
  requête d'image, extinction du serveur puis réindexation ; les pochettes
  de l'album et des trois pistes restent locales et leurs octets lisibles ;
- cache périmé conservé face à une réponse HTML, mais index sans fichier
  jamais annoncé comme une image disponible ;
- réponse en flux sans Content-Length rejetée au-delà de huit Mio.

Le premier test vérifie aussi le remplacement d'une image déjà indexée.
Il a détecté que `TrackRepo.update` ne modifie pas `cover_path` : l'écriture
est maintenant explicite et limitée aux pistes `source = 'upnp'`.

Contre-épreuve : passer une carte de pochettes vide à `ecrire`, sans modifier
le test, échoue à l'exécution sur « pochette locale de l'album ». Restauration
par copie, puis trois tests verts.

Régression : les 13 tests des bancs `indexation_upnp_2219` et
`plafonds_indexation_upnp_4154` passent avec `--no-default-features
--features oaat`. `cargo fmt --all -- --check` et `git diff --check` passent.

## Limites

Les copies existantes restent consultables serveur éteint ; une pochette
jamais acquise ne peut pas être reconstituée hors connexion. Le stockage
utilise le cache d'illustrations existant, sans nouvelle politique de quota
ou de purge. Les gros volumes et les serveurs matériels restent à qualifier.
La présence du serveur est affichée côté client depuis le registre durable,
avec des états distincts pour détection récente, absence et état inconnu.
