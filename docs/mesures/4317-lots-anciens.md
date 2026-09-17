# #4317 — qualifier les anciens lots avant de les reprendre

JP Robbe / OpenAI Codex / jp-robbe-20260916-4317-audit

## Ce que l'inventaire Git établit, et sa limite

Base observée : `3a2b710a151a257e217143b2900eeb53076ea6ef` (`main`).
Les six branches portent bien **25 commits hors merges en avance** :

| Lot | Tête observée | Commits |
|---|---|---:|
| `batch/vague-8` | `637138173759` | 3 |
| `batch/bugs-1` | `de6b3c8e6a89` | 6 |
| `batch/p1-audio-output-contracts-2` | `32538e723ce0` | 7 |
| `batch/p2-anciennes-2` | `1f8234a35acb` | 4 |
| `batch/p1-replaygain-analysis-integrity-v2` | `e14afd7bdf50` | 4 |
| `batch/vague-11` | `1fd1ec4f920f` | 1 |

Ces commits comprennent du formatage, des gardes et plusieurs étapes pour un
même sujet. Leur nombre ne mesure donc pas un nombre de défauts indépendants.

Surtout, une avance Git ne prouve pas qu'un comportement manque : des
correctifs ont été réécrits dans `main`. Une comparaison des seuls noms de
fonctions se trompe aussi : OAAT et OpenHome ont changé de structure.
L'inventaire ci-dessous confronte le changement ancien, son site de production
actuel et les témoins exécutables. Il ne rejoue aucun lot pendant le train.

Les arbres de `tune-core/src`, `tune-server/src` et `.github/workflows` sont
identiques entre cette base et le tag `v0.9.151` (`git diff --stat` vide).
C'est une preuve sur les **sources du tag**, pas une inspection des binaires
distribués ni une recette sur le matériel des testeurs.

## Qualification par changement

Les références de production ci-dessous désignent les fichiers actuels,
après l'éclatement de l'ancien `orchestrator.rs`.

| Ancien commit | Sujet | Qualification sur la base |
|---|---|---|
| `ff633803` | Historique / annonce navigateur #1998 | Présent : annonce différée dans `orchestrator/history.rs` et `commun.rs`, libérée quand le flux est consommé ; témoins `zone_navigateur_*`. |
| `c8de866f` | Arrêt Chromecast | Présent : plan d'arrêt du média dans `outputs/chromecast.rs` ; les témoins gardent l'application de l'autre émetteur et l'appareil au repos. |
| `3443d916` | Borne PostgreSQL 039 | Ancienne borne dépassée. Ne pas rétablir 039 dans le registre courant. |
| `6defe9e7` / `5cda7c8c` | `cloud-relay` dans les binaires et la CI #3355 | Présent dans `release.yml`, `ci.yml` et les gardes `workflows_bornes`. Pas de rebuild de release dans cet audit. |
| `5212c00f` | Cible de transcodage encodable #3357 | Présent : `cible_encodable` en amont de l'extension et du MIME, avec témoin AIFF → FLAC. |
| `47d8f2d3` | Repli DSF/DFF #3180 | Présent : `dsf_dff_fallback_complete` et priorité des balises dans `metadata/mod.rs`. |
| `476cd5b6` | Genres, découverte, top mixes #3181 | Présent dans `routes/home.rs` ; tests dédiés des trois usages. |
| `2dea959f` | rustfmt des gardes | Pas de comportement autonome à reporter ; ne pas importer un ancien formatage sur le fichier actuel. |
| `09be1df6` | Superposition PCM pour le crossfade #2211 | **Partiel, raccordement absent** : le moteur PCM `audio/fondu_enchaine.rs` existe (8 témoins réussis), mais aucun appelant de production ne le raccorde à la lecture et la route refuse toujours l'activation par `crossfade_unavailable`. L'ancien module et son raccord local ne sont pas repris ; ne pas confondre moteur testé et parcours disponible. |
| `a2a8c0a6` | Contrat de synchronisation OAAT #2215 | Présent sous `oaat_synchronization_contract` et les contrats de groupes. La calibration physique reste un autre périmètre. |
| `0efb5817` | Réveil HEOS sur URI vide #2749 | Réécrit : `verifier_uri_appliquee` a une fenêtre de réveil bornée ; témoins `i2749_*`. |
| `060a5f10` | Qualité par zone #2723 | Présent : modèle `streaming/quality.rs`, demande par service et preuve distincte du format observé. |
| `49885b68` | Pins OpenHome #2722 | Réécrit : `outputs/openhome_pins.rs`, raccord depuis `openhome.rs` et routes ; témoins HTTP avec faux renderer. |
| `cb4a23be` | Réponses OAAT périmées #2730 | Réécrit : `attendre_accord_format_sur` et `Verdict::Reliquat`, échéance globale conservée. |
| `49f6c56f` | Statistiques OAAT pendant la négociation #2758 | Réécrit : `Verdict::HorsNegociation` ; borne supplémentaire `MAX_TRAMES_ECARTEES = 64`. |
| `6199782f` | Faux succès des playlists de service #1848 | Présent : refus de la demande distante seule et `skipped_streaming` pour une demande mixte ; témoins HTTP. |
| `84522291` | Recherche dans l'annuaire radio #2119 | **Résidu réel** : `search_radios` interroge seulement `RadioRepo::search` ; le rattrapage des logos ne branche pas un annuaire de recherche. |
| `82f7effb` | Ordre manuel des favoris #2001 | Présent : ordre manuel local et de service, schémas et routes actuels. L'ancien numéro de migration ne doit pas être rejoué. |
| `f945bd86` | Retrait d'une racine musicale #2149 | Présent : `racines_retirees` et bilan du `PATCH /system/config`, avec témoins de racines sœurs et imbriquées. |
| `a082c136` | True peak BS.1770-5 #2713 | **Pas équivalent** : le code actuel calcule un true peak par interpolation Catmull-Rom 4× ; l'algorithme de l'ancien commit est différent. La présence de `true_peak` ne prouve pas l'intégration de cet algorithme. |
| `2573b9ed` | Version des valeurs / agrégation #2713 | Partiellement remplacé : complétude et préservation des tags ont des gardes actuelles, et la provenance utilise `rg_track_source` / `rg_album_source`. Les clés versionnées `rg_*_analysis_version` et la version `bs1770-5-true-peak-v1` sont absentes ; ne pas confondre provenance et version d'algorithme. |
| `7aa66ab7` | Invalidation des anciennes valeurs #2713 | **Ne pas rejouer telle quelle** : PostgreSQL 041 et SQLite 089 appartiennent à l'ancien arbre ; l'invalidation dépend de l'algorithme et de la provenance réellement livrés. |
| `823c0d32` | Analyse cédant pendant le décodage #2495 | Présent : `mesurer_en_cedant_a_la_lecture` et abandon du décodage en cours, avec vrai WAV dans le témoin. |
| `a550545f` | Validation de l'URL radio #2097 | Présent : `valider_url_flux`, refus traduits et échappement HTML ; témoins HTTP de création et modification. |

## Validation sur Shrek

Les exécutables de tests ont été construits avec
`cargo test --locked -j6 -p <paquet> --no-default-features --features oaat
--no-run --message-format=json`, puis leur liste réelle a servi à sélectionner
les noms exacts. Chaque groupe est lancé dans le répertoire de sa caisse,
avec au plus quatre threads de tests. Les commandes exactes et les résultats
sont archivés.

| Groupe exécuté | Réussis | Filtrés |
|---|---:|---:|
| `tune-core --lib` : navigateur, Chromecast, cible de transcodage, métadonnées, HEOS, OAAT, qualité, favoris, préemption ReplayGain | 51 | 4494 |
| `tune-core --lib` : true peak actuel, complétude et provenance d'album | 6 | 4539 |
| `tune-core --lib` : lecteur DSF réel et repli du titre | 1 | 4544 |
| `tune-core --lib` : moteur PCM de fondu enchaîné | 8 | 4537 |
| `tune-server --lib` : accueil, contrats de groupes, racines et préférence de qualité | 14 | 1217 |
| `tune-server --lib` : activation du crossfade refusée, désactivation conservée | 2 | 1229 |
| `server_contracts` : Pins OpenHome, playlists, favoris, URL de radios et deux gardes de workflow | 42 | 673 |

**124 tests sélectionnés réussis**, aucune production modifiée. Les deux
gardes de workflow examinent le texte des workflows : elles ne sont pas des
compilations des artefacts livrés. Les autres groupes comprennent notamment
des bases SQLite réelles, des routes Axum et un faux renderer HTTP OpenHome.
La préférence de qualité des services est testée sans compte musical réel.

Une première commande a cherché le témoin DSF dans `integration_contracts` :
**zéro test exécuté**, donc aucune preuve. Le témoin est en réalité dans
`metadata::tests` ; le résultat à un test ci-dessus vient de cette cible,
vérifiée dans la liste des tests compilés.

Ces essais qualifient les comportements des fixtures sur ce SHA. Ils ne
rejouent pas les contre-épreuves de chaque ancien correctif et ne prouvent
ni une recette matérielle, ni l'ensemble des parcours, ni PostgreSQL sur
Shrek. Ils ne ferment donc aucun ticket terrain et ne remplacent pas les
portes du prochain correctif de production.

Worktree : `/srv/builds/worktrees/jp-robbe-20260916-4317-audit`.
Target isolé : `TUNE_TARGET_KEY=jp-robbe-20260916-4317-audit`.
Environnement : `/srv/cache/tune/env.sh`, compilation à 6 jobs.
Preuves : `/srv/builds/jp-evidence/jp-robbe-20260916-4317-audit`.

## Les trois branches sans changement à reprendre

- `batch/vague-10` @ `943bb37cadcb` : aucun commit hors merges en avance.
- `batch/p2-vague-7` @ `54f9168a4fd7` : aucun commit hors merges en avance.
- `batch/vague-14` @ `66b5229a9ded` : trois commits en avance, mais
  `git cherry origin/main origin/batch/vague-14` classe les trois avec `-` :
  `e2734b90`, `506e5da2` et `85c813f6` ont des patches équivalents dans `main`.

Ce dernier constat est une comparaison de patches, pas une recette
supplémentaire de leurs fonctionnalités. Aucune branche n'a été supprimée.

## Conséquence pour la reprise

Ne pas fusionner en bloc les six branches ni résoudre leurs conflits à
l'aveugle. Les changements déjà présents doivent être retirés de la liste des
correctifs supposés absents, en conservant leurs réserves de terrain.

Les résidus crossfade, annuaire radio et algorithme/provenance true peak
demandent une reprise distincte après le train, avec inventaire des verrous,
des fichiers modifiés et des migrations de **toutes** les PR actives.
L'audit ne réserve pas les issues historiques à la place de leurs propriétaires.

Ce rapport ne ferme pas #4317 et n'autorise aucune suppression de branche,
migration, fusion, publication ou modification du train. Il ne change aucun
comportement de production.
