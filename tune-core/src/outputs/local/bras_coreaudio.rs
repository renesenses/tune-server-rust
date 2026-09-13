//! Le bras CoreAudio exclusif de `play_url` (R6 bis, #2219).
//!
//! Ce module est le bloc `#[cfg(target_os = "macos")] if exclusive_mode { … }`
//! de `play_url`, sorti à l'identique : le texte est celui du bloc, ce qu'il
//! lisait du contexte de `play_url` est devenu [`EntreesCoreAudio`], construit
//! par `play_url` juste avant l'appel. Le bloc sortait TOUJOURS par `return` :
//! le bras est terminal, il ne rend rien à la suite de `play_url`, qui fait
//! `return` après l'appel. Aucune condition ne change — le `if` et sa bannière
//! restent dans `play_url`.
//!
//! Compilé par la seule porte macOS (`macos-pr` de `ci.yml`) : Shrek ne voit
//! pas ce fichier. Les gardes de texte qui comptaient ce bras dans `local.rs`
//! (`dsp_track_boundary`, `refus_exclusif_dit_sa_cause_i3108`,
//! `backend_fallback_tests`) le relisent ici.

// ------- Exclusive mode path (macOS only) -------

use std::io::Read;

use super::*;
use crate::outputs::coreaudio_exclusive::ExclusiveOutput;

/// Ce que le bras lisait du contexte de `play_url` — trente-deux valeurs,
/// toutes déjà possédées par le fil de lecture (des `Arc` clonés dans le
/// préambule de `play_url`, ou des valeurs). Elles sont DÉPLACÉES dans la
/// structure, jamais empruntées : le bras est terminal, `play_url` n'en a
/// plus besoin après l'appel, et rien ne peut rendre E0521 (leçon de #3386).
pub(super) struct EntreesCoreAudio {
    pub(super) device_name: String,
    pub(super) url: String,
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
    pub(super) spec: AudioSpec,
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

/// Joue la piste sur CoreAudio en mode exclusif (hog), au format source,
/// jusqu'à la fin du flux ou l'ordre d'arrêt. Terminal : quand il rend, le
/// fil de lecture n'a plus rien à faire.
pub(super) fn jouer_via_coreaudio(entrees: EntreesCoreAudio) {
    let EntreesCoreAudio {
        device_name,
        url,
        sample_rate,
        bit_depth,
        channels,
        data_offset,
        header_buf,
        mut reader,
        frame_bytes,
        spec,
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
        "local_audio_exclusive_mode_active"
    );

    // Ring buffer: ~2 seconds of audio at source sample rate
    let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
    starvation.begin_stream(sample_rate, channels);
    let ring = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
    ring.clear(); // Defensive: zero-fill before callback reads

    let exclusive = match ExclusiveOutput::new(
        &device_name,
        sample_rate,
        bit_depth as u32,
        channels as u32,
        ring.clone(),
        volume.clone(),
        paused.clone(),
    ) {
        Ok(ex) => ex,
        Err(e) => {
            record_exclusive_open_failure("CoreAudio", &device_name, &e.to_string(), &open_failure);
            playing.store(false, Ordering::SeqCst);
            return;
        }
    };

    info!(device = %device_name, url = %url, "local_audio_exclusive_playing");
    // CoreAudio exclusif : `resolve_output_device` retombe sur le
    // périphérique système quand le nom stocké n'existe plus (DAC
    // débranché, renommé, routage macOS changé). `opened_id` reste
    // `None` : l'`AudioDeviceID` est un entier réattribué au
    // redémarrage, ce n'est pas une identité qu'on peut afficher.
    note_opened_device(
        "CoreAudio",
        &device_name,
        &exclusive.format_info().device_name,
        None,
    );

    // Feed audio data (no resampling needed -- hardware is set to source rate)
    let pcm_data = if data_offset < header_buf.len() {
        header_buf[data_offset..].to_vec()
    } else {
        Vec::new()
    };

    let mut total_frames_fed: u64 = 0;

    // Read and feed the rest of the stream
    let mut read_buf = vec![0u8; 65536];
    let mut leftover = pcm_data;
    let mut pcm_kind = LocalPcmKind::for_bit_depth(bit_depth);
    let pcm_processor = LocalPcmProcessor {
        eq: &eq,
        convolver: &convolver,
        crossfeed: &crossfeed,
        pure_bypass: &pure_bypass,
        mono_downmix: &mono_downmix,
        dop_active: &dop_active,
        volume: &volume,
        user_volume: &user_volume_ref,
        rg_factor: &rg_factor_ref,
    };

    // Process leftover from header read
    // #3108 — le verdict de blocage était JETÉ aux trois sites de
    // ce chemin, seul de tous les chemins de lecture. Conséquence
    // exacte du constat : l'anneau exclusif tient deux secondes
    // d'audio (`ring_cap` ci-dessus), il se remplit une fois, le
    // rappel de rendu ne tire rien, et la position reste sur 2 000
    // ms pour toujours — sans un mot.
    let mut feed_stalled = false;
    if let Some(processed) = pcm_processor.process_pcm_chunk(&mut leftover, spec, &mut pcm_kind) {
        if !feed_ring_abortable(
            &ring,
            &processed.samples,
            &stop_rx,
            &paused,
            Some(&force_silent),
        ) {
            feed_stalled = true;
        }
        total_frames_fed += processed.source_frames;
    }

    let mut http_eof_excl = false;
    while !feed_stalled {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if force_silent.load(Ordering::Relaxed) {
            debug!("local_audio_exclusive_aborted_by_stop");
            break;
        }

        let n = match reader.read(&mut read_buf) {
            Ok(0) => {
                http_eof_excl = true;
                break;
            }
            Ok(n) => n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // Read timeout — check abort flag and retry
                continue;
            }
            Err(e) => {
                warn!(error = %e, "local_audio_exclusive_read_error");
                http_eof_excl = true;
                break;
            }
        };

        leftover.extend_from_slice(&read_buf[..n]);

        let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
        if aligned_len == 0 {
            continue;
        }

        let Some(processed) = pcm_processor.process_pcm_chunk(&mut leftover, spec, &mut pcm_kind)
        else {
            continue;
        };

        if !feed_ring_abortable(
            &ring,
            &processed.samples,
            &stop_rx,
            &paused,
            Some(&force_silent),
        ) {
            feed_stalled = true;
            break;
        }

        total_frames_fed += processed.source_frames;

        let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64 + seek_offset;
        position_ms.store(pos, Ordering::Relaxed);
    }

    if feed_stalled {
        // La piste n'a PAS fini : `http_eof_excl` reste faux, donc
        // aucune fin naturelle n'est signalée et la file n'avance
        // pas vers un morceau qui heurterait le même périphérique
        // mort. Le seul mot dit à l'utilisateur part d'ici.
        record_feed_stall_failure(
            "CoreAudio",
            &device_name,
            position_ms.load(Ordering::Relaxed),
            &open_failure,
        );
    }

    if http_eof_excl {
        report_incomplete_local_pcm_probe(pcm_kind, leftover.len());
    }

    // Fin de piste : rendre au périphérique ce que le convolveur
    // retient encore. Sans ça, `latency_frames()` trames restaient
    // dans le moteur et la fin de chaque piste était tronquée
    // (#2209, revue JP Robbe — la fonction etait morte).
    let queue = flush_local_dsp(
        &convolver,
        &crossfeed,
        &pure_bypass,
        &mono_downmix,
        channels,
        dop_active.load(Ordering::Relaxed),
    );
    if !queue.is_empty() && !feed_stalled {
        feed_ring_abortable(&ring, &queue, &stop_rx, &paused, Some(&force_silent));
        total_frames_fed += (queue.len() / channels.max(1) as usize) as u64;
    }

    // Signal natural track end BEFORE draining when the HTTP
    // stream reached EOF, so the orchestrator can detect
    // end-of-track even if force_silent is set during slow drain.
    if http_eof_excl {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }

    // Wait for ring buffer to drain — JAMAIS sans fin (#3108).
    // Les chemins ASIO, WASAPI et partagé bornaient déjà leur
    // vidage ; celui-ci, seul, tournait tant que l'anneau n'était
    // pas vide. Face à un rappel de rendu mort il ne se vide
    // jamais : le fil restait vivant, la zone « en lecture », et le
    // réexamen des branchements gelé avec elle.
    let drain_deadline = drain_deadline_for(ring.available(), sample_rate as u64, channels as u64);
    let drain_started = std::time::Instant::now();
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
        if drain_started.elapsed() >= drain_deadline {
            warn!(
                device = %device_name,
                remaining_samples = ring.available(),
                "local_audio_exclusive_drain_timeout"
            );
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // ExclusiveOutput::drop() restores sample rate and releases hog mode
    drop(exclusive);
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        frames = total_frames_fed,
        "local_audio_exclusive_stopped"
    );
    return;
}
