//! Le bras ASIO exclusif de `play_url` (R6 bis, #2219).
//!
//! Ce module est le bloc `#[cfg(all(target_os = "windows", feature = "asio"))]
//! if exclusive_mode && audio_backend == "asio" { … }` de `play_url`, sorti à
//! l'identique : le texte est celui du bloc, ce qu'il lisait du contexte de
//! `play_url` est devenu [`EntreesAsio`], construit par `play_url` juste avant
//! l'appel. Le bloc sortait TOUJOURS par `return` : le bras est terminal, il ne
//! rend rien à la suite de `play_url`, qui fait `return` après l'appel. Aucune
//! condition ne change — le `if` et sa bannière restent dans `play_url`.
//!
//! Compilé par la seule étape « ASIO » du job `windows-pr` de `ci.yml` : ni
//! Shrek ni le Mac ne voient ce fichier. `super` désigne ici `outputs::local`,
//! pas `outputs` : le module ASIO se nomme par `crate::outputs::asio_exclusive`
//! (bloquant de R6, #3981).

// ------- Exclusive mode path (Windows ASIO) -------

use std::io::Read;

use super::*;
use crate::outputs::asio_exclusive::AsioExclusiveOutput;

/// Ce que le bras lisait du contexte de `play_url` — trente-trois valeurs,
/// toutes déjà possédées par le fil de lecture. Elles sont DÉPLACÉES, jamais
/// empruntées : la pompe HTTP du bras (`std::thread::spawn(move …)`) prend
/// `reader` par valeur, ce qu'un emprunt ne permettrait pas (E0521, leçon de
/// #3386).
pub(super) struct EntreesAsio {
    pub(super) device_name: String,
    pub(super) url: String,
    pub(super) sample_rate: u32,
    pub(super) bit_depth: u16,
    pub(super) channels: u16,
    pub(super) data_offset: usize,
    /// Les 4 096 premiers octets lus par `play_url` ; ce qui suit
    /// `data_offset` est le début du PCM.
    pub(super) header_buf: Vec<u8>,
    /// La réponse HTTP, positionnée après `header_buf`. Le bras la confie à
    /// son fil pompe.
    pub(super) reader: reqwest::blocking::Response,
    pub(super) frame_bytes: usize,
    pub(super) bytes_per_sample: usize,
    pub(super) seek_offset: u64,
    pub(super) pre_seeked: bool,
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

/// Joue la piste sur un pilote ASIO en mode exclusif, au format source, par
/// l'anneau natif ou flottant selon le transport du pilote, jusqu'à la fin du
/// flux ou l'ordre d'arrêt. Terminal : quand il rend, le fil de lecture n'a
/// plus rien à faire.
pub(super) fn jouer_via_asio(entrees: EntreesAsio) {
    let EntreesAsio {
        device_name,
        url,
        sample_rate,
        bit_depth,
        channels,
        data_offset,
        header_buf,
        reader,
        frame_bytes,
        bytes_per_sample,
        seek_offset,
        pre_seeked,
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
        "local_audio_asio_exclusive_mode_active"
    );

    // Ring buffer: ~2 seconds of audio at source sample rate
    let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
    starvation.begin_stream(sample_rate, channels);
    let float_ring = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
    let native_ring = Arc::new(NativePcmRing::new_metered(ring_cap, starvation.clone()));
    float_ring.clear();
    native_ring.clear();

    let exclusive = match AsioExclusiveOutput::new(
        &device_name,
        sample_rate,
        bit_depth as u32,
        channels as u32,
        float_ring.clone(),
        native_ring.clone(),
        volume.clone(),
        paused.clone(),
    ) {
        Ok(ex) => ex,
        Err(e) => {
            record_exclusive_open_failure("ASIO", &device_name, &e.to_string(), &open_failure);
            playing.store(false, Ordering::SeqCst);
            return;
        }
    };

    let selected_ring = if exclusive.uses_native_transport() {
        WindowsExclusiveRingRef::Native(&native_ring)
    } else {
        WindowsExclusiveRingRef::Float(&float_ring)
    };
    if let Some(reason) = exclusive.bit_perfect_unavailable_reason() {
        info!(
            backend = "ASIO",
            device = %device_name,
            bit_perfect = false,
            reason,
            "windows_exclusive_signal_contract"
        );
    }

    info!(device = %device_name, url = %url, "local_audio_asio_exclusive_playing");
    // ASIO exclusif : résolution par sous-chaîne, et `"default"`
    // prend le premier pilote listé. `opened_id` reste `None` :
    // ASIO n'expose aucun identifiant d'endpoint.
    note_opened_device("ASIO", &device_name, exclusive.opened_device_name(), None);

    // Feed audio data (no resampling needed -- hardware is set to source rate)
    let pcm_data = if data_offset < header_buf.len() {
        header_buf[data_offset..].to_vec()
    } else {
        Vec::new()
    };

    let mut total_frames_fed: u64 = 0;

    // Only skip bytes if the stream was NOT pre-seeked by the
    // decoder. When pre_seeked=true, the decoder already produced
    // audio starting at the seek position — skipping would discard
    // the entire stream (double-seek bug reported by DEvir).
    let skip_bytes_asio: u64 = if seek_offset > 0 && !pre_seeked {
        let skip_frames = (seek_offset as f64 / 1000.0 * sample_rate as f64) as u64;
        skip_frames * channels as u64 * bytes_per_sample as u64
    } else {
        0
    };
    let mut skipped_bytes_asio: u64 = 0;

    // Read and feed the rest of the stream. The raw-byte staging
    // buffer is also the DoP quarantine: no initial 24-bit sample
    // may reach the f32 ring until 32 frames have ruled DoP out.
    let mut leftover: Vec<u8> = Vec::new();
    let mut must_classify_24_bit = bit_depth == 24;
    let mut dop_latched = false;
    let mut bit_perfect_state = None;

    // Track-local contract: never inherit the prior stream's DoP
    // state while the first 24-bit probe is still quarantined.
    if dop_active.swap(false, Ordering::SeqCst) {
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    if !pcm_data.is_empty() {
        let discard = if skip_bytes_asio > skipped_bytes_asio {
            ((skip_bytes_asio - skipped_bytes_asio) as usize).min(pcm_data.len())
        } else {
            0
        };
        skipped_bytes_asio += discard as u64;
        leftover.extend_from_slice(&pcm_data[discard..]);
    }

    match feed_selected_windows_exclusive_leftover(
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
        selected_ring,
        &stop_rx,
        &paused,
        &force_silent,
    ) {
        Ok(Some(outcome)) => {
            total_frames_fed += outcome.frames;
            if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                info!(dop = outcome.dop, "local_audio_dop_stream_state_changed");
                sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, outcome.dop);
            }
            let volume_units = volume.load(Ordering::SeqCst);
            let runtime = publish_windows_signal_path_status(
                &signal_path_status,
                outcome.bit_perfect,
                matches!(selected_ring, WindowsExclusiveRingRef::Native(_)),
                outcome.dop,
                volume_units,
                &eq,
                &convolver,
                &crossfeed,
                &pure_bypass,
                &mono_downmix,
            );
            bit_perfect_state = Some(runtime.bit_perfect);
            info!(
                backend = "ASIO",
                bit_perfect = runtime.bit_perfect,
                dop = outcome.dop,
                volume_units,
                reasons = ?runtime.reasons,
                "windows_exclusive_signal_contract"
            );
        }
        Ok(None) => {}
        Err(error) => {
            record_windows_exclusive_pcm_refusal(error, "ASIO", &device_name, &open_failure);
            force_silent.store(true, Ordering::SeqCst);
            dop_active.store(false, Ordering::SeqCst);
            sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
            drop(exclusive);
            if play_generation.load(Ordering::SeqCst) == my_generation {
                playing.store(false, Ordering::SeqCst);
            }
            return;
        }
    }
    if !must_classify_24_bit && dop_active.swap(false, Ordering::SeqCst) {
        info!(dop = false, "local_audio_dop_stream_state_changed");
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    let mut http_eof_asio = false;
    let mut last_data_at = std::time::Instant::now();
    // Pump thread: it owns the blocking HTTP read so the thread
    // that HOLDS THE ASIO DEVICE never blocks on the network.
    // Before this, stop() set force_silent but the device thread
    // sat in reader.read() until the HTTP session died as a side
    // effect of the NEXT play — it then released the ASIO lock
    // ~2.5s INTO the new play. Two repeats survived by timing;
    // the 3rd hit the wrong interleaving: silent output and the
    // poller oscillating at EOF (DEvir, Fireface ASIO, repeat-all,
    // v0.9.0). With the pump, the device thread polls a channel
    // (500ms) and honours stop within one tick; the pump thread
    // may linger in a blocked read but only owns the socket, and
    // exits when the receiver drops or the session closes.
    // Approximate depth of the pump→device channel. Incremented by
    // the pump before each send, decremented by the device loop on
    // each successful recv. A high steady depth means the device
    // thread is NOT draining (ring full / callback dead); a depth of
    // ~0 means the device is starved (EOF never latches). Surfaced in
    // the periodic `asio_exclusive_feed_stats` log (DEvir bug-22).
    let pump_depth = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (pump_tx, pump_rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(64);
    {
        let pump_depth = pump_depth.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = vec![0u8; 65536];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        pump_depth.fetch_add(1, Ordering::Relaxed);
                        if pump_tx.send(Ok(Vec::new())).is_err() {
                            pump_depth.fetch_sub(1, Ordering::Relaxed);
                        }
                        break;
                    }
                    Ok(n) => {
                        pump_depth.fetch_add(1, Ordering::Relaxed);
                        if pump_tx.send(Ok(buf[..n].to_vec())).is_err() {
                            pump_depth.fetch_sub(1, Ordering::Relaxed);
                            break; // receiver gone — playback stopped
                        }
                    }
                    Err(e) => {
                        let transient = matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        );
                        pump_depth.fetch_add(1, Ordering::Relaxed);
                        if pump_tx.send(Err(e)).is_err() {
                            pump_depth.fetch_sub(1, Ordering::Relaxed);
                            break;
                        }
                        if !transient {
                            break;
                        }
                    }
                }
            }
        });
    }
    let mut last_stats_at = std::time::Instant::now();
    let mut pcm_refusal = None;
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if force_silent.load(Ordering::Relaxed) {
            debug!("local_audio_asio_exclusive_aborted_by_stop");
            break;
        }

        // Periodic health snapshot (~500ms) so a wedge is diagnosable
        // from DEvir's next log: ring full + high pump_depth => the
        // callback stopped draining; ring/pump ~empty => starved / EOF
        // never latched (bug-22 / #789).
        if last_stats_at.elapsed() >= std::time::Duration::from_millis(500) {
            debug!(
                ring_available = selected_ring.available(),
                ring_capacity = selected_ring.capacity(),
                total_frames_fed,
                pump_depth = pump_depth.load(Ordering::Relaxed),
                leftover_bytes = leftover.len(),
                "asio_exclusive_feed_stats"
            );
            last_stats_at = std::time::Instant::now();
        }

        let recv = pump_rx.recv_timeout(std::time::Duration::from_millis(500));
        if recv.is_ok() {
            pump_depth.fetch_sub(1, Ordering::Relaxed);
        }
        let chunk = match recv {
            Ok(Ok(data)) if data.is_empty() => {
                http_eof_asio = true;
                break;
            }
            Ok(Ok(data)) => {
                last_data_at = std::time::Instant::now();
                data
            }
            Ok(Err(ref e))
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // A streaming HTTP source (transcoded WAV over a
                // keep-alive connection) may never return a clean
                // EOF: after the last byte it just keeps timing out.
                // Once the whole track has been fed AND the ring has
                // fully drained (everything played), a sustained read
                // idle means the track ended — signal EOF so the
                // orchestrator can advance/repeat. Without this, the
                // loop spins forever and end-of-track is never
                // detected on exclusive ASIO outputs (DEvir: repeat
                // never fired on a clean playthrough).
                if total_frames_fed > 0
                    && leftover.is_empty()
                    && selected_ring.available() == 0
                    && last_data_at.elapsed() > std::time::Duration::from_secs(5)
                {
                    info!("local_audio_asio_exclusive_stream_idle_eof");
                    http_eof_asio = true;
                    break;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Same sustained-idle EOF heuristic as the
                // transient-read-error arm above.
                if total_frames_fed > 0
                    && leftover.is_empty()
                    && selected_ring.available() == 0
                    && last_data_at.elapsed() > std::time::Duration::from_secs(5)
                {
                    info!("local_audio_asio_exclusive_stream_idle_eof");
                    http_eof_asio = true;
                    break;
                }
                continue;
            }
            Ok(Err(e)) => {
                warn!(error = %e, "local_audio_asio_exclusive_read_error");
                http_eof_asio = true;
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                http_eof_asio = true;
                break;
            }
        };
        let n = chunk.len();

        if skip_bytes_asio > 0 && skipped_bytes_asio < skip_bytes_asio {
            let remaining = (skip_bytes_asio - skipped_bytes_asio) as usize;
            if n <= remaining {
                skipped_bytes_asio += n as u64;
                continue;
            }
            skipped_bytes_asio = skip_bytes_asio;
            leftover.extend_from_slice(&chunk[remaining..]);
        } else {
            leftover.extend_from_slice(&chunk);
        }

        match feed_selected_windows_exclusive_leftover(
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
            selected_ring,
            &stop_rx,
            &paused,
            &force_silent,
        ) {
            Ok(Some(outcome)) => {
                total_frames_fed += outcome.frames;
                if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                    info!(dop = outcome.dop, "local_audio_dop_stream_state_changed");
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, outcome.dop);
                }
                let volume_units = volume.load(Ordering::SeqCst);
                let runtime = publish_windows_signal_path_status(
                    &signal_path_status,
                    outcome.bit_perfect,
                    matches!(selected_ring, WindowsExclusiveRingRef::Native(_)),
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
                        backend = "ASIO",
                        bit_perfect = runtime.bit_perfect,
                        dop = outcome.dop,
                        volume_units,
                        reasons = ?runtime.reasons,
                        "windows_exclusive_signal_contract"
                    );
                }
            }
            Ok(None) => {}
            Err(error) => {
                pcm_refusal = Some(error);
                break;
            }
        }

        let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64 + seek_offset;
        position_ms.store(pos, Ordering::Relaxed);
    }

    if pcm_refusal.is_none() && http_eof_asio {
        match selected_ring {
            WindowsExclusiveRingRef::Float(_) => {
                if let Err(error) =
                    finish_windows_exclusive_probe(bit_depth, must_classify_24_bit, leftover.len())
                {
                    pcm_refusal = Some(error);
                }
            }
            WindowsExclusiveRingRef::Native(ring)
                if must_classify_24_bit && !leftover.is_empty() =>
            {
                let aligned = (leftover.len() / frame_bytes) * frame_bytes;
                let native = pcm_bytes_to_native_i32(&leftover[..aligned], bit_depth);
                feed_native_ring_abortable(ring, &native, &stop_rx, &paused, Some(&force_silent));
                leftover.drain(..aligned);
                total_frames_fed += (aligned / frame_bytes) as u64;
                info!(
                    backend = "ASIO",
                    bytes = aligned,
                    bit_perfect = true,
                    "windows_exclusive_short_24bit_stream_forced_raw"
                );
            }
            WindowsExclusiveRingRef::Native(_) => {}
        }
    }
    if let Some(error) = pcm_refusal {
        record_windows_exclusive_pcm_refusal(error, "ASIO", &device_name, &open_failure);
        force_silent.store(true, Ordering::SeqCst);
        dop_active.store(false, Ordering::SeqCst);
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
        drop(exclusive);
        if play_generation.load(Ordering::SeqCst) == my_generation {
            playing.store(false, Ordering::SeqCst);
        }
        return;
    }

    // Fin de piste : rendre ce que le convolveur retient (#2209).
    let queue = flush_local_dsp(
        &convolver,
        &crossfeed,
        &pure_bypass,
        &mono_downmix,
        channels,
        dop_active.load(Ordering::Relaxed),
    );
    if !queue.is_empty() {
        feed_selected_windows_exclusive_tail(
            selected_ring,
            queue,
            bit_depth,
            volume.load(Ordering::SeqCst),
            &stop_rx,
            &paused,
            &force_silent,
        );
    }

    // Signal natural track end BEFORE draining when the HTTP
    // stream reached EOF, so the orchestrator can detect
    // end-of-track even if force_silent is set during slow drain.
    if http_eof_asio {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }

    // Wait for the ring to drain — but NEVER block forever. If the
    // ASIO render callback stops consuming (RME driver wedged after a
    // stop→start reopen at a Repeat loop point — DEvir bug-22, the
    // #789 regression), `ring.available()` never reaches 0 and this
    // loop used to spin indefinitely, stranding this thread AND the
    // process-wide ASIO_DEVICE_LOCK (held until `exclusive` is dropped
    // just below). Bound it two ways: a hard wall-clock deadline of
    // ~2× the ring's time-capacity, and a stall detector that bails if
    // `available()` has not decreased for ~1.5s.
    let ring_capacity_ms = if sample_rate > 0 && channels > 0 {
        (selected_ring.capacity() as u64 * 1000) / (sample_rate as u64 * channels as u64)
    } else {
        0
    };
    let drain_deadline = std::time::Duration::from_millis((ring_capacity_ms * 2).max(1000));
    let drain_started = std::time::Instant::now();
    let mut last_avail = selected_ring.available();
    let mut last_progress_at = std::time::Instant::now();
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if force_silent.load(Ordering::Relaxed) {
            break;
        }
        let avail = selected_ring.available();
        if avail == 0 {
            break;
        }
        if avail < last_avail {
            last_avail = avail;
            last_progress_at = std::time::Instant::now();
        } else if last_progress_at.elapsed() >= std::time::Duration::from_millis(1500) {
            warn!(
                device = %device_name,
                ring_available = avail,
                total_frames_fed,
                "asio_drain_timeout"
            );
            break;
        }
        if drain_started.elapsed() >= drain_deadline {
            warn!(
                device = %device_name,
                ring_available = avail,
                elapsed_ms = drain_started.elapsed().as_millis() as u64,
                "asio_drain_timeout"
            );
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // AsioExclusiveOutput::drop() releases the ASIO device and, with
    // it, the process-wide ASIO_DEVICE_LOCK. Both loops above are now
    // bounded and any panic unwinds through this owned local, so this
    // drop runs on EVERY exit path — the lock can never be stranded.
    drop(exclusive);
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        frames = total_frames_fed,
        "local_audio_asio_exclusive_stopped"
    );
    return;
}
