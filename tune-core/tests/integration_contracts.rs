//! Harnais unique des contrats d’intégration de `tune-core`.
//!
//! Les cas restent séparés par module et conservent leurs fixtures. Les réunir
//! évite de lier sept fois la même crate de 185 000 lignes.

#[path = "audio_integration.rs"]
mod audio_integration;
#[path = "dsd_streaming_repro.rs"]
mod dsd_streaming_repro;
#[path = "dsp_track_boundary.rs"]
mod dsp_track_boundary;
#[path = "migration_on_real_db.rs"]
mod migration_on_real_db;
#[path = "no_blind_ffmpeg.rs"]
mod no_blind_ffmpeg;
#[path = "oaat_negociation.rs"]
mod oaat_negociation;
#[path = "pochette_radio_source_unique.rs"]
mod pochette_radio_source_unique;
#[path = "poller_bascule.rs"]
mod poller_bascule;
// Refuse tout fichier de tests/ que ni le manifeste ni cet agrégateur n'atteint
// — sans quoi le prochain harnais posé ici serait vert sans jamais tourner.
#[path = "tests_orphelins.rs"]
mod tests_orphelins;
