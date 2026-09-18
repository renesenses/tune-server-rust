# Preuves SDK premium — #4363

JP Robbe / OpenAI Codex / `jp-robbe-20260917-sdk-premium`.
Base : RC `v0.9.153`, `af70d7e251735d8be2c5d7ddc9d6539f61e2ac34`.
Validation Linux ciblée sur Shrek, Rust 1.98.0, unité isolée `jp-sdk-full-4363`.

## Résultats exécutés

- Workspace SDK : 79 tests/doctest réussis, dont les tests historiques déplacés avec les algorithmes, les contrats et les tests des quatre implémentations.
- Frontière native : trois tests chargent les vrais cdylibs, traitent F32 et PCM 16/24/32, transmettent les historiques, exécutent les callbacks batch et vérifient annulation/métadonnées. Un paquet signé est installé, chargé, mis à jour et rollbacké ; un processeur de l'ancienne version reste utilisable après désactivation.
- Paquets : signatures de fixture, refus d'altération du paquet/fichier installé, cible incompatible, mauvais identifiant et chemins dangereux ; conservation de l'activation précédente.
- Parité historique/composée/native : **4 644 864 octets identiques**, SHA-256 `cc87f7716466f522bd095a5d0aa76f7860be493fc128ae33c032a25ac0c4e49d`. Cadences 44,1/48/96/192 kHz, profondeurs 16/24/32, canaux 1/2/6, profils et mises à jour avec historique. Les données de référence proviennent du source historique via `git show`, pas d'un second import du nouveau moteur.
- Six projets externes générés, tests réellement exécutés, exports natifs compilés. `dev` transforme un WAV relu ; `pack` produit une archive dont manifeste, cible, ABI, bibliothèque et UI sont inspectés. Refus d'écrasement et de version incompatible.
- Schémas de configuration, manifeste, commandes, événements et jobs générés depuis les types Rust. Le script de vérification compare les fichiers conservés aux sorties du générateur compilé.
- UI : cinq tests Node portent sur le routage des commandes, l'isolation du port/iframe, la libération des abonnements, la zone des presets/AutoEq, la réponse préparée et le rejet des spectres périmés/incomplets.
- Adaptateur serveur : FLAC relu à PCM identique, WAV Dé-ploc, sources et destinations préexistantes intactes, annulation sans artefact, handles forgés refusés, journal récupéré après disparition du worker.
- Hôte core : les seize combinaisons d'installation et la migration idempotente passent ; le vrai analyseur délivre un spectre sans plugins et refuse une époque périmée ou un point de mesure inexistant.
- Format SDK, Clippy incluant tous les features avec `-D warnings`, Rustdoc avec `-D warnings` et inventaire de 45 exigences vérifiés.

Après les derniers raccordements, 4 témoins hôtes serveur, 5 tests de la chaîne levels/spectre, les 32 contrats historiques du gestionnaire de plugins et la garde canonique des workflows passent. Le dernier changement fonctionnel complémentaire refuse une bibliothèque batch dans un slot DSP avant activation ; il est couvert par le test de packaging.

La CI SDK du commit `cc78ef8f` réussit sous Linux et macOS. Sous Windows, les schémas comportant des descriptions françaises étaient lus avec l'encodage système ; les scripts imposent désormais UTF-8, y compris pour les sources historiques de la parité. La CI du dernier SHA publié reste la référence pour la validation finale multiplateforme.

## Contre-épreuves

`python3 sdk/scripts/verify_native_counterproofs.py` a exécuté les trois cycles vert → rouge → restauration par copie → vert. Les tests sont inchangés :

| Code retiré | Échec observé |
|---|---|
| Appel effectif du processeur à travers l'ABI | `native_dsp_processes_real_buffers_preserves_history_and_library_lifetime` détecte un DSP qui ne modifie plus le signal ; le témoin d'activation signée détecte aussi la perte d'effet. |
| Vérification de confiance/signature | `signed_install_update_rollback_and_tamper_refusal` détecte l'acceptation du paquet avec une signature différente. |
| Filtre de génération du tap réel | `premium_sdk_real_spectrum_without_plugins_refuses_fake_points_and_stale_epochs` échoue : « obsolete epoch was published ». |

La première tranche possède également ses trois contre-épreuves : capacité requise absente, spectre sans données et gain remplacé par l'identité (`verify_counterproofs.py`).

## Dispositions des échecs rencontrés

- La fixture de migration n'avait pas créé la table settings : elle utilise désormais les migrations réelles. Les seize combinaisons passent après correction.
- Le premier test natif EQ passait un profil incomplet : les champs historiques obligatoires sont maintenant explicites dans la fixture.
- Cargo exposait sous le même nom un cdylib compilé avec/sans exports natifs. Le test chargeait ensuite un fichier sans `tune_audio_plugin_v1`. `verify_native.py` utilise désormais un sous-target dédié `sdk-native`, distinct des captures source et projets générés. Le problème est reproduit puis le chargement réel repasse.
- Windows : une lecture des fichiers JSON avec l’encodage système altérait les descriptions françaises. Les lectures/écritures de vérification utilisent explicitement UTF-8.
- La CI de la première tranche a détecté l'absence de `--no-fail-fast` dans la nouvelle porte SDK. Le workflow est corrigé ; le témoin canonique `workflows_bornes::toute_porte_cargo_test_va_jusqu_au_bout` est inclus dans la validation de cette suite.

## Reproduction

```sh
export TUNE_TARGET_KEY=jp-sdk-full-4363
. /srv/cache/tune/env.sh
cargo fmt --manifest-path sdk/Cargo.toml --all -- --check
cargo clippy --manifest-path sdk/Cargo.toml --workspace --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path sdk/Cargo.toml --workspace --locked --no-fail-fast
python3 sdk/scripts/verify_schemas.py
python3 sdk/scripts/verify_native.py
python3 sdk/scripts/verify_dsp_parity.py
python3 sdk/scripts/verify_scaffolding.py --binary "$CARGO_TARGET_DIR/sdk-native/debug/cargo-tune-plugin"
python3 sdk/scripts/verify_native_counterproofs.py
cargo test -p tune-server --lib premium_sdk --no-default-features --features oaat
cargo test -p tune-core --lib premium_sdk --no-default-features --features oaat
cargo test -p tune-core --lib levels_ --no-default-features --features oaat
cargo test -p tune-server --test plugin_contracts --no-default-features --features oaat
cargo test -p tune-server --test server_contracts workflows_bornes::toute_porte_cargo_test_va_jusqu_au_bout --no-default-features --features oaat
node --test sdk/ui/client.test.mjs sdk/ui/bridge.test.mjs
python3 sdk/scripts/verify_matrix.py
```

## Limites d'acceptation

Ces résultats prouvent les périmètres exécutés, pas une qualification de tous les codecs externes, cartes son ou appareils réseau. Les écrans métier existants restent intégrés ; le SDK fournit le montage et le panneau distribué de référence, dont la qualification navigateur complète reste à effectuer. La charge multizone, CoreAudio/WASAPI, l'écoute et l'acceptation métier restent distinctes. La matrice maintient les états de production `pending`.

Les paquets de développement ne sont pas une publication. Aucune clé de signature de production, configuration de confiance, fusion, release ou installation chez un utilisateur n'a été effectuée.

Clôture de la parité : les compteurs locaux et le registre hôte d’écrêtage sont
équivalents entre historique, composé et natif (1 999 échantillons écrêtés, un
échantillon non fini, première position, pic et journal de clôture conservés).
Le témoin utilise les sources historiques épinglées et ne partage pas le nouveau moteur.
Sur Windows, les lecteurs de schémas et les flux de logs Python imposent UTF-8 ;
les échecs cp1252 ont été identifiés séparément des résultats audio.

La garde globale de reachabilité #2816 demandait une étape Cargo explicite pour
`native-conformance`, malgré l’exécution réelle par `verify_native.py`. Le workflow
sépare désormais la construction des cdylibs et l’étape Cargo avec feature et
répertoire natif déclarés ; les assertions et la garde restent inchangées.


## Suivi du 17 septembre : catalogue unique et EQ FREE

Sur le lot `batch/jp-sdk-premium-20260917`, hors RC publiée :

- Catalogue `sdk/plugins.json`, 19 expansions CI/release/Docker : cinq tests Python, ajout réel d’un membre Cargo fictif et retrait de chaque feature attendue. Désactiver le contrôle de dérive fait échouer les tests ; restauration verte.
- Shrek, `TUNE_TARGET_KEY=jp-sdk-catalog-free-4363` : quatre tests core (migration, PCM FREE, PURE, spectre), dix tests HTTP (écriture/lecture EQ et presets FREE, requête mixte atomique, refus crossfeed localisés), 33 gardes workflows réussis. Les deux gardes Windows ont conservé leurs capacités exigées et comparent désormais les tokens plutôt que leur ordre.
- Client web compagnon : trois tests sur l’écran réellement monté (curseur FREE → POST des bandes, avertissement PURE, ancien serveur 402), contre-épreuve par restauration du verrou Premium puis retour au vert. Validation complète conservée dans l’archive de suivi.
- `actionlint` retrouve les mêmes cinq diagnostics préexistants dans `release.yml` qu’au parent, aucun diagnostic nouveau dans les quatre workflows modifiés.

Les preuves du paragraphe précédent restent attachées à leur commit initial. Les nouveaux journaux sont conservés séparément dans `reports/sdk-premium-4363/catalog-free-20260917/`. Les anciens paquets Linux de l’archive initiale précèdent les nouveaux droits FREE/crossfeed ; ils ne représentent pas cette révision. À cette date, la politique de migration du crossfeed FREE restait à décider. La décision du suivi du 18 septembre est désormais la coupure nette, avec conservation des réglages.

## Suivi du 18 septembre : livraison embarquée et offre commerciale

- `bundled_in` rend explicite le lien obligatoire des quatre fournisseurs dans le binaire. Le catalogue refuse la suppression de la dépendance EQ ou son passage en dépendance optionnelle ; aucun paquet natif n’est nécessaire pour le démarrage FREE.
- Sept tests Python passent. Inverser successivement l’entitlement de chacun des quatre manifests fait échouer le témoin commercial ; restaurer les fichiers par copie rétablit le vert.
- Shrek, unité `jp-sdk-offer-4363` : le contrat HTTP `audio_offer_free_eq_and_premium_four_survive_real_startup` passe. Il démarre le vrai chargeur sans paquet natif, écrit EQ/preset pour les deux offres, refuse crossfeed/convertisseur/Dé-ploc en FREE et termine les deux tâches WAV en Premium. Les réglages historiques du crossfeed restent identiques en base après la migration et le refus ; le passage Premium suivi de la réactivation les retrouve.
- Un témoin supplémentaire de l’orchestrateur protège la préparation PCM contre un contournement de licence par les drapeaux d’installation, puis vérifie le traitement Premium et la rétrogradation.
- Client : 449 fichiers et 4 808 tests passent, avec les gardes Svelte/i18n et onze traductions complètes. Les écrans EQ et Crossfeed v2 sont réellement montés. Retirer l’écran de coupure FREE fait échouer son témoin ; restauration verte. Le panneau Lecture en cours utilise le même motif et la même traduction.
- Notes de version préparées dans `docs/release-notes/sdk-audio.md` : EQ gratuit embarqué, coupure nette du crossfeed FREE, conservation puis récupération des réglages. Aucune release existante n’est modifiée.

Les journaux de ce suivi sont isolés dans `reports/sdk-premium-4363/offer-20260918/`. Les preuves DSP antérieures restent attachées à leurs révisions ; aucun algorithme DSP n’est modifié par ce suivi. Les checks GitHub du nouveau SHA et la qualification matérielle restent des validations distinctes.
