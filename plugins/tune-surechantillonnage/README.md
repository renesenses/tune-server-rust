# tune-surechantillonnage — prototype de l'étude #7614

Crate **autonome** (son propre `[workspace]`, aucune dépendance tierce), hors du
serveur : rien n'est branché dans la chaîne audio de Tune. Elle sert à juger
les conceptions de filtres, le dither et les formeurs de bruit, et à mesurer
ce qu'une machine tient en temps réel. La note de conception est
`docs/etudes/7614-surechantillonnage.md`.

```sh
cd plugins/tune-surechantillonnage
cargo test --release            # gabarits (tests/gabarits.rs) et tests unitaires
cargo run --release --bin banc  # écrit banc/ : CSV, SVG, resume.md
cargo run --release --bin banc -- banc --rapide
```

| module | contenu |
|---|---|
| `conception` | les filtres (polyvalent gaussien apodisant, précision Kaiser 150 dB, précision longue, résolution à transition étendue, variantes en phase minimale), le plan à deux étages des rapports non entiers |
| `fenetres` | gaussienne, Kaiser (I₀), Blackman-Harris, formules de Kaiser |
| `phase_minimale` | transformation par le cepstre réel |
| `polyphase` | interpolateur `L/M` multicanal, produit scalaire AVX2+FMA / NEON / scalaire |
| `dither` | PRNG déterministe, TPDF, formeurs par rétroaction d'erreur (binomiaux et publiés : Lipshitz 1991, Wannamaker 1992) |
| `machine` | débit réel de chaque filtre sur chaque rapport, admission avec marge |
| `mesures` | réponses impulsionnelle et fréquentielle, atténuations, rejet des images par balayage, THD+N, bruit après quantification (brut, 20 Hz–20 kHz, pondéré Terhardt), latence |
| `svg` | courbes SVG et CSV |
| `bin/banc` | le banc complet |
| `tests/gabarits.rs` | les promesses verrouillées |
