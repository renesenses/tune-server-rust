//! Le bras WASAPI exclusif de `play_url` (R6 bis, #2219).
//!
//! Ce module est le bloc `#[cfg(target_os = "windows")] if exclusive_mode &&
//! audio_backend != "asio" { … }` de `play_url`, sorti à l'identique : le
//! texte est celui du bloc, ce qu'il lisait du contexte de `play_url` est
//! devenu [`EntreesWasapi`], construit par `play_url` juste avant l'appel. Le
//! bloc sortait TOUJOURS par `return` (les trois bras de son `match`) : le
//! bras est terminal, il ne rend rien à la suite de `play_url`, qui fait
//! `return` après l'appel. Aucune condition ne change — le `if` et sa
//! bannière restent dans `play_url`.
//!
//! Compilé par la seule étape « ASIO » du job `windows-pr` de `ci.yml` (la
//! seule qui active `local-audio` sous Windows) : ni Shrek ni le Mac ne
//! voient ce fichier. `super` désigne ici `outputs::local`, pas `outputs` : le
//! module WASAPI se nomme par `crate::outputs::wasapi_exclusive`.

// ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------

use std::io::Read;

use super::*;
use crate::outputs::wasapi_exclusive::WasapiExclusiveOutput;

/// Ce que le bras lisait du contexte de `play_url` — trente et une valeurs,
/// toutes déjà possédées par le fil de lecture. Elles sont DÉPLACÉES, jamais
/// empruntées (leçon de #3386). Seul bras à lire `endpoint_id` : WASAPI ouvre
/// l'`IMMDevice` exact capturé à la découverte (#2207).
pub(super) struct EntreesWasapi {
    pub(super) device_name: String,
    pub(super) endpoint_id: Option<String>,
    pub(super) sample_rate: u32,
    pub(super) bit_depth: u16,
    pub(super) channels: u16,
    pub(super) data_offset: usize,
    /// Les 4 096 premiers octets lus par `play_url` ; ce qui suit
    /// `data_offset` est le début du PCM.
    pub(super) header_buf: Vec<u8>,
    /// La réponse HTTP, positionnée après `header_buf`.
    pub(super) reader: reqwest::blocking::Response,
    pub(super) frame_bytes: usize,
    pub(super) seek_offset: u64,
    pub(super) my_generation: u64,
    pub(super) starvation: Arc<RingStarvation>,
    pub(super) volume: Arc<AtomicU32>,
    pub(super) user_volume_ref: Arc<AtomicU32>,
    pub(super) rg_factor_ref: Arc<AtomicU32>,
    pub(super) paused: Arc<AtomicBool>,
    pub(super) playing: Arc<AtomicBool>,
    pub(super) force_silent: Arc<AtomicBool>,
    pub(super) stop_rx: std::sync::mpsc::Receiver<()>,
    pub(super) open_failure: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) signal_path_status: Arc<std::sync::Mutex<Option<OutputSignalPathStatus>>>,
    pub(super) position_ms: Arc<AtomicU64>,
    pub(super) play_generation: Arc<AtomicU64>,
    pub(super) track_ended_naturally: Arc<AtomicBool>,
    pub(super) track_ended_generation: Arc<AtomicU64>,
    pub(super) eq: Arc<std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>>,
    pub(super) convolver: Arc<std::sync::Mutex<Option<crate::audio::convolver::Convolver>>>,
    pub(super) crossfeed:
        Arc<std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>>,
    pub(super) pure_bypass: Arc<AtomicBool>,
    pub(super) mono_downmix: Arc<AtomicBool>,
    pub(super) dop_active: Arc<AtomicBool>,
}

/// Joue la piste sur WASAPI en mode exclusif événementiel, au format source,
/// par l'anneau natif i32, jusqu'à la fin du flux ou l'ordre d'arrêt.
/// Terminal : quand il rend, le fil de lecture n'a plus rien à faire.
pub(super) fn jouer_via_wasapi(entrees: EntreesWasapi) {
    let EntreesWasapi {
        device_name,
        endpoint_id,
        sample_rate,
        bit_depth,
        channels,
        data_offset,
        header_buf,
        mut reader,
        frame_bytes,
        seek_offset,
        my_generation,
        starvation,
        volume,
        user_volume_ref,
        rg_factor_ref,
        paused,
        playing,
        force_silent,
        stop_rx,
        open_failure,
        signal_path_status,
        position_ms,
        play_generation,
        track_ended_naturally,
        track_ended_generation,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
        dop_active,
    } = entrees;

    info!(
        device = %device_name,
        sample_rate,
        bit_depth,
        channels,
        "local_audio_wasapi_exclusive_mode_active"
    );

    let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
    starvation.begin_stream(sample_rate, channels);
    let ring = Arc::new(NativePcmRing::new_metered(ring_cap, starvation.clone()));
    ring.clear();

    match WasapiExclusiveOutput::new(
        &device_name,
        endpoint_id.as_deref(),
        sample_rate,
        bit_depth as u32,
        channels as u32,
        ring.clone(),
        paused.clone(),
    ) {
        Ok(mut wasapi) => {
            if let Err(e) = wasapi.start() {
                record_exclusive_open_failure("WASAPI", &device_name, &e, &open_failure);
                playing.store(false, Ordering::SeqCst);
                return;
            } else {
                info!(
                    requested_device = %device_name,
                    device = %wasapi.opened_device_name(),
                    endpoint_id = %wasapi.opened_device_id(),
                    info = %wasapi.format_info(),
                    "wasapi_exclusive_playing"
                );
                // Ces deux accesseurs existaient depuis #2207 et
                // n'avaient que cette ligne de journal pour
                // lecteur. La zone les porte désormais.
                note_opened_device(
                    "WASAPI",
                    &device_name,
                    wasapi.opened_device_name(),
                    Some(wasapi.opened_device_id()),
                );

                let pcm_data = if data_offset < header_buf.len() {
                    header_buf[data_offset..].to_vec()
                } else {
                    Vec::new()
                };

                let mut total_frames_fed: u64 = 0;
                let mut read_buf = vec![0u8; 65536];
                let mut leftover = pcm_data;
                let mut must_classify_24_bit = bit_depth == 24;
                let mut dop_latched = false;
                let mut bit_perfect_state = None;

                // A new track never inherits the DoP/volume state
                // of the previous one while its first 24-bit probe
                // is still quarantined.
                if dop_active.swap(false, Ordering::SeqCst) {
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                }

                if let Some(outcome) = feed_windows_native_exclusive_leftover(
                    &mut leftover,
                    frame_bytes,
                    bit_depth,
                    channels,
                    &mut must_classify_24_bit,
                    &mut dop_latched,
                    volume.load(Ordering::SeqCst),
                    &eq,
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    &ring,
                    &stop_rx,
                    &paused,
                    &force_silent,
                ) {
                    total_frames_fed += outcome.frames;
                    if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                        info!(dop = outcome.dop, "local_audio_dop_stream_state_changed");
                        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, outcome.dop);
                    }
                    let volume_units = volume.load(Ordering::SeqCst);
                    let runtime = publish_windows_signal_path_status(
                        &signal_path_status,
                        outcome.bit_perfect,
                        true,
                        outcome.dop,
                        volume_units,
                        &eq,
                        &convolver,
                        &crossfeed,
                        &pure_bypass,
                        &mono_downmix,
                    );
                    if bit_perfect_state != Some(runtime.bit_perfect) {
                        bit_perfect_state = Some(runtime.bit_perfect);
                        info!(
                            backend = "WASAPI",
                            bit_perfect = runtime.bit_perfect,
                            dop = outcome.dop,
                            volume_units,
                            reasons = ?runtime.reasons,
                            "windows_exclusive_signal_contract"
                        );
                    }
                }
                if !must_classify_24_bit && dop_active.swap(false, Ordering::SeqCst) {
                    info!(dop = false, "local_audio_dop_stream_state_changed");
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                }

                let mut http_eof_wasapi = false;
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_wasapi_exclusive_aborted_by_stop");
                        break;
                    }

                    match reader.read(&mut read_buf) {
                        Ok(0) => {
                            http_eof_wasapi = true;
                            break;
                        }
                        Ok(n) => {
                            leftover.extend_from_slice(&read_buf[..n]);
                            if let Some(outcome) = feed_windows_native_exclusive_leftover(
                                &mut leftover,
                                frame_bytes,
                                bit_depth,
                                channels,
                                &mut must_classify_24_bit,
                                &mut dop_latched,
                                volume.load(Ordering::SeqCst),
                                &eq,
                                &convolver,
                                &crossfeed,
                                &pure_bypass,
                                &mono_downmix,
                                &ring,
                                &stop_rx,
                                &paused,
                                &force_silent,
                            ) {
                                total_frames_fed += outcome.frames;
                                if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                                    info!(
                                        dop = outcome.dop,
                                        "local_audio_dop_stream_state_changed"
                                    );
                                    sync_volume_to_dop(
                                        &volume,
                                        &user_volume_ref,
                                        &rg_factor_ref,
                                        outcome.dop,
                                    );
                                }
                                let volume_units = volume.load(Ordering::SeqCst);
                                let runtime = publish_windows_signal_path_status(
                                    &signal_path_status,
                                    outcome.bit_perfect,
                                    true,
                                    outcome.dop,
                                    volume_units,
                                    &eq,
                                    &convolver,
                                    &crossfeed,
                                    &pure_bypass,
                                    &mono_downmix,
                                );
                                if bit_perfect_state != Some(runtime.bit_perfect) {
                                    bit_perfect_state = Some(runtime.bit_perfect);
                                    info!(
                                        backend = "WASAPI",
                                        bit_perfect = runtime.bit_perfect,
                                        dop = outcome.dop,
                                        volume_units,
                                        reasons = ?runtime.reasons,
                                        "windows_exclusive_signal_contract"
                                    );
                                }
                            }

                            let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0)
                                as u64
                                + seek_offset;
                            position_ms.store(pos, Ordering::Relaxed);
                        }
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_wasapi_exclusive_read_error");
                            http_eof_wasapi = true;
                            break;
                        }
                    }
                }

                // Less than 32 initial 24-bit frames cannot be
                // classified, but the integer ring can still carry
                // them safely. Keep them raw and at unity rather
                // than guessing PCM and applying sample arithmetic.
                if http_eof_wasapi && must_classify_24_bit && !leftover.is_empty() {
                    let aligned = (leftover.len() / frame_bytes) * frame_bytes;
                    let native = pcm_bytes_to_native_i32(&leftover[..aligned], bit_depth);
                    feed_native_ring_abortable(
                        &ring,
                        &native,
                        &stop_rx,
                        &paused,
                        Some(&force_silent),
                    );
                    leftover.drain(..aligned);
                    total_frames_fed += (aligned / frame_bytes) as u64;
                    info!(
                        backend = "WASAPI",
                        bytes = aligned,
                        "windows_exclusive_short_24bit_stream_forced_raw"
                    );
                }

                // WASAPI exclusive now follows the same DSP tail
                // contract as the other local PCM paths (#2209).
                let queue = flush_local_dsp(
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    channels,
                    false,
                );
                if !queue.is_empty() {
                    let volume_factor = volume.load(Ordering::SeqCst) as f32 / 1000.0;
                    let mut queue = queue;
                    if volume_factor != 1.0 {
                        for sample in &mut queue {
                            *sample *= volume_factor;
                        }
                    }
                    let native = f32_to_native_i32(&queue, bit_depth);
                    feed_native_ring_abortable(
                        &ring,
                        &native,
                        &stop_rx,
                        &paused,
                        Some(&force_silent),
                    );
                }

                // Signal natural track end BEFORE draining when
                // the HTTP stream reached EOF, so the orchestrator
                // can detect end-of-track even if force_silent is
                // set during slow drain (e.g. 44.1→192 kHz resample).
                if http_eof_wasapi {
                    track_ended_naturally.store(true, Ordering::SeqCst);
                    track_ended_generation.store(my_generation, Ordering::SeqCst);
                    TRACK_END_NOTIFY.notify_one();
                }

                // Wait for ring buffer to drain
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    if ring.available() == 0 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                wasapi.stop();
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(
                    device = %device_name,
                    frames = total_frames_fed,
                    "local_audio_wasapi_exclusive_stopped"
                );
                return;
            }
        }
        Err(e) => {
            record_exclusive_open_failure("WASAPI", &device_name, &e, &open_failure);
            playing.store(false, Ordering::SeqCst);
            return;
        }
    }
}
