//! #5439 — à quelle cadence le chemin COMPRESSÉ ouvre le périphérique.
//!
//! Belkadi Yacine, fil 1909, Tune 0.9.167, DENAFRIPS Terminator II sur
//! `alsa:hw:CARD=2,DEV=0` : un FLAC 48 kHz servi par la Freebox part au DAC à
//! 44,1 kHz, et le son n'arrive qu'au bout de 17 s.
//!
//! ```text
//! 15:39:32.532 local_audio_decoded_compressed_stream channels=2 sample_rate=48000
//! 15:39:32.629 local_audio_compressed_rate_mismatch_will_resample source_sr=48000 device_sr=44100
//! 15:39:46.106 rubato_batch_resample_complete from_sr=48000 to_sr=44100
//! 15:39:46.155 local_audio_compressed_playing_after_prefill demarrage_ms=17110
//! ```
//!
//! La cadence de la source était JUSTE : c'est le décodage qui la lit (48 kHz),
//! ni le DIDL ni un en-tête HTTP n'entrent dans ce chemin. Le 44,1 kHz vient de
//! `default_output_config()`, que la branche compressée prenait tel quel dès
//! qu'il différait de la source. Or sur ALSA ce « défaut » n'est pas la cadence
//! du DAC : cpal rend 44 100 Hz dès que la plage du PCM le contient
//! (`cpal-0.17.3/src/host/alsa/mod.rs:634-637`). Le chemin PCM, lui, passe par
//! [`decide_local_rate_opening`] et ouvre le même DAC à 96 kHz quand la
//! source l'est (`local_audio_open_at_source_rate_reported_supported`, même
//! journal, 15:06:55).
//!
//! Les 17 s sans son en découlent pour l'essentiel : la piste entière est
//! rééchantillonnée d'un bloc AVANT le pré-remplissage (13,5 s ici). Ouverte à
//! la cadence de la source, elle n'a plus rien à convertir.
//!
//! Ce module est la décision du chemin compressé, rendue identique à celle du
//! chemin PCM, et la mise au format de sortie de la piste décodée : les deux
//! morceaux de `play_url` que le témoin peut jouer sans carte son.

use tracing::{info, warn};

use super::{LocalRateOpening, SampleRateEvidence, adapt_channels, decide_local_rate_opening};
use crate::outputs::traits::FormatOuvert;

/// La configuration que le chemin compressé ouvre, et la décision qui l'a
/// choisie.
///
/// - `defaut` : ce que rend `default_output_config()`, ou `None` si la sonde a
///   échoué ;
/// - `enumeree` : ce que rend `find_matching_config` **filtré** sur la cadence
///   de la source — l'appelant ne le calcule que si `defaut` n'y est pas
///   déjà, comme le chemin PCM (`backend.rs`) ;
/// - `preuve` : ce que vaut la liste de cadences de CE périphérique
///   ([`super::sample_rate_evidence_for_device`]) ;
/// - `dernier_recours` : l'ouverture quand le périphérique n'annonce aucune
///   cadence par défaut, inchangée depuis avant #5439.
pub(super) fn choisir_la_config_du_flux_compresse(
    source_sr: u32,
    defaut: Option<cpal::StreamConfig>,
    enumeree: Option<cpal::StreamConfig>,
    preuve: SampleRateEvidence,
    dernier_recours: impl FnOnce() -> cpal::StreamConfig,
) -> (cpal::StreamConfig, LocalRateOpening) {
    let defaut_sr = defaut.as_ref().map(|c| c.sample_rate);
    let decision = decide_local_rate_opening(source_sr, defaut_sr, enumeree.is_some(), preuve);
    let config = match (decision, defaut, enumeree) {
        (LocalRateOpening::DeviceAlreadyAtSourceRate, Some(cfg), _) => cfg,
        (LocalRateOpening::AtSourceRateMeasured, _, Some(cfg)) => cfg,
        (LocalRateOpening::ResampleToDeviceRate { .. }, Some(cfg), _) => cfg,
        // `LastResortSourceRate` — et, par exhaustivité, les couples que
        // `decide_local_rate_opening` ne produit pas.
        _ => dernier_recours(),
    };
    (config, decision)
}

/// Journalise la décision du chemin compressé.
///
/// Le nom `local_audio_compressed_rate_mismatch_will_resample` est gardé pour
/// le bras qui convertit : c'est lui que les relevés de terrain cherchent.
pub(super) fn journaliser_la_cadence_du_flux_compresse(
    decision: LocalRateOpening,
    source_sr: u32,
    ouverte_sr: u32,
    backend: &str,
    endpoint_id: &str,
    preuve: SampleRateEvidence,
) {
    match decision {
        LocalRateOpening::DeviceAlreadyAtSourceRate => info!(
            source_sr,
            backend = %backend,
            endpoint_id = %endpoint_id,
            rate_support_measured = preuve.is_measured(),
            "local_audio_compressed_device_already_at_source_rate"
        ),
        LocalRateOpening::AtSourceRateMeasured => info!(
            source_sr,
            backend = %backend,
            endpoint_id = %endpoint_id,
            rate_support_measured = preuve.is_measured(),
            "local_audio_compressed_open_at_source_rate"
        ),
        LocalRateOpening::ResampleToDeviceRate { reason, .. } => warn!(
            source_sr,
            device_sr = ouverte_sr,
            backend = %backend,
            endpoint_id = %endpoint_id,
            rate_support_measured = preuve.is_measured(),
            reason = reason.code(),
            "local_audio_compressed_rate_mismatch_will_resample"
        ),
        LocalRateOpening::LastResortSourceRate => info!(
            source_sr,
            opened_sr = ouverte_sr,
            backend = %backend,
            endpoint_id = %endpoint_id,
            "local_audio_compressed_rate_last_resort_no_device_default"
        ),
    }
}

/// Met la piste décodée (après DSP) au format réellement ouvert : canaux,
/// puis cadence. Piste entière en mémoire : `rubato_resample_track` retire le
/// délai de groupe du sinc et rend exactement `round(trames × ratio)` (#2246).
pub(super) fn conformer_la_piste_decodee(
    samples: Vec<f32>,
    source_sr: u32,
    source_ch: u16,
    sortie: FormatOuvert,
) -> Vec<f32> {
    let mut samples = samples;
    if source_ch != sortie.canaux {
        samples = adapt_channels(&samples, source_ch, sortie.canaux);
    }
    if source_sr != sortie.cadence {
        samples = super::rubato_resample_track(&samples, source_sr, sortie.cadence, sortie.canaux);
    }
    samples
}

/// macOS : cpal ne recadence PAS le matériel pour un flux de sortie partagé.
/// Ouvrir « à la cadence de la source » laisserait le DAC à la cadence du
/// système et CoreAudio convertirait en silence — voire rendrait du silence
/// pour le DSD→PCM haute cadence (Cyrille, iFi Neo iDSD / FiiO K3). On fixe
/// donc la cadence nominale, au mieux : si le périphérique ne se résout pas,
/// rien ne change.
///
/// Sorti de `BackendCpal::ouvrir` (chemin PCM) pour servir aussi le chemin
/// compressé (#5439) : les deux ouvrent désormais à la cadence de la source
/// dans le même cas.
#[cfg(target_os = "macos")]
pub(super) fn caler_la_cadence_nominale_coreaudio(device_name: &str, sample_rate: u32) {
    use coreaudio::audio_unit::macos_helpers;
    if let Some(dev_id) = macos_helpers::get_device_id_from_name(device_name, false) {
        let want = sample_rate as f64;
        match macos_helpers::set_device_sample_rate(dev_id, want) {
            Ok(_) => info!(
                device = %device_name,
                to = sample_rate,
                "local_audio_coreaudio_nominal_rate_set_shared"
            ),
            Err(e) => warn!(
                error = %e,
                wanted = sample_rate,
                "local_audio_coreaudio_set_rate_failed"
            ),
        }
    }
}
