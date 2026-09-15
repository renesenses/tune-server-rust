# Bibliothèque UPnP — preuves du lot #4201, 15 septembre 2026

OpenAI/Codex, run `upnp-unifiee-20260915`.
Base serveur `24123a4e`, base web `28ece4ea`. Aucun essai de suppression
sur une bibliothèque utilisateur : bancs HTTP et bases temporaires uniquement.

## Serveur

`cargo test -p tune-server --test indexation_upnp_2219 --test plafonds_indexation_upnp_4154 --no-default-features --features oaat`

13 tests réussis. Le serveur HTTP du banc produit de vraies enveloppes SOAP.
Les propriétés nouvelles vérifiées :

- un HTTP 502 ne devient pas un catalogue vide complet ;
- une seconde page absente garde les entrées déjà reçues mais interdit la purge ;
- un retrait de 1/6 est appliqué ; un retrait de 2/5 attend confirmation ;
- une génération périmée est refusée par HTTP 409 ;
- une piste possédée par un autre abonnement reste présente ;
- une piste locale ne peut pas être supprimée, même si une appartenance
  erronée la désigne ; le bilan est conservé en base.

### Contre-épreuve

Dans `synchronisation_upnp.rs`, remplacer temporairement
`let complete = report["complet"] == true;` par
`let complete = report["indexe"] == true;`, sans toucher au test.

Commande : même `cargo test`, cible `indexation_upnp_2219`, filtre
`synchronisation_durable_refuse_les_pannes_et_confirme_les_retraits_massifs`.

Rouge attendu et obtenu à l’exécution : état reçu `confirmation`, attendu
`partial`, après HTTP 502. La version fautive proposait donc une purge sur
une panne. Restauration par copie puis retour au vert des 13 tests.

## Bases

`cargo test -p tune-core --lib db::migrations:: --no-default-features --features oaat`

40 tests réussis (création, anciennes bases, idempotence, parité des chemins).

PostgreSQL 15 temporaire, port local 56421 : application de `PG_FULL_SCHEMA`,
migration 059 appliquée deux fois ; insertion de deux abonnements possédant
la même piste, mise à jour de génération, suppression d’un abonnement.
Assertions SQL réussies : migration enregistrée une fois, appartenance restante
conservée, piste conservée, index sur `track_id` créé.

Le premier essai a révélé que le schéma de conversion porte temporairement
`tracks.id` en TEXT : la FK BIGINT ne pouvait pas y être créée. La relation
au serveur reste contrainte, et les appartenances à des pistes retirées sont
nettoyées explicitement dans la transaction de réconciliation.

### Validation PostgreSQL encore requise

`cargo check -p tune-server --no-default-features --features oaat,postgres`
s’arrête localement au chargement de `libsqlx_macros` :
`mis-aligned LINKEDIT string pool`. Le chargement du module compilé échoue
avant la vérification du code serveur. Les essais SQL ne remplacent pas ce
contrôle de compilation ni les tests complets PostgreSQL en CI.

## Client web

Node 22, comme `.github/workflows/ci.yml` ; dépendances de `package-lock.json`.

- `npm test` : 404 fichiers, 4 531 tests réussis ; contrôles de types comparés
  au socle, traductions (11 langues), CSS et dialogues inclus.
- `npm run build` : réussi.
- 93 tests ciblés API, sources et interface : réussis après restauration.

Contre-épreuve : retirer l’initialisation à zéro des provenances, et neutraliser
l’émission des avertissements de lecture, sans toucher aux tests. Deux rouges
à l’exécution : source locale `undefined` au lieu de `0`, et notification
attendue jamais émise. Restaurations par copies, puis 93 tests verts.

Node 26 avait fait échouer des tests existants liés à son `localStorage`
global ; la batterie complète a été reprise avec Node 22.

## Périmètre restant au terme du premier lot

Ce relevé décrit le premier lot. Les PR ont depuis été complétées par les
filtres des trois onglets, la disponibilité et les pochettes locales, puis
les identités durables. Voir `4201-pochettes-hors-connexion.md` et
`4201-identites-durables.md` pour les preuves et limites des ajouts serveur.

Les PR sont à relire et leur CI complète doit passer. Ce lot ne prétend pas
clore la qualification matérielle, la conservation des pochettes hors connexion,
les filtres Source des vues Artistes/Pistes, ni l’identité après modification
des métadonnées constituant la clé. La parité audio et `SystemUpdateID` restent
suivis dans le document de chantier et l’issue #4201.
