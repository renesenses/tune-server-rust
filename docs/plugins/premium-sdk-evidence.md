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
