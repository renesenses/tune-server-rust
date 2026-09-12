//! Harnais unique des contrats d’intégration de `tune-core`.
//!
//! Les cas restent séparés par module et conservent leurs fixtures. Les réunir
//! évite de lier sept fois la même crate de 185 000 lignes.

#[path = "alac_aiff_wav_empreintes_reference.rs"]
mod alac_aiff_wav_empreintes_reference;
#[path = "aucune_fuite_de_temporaires.rs"]
mod aucune_fuite_de_temporaires;
#[path = "audio_integration.rs"]
mod audio_integration;
// Refuse tout nouveau fabricant de fichier dans audio_integration.rs qui
// n'annoncerait pas où la justesse du PCM de son format est prouvée (#2218, T5).
#[path = "audio_integration_perimetre_2218.rs"]
mod audio_integration_perimetre_2218;
#[path = "crossfade_pas_de_rampe_de_volume.rs"]
mod crossfade_pas_de_rampe_de_volume;
#[path = "dsd_empreintes_reference.rs"]
mod dsd_empreintes_reference;
#[path = "dsd_streaming_repro.rs"]
mod dsd_streaming_repro;
#[path = "dsp_track_boundary.rs"]
mod dsp_track_boundary;
#[path = "flac_empreintes_reference.rs"]
mod flac_empreintes_reference;
// Les contrôles d'intégrité que les conteneurs offrent, et ce que le décodeur
// en fait — tranche T4 de #2218.
#[path = "integrite_conteneurs_2218_t4.rs"]
mod integrite_conteneurs_2218_t4;
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
