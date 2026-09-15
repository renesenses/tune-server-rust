# #4191 — entropie APE 3.97

JP Robbe / OpenAI Codex / jp-robbe-20260915-211950-4191.
Mesures sur Shrek, base `24123a4e6b5589e7ce90bb1f3cbe7d132cc4d52e`,
Rust 1.98, six jobs, target propre à cette unité.

## Défaillance reproduite

`ape-decoder` 0.3.2 accepte les en-têtes ≥ 3950 mais utilise partout
l'entropie introduite en 3990. Le fichier public `sh3.ape`, 3970/2000,
échoue à la trame 0 avec le même `overflow range_total out of bounds` que
le journal de #4191. Le correctif décode ses trois trames ; les octets PCM
sont identiques à la référence indépendante. Empreintes et provenance :
`vendor/ape-decoder/TUNE-PATCH.md`.

## Corpus synthétique durable

Huit fixtures encodées par le SDK 3.97 historique, à partir de PCM défini
avant encodage, couvrent 3970/4000 et 3970/2000, 8/16/24 bits, mono,
stéréo, pseudo-stéréo, silence et plusieurs trames. La comparaison
PCM d'entrée / référence FFmpeg 8.0 / dépendance corrigée est exacte
pour les huit. La régénération dans un second répertoire produit les mêmes
fichiers et le même manifeste.

Les tests passent aussi par `decode_to_pcm` et le chemin progressif réel.
Ils gardent la recherche à travers une limite de trame et le refus du CRC
corrompu. Deux fichiers modernes, 3990/3000 et 3990/4000, conservent exactement
le WAV de référence déjà versionné.

La recette complète et les SHA-256 sont dans
`tune-core/tests/fixtures/ape/legacy3970/README.md`. Aucun outil externe
n'est requis à l'exécution de la suite Rust.

## Commandes

Avec `TUNE_TARGET_KEY=jp-robbe-20260915-211950-4191`,
`CARGO_BUILD_JOBS=6` et `/srv/cache/tune/env.sh` chargé :

```sh
cargo test -p tune-core --no-default-features --features oaat   --test integration_contracts ape_legacy_4191
cargo test -p tune-core --no-default-features --features oaat   --test ape_incremental_i2505
cargo test -p tune-core --no-default-features --features oaat   --test integration_contracts audio_integration::
cargo clippy -p tune-core --lib --test integration_contracts   --no-default-features --features oaat -- -D clippy::correctness
cargo fmt --all -- --check
rustfmt --check --edition 2021 vendor/ape-decoder/src/{decoder,entropy,range_coder}.rs
```

## Résultats

12/12 tests ciblés, 4/4 tests APE incrémentaux et 30/30 tests audio réussissent,
aucun ignoré. Clippy correctness et les deux vérifications de formatage
réussissent. Les avertissements préexistants restent visibles dans les journaux.

## Contre-épreuve

Tests inchangés, seule la condition de dispatch est neutralisée :
`if version >= 3990` devient `if version >= 3950`. Ainsi les anciens fichiers
retombent dans le décodeur entropique moderne d'avant correction.

Compilation réussie, **10 rouges / 2 verts** :

- `legacy_streaming_emits_exact_pcm` :
  `ape decode_frame 0/2 à 0.0 s: decoding error: range coder: overflow range_total out of bounds` ;
- `stereo16_c4000` : `3.97 entropy must decode: DecodingError("16-bit sample overflow")` ;
- le témoin moderne 3990/3000 + 3990/4000 reste vert ;
- le silence reste vert car il n'a aucun résidu à décoder.

Restauration de `entropy.rs` par `cp` de sa sauvegarde puis relance :
**12/12 verts**.
Journaux Shrek : `/tmp/jp-4191-counterproof-final.log`,
`/tmp/jp-4191-tune-final.log`, `/tmp/jp-4191-incremental.log`,
`/tmp/jp-4191-audio.log`, `/tmp/jp-4191-clippy.log` et
`/tmp/jp-4191-fmt.log`.

## Limites et intégration

Le CDImage 3970/4000 complet du testeur n'a pas été disponible ; aucun
résultat sur ses 624 trames ni sur son DAC Windows n'est affirmé.
Les versions historiques autres que 3970 ne disposent pas encore d'un
corpus dédié ici. Les prédicteurs existants, contrôles CRC et limites de
samples ne sont pas assouplis.

Le correctif ne touche pas `tune-core/src/audio/decode.rs` ni les fichiers
de la PR WavPack #4207. Il rejoint le lot audio JP
`batch/jp-p2-resampling-20260915-201930`, qui porte déjà les changements des
manifestes Cargo : l'intégration doit conserver les deux chemins vendored
indépendants, Rubato (#4229) et ape-decoder. L'audit de sécurité de la base
reste suivi par #4200. Pas de migration ni bump de version.
