# #4201 — SystemUpdateID et notifications du catalogue

OpenAI/Codex, run `upnp-unifiee-20260915`, 15 septembre 2026.
Lot 5 de la PR serveur #4205, base `batch/p2-upnp-history-20260915`.

## Comportement

`GetSystemUpdateID`, `Browse` et `Search` renvoyaient une constante `1`.
Le serveur acceptait les abonnements ContentDirectory sans conserver les
abonnés ni envoyer de notification.

Le compteur ui4 est maintenant conservé dans `upnp_catalog_revision` et
modifié dans la même transaction que le catalogue. Les migrations SQLite 103
et PostgreSQL 060 installent les déclencheurs pour pistes, albums, artistes,
listes et leur contenu, stations, éléments masqués et URL de lecture UPnP.
Les champs suivis comprennent les clés de tri des artistes et les favoris
radio, qui changent l'ordre des pages. Les UPDATE identiques, les bilans de
synchronisation, l'analyse ReplayGain et les compteurs d'écoute ne le changent
pas. Une annulation de transaction annule aussi l'incrément. La valeur revient
à zéro après 4294967295.

Les réponses SOAP utilisent cette même valeur. Si le catalogue change pendant
la construction d'une page, la réponse est un fault 720, converti en HTTP 500.
Un compteur absent ou illisible produit un fault 501, sans valeur de secours.
Le contrôle existant des UpdateID entre pages côté importateur continue de
refuser un parcours instable : le lot ne remplace pas cette garde.

Les migrations rejouées préservent la valeur. PostgreSQL reçoit aussi une
installation idempotente en fin de migration pour réparer les déclencheurs
sur une base déjà marquée à jour. Le migrateur existant retire la sentinelle 99
des anciennes conversions SQLite → PG avant de rejouer les scripts numérotés.
Une conversion vers une autre base initialise son propre compteur ; ce lot
ne transfère pas sa valeur depuis la base source.

## Abonnements ContentDirectory

- `SUBSCRIBE` crée un SID et produit un NOTIFY initial, SEQ 0.
- Une modification produit un événement avec la nouvelle révision ; plusieurs
  écritures entre deux contrôles sont regroupées.
- Le renouvellement garde le SID et la séquence, sans événement initial répété.
- `UNSUBSCRIBE` retire l'abonnement. Un SID expiré ne se renouvelle pas.
- Le compteur d'événements revient à 1 après débordement ; 0 est réservé au
  premier envoi. Un envoi échoué est retenté tant que l'abonnement est valide.
- Un seul worker lit la révision, au plus une fois par seconde, via
  `spawn_blocking`. Il démarre au premier abonnement et s'arrête lorsqu'il n'y
  en a plus ou que le routeur est détruit.
- Plafonds : 64 abonnés, durée accordée maximale de 1800 secondes, quatre
  envois simultanés et délai de trois secondes par envoi. Sous charge ou avec
  des récepteurs injoignables, les notifications peuvent donc prendre plus
  d'une seconde.
- Le callback doit être une URL HTTP unique avec une IP littérale correspondant
  au pair TCP. Pas de DNS, proxy, redirection, identifiants ni fragment.
  Les callbacks nommés, multiples ou sur une autre interface sont refusés.
  Le routeur principal fournit déjà `ConnectInfo<SocketAddr>`.

Le périmètre est ContentDirectory. Le gestionnaire historique ConnectionManager
ne reçoit pas d'implémentation de notification dans ce lot. Un envoi déjà en
cours peut se terminer après un désabonnement.

Référence des échanges GENA :
[UPnP Device Architecture 1.0, Eventing](https://upnp.org/specs/arch/UPnPDA10_20000613.htm).

## Vérifications

Commandes exécutées depuis le worktree serveur, avec
`CARGO_TARGET_DIR=/Users/bertrand/DEV/tune-server-rust/target` :

```sh
cargo test -p tune-core --lib --no-default-features --features oaat -- system_update_id upnp_server:: db::migrations::tests
cargo test -p tune-server --lib --no-default-features --features oaat -- system_update_id indexation_upnp pochettes_upnp identites_upnp
cargo test -p tune-server --test indexation_upnp_2219 --test plafonds_indexation_upnp_4154 --no-default-features --features oaat
cargo fmt --all -- --check
git diff --check
```

Résultat final après restauration : **133 tests cœur, 20 tests unitaires
serveur et 14 tests HTTP passent**. Le formatage et le diff sont valides.
Le témoin HTTP final reçoit aussi son événement initial pendant la seconde
accordée à un abonnement court, avant de le renouveler.

Les témoins nouveaux vérifient une base SQLite sur disque rouverte, les huit
familles de mutations, les écritures sans changement, les transactions
annulées, le débordement, les réponses SOAP après ajout/correction/retrait,
le compteur manquant et une écriture concurrente pendant une page.
Le récepteur HTTP local reçoit réellement les NOTIFY et en vérifie SID, SEQ,
NT, NTS et corps XML ; il contrôle aussi l'absence d'événement superflu.
Le banc d'indexation vérifie qu'une deuxième passe identique conserve le compteur.

PostgreSQL 15 local : migration appliquée et rejouée sur le schéma de conversion,
mutations et rollback vérifiés par `4201-system-update-id-postgres.sql`.
Le compteur fixé à 73 reste à 73 après réapplication de la migration puis arrêt
et redémarrage du processus PostgreSQL.

## Contre-épreuves

Les fichiers corrects sont sauvegardés avant chaque neutralisation et restaurés
par `cp`. Aucun témoin n'est modifié pour produire le rouge.

1. Remplacer l'incrément SQL par `SET value = value`, puis exécuter
   `cargo test -p tune-core --lib db::upnp_revision::tests --no-default-features --features oaat`.
   Les deux tests échouent à l'exécution : « un ajout change le SystemUpdateID »
   et « changement non annoncé : INSERT INTO artists… » (0 contre 0).
2. Appliquer la même neutralisation de la fonction PostgreSQL puis exécuter le
   banc SQL. Il échoue sur « Modification non annoncée : INSERT INTO artists… ».
   Restauration du fichier par copie, réapplication et banc de nouveau vert.
3. Rétablir les constantes SOAP et neutraliser la comparaison de fin de page,
   puis `cargo test -p tune-core --lib upnp_server::tests::system_update_id --no-default-features --features oaat`.
   Les deux témoins échouent : « GetSystemUpdateID figé » et « une page modifiée
   pendant la lecture ne doit pas être publiée ».
4. Limiter les notifications aux abonnements n'ayant encore aucune révision,
   puis `cargo test -p tune-server --lib system_update_id_gena_notifie --no-default-features --features oaat`.
   Le témoin échoue à l’exécution : « une modification du catalogue doit
   produire un NOTIFY: Elapsed(()) ». Restauration par copie avant le banc final.

## Qualification restante

Ces bancs ne constituent pas une qualification avec JPLAY, un lecteur DLNA ou
plusieurs serveurs physiques, ni une mesure de débit sur un gros catalogue.
Les triggers PostgreSQL sérialisent les écritures sur une ligne de compteur ;
l'effet sur les gros scans concurrents reste à mesurer. Les abonnements sont
en mémoire et doivent être recréés après redémarrage, tandis que la révision
reste en base. La validation complète du binaire avec la feature PostgreSQL
reste distincte des vérifications SQL locales.

La bibliothèque unifiée n'est pas déclarée entièrement qualifiée. Les limites
DSP/seek/ReplayGain/OAAT restent suivies séparément. Aucun merge, bump de version,
tag ou déploiement dans ce lot.
