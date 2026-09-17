# Preuves de la première tranche SDK — #4363

JP Robbe / OpenAI Codex / `jp-robbe-20260917-sdk-premium`.
Base : `rc/v0.9.153`, `af70d7e251735d8be2c5d7ddc9d6539f61e2ac34`.
Validation ciblée le 17/09/2026 sur Shrek, Rust 1.98.0.

## Résultats exécutés

- Formatage : réussi.
- SDK : 8 tests de contrats, 2 tests du testkit, 1 doctest réussis.
- Projets générés **hors workspace** : 3 tests DSP et 3 tests batch réussis.
- Scaffolding : noms invalides, refus d'écrasement, version SDK incompatible,
  chemins avec espaces et indépendance des dépendances vérifiés.
- Clippy de toutes les cibles du workspace SDK : réussi avec `-D warnings`.
- Rustdoc : généré avec `-D warnings`, sans diagnostic.
- Matrice : 45 exigences recensées ; empreintes de référence et références des
  témoins vérifiées. Les 45 états de production restent `pending`.
- `cargo check` du workspace SDK pour macOS ARM/Intel et Windows x64 : réussi.
  Ce sont des vérifications croisées de compilation, **pas des exécutions** sur
  ces systèmes, ni des plugins natifs chargés dynamiquement.

Les 17 tests distincts réussis ne couvrent ni le pipeline de production Tune,
ni un codec réel, ni une carte son, ni un écran installé. Les exemples sont
un gain f32 et une copie PCM via un hôte mémoire. Aucun résultat du testkit
n'est présenté comme acceptation des quatre plugins premium.

## Commandes

Environnement de l'unité :

```sh
export TUNE_TARGET_KEY=jp-sdk-premium-20260917
. /srv/cache/tune/env.sh
cd /srv/builds/worktrees/jp-sdk-premium-20260917
cargo fmt --manifest-path sdk/Cargo.toml --all -- --check
cargo test --manifest-path sdk/Cargo.toml --workspace --locked
cargo clippy --manifest-path sdk/Cargo.toml --workspace --all-targets --locked -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --manifest-path sdk/Cargo.toml --workspace --no-deps --locked
cargo build --manifest-path sdk/Cargo.toml -p cargo-tune-plugin --locked
python3 sdk/scripts/verify_scaffolding.py --binary "$CARGO_TARGET_DIR/debug/cargo-tune-plugin"
python3 sdk/scripts/verify_matrix.py
python3 sdk/scripts/verify_counterproofs.py --binary "$CARGO_TARGET_DIR/debug/cargo-tune-plugin"
cargo check --manifest-path sdk/Cargo.toml --workspace --locked \
  --target aarch64-apple-darwin --target x86_64-apple-darwin \
  --target x86_64-pc-windows-msvc
```

## Contre-épreuves : code muté, témoins inchangés

Le script conservé `verify_counterproofs.py` reproduit les trois cycles
vert → rouge comportemental → restauration par copie → vert :

| Mutation | Témoin | Rouge observé |
|---|---|---|
| Ignorer une capacité requise absente | `missing_required_capability_is_rejected_before_setup` | `left: Ok({})`, `right: Err(MissingCapability("audio-process"))` |
| Accepter un spectre privé de ses valeurs | `spectrum_requires_real_axes_resolution_and_measurement_provenance` | `missing spectrum data must fail` |
| Remplacer le gain demandé par l'identité dans le plugin généré | `gain_reaches_captured_samples_across_block_sizes` | `PCM mismatch at sample 0: actual=0.8, expected=0.4` |

Chaque rouge compile et échoue dans le test nommé. Chaque retour au vert est
exécuté ; aucune contre-épreuve ne consiste à casser le test ou sa compilation.

Deux dispositions de validation : le premier Clippy a signalé des formes
remplaçables par `as_chunks`, `is_multiple_of` et `derive(Default)` ; elles sont
corrigées. Le premier script de contre-épreuve restaurait aussi l'ancien mtime
avec `copy2`, ce qui laissait Cargo réutiliser le binaire muté. La restauration
emploie maintenant `copyfile`, avec un mtime neuf ; les trois cycles complets
ont ensuite réussi et la suite complète a été rejouée.

## Artefacts locaux conservés

Répertoire opérateur : `/Users/jp/dev/perso/tune/reports/sdk-premium-4363/`.

| Fichier | SHA-256 |
|---|---|
| `source-sha256.json` (empreinte de chaque fichier SDK) | `5d653ab0d0237fcd53781b6e32468a359767e7b0ac3da732381fe3f89102ebbc` |
| `jp-sdk-premium-20260917-final.log` | `4c75def2327aa5b72ef0c91451803aebe6e3b19b85f4dd8bd1d65b29331aa025` |
| `jp-sdk-premium-20260917-counterproof.log` | `a65eef84c7e8801dec82a8c626ce787bddea403b1213b15d6ab917f19dd95651` |
| `jp-sdk-premium-20260917-validation.log` (essai initial avec l'échec de restauration, non présenté comme vert) | `eac7d16ba373a95d2845c1550f18d2d1505ee321a5fc80799ed2a8a6f22abae2` |

La CI GitHub, les branchements hôtes, les quatre plugins et l'acceptation
multiplateforme du produit sont des preuves distinctes de ces résultats locaux.
