//! Étude #7614 — prototype de suréchantillonnage haute qualité pour Tune.
//!
//! Crate autonome, HORS du serveur : rien ici n'est branché dans la chaîne
//! audio. Elle contient les conceptions de filtres (`conception`), leur mise
//! en phase minimale (`phase_minimale`), l'interpolateur polyphase et son
//! produit scalaire SIMD (`polyphase`), le dither et les formeurs de bruit
//! (`dither`), le test de la machine (`machine`), le banc de mesure
//! (`mesures`) et l'écriture des courbes (`svg`). Le binaire `banc` produit
//! les CSV, SVG et le résumé ; `tests/gabarits.rs` verrouille les gabarits.
//!
//! La note de conception est `docs/etudes/7614-surechantillonnage.md`.

pub mod conception;
pub mod dither;
pub mod fenetres;
pub mod fft;
pub mod machine;
pub mod mesures;
pub mod phase_minimale;
pub mod polyphase;
pub mod svg;

pub use conception::{Conception, Fenetre, Phase, Plan, catalogue, par_id, plan};
pub use dither::{Formeur, Quantificateur, formeur_par_id, formeurs, formeurs_pour};
pub use machine::{RapportMachine, Resultat, tester_la_machine, tester_la_machine_sur};
pub use polyphase::{Surechantillonneur, chemin_simd};
