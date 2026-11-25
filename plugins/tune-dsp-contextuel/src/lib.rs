//! Greffons DSP « contextuels » de TESS — vague 1 (corrections techniques,
//! gratuites).
//!
//! - [`profil`] : le moteur de profils (règles → traitements, raison,
//!   priorité, droit ; choix « toujours / jamais » de l'utilisateur) ;
//! - [`analyse`] : les mesures hors ligne d'un fichier (continu, polarité,
//!   écart L/R, double-mono, crête vraie, DR) et l'indicateur de
//!   pré-accentuation (TOC, CUE) ;
//! - [`traitements`] : les neuf traitements temps réel, bit-perfect quand ils
//!   sont inactifs ou neutres, sans saut de niveau ;
//! - [`tess`] : l'adaptateur vers `EtageTess`, à brancher après la fusion des
//!   lots TESS.
//!
//! Contrat : `coordination/dsp-contextuels/contrat-moteur.md`.

pub mod analyse;
pub mod hausses;
pub mod iso226;
pub mod profil;
pub mod tess;
pub mod traitements;
