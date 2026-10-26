# Construire et tester : savoir pratique

Ce que les agents ont constaté en travaillant sur ce dépôt : durées, commandes,
pièges. Les mesures datent des 24 et 25/10/2026, sur la machine des agents
(40 cœurs, souvent chargée). Une note fausse ou périmée se corrige dans la PR
qui le constate ; ce qu'on apprend et qui servira à la suivante s'ajoute de
même.

## Avant le premier commit

- Le clone n'a pas d'identité git. La régler en local, comme les commits des
  lots :
  ```sh
  git config user.name "agentic-mozaik-labs[bot]"
  git config user.email "347280180+agentic-mozaik-labs[bot]@users.noreply.github.com"
  ```
- Si la compilation modifie `Cargo.lock` sans rapport avec la tâche (sur le lot
  rc8, cargo ajoutait `regex` aux dépendances de `tune-assistant-ia`), remettre
  le fichier avant de commiter : `git checkout -- Cargo.lock`.
- `rustfmt --edition 2024 --check <fichier>` vérifie un fichier sans reformater
  toute la caisse.

## Durées de compilation

Une commande est arrêtée au bout de 10 minutes. Une première compilation dépasse
souvent cette limite : la lancer tôt, en arrière-plan, et lire son journal.

```sh
nohup cargo test -p tune-server --lib <filtre> > target/x.log 2>&1 &
```

| Commande | Premier lancement | Relance après modification |
|---|---|---|
| `cargo test -p tune-core --lib --no-run` | 8 à 12 min 30 | 1 à 3 min |
| `cargo test -p tune-server …` | plus de 10 min, jusqu'à 15 | environ 2 min |
| `cargo clippy -p tune-core --lib --tests` | environ 6 min 40 après les tests | |
| `cargo clippy -p tune-server --tests` | environ 8 min | |

## Lancer peu de tests

- Filtrer par nom : `cargo test -p tune-core --lib -- <filtre1> <filtre2>`.
- Répéter un test N fois sans recompiler : `cargo test -p tune-core --lib --no-run`,
  copier l'exécutable annoncé (`target/debug/deps/tune_core-<hash>`), puis le
  lancer en boucle (`<binaire> <filtre> --test-threads=16`), environ 3 s par
  passage. Le binaire copié depuis la base mesure l'état avant le correctif avec
  la même commande.
- `cargo test -p tune-core --test '*'` échoue sur les cibles qui exigent
  `audio-embedding` : nommer les cibles une par une.
- `cargo test -p tune-smart-http` en entier : 198 tests, quelques secondes une
  fois compilé.

## Écrire un test dans tune-server

- `tune-server` a `autotests = false` : un nouveau fichier de `tests/` exige un
  bloc `[[test]]` dans `tune-server/Cargo.toml`. Le placer au milieu du fichier,
  près d'un test voisin, limite les conflits avec les PR qui ajoutent en fin de
  fichier.
- Serveur de test : `crate::routes::router(AppState::new(":memory:", 0, Default::default()))`
  et `tower::ServiceExt::oneshot`, sans authentification. Un redémarrage se
  simule par un second `AppState::new` sur le même fichier de base (tempdir).
- Compte Premium : `state.license.set_account_premium(true, None).await`, ou
  `state.license.update_from_server(tune_core::license::Tier::Premium, None).await`.
  Les routes Premium (par exemple `POST /zones/group`) l'exigent.
- Égaliseur utilisable : Premium, plus les réglages `plugin_equalizer_installed`
  et `plugin_equalizer_enabled` à `true`. Modèle : `tests/reserve_egaliseur_5171.rs`.
  L'interrupteur de l'onglet Égaliseur du client web lit `GET /zones/{id}/eq`
  (`enabled` = `profile.enabled && !bands.is_empty()`), pas `GET /plugins/equalizer`.
- Les tests de routes du greffon égaliseur vivent dans
  `tune-server/src/premium_audio_plugins.rs` (module tests), avec les aides
  `appel`, `appel_avec_corps`, `fiche_du_catalogue`, `visible_dans_la_barre` et
  `dossier_de_donnees_jetable()` à appeler avant `AppState::new`.
- Renderers DLNA factices, sans matériel : zones créées par
  `ZoneRepo::create(nom, Some("dlna"), Some(device_id))`, et des
  `tune_core::outputs::mock::MockOutput::new(id, nom).with_type("dlna")`
  enregistrées dans `state.outputs`. Ce qu'un renderer a reçu se lit avec
  `play_titles()` et `stop_call_count()`. Piste réelle : copier
  `tune-core/tests/fixtures/test.flac` et l'insérer dans `tracks`. Modèle :
  `tune-server/tests/transfert_de_zone_joue_sur_la_cible.rs`.
- `POST /api/v1/zones/{id}/play` avec `{"track_id": n}` sur une zone `local` et
  une piste WAV écrite dans un tempdir répond 200 en test, sans carte son.
- Les tâches de fond (`background.rs`) ne tournent pas sous `AppState::new` : le
  test les lance lui-même.
- Un test qui lance un scan prend `serialiser_les_scans_de_test_sans_bloquer().await`
  et `attendre_que_le_droit_de_scanner_soit_libre().await`, et porte
  `#[allow(clippy::await_holding_lock)]` avec un commentaire, comme
  `arret_du_scan_de_demarrage_tests_5552.rs`.

## Écrire un test dans tune-core

- Les tests de l'orchestrateur sont des modules `#[cfg(test)]` déclarés dans
  `tune-core/src/orchestrator.rs` ; un nouveau fichier de tests doit y être
  déclaré.
- Banc d'avance DLNA réutilisable :
  `tune-core/src/orchestrator/seek_natif_6059_bout_en_bout.rs` (MockOutput de
  type dlna et session fichier FLAC encodée par `audio::encoder::AudioEncoder`,
  qui accepte aussi le 24 bits en échantillons de 3 octets).
- Les bases SQLite en mémoire des tests numérotent leur première zone 1 : un état
  global classé par `zone_id` se partage donc entre tests. Imposer un `zone_id`
  propre (`INSERT INTO zones (id, …)`).
- Banc du sondeur qui appelle le vrai `PositionPoller::tick` en faisant avancer
  `OutputStatus.position_ms` : reculer `ZonePollState.track_started_at` (et
  `track_loaded_at`) du même pas. Sinon, dès que la position dépasse l'horloge de
  plus de 15 s, `decisions::stale_start_position` saute chaque tour,
  `stopped_ticks` se fige et `STOPPED_FAILURE_THRESHOLD` n'est jamais atteint :
  le témoin reste vert sans le correctif.

## Ajouter une colonne à streaming_favorites

Sans numéro de migration, une colonne se pose à quatre endroits :
`migrations.rs` (fonction appelée depuis `run_migrations`, à côté de
`favoris_miroir_colonnes`), `postgres.rs` (deux listes ENSURE, chercher la ligne
`isrc`), `pg_migrate.rs` (`PG_FULL_SCHEMA`). Le type TEXT partout évite l'écart
de parité PostgreSQL/SQLite ; comparer ensuite par `CAST(col AS INTEGER)`.

Piège : des bancs de tune-smart-http (`smart_collections.rs`,
`smart_playlists.rs`) créent la table à la main. Toute colonne ajoutée au SELECT
de `source_streaming::requete` les casse (« no such column ») : lancer
`cargo test -p tune-smart-http` en entier.

## CI

- La CI ne refuse que `clippy::correctness` (`-D clippy::correctness`, `ci.yml`).
  `cargo clippy -p tune-server -- -D warnings` s'arrête sur des avertissements
  préexistants de `tune-output-api` : lancer clippy sans `-D warnings` et
  chercher son propre fichier dans la sortie.
- Le job `test` a `timeout-minutes: 45` et durait environ 40 min sur les PR le
  25/10/2026 (sans cache : `cache-save-if` ne sauve le cache que sur main ou
  `release/v0.9`). Le binaire de tests de tune-core prend à lui seul environ
  850 s.
- Un job annulé qui affiche 55 min dans `gh pr checks` a en général dépassé
  cette limite. Quand ses journaux ont expiré (`BlobNotFound`), la cause se lit
  dans les annotations :
  `gh api repos/renesenses/tune-server-rust/check-runs/<job_id>/annotations`.
- Vérifier qu'un témoin a tourné :
  `gh api repos/renesenses/tune-server-rust/actions/jobs/<job_id>/logs | grep <nom_du_test>`.
- Job « Test » rouge en code 143 sans ligne « test result » ni « FAILED » : si la
  ligne juste au-dessus contient « The runner has received a shutdown signal »,
  la machine de CI a été arrêtée, le code n'y est pour rien. Relancer le seul
  job : `gh run rerun <run_id> --repo renesenses/tune-server-rust --failed`,
  sans commit.

## Constater la fusion d'une PR passée par une fusion groupée

La fusion groupée fusionne la PR de groupe dans le lot `batch/*` en un seul
commit qui recrée le contenu. La PR unitaire reste alors ouverte et son commit
n'est pas un ancêtre du lot, alors que son contenu y est. Pour constater la
fusion : lire la PR de groupe qui cite la PR unitaire
(`gh pr view <groupe> --json state,mergeCommit`), puis
`git diff --stat <commit unitaire> origin/<lot> -- <fichiers de la PR>` : une
sortie vide veut dire que le contenu est fusionné.
