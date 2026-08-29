#[derive(Debug, Clone, Default)]
pub struct AudioLevels {
    pub rms_left: f64,
    pub rms_right: f64,
    pub peak_left: f64,
    pub peak_right: f64,
    pub spectrum: Vec<f32>,
    /// Niveau ABSOLU de chaque bande, en dBFS.
    ///
    /// `spectrum` ci-dessus est une *forme* : chaque trame y est divisée par sa
    /// propre bande la plus forte, donc la dominante vaut toujours 1,0 et un
    /// pianissimo s'affiche comme un tutti. Ce champ-ci dit le vrai niveau, sur
    /// la même échelle que `rms_*_db` / `peak_*_db`, ce qui permet à un client
    /// de tracer un analyseur gradué au lieu d'une silhouette.
    ///
    /// Les deux coexistent volontairement : `spectrum` reste le contrat des
    /// clients déjà déployés (application iOS comprise).
    pub spectrum_db: Vec<f32>,
    /// Durée audio couverte par ces niveaux — permet au forwarder de
    /// l'orchestrateur de cadencer l'émission sur l'horloge de lecture
    /// (le décodage va bien plus vite que le temps réel).
    pub window: std::time::Duration,
}

impl AudioLevels {
    pub fn rms_left_db(&self) -> f32 {
        to_db(self.rms_left)
    }
    pub fn rms_right_db(&self) -> f32 {
        to_db(self.rms_right)
    }
    pub fn peak_left_db(&self) -> f32 {
        to_db(self.peak_left)
    }
    pub fn peak_right_db(&self) -> f32 {
        to_db(self.peak_right)
    }
}

/// Plancher des niveaux par bande. Aligné sur celui de `to_db` : au-dessous,
/// c'est du silence, et un client n'a rien à y afficher.
pub const SPECTRUM_FLOOR_DB: f32 = -96.0;

/// Fenêtre de tenue du peak-hold serveur (#1694) : max glissant ~300 ms.
///
/// Le forwarder émet une crête PAR fenêtre (~40 ms) ; si un client rate une
/// trame, le transitoire disparaît. La crête tenue survit à quelques trames
/// perdues sans figer l'instrument. La balistique d'affichage (hold ~750 ms,
/// retombée) reste côté client — ici on ne fait que porter le transitoire.
pub const PEAK_HOLD_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

/// Crête TENUE sur les dernières [`PEAK_HOLD_WINDOW`] de signal — l'état vit
/// dans le forwarder d'une lecture, un par piste, donc la tenue repart
/// naturellement de zéro au changement de titre.
///
/// Chemin temps réel : aucune allocation par trame en régime établi — le
/// tampon est borné par `MAX_ENTRIES` et `VecDeque` ne réalloue qu'à la
/// croissance initiale (~8 entrées à 25 trames/s).
#[derive(Debug, Default)]
pub struct PeakHold {
    /// (fin de fenêtre sur l'horloge cumulée, crête G, crête D) — linéaire.
    entries: std::collections::VecDeque<(std::time::Duration, f64, f64)>,
    /// Horloge cumulée des fenêtres reçues (pas l'horloge murale : le
    /// forwarder cadence déjà l'émission sur l'horloge de lecture).
    elapsed: std::time::Duration,
}

impl PeakHold {
    /// Borne dure du tampon : des fenêtres dégénérées (durée nulle) ne
    /// doivent pas le faire croître sans limite.
    const MAX_ENTRIES: usize = 64;

    /// Intègre la crête d'une fenêtre et rend la crête tenue `(gauche,
    /// droite)` en dBFS, sur la même échelle que `peak_*_db`.
    pub fn update(
        &mut self,
        window: std::time::Duration,
        peak_left: f64,
        peak_right: f64,
    ) -> (f32, f32) {
        self.elapsed += window;
        if self.entries.len() >= Self::MAX_ENTRIES {
            self.entries.pop_front();
        }
        self.entries
            .push_back((self.elapsed, peak_left, peak_right));
        // Ne garder que les fenêtres dont la fin tombe dans la tenue.
        let cutoff = self.elapsed.saturating_sub(PEAK_HOLD_WINDOW);
        while let Some(&(end, _, _)) = self.entries.front() {
            if end <= cutoff {
                self.entries.pop_front();
            } else {
                break;
            }
        }
        let (mut l, mut r) = (0.0f64, 0.0f64);
        for &(_, pl, pr) in &self.entries {
            l = l.max(pl);
            r = r.max(pr);
        }
        (to_db(l), to_db(r))
    }
}

fn to_db(linear: f64) -> f32 {
    if linear <= 0.0 {
        -96.0
    } else {
        (20.0 * linear.log10()).max(-96.0) as f32
    }
}

pub fn compute_levels(pcm: &[u8], bit_depth: u16, channels: u16, sample_rate: u32) -> AudioLevels {
    if pcm.is_empty() || channels == 0 {
        return AudioLevels::default();
    }

    let bytes_per_sample = (bit_depth / 8) as usize;
    let frame_size = bytes_per_sample * channels as usize;
    if frame_size == 0 {
        return AudioLevels::default();
    }

    let mut sum_sq_l: f64 = 0.0;
    let mut sum_sq_r: f64 = 0.0;
    let mut peak_l: f64 = 0.0;
    let mut peak_r: f64 = 0.0;
    let mut frames: usize = 0;

    let stereo = channels >= 2;

    for frame in pcm.chunks_exact(frame_size) {
        let left = read_sample(frame, 0, bytes_per_sample, bit_depth);
        let right = if stereo {
            read_sample(frame, bytes_per_sample, bytes_per_sample, bit_depth)
        } else {
            left
        };

        sum_sq_l += left * left;
        sum_sq_r += right * right;
        peak_l = peak_l.max(left.abs());
        peak_r = peak_r.max(right.abs());
        frames += 1;
    }

    if frames == 0 {
        return AudioLevels::default();
    }

    // Une seule FFT pour les deux formes : les recalculer séparément
    // doublerait le coût d'analyse, déjà en cause dans #1110.
    let spectrum = analyze_spectrum(pcm, bit_depth, channels, 32, sample_rate);

    AudioLevels {
        rms_left: (sum_sq_l / frames as f64).sqrt(),
        rms_right: (sum_sq_r / frames as f64).sqrt(),
        peak_left: peak_l,
        peak_right: peak_r,
        spectrum_db: spectrum.db,
        spectrum: spectrum.shape,
        window: if sample_rate > 0 {
            std::time::Duration::from_secs_f64(frames as f64 / sample_rate as f64)
        } else {
            std::time::Duration::ZERO
        },
    }
}

fn read_sample(frame: &[u8], offset: usize, bytes: usize, bit_depth: u16) -> f64 {
    let max_val = (1i64 << (bit_depth - 1)) as f64;
    let raw = match bytes {
        2 => {
            let b = [frame[offset], frame[offset + 1]];
            i16::from_le_bytes(b) as f64
        }
        3 => {
            let val = frame[offset] as i32
                | (frame[offset + 1] as i32) << 8
                | ((frame[offset + 2] as i8) as i32) << 16;
            val as f64
        }
        4 => {
            let b = [
                frame[offset],
                frame[offset + 1],
                frame[offset + 2],
                frame[offset + 3],
            ];
            i32::from_le_bytes(b) as f64
        }
        _ => 0.0,
    };
    raw / max_val
}

/// Le spectre d'une trame, sous ses deux formes.
#[derive(Debug, Clone, Default)]
pub struct Spectrum {
    /// Forme normalisée trame par trame (0..1) — contrat historique.
    pub shape: Vec<f32>,
    /// Niveau absolu par bande, en dBFS.
    pub db: Vec<f32>,
}

/// Compute spectrum bins from PCM data using a simple FFT.
/// Returns `bins` magnitude values (0.0..1.0) spread across the frequency range.
pub fn compute_spectrum(
    pcm: &[u8],
    bit_depth: u16,
    channels: u16,
    bins: usize,
    sample_rate: u32,
) -> Vec<f32> {
    analyze_spectrum(pcm, bit_depth, channels, bins, sample_rate).shape
}

/// Analyse spectrale d'une trame : une FFT, deux lectures.
///
/// `shape` conserve la normalisation trame-par-trame attendue par les clients
/// déjà déployés ; `db` donne le niveau absolu de chaque bande en dBFS.
///
/// Référence du 0 dBFS : les échantillons sont ramenés à ±1, la fenêtre de Hann
/// a un gain cohérent de 0,5, donc une sinusoïde pleine échelle tombant au
/// centre d'une raie donne une magnitude de `n/4`. C'est cette valeur qui sert
/// de référence, et c'est ce qui rend la lecture comparable à `peak_*_db`.
pub fn analyze_spectrum(
    pcm: &[u8],
    bit_depth: u16,
    channels: u16,
    bins: usize,
    sample_rate: u32,
) -> Spectrum {
    let empty = || Spectrum {
        shape: vec![0.0; bins],
        db: vec![SPECTRUM_FLOOR_DB; bins],
    };
    if pcm.is_empty() || channels == 0 || bins == 0 {
        return empty();
    }

    let bytes_per_sample = (bit_depth / 8) as usize;
    let frame_size = bytes_per_sample * channels as usize;
    if frame_size == 0 {
        return empty();
    }

    // Extract mono samples (mix L+R), max 2048 samples for FFT
    let fft_size = 2048usize;
    let mut samples: Vec<f64> = Vec::with_capacity(fft_size);
    for frame in pcm.chunks_exact(frame_size).take(fft_size) {
        let left = read_sample(frame, 0, bytes_per_sample, bit_depth);
        let right = if channels >= 2 {
            read_sample(frame, bytes_per_sample, bytes_per_sample, bit_depth)
        } else {
            left
        };
        samples.push((left + right) * 0.5);
    }

    let n = samples.len().next_power_of_two().min(fft_size);
    samples.resize(n, 0.0);

    // Apply Hann window
    for i in 0..n {
        let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos());
        samples[i] *= w;
    }

    // In-place Cooley-Tukey FFT
    let mut re = samples;
    let mut im = vec![0.0f64; n];

    // Bit-reversal permutation
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            re.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j >= m {
            j -= m;
            m >>= 1;
        }
        j += m;
    }

    // FFT butterfly
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let angle_step = -2.0 * std::f64::consts::PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let angle = angle_step * k as f64;
                let wr = angle.cos();
                let wi = angle.sin();
                let a = start + k;
                let b = start + k + half;
                let tr = wr * re[b] - wi * im[b];
                let ti = wr * im[b] + wi * re[b];
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }

    // Compute magnitudes for the first half (positive frequencies)
    let half = n / 2;
    let mut mags: Vec<f64> = Vec::with_capacity(half);
    let mut max_mag: f64 = 1e-10;
    for i in 0..half {
        let mag = (re[i] * re[i] + im[i] * im[i]).sqrt();
        max_mag = max_mag.max(mag);
        mags.push(mag);
    }

    // Map FFT bins to output bins using true logarithmic frequency scale.
    // Each output bin covers one equal fraction of the audible range on a
    // log axis (20 Hz – 20 kHz), matching human pitch perception.
    let nyquist = sample_rate as f64 / 2.0;
    let freq_min = 20.0_f64;
    let freq_max = nyquist.min(20000.0);
    let log_ratio = freq_max / freq_min;
    let mut result = vec![0.0f32; bins];
    let mut result_db = vec![SPECTRUM_FLOOR_DB; bins];
    // Sinusoïde pleine échelle au centre d'une raie, fenêtre de Hann : n/4.
    let full_scale = (n as f64) / 4.0;
    for b in 0..bins {
        let hz_low = freq_min * log_ratio.powf(b as f64 / bins as f64);
        let hz_high = freq_min * log_ratio.powf((b + 1) as f64 / bins as f64);
        let f_low = ((hz_low / nyquist) * half as f64) as usize;
        let f_high = ((hz_high / nyquist) * half as f64) as usize;
        let f_low = f_low.min(half - 1);
        let f_high = f_high.max(f_low + 1).min(half);

        let mut sum = 0.0;
        let mut sum_sq = 0.0;
        let count = (f_high - f_low).max(1);
        for i in f_low..f_high {
            sum += mags[i];
            sum_sq += mags[i] * mags[i];
        }
        let avg = sum / count as f64;
        // Normalize to 0..1, apply some compression
        let normalized = (avg / max_mag).powf(0.6);
        result[b] = normalized as f32;

        // Niveau absolu : énergie de la bande, pas moyenne des magnitudes —
        // une raie isolée dans une bande large ne doit pas être diluée par ses
        // voisines silencieuses.
        let band_mag = (sum_sq / count as f64).sqrt() * (count as f64).sqrt();
        result_db[b] = if band_mag <= 0.0 || full_scale <= 0.0 {
            SPECTRUM_FLOOR_DB
        } else {
            ((20.0 * (band_mag / full_scale).log10()) as f32).clamp(SPECTRUM_FLOOR_DB, 6.0)
        };
    }

    Spectrum {
        shape: result,
        db: result_db,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // #1694 — crête tenue (max glissant ~300 ms) dans le flux audio_levels.
    // ------------------------------------------------------------------

    /// Un transitoire sur UNE fenêtre doit rester lisible pendant toute la
    /// tenue (~300 ms), puis disparaître : c'est exactement ce qu'une trame
    /// WebSocket perdue ne pardonne pas à `peak_*_db` seul.
    #[test]
    fn peak_hold_survives_the_window_then_expires() {
        let w = std::time::Duration::from_millis(100);
        let mut ph = PeakHold::default();

        // Fenêtre 1 : transitoire pleine échelle à gauche, -6 dB à droite.
        let (l, r) = ph.update(w, 1.0, 0.5);
        assert!(l.abs() < 1e-3, "0 dBFS attendu, obtenu {l}");
        assert!((r - (-6.0206)).abs() < 0.01, "-6 dBFS attendu, obtenu {r}");

        // Fenêtres 2 et 3 : silence — le transitoire est TENU (sa fenêtre
        // reste dans les 300 ms glissantes).
        for _ in 0..2 {
            let (l, r) = ph.update(w, 0.0, 0.0);
            assert!(l.abs() < 1e-3, "tenue attendue, obtenu {l}");
            assert!((r - (-6.0206)).abs() < 0.01, "tenue attendue, obtenu {r}");
        }

        // Fenêtre 4 : le transitoire est sorti de la tenue — retour plancher.
        let (l, r) = ph.update(w, 0.0, 0.0);
        assert!(l <= -96.0, "expiration attendue, obtenu {l}");
        assert!(r <= -96.0, "expiration attendue, obtenu {r}");
    }

    /// La tenue rend le MAX de la fenêtre glissante, pas la dernière valeur :
    /// une crête plus forte remplace immédiatement une plus faible, jamais
    /// l'inverse.
    #[test]
    fn peak_hold_is_a_sliding_max_not_a_last_value() {
        let w = std::time::Duration::from_millis(100);
        let mut ph = PeakHold::default();
        ph.update(w, 0.25, 0.25);
        let (l, _) = ph.update(w, 1.0, 0.25);
        assert!(l.abs() < 1e-3, "la crête forte doit primer, obtenu {l}");
        let (l, _) = ph.update(w, 0.25, 0.25);
        assert!(
            l.abs() < 1e-3,
            "la crête forte est encore tenue, obtenu {l}"
        );
    }

    /// Des fenêtres dégénérées (durée nulle) ne doivent pas faire croître le
    /// tampon sans borne — chemin temps réel, mémoire bornée par contrat.
    #[test]
    fn peak_hold_buffer_is_bounded_on_zero_length_windows() {
        let mut ph = PeakHold::default();
        for _ in 0..10_000 {
            ph.update(std::time::Duration::ZERO, 0.1, 0.1);
        }
        assert!(ph.entries.len() <= PeakHold::MAX_ENTRIES);
    }

    #[test]
    fn silence_returns_low_db() {
        let pcm = vec![0u8; 1024];
        let levels = compute_levels(&pcm, 16, 2, 44100);
        assert!(levels.rms_left_db() <= -96.0);
        assert!(levels.peak_left_db() <= -96.0);
    }

    #[test]
    fn full_scale_returns_zero_db() {
        let mut pcm = Vec::new();
        for _ in 0..100 {
            pcm.extend_from_slice(&i16::MAX.to_le_bytes()); // left
            pcm.extend_from_slice(&i16::MAX.to_le_bytes()); // right
        }
        let levels = compute_levels(&pcm, 16, 2, 44100);
        assert!(levels.peak_left_db() > -1.0);
        assert!(levels.peak_right_db() > -1.0);
    }

    /// Soft 24-bit stereo sine (~−20 dBFS). Packs signed 24-bit LE samples.
    fn sine_pcm_24(freq: f64, amp: f64, sr: u32, frames: usize) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(frames * 6);
        let max_24 = (1i32 << 23) - 1;
        for i in 0..frames {
            let t = i as f64 / sr as f64;
            let sample = amp * (2.0 * std::f64::consts::PI * freq * t).sin();
            let val = (sample * max_24 as f64) as i32;
            let b = val.to_le_bytes();
            pcm.extend_from_slice(&[b[0], b[1], b[2]]);
            pcm.extend_from_slice(&[b[0], b[1], b[2]]);
        }
        pcm
    }

    #[test]
    fn compute_levels_24bit_aligned_reports_quiet_peak() {
        // OAAT uses 24-bit WAV. When chunks stay frame-aligned, a −20 dBFS
        // sine must not peg the VU near 0 dBFS.
        let pcm = sine_pcm_24(1000.0, 0.1, 44100, 4096);
        assert_eq!(pcm.len() % 6, 0);
        let levels = compute_levels(&pcm, 24, 2, 44100);
        let peak = levels.peak_left_db();
        assert!(
            peak < -15.0 && peak > -25.0,
            "aligned 24-bit −20 dBFS sine peaked at {peak} dBFS"
        );
    }

    #[test]
    fn compute_levels_24bit_misaligned_chunk_pegs_peak() {
        // Reproduces the fixed-32768 drain bug: 32768 % 6 == 2, so the 2nd
        // batch starts mid-sample and compute_levels reads garbage → ~0 dBFS.
        let pcm = sine_pcm_24(1000.0, 0.1, 44100, 20_000);
        assert!(pcm.len() > 32768 + 6);
        let misaligned = &pcm[32768..32768 + 32766];
        assert_eq!(misaligned.len() % 6, 0);
        let levels = compute_levels(misaligned, 24, 2, 44100);
        let peak = levels.peak_left_db();
        assert!(
            peak > -6.0,
            "misaligned 24-bit chunk should peg the VU (got {peak} dBFS)"
        );
    }

    /// PCM stéréo d'une sinusoïde à `freq`, d'amplitude `amp` (1.0 = pleine échelle).
    fn sine_pcm(freq: f64, amp: f64, sr: u32, frames: usize) -> Vec<u8> {
        let mut pcm = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let t = i as f64 / sr as f64;
            let sample = amp * (2.0 * std::f64::consts::PI * freq * t).sin();
            let val = (sample * i16::MAX as f64) as i16;
            pcm.extend_from_slice(&val.to_le_bytes()); // left
            pcm.extend_from_slice(&val.to_le_bytes()); // right
        }
        pcm
    }

    /// Indice de la bande logarithmique (20 Hz–20 kHz) contenant `freq`.
    fn bin_of(freq: f64, bins: usize, sr: u32) -> usize {
        let nyquist = sr as f64 / 2.0;
        let ratio = nyquist.min(20000.0) / 20.0;
        (((freq / 20.0).log10() / ratio.log10()) * bins as f64) as usize
    }

    #[test]
    fn spectrum_db_reads_absolute_level() {
        // Une sinusoïde pleine échelle doit lire ~0 dBFS dans SA bande, et le
        // niveau doit SUIVRE l'amplitude : c'est tout l'intérêt du champ, là où
        // `spectrum` (normalisé trame par trame) donne 1,0 dans les deux cas.
        let sr = 44100u32;
        let bins = 32usize;
        let b440 = bin_of(440.0, bins, sr);

        let full = analyze_spectrum(&sine_pcm(440.0, 1.0, sr, 2048), 16, 2, bins, sr);
        let quiet = analyze_spectrum(&sine_pcm(440.0, 0.1, sr, 2048), 16, 2, bins, sr);

        assert!(
            (full.db[b440] - 0.0).abs() < 3.0,
            "pleine échelle lue à {} dBFS dans la bande {b440}",
            full.db[b440]
        );
        // −20 dB d'amplitude → −20 dB de niveau.
        let delta = full.db[b440] - quiet.db[b440];
        assert!(
            (delta - 20.0).abs() < 3.0,
            "écart plein/−20 dB mesuré à {delta} dB"
        );
        // La forme, elle, ne distingue pas les deux : c'est le défaut corrigé.
        assert!((full.shape[b440] - quiet.shape[b440]).abs() < 0.05);
    }

    #[test]
    fn spectrum_db_is_quiet_away_from_the_tone() {
        let sr = 44100u32;
        let bins = 32usize;
        let s = analyze_spectrum(&sine_pcm(440.0, 1.0, sr, 2048), 16, 2, bins, sr);
        let b440 = bin_of(440.0, bins, sr);
        // Une bande deux octaves plus haut ne doit pas voir la fondamentale.
        let far = bin_of(4000.0, bins, sr);
        assert!(
            s.db[far] < s.db[b440] - 30.0,
            "fuite à 4 kHz : {} dBFS contre {} dBFS à 440 Hz",
            s.db[far],
            s.db[b440]
        );
    }

    #[test]
    fn spectrum_db_floors_on_silence() {
        let s = analyze_spectrum(&vec![0u8; 4096], 16, 2, 32, 44100);
        assert_eq!(s.db.len(), 32);
        assert!(
            s.db.iter().all(|&d| d <= SPECTRUM_FLOOR_DB + 0.01),
            "le silence ne descend pas au plancher : {:?}",
            &s.db[..4]
        );
    }

    #[test]
    fn spectrum_440hz_peak_in_correct_bin() {
        let sr = 44100u32;
        let freq = 440.0f64;
        let mut pcm = Vec::new();
        for i in 0..2048 {
            let t = i as f64 / sr as f64;
            let sample = (2.0 * std::f64::consts::PI * freq * t).sin();
            let val = (sample * i16::MAX as f64) as i16;
            pcm.extend_from_slice(&val.to_le_bytes()); // left
            pcm.extend_from_slice(&val.to_le_bytes()); // right
        }
        let spectrum = compute_spectrum(&pcm, 16, 2, 32, sr);
        let peak_bin = spectrum
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .unwrap()
            .0;
        // 440 Hz in 32 log-scale bins (20–20000 Hz) should be around bin 10-12
        assert!(
            peak_bin >= 8 && peak_bin <= 14,
            "440Hz peak at bin {peak_bin}"
        );
    }
}
