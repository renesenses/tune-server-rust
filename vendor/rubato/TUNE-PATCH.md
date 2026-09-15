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
