# Bibliothèque UPnP : identités durables

Suivi #4201, PR serveur #4205. Lot préparé sans migration ni déploiement.

## Comportement

Le condensat des tags servait à la fois à retrouver une piste et à la nommer
durablement. Corriger un titre ou un artiste produisait donc une nouvelle
ligne, puis la réconciliation pouvait supprimer l'ancienne et ses liens.

Tune conserve maintenant l'ID numérique et la clé `source_id` de la ligne.
Le rapprochement utilise les métadonnées **courantes** en base, calculées au
début de chaque passe. Il reste donc valable après plusieurs corrections,
un changement d'adresse et une réouverture de la base.

Si les tags diffèrent, Tune cherche les ObjectID et URL déjà observés sur le
même serveur. Une correspondance exige une durée positive identique à la
seconde, un format connu identique, et soit les deux indices concordants,
soit une taille positive identique. Ces indices sont contextuels : ils ne
deviennent pas des clés primaires. Aucun rapprochement entre serveurs, ni
avec une piste locale, n'est effectué.

Les décisions sont prises avant toute écriture. Des indices contradictoires,
plusieurs anciennes lignes candidates, ou deux pistes entrantes réclamant
une même ligne produisent une erreur dans le bilan. La passe est partielle
et ne peut autoriser aucun retrait, même inférieur à 20 %.

Une ancienne empreinte devenue le nom d'une autre piste ne récupère pas la
clé durable déjà attribuée. La nouvelle ligne reçoit alors une clé distincte.
Le rapport transmet les clés réellement écrites à la réconciliation : les
appartenances aux dossiers suivent les mêmes IDs.

Une disparition sans correspondance qui touche un favori (piste ou album)
ou une playlist exige une confirmation, même sous le seuil de 20 %. Le
contrôle est revérifié dans la transaction de suppression : un lien ajouté
après le bilan provoque l'annulation du retrait et de ses appartenances.
La confirmation explicite, chiffrée et liée à une génération reste possible.
Le nettoyage des albums vides conserve les albums favoris. Le texte de
confirmation prévient de l'impact sur favoris et playlists dans onze langues.

Le renommage complet d'un album conserve aussi son ID et sa clé source,
si toutes ses pistes se retrouvent dans un seul groupe et qu'aucun autre
album existant ne revendique ce groupe. Un déplacement partiel, une fusion
ou une scission ne renomme pas arbitrairement un album favori.

Les écritures restent des mises à jour : favoris, positions et répétitions
dans les playlists restent attachés aux mêmes lignes. Les notes personnelles et les autres champs de piste que l'indexation
n'actualise pas sont conservés.

## Périmètre de preuve

Le banc `identites_upnp` couvre les corrections de tags et de taille, le
changement ultérieur des URL/ObjectID après réouverture de la base, les
favoris de piste et d'album, l'ordre et les répétitions d'une playlist,
la réutilisation d'une ancienne empreinte, les ambiguïtés et l'isolation
exacte des serveurs (y compris casse et caractères `%`/`_`).

Le banc HTTP `indexation_upnp_2219` exerce le parcours SOAP, les abonnements
et la réconciliation : six pistes corrigées conservent leurs appartenances ;
un identifiant réutilisé avec une durée contradictoire rend ensuite la passe
partielle et protège la piste favorite contre un retrait de 1/6. Quand tous
les indices de cette piste disparaissent, sa suppression passe ensuite en
attente de confirmation. Un test transactionnel vérifie séparément les
favoris de piste, les favoris d'album et les playlists, ainsi que le retour
des appartenances après un refus de suppression.

Contre-épreuve du rapprochement : neutraliser le recours aux indices, puis
lancer `cargo test -p tune-server --lib identites_upnp_tags_puis_adresses
--no-default-features --features oaat`, échoue à l'exécution sur « les
corrections de tags doivent garder les pistes existantes » (0 au lieu de 2).
Restauration par copie et six tests ciblés verts.

Contre-épreuve de la transaction : neutraliser la garde des liens dans
`remove_missing`, puis lancer `cargo test -p tune-server --lib
identites_upnp_les_liens_interdisent --no-default-features --features oaat`,
échoue à l'exécution sur « le retrait automatique doit refuser le lien track ».
Restauration par copie avant les vérifications finales.

Vérification finale unitaire : `cargo test -p tune-server --lib
--no-default-features --features oaat -- indexation_upnp pochettes_upnp
identites_upnp` passe : 18 tests, dont les sept témoins d'identité/protection
des liens et la régression des pochettes.

Régression HTTP finale : `cargo test -p tune-server --test
indexation_upnp_2219 --test plafonds_indexation_upnp_4154
--no-default-features --features oaat` passe : six tests d'indexation et
huit tests de plafonds, soit 14 tests. `cargo fmt --all -- --check` et
`git diff --check` passent.

Client : le message traduit passe `npm test -- --maxWorkers=4
src/lib/__tests__/upnpLibrarySources4201.test.ts
src/lib/__tests__/sourcesOnglets4201.test.ts` (dix tests), avec les contrôles
i18n et Svelte par rapport au socle. CI `npm test` verte sur `a8354103` :
https://github.com/renesenses/tune-web-client/actions/runs/35000785877/job/104488359798 .

## Limites explicites

Il s'agit d'un rapprochement sur les informations annoncées par le serveur,
pas d'une comparaison des octets audio. Si tags, ObjectID et URL changent
tous simultanément sans aucun indice conservé, le DIDL ne permet pas de
prouver qu'il s'agit du même fichier : Tune ne devine pas cette association.
Les collisions de tags/durée/taille déjà documentées dans le socle restent
possibles. Un remplacement de fichier qui conserve tous ces indices ne
peut pas être distingué d'une correction de tags avec ces seules données.

Les artistes sont partagés avec les autres sources : leurs favoris ne sont
pas réattribués automatiquement lors d'une correction du nom d'artiste.

Un renommage d'album fragmenté entre plusieurs passes n'offre pas la même
preuve qu'un renommage complet. La qualification sur les serveurs réels
et sur gros catalogues reste nécessaire. Le compteur `SystemUpdateID` et
la qualification finale du chantier restent des lots distincts.
