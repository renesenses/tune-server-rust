# Rubato 3.0.0 — normalisation du noyau (#4079)

JP Robbe / OpenAI Codex / jp-robbe-20260915-201930-4079.

Source : https://crates.io/api/v1/crates/rubato/3.0.0/download
SHA-256 : d4be9c88e3d722d3d36939e41941f6b6f52810c1235bf19998a7d4535b402892

Copie de la version publiée, sans changement d'API ni de version. Les fichiers
d'origine et la licence MIT sont conservés ; .cargo-ok, .github et le
Cargo.lock de développement de la bibliothèque sont exclus. Les espaces
finaux présents dans les fichiers amont sont conservés pour garder leur
comparaison exacte ; les contrôles de formatage de Tune excluent ce vendor.

Seul correctif source : src/sinc.rs accumule la somme de normalisation par
sommation compensée de Kahan. La table et le traitement en flux restent f32 ;
les coefficients ne perdent plus leur gain à cause de l'addition des petites
queues du sinc à une somme déjà grande.

Le banc indépendant de Tune est tune-core/tests/reechantillonnage_reference_2218.rs.
Les mesures et la contre-épreuve sont documentées dans
docs/mesures/2218-reechantillonnage-reference.md.
La copie est une dépendance path pour que les consommateurs externes de
tune-core reçoivent aussi le correctif (précédent vendor/rust_cast).

# Table indépendante de la libm du système (#4532)

Tune / Claude, 19/09/2026.

Deuxième correctif source. Rubato calculait sa table de sincs avec
`f32::sin` (`src/sinc.rs`, via `Sample::sin`) et sa fenêtre avec `f64::cos`
(crate `windowfunctions`). Ces deux fonctions délèguent à la libm du système
(glibc, CRT MSVC, libm d'Apple), dont les derniers bits diffèrent : sur le même
processeur AVX, Tune construisait une autre table sous Windows que sous Linux,
et rendait donc d'autres octets. Mesuré le 19/09 : table `3ee45849…` (Linux),
`ebd54afd…` (Windows), `60be8845…` (macOS) pour 44,1 → 48 kHz.

- `src/sample.rs` : `Sample::sin`/`cos` appellent `libm::sinf`/`cosf`/`sin`/`cos`
  (crate `libm`, Rust pur, mêmes bits partout).
- `src/windows.rs` : la fenêtre cosinus est évaluée ici, avec la formule, les
  coefficients et l'ordre d'opérations de `windowfunctions` 0.1.1, mais avec
  `libm::cos`. La dépendance `windowfunctions` est retirée.

Construction seulement : le traitement en flux ne change pas. La table vaut
désormais `1669306f…` sur Linux, Windows et macOS. Ce qui reste différent d'un
processeur à l'autre est le produit scalaire choisi à l'exécution (AVX+FMA,
SSE3, NEON, scalaire) : c'est le contrat amont, relevé par noyau dans
`tune-core/src/outputs/local/empreinte_du_puits_r1.rs`.
