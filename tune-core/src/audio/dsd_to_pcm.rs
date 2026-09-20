//! DSD-to-PCM converter using FIR decimation.
//!
//! DSD is a 1-bit stream at very high sample rates (e.g., 2.8224 MHz for DSD64).
//! This module decimates DSD data to multi-bit PCM at a conventional sample rate
//! (e.g., 176.4 kHz or 88.2 kHz) using a windowed-sinc FIR lowpass filter.
//!
//! The converter produces 24-bit signed PCM samples packed as little-endian i32
//! values in the output byte stream (3 bytes per sample, packed as 4 bytes for
//! alignment, or as raw 24-bit LE).

use std::f64::consts::PI;

/// Échelle SACD appliquée à la conversion DSD→PCM : +6,02 dB (×2,0).
///
/// La référence 0 dB du DSD (Scarlet Book) est posée 6 dB SOUS la pleine
/// échelle du domaine ±1 issu du filtre — les crêtes autorisées montent à
/// +3,1 dB au-dessus de cette référence. Convertir à l'échelle 1:1 rendait
/// donc tout le parc DSD ~5-6 dB plus bas que les éditions PCM des mêmes
/// masters — mesuré par Reivax66 (Head Hunters DSD vs 24/96, mêmes ISRC :
/// crêtes DSD à −5/−7 dBFS, FLAC à 0 dBFS, ~+4,4 dB d'écart album — #1638).
/// Tous les convertisseurs de place (foobar SACD, Weiss, HQPlayer) appliquent
/// ce ×2 ; le clamp aval absorbe les crêtes extrêmes au-delà de la pleine
/// échelle (rarissimes : il faut dépasser la référence de plus de 6 dB).
///
/// Les ReplayGain calculés sur l'ancienne échelle sont invalidés par la
/// migration 75 (SQLite) / 024 (PG) — sans elle, un gain stocké (+2,25 dB
/// typiquement) cumulé au ×2 sur-amplifierait de 6 dB.
pub const DSD_SACD_GAIN: f64 = 2.0;

/// DSD-to-PCM decimation converter.
pub struct DsdToPcmConverter {
    /// How many DSD bits map to one PCM sample.
    decimation_ratio: usize,
    /// FIR filter coefficients (one per DSD sample in a decimation window).
    filter_coeffs: Vec<f64>,
    /// Number of audio channels.
    channels: usize,
    /// Output PCM sample rate in Hz.
    pub output_rate: u32,
    /// Output bit depth (always 24).
    pub output_depth: u32,
    /// Whether input DSD bits are LSB-first (DSF) or MSB-first (DFF).
    lsb_first: bool,
}

impl DsdToPcmConverter {
    /// Create a new converter.
    ///
    /// - `dsd_rate`: DSD sample rate (e.g., 2_822_400 for DSD64)
    /// - `target_rate`: desired PCM output rate (e.g., 176_400)
    /// - `channels`: number of audio channels
    /// - `lsb_first`: true for DSF (LSB first), false for DFF (MSB first)
    pub fn new(dsd_rate: u32, target_rate: u32, channels: usize, lsb_first: bool) -> Self {
        let decimation_ratio = (dsd_rate / target_rate) as usize;
        assert!(decimation_ratio > 0, "decimation ratio must be > 0");

        // FIR filter length: longer for higher decimation ratios
        let filter_len = match decimation_ratio {
            1..=16 => 256,
            17..=32 => 512,
            33..=64 => 1024,
            _ => 2048,
        };

        let filter_coeffs = design_lowpass_fir(filter_len, decimation_ratio);

        DsdToPcmConverter {
            decimation_ratio,
            filter_coeffs,
            channels,
            output_rate: target_rate,
            output_depth: 24,
            lsb_first,
        }
    }

    /// Convert DSD data bytes to PCM samples.
    ///
    /// Input: interleaved DSD bytes (ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...)
    /// Each byte contains 8 DSD samples.
    ///
    /// Output: interleaved 24-bit PCM samples as little-endian bytes (3 bytes per sample).
    /// Layout: ch0_sample0 (3 bytes), ch1_sample0 (3 bytes), ch0_sample1 (3 bytes), ...
    pub fn process(&self, dsd_data: &[u8]) -> Vec<u8> {
        let channels = self.channels;
        if channels == 0 || dsd_data.is_empty() {
            return Vec::new();
        }

        // Total DSD bytes per channel
        let total_bytes = dsd_data.len() / channels;
        // Total DSD samples (bits) per channel
        let total_dsd_samples = total_bytes * 8;
        // Number of output PCM samples per channel
        let output_samples_per_ch = total_dsd_samples / self.decimation_ratio;

        if output_samples_per_ch == 0 {
            return Vec::new();
        }

        // De-interleave DSD data into per-channel bit streams
        // and expand bytes to +1/-1 sample values for filtering
        let mut channel_bits: Vec<Vec<f64>> = vec![Vec::with_capacity(total_dsd_samples); channels];

        for byte_idx in 0..total_bytes {
            for ch in 0..channels {
                let src_idx = byte_idx * channels + ch;
                if src_idx >= dsd_data.len() {
                    break;
                }
                let byte = dsd_data[src_idx];
                // Extract 8 bits from this byte
                for bit in 0..8u8 {
                    let bit_val = if self.lsb_first {
                        // DSF: LSB first
                        (byte >> bit) & 1
                    } else {
                        // DFF: MSB first
                        (byte >> (7 - bit)) & 1
                    };
                    // DSD: 1 = positive, 0 = negative
                    channel_bits[ch].push(if bit_val == 1 { 1.0 } else { -1.0 });
                }
            }
        }

        // Apply FIR decimation filter per channel
        let filter_len = self.filter_coeffs.len();
        let half_filter = filter_len / 2;

        // Output buffer: 3 bytes per sample, interleaved channels
        let mut output = Vec::with_capacity(output_samples_per_ch * channels * 3);

        for sample_idx in 0..output_samples_per_ch {
            for ch in 0..channels {
                let center = sample_idx * self.decimation_ratio + self.decimation_ratio / 2;
                let mut sum = 0.0f64;

                for (k, &coeff) in self.filter_coeffs.iter().enumerate() {
                    // Position of this filter tap in the DSD stream
                    let pos = (center as isize) - (half_filter as isize) + (k as isize);
                    if pos >= 0 && (pos as usize) < channel_bits[ch].len() {
                        sum += channel_bits[ch][pos as usize] * coeff;
                    }
                }

                // Clamp to [-1.0, 1.0] and convert to 24-bit signed integer
                let clamped = (sum * DSD_SACD_GAIN).clamp(-1.0, 1.0);
                let pcm_val = (clamped * 8_388_607.0) as i32; // 2^23 - 1

                // Write as 24-bit little-endian
                let bytes = pcm_val.to_le_bytes();
                output.push(bytes[0]);
                output.push(bytes[1]);
                output.push(bytes[2]);
            }
        }

        output
    }

    /// Convert DSD data and return as i16 samples (for compatibility with DecodedAudio).
    ///
    /// This is a convenience wrapper that converts 24-bit output to 16-bit.
    pub fn process_to_i16(&self, dsd_data: &[u8]) -> Vec<i16> {
        let pcm_24 = self.process(dsd_data);
        let num_samples = pcm_24.len() / 3;
        let mut samples = Vec::with_capacity(num_samples);

        for i in 0..num_samples {
            let offset = i * 3;
            if offset + 2 >= pcm_24.len() {
                break;
            }
            // Reconstruct 24-bit signed value from LE bytes
            let lo = pcm_24[offset] as u32;
            let mid = pcm_24[offset + 1] as u32;
            let hi = pcm_24[offset + 2] as u32;
            let val24 = lo | (mid << 8) | (hi << 16);
            // Sign-extend from 24-bit to 32-bit
            let val32 = if val24 & 0x80_0000 != 0 {
                (val24 | 0xFF00_0000) as i32
            } else {
                val24 as i32
            };
            // Truncate 24-bit to 16-bit (shift right by 8)
            let val16 = (val32 >> 8) as i16;
            samples.push(val16);
        }

        samples
    }
}

/// Design a lowpass FIR filter using windowed sinc method.
///
/// - `length`: number of filter taps
/// - `decimation`: decimation ratio (cutoff = 0.45 / decimation)
fn design_lowpass_fir(length: usize, decimation: usize) -> Vec<f64> {
    let mut coeffs = vec![0.0f64; length];
    let cutoff = 0.45 / decimation as f64; // normalized cutoff frequency
    let center = (length - 1) as f64 / 2.0;

    for i in 0..length {
        let x = i as f64 - center;

        // Sinc function
        let sinc = if x.abs() < 1e-10 {
            2.0 * PI * cutoff
        } else {
            (2.0 * PI * cutoff * x).sin() / x
        };

        // Blackman-Harris window
        let n = i as f64 / (length - 1) as f64;
        let window = 0.35875 - 0.48829 * (2.0 * PI * n).cos() + 0.14128 * (4.0 * PI * n).cos()
            - 0.01168 * (6.0 * PI * n).cos();

        coeffs[i] = sinc * window;
    }

    // Normalize so that the filter has unity gain at DC
    let sum: f64 = coeffs.iter().sum();
    if sum.abs() > 1e-10 {
        for c in &mut coeffs {
            *c /= sum;
        }
    }

    coeffs
}

/// Choose the best output sample rate for a given DSD rate.
///
/// Returns a rate that's a clean integer divisor of the DSD rate,
/// preferring 176.4 kHz for DSD64 and 352.8 kHz for DSD128+.
pub fn choose_output_rate(dsd_rate: u32) -> u32 {
    match dsd_rate {
        r if r >= 11_000_000 => 352_800, // DSD256/512
        r if r >= 5_000_000 => 352_800,  // DSD128
        r if r >= 2_000_000 => 176_400,  // DSD64
        _ => 176_400,                    // fallback
    }
}

/// Streaming DSD-to-PCM converter that processes DSD data in chunks.
///
/// Unlike `DsdToPcmConverter::process()` which loads the entire DSD file
/// into memory (causing OOM for large DSD files -- a 5-min DSD64 stereo
/// file expands to ~13 GB of f64 arrays), this converter keeps only the
/// DSD history the next FIR windows still need, and produces PCM output
/// incrementally.
///
/// Memory usage: O((filter_len + chunk) * channels), independent of file size.
///
/// ## Ce que calcule le convertisseur, et dans quel ordre (#4354)
///
/// La sortie `n` d'un canal vaut
/// `y[n] = Σ_{k=0}^{L-1} c[k] · x(n·D + D/2 − L/2 + k)`, sommée **dans l'ordre
/// k croissant**, avec `x(p) = ±1` pour un bit du flux et `x(p) = 0` hors du
/// flux (avant le premier bit, et après le dernier au `flush`). C'est le modèle
/// du convertisseur de référence [`DsdToPcmConverter::process`].
///
/// Jusqu'à la v0.9.157, chaque sortie était calculée seule, à l'instant où son
/// dernier bit arrivait : 256 additions f64 **enchaînées** (DSD128 → 352,8 kHz)
/// dont chacune attend la précédente. La boucle était bornée par la LATENCE de
/// l'additionneur, pas par son débit. C'est le seul point chaud de la chaîne
/// DSD → WAV réseau (lecture DSF : < 0,3 % du temps) ; mesuré en release par
/// `examples/banc_dsd_4354.rs` sur DSD128 → 352,8 kHz : 4,1× le temps réel sur
/// un Xeon E5-2630 v4 (Shrek), 11× sur un cœur P et 5× sur un cœur E du Core
/// i5-1340P du .42. Après ce changement : 9,9× (Shrek), 23× (cœur P) et 11,8×
/// (cœur E), PCM inchangé. Aucun hôte ARM ou NAS n'a été mesuré.
///
/// Les sorties voisines sont pourtant indépendantes. `feed` pousse désormais
/// tout le bloc dans un historique linéaire, puis calcule les sorties prêtes
/// **par lots de huit** ([`fir_lot`]) : huit accumulateurs indépendants avancent
/// ensemble, le processeur les entrelace. Chaque accumulateur garde exactement
/// sa suite d'opérations — mêmes produits `x·c` (exacts : `x = ±1`), même ordre
/// k croissant —, donc le PCM est **identique au bit près** à l'ancien calcul,
/// ce que verrouillent `streamer_identique_au_bit_a_la_reference_*` (contre
/// [`DsdToPcmConverter::process`]) et l'empreinte figée
/// `streamer_empreinte_figee_dsd128` (calculée par l'ancien code). Retirer un
/// seul tap les fait tomber toutes les deux.
pub struct DsdToPcmStreamer {
    /// How many DSD bits map to one PCM sample.
    decimation_ratio: usize,
    /// FIR filter coefficients.
    filter_coeffs: Vec<f64>,
    /// Number of audio channels.
    channels: usize,
    /// Output PCM sample rate in Hz.
    pub output_rate: u32,
    /// Output bit depth (always 24).
    pub output_depth: u32,
    /// Whether input DSD bits are LSB-first (DSF) or MSB-first (DFF).
    lsb_first: bool,
    /// Historique linéaire par canal des échantillons DSD (+1.0 / −1.0) :
    /// `hist[ch][i]` est la position absolue `hist_base + i`. Les positions
    /// négatives valent 0.0 — le bourrage que voit le filtre avant le début
    /// du flux. Tronqué en tête après chaque bloc à la fenêtre de la
    /// prochaine sortie.
    hist: Vec<Vec<f64>>,
    /// Position absolue de `hist[ch][0]` (négative tant que le bourrage
    /// initial est encore là).
    hist_base: isize,
    /// Total DSD samples fed so far (across all calls to `feed`), per channel.
    total_dsd_samples: usize,
    /// Number of PCM output samples already emitted per channel.
    output_sample_idx: usize,
}

/// Nombre de sorties calculées ensemble par [`fir_lot`].
const LOT_FIR: usize = 8;

/// Huit sorties FIR voisines d'un même canal, d'un seul passage sur les
/// coefficients. `x` commence à la fenêtre de la première sortie ; la sortie
/// `j` lit `x[j·d .. j·d + L]`. Chaque accumulateur suit l'ordre k croissant,
/// exactement comme [`fir_une`] : seul l'entrelacement change, pas les
/// opérations de chaque somme.
#[inline]
fn fir_lot(x: &[f64], d: usize, c: &[f64]) -> [f64; LOT_FIR] {
    let l = c.len();
    let fenetres: [&[f64]; LOT_FIR] = std::array::from_fn(|j| &x[j * d..j * d + l]);
    let mut acc = [0.0f64; LOT_FIR];
    for (k, &ck) in c.iter().enumerate() {
        for j in 0..LOT_FIR {
            acc[j] += fenetres[j][k] * ck;
        }
    }
    acc
}

/// Une sortie FIR seule (reliquat d'un lot incomplet), même ordre k croissant.
#[inline]
fn fir_une(x: &[f64], c: &[f64]) -> f64 {
    let mut s = 0.0f64;
    for (xk, ck) in x[..c.len()].iter().zip(c) {
        s += *xk * *ck;
    }
    s
}

/// Échantillon filtré → 24 bits LE, échelle SACD et saturation comprises.
#[inline]
fn pousser_24(sortie: &mut Vec<u8>, somme: f64) {
    let clamped = (somme * DSD_SACD_GAIN).clamp(-1.0, 1.0);
    let pcm_val = (clamped * 8_388_607.0) as i32;
    sortie.extend_from_slice(&pcm_val.to_le_bytes()[..3]);
}

impl DsdToPcmStreamer {
    /// Create a new streaming converter.
    ///
    /// - `dsd_rate`: DSD sample rate (e.g., 2_822_400 for DSD64)
    /// - `target_rate`: desired PCM output rate (e.g., 176_400)
    /// - `channels`: number of audio channels
    /// - `lsb_first`: true for DSF (LSB first), false for DFF (MSB first)
    pub fn new(dsd_rate: u32, target_rate: u32, channels: usize, lsb_first: bool) -> Self {
        let decimation_ratio = (dsd_rate / target_rate) as usize;
        assert!(decimation_ratio > 0, "decimation ratio must be > 0");

        let filter_len = match decimation_ratio {
            1..=16 => 256,
            17..=32 => 512,
            33..=64 => 1024,
            _ => 2048,
        };

        let filter_coeffs = design_lowpass_fir(filter_len, decimation_ratio);

        // La fenêtre de la sortie 0 commence à D/2 − L/2 : autant de zéros de
        // bourrage devant le premier bit.
        let bourrage = (filter_len / 2).saturating_sub(decimation_ratio / 2);

        DsdToPcmStreamer {
            decimation_ratio,
            filter_coeffs,
            channels,
            output_rate: target_rate,
            output_depth: 24,
            lsb_first,
            hist: vec![vec![0.0f64; bourrage]; channels],
            hist_base: -(bourrage as isize),
            total_dsd_samples: 0,
            output_sample_idx: 0,
        }
    }

    /// Position absolue du premier tap de la sortie `n`.
    fn debut_fenetre(&self, n: usize) -> isize {
        (n * self.decimation_ratio + self.decimation_ratio / 2) as isize
            - (self.filter_coeffs.len() / 2) as isize
    }

    /// Calcule `nombre` sorties à partir de `output_sample_idx`, entrelacées
    /// par canal, puis libère l'historique devenu inutile. L'historique doit
    /// couvrir la fenêtre de la dernière de ces sorties.
    fn emettre(&mut self, nombre: usize, sortie: &mut Vec<u8>) {
        let channels = self.channels;
        let d = self.decimation_ratio;
        let coeffs = &self.filter_coeffs;
        let mut sommes = vec![[0.0f64; LOT_FIR]; channels];
        let mut fait = 0usize;
        while fait < nombre {
            let n = self.output_sample_idx + fait;
            let depart = (self.debut_fenetre(n) - self.hist_base) as usize;
            let lot = (nombre - fait).min(LOT_FIR);
            for (ch, somme) in sommes.iter_mut().enumerate() {
                let x = &self.hist[ch][depart..];
                if lot == LOT_FIR {
                    *somme = fir_lot(x, d, coeffs);
                } else {
                    for (j, s) in somme.iter_mut().take(lot).enumerate() {
                        *s = fir_une(&x[j * d..], coeffs);
                    }
                }
            }
            for j in 0..lot {
                for somme in &sommes {
                    pousser_24(sortie, somme[j]);
                }
            }
            fait += lot;
        }
        self.output_sample_idx += nombre;

        // Rien avant la fenêtre de la prochaine sortie ne servira plus.
        let garder_depuis = self.debut_fenetre(self.output_sample_idx) - self.hist_base;
        if garder_depuis > 0 {
            let a_jeter = (garder_depuis as usize).min(self.hist.first().map_or(0, Vec::len));
            for h in &mut self.hist {
                h.drain(..a_jeter);
            }
            self.hist_base += a_jeter as isize;
        }
    }

    /// Feed a chunk of interleaved DSD bytes and return the resulting PCM.
    ///
    /// Input layout: ch0_byte0, ch1_byte0, ch0_byte1, ch1_byte1, ...
    /// (same interleaving as DSF after de-blocking, or DFF natively).
    ///
    /// Output: 24-bit LE PCM bytes (3 bytes per sample, interleaved channels).
    ///
    /// Call this repeatedly with successive chunks from the file. The converter
    /// maintains internal state between calls. The chunk size can vary.
    /// A trailing partial frame (fewer bytes than `channels`) is ignored.
    pub fn feed(&mut self, dsd_chunk: &[u8]) -> Vec<u8> {
        let channels = self.channels;
        if channels == 0 || dsd_chunk.is_empty() {
            return Vec::new();
        }
        let total_bytes = dsd_chunk.len() / channels;
        if total_bytes == 0 {
            return Vec::new();
        }

        // 1 bit DSD → ±1.0, par canal, dans l'ordre du flux.
        for (ch, hist) in self.hist.iter_mut().enumerate() {
            hist.reserve(total_bytes * 8);
            for byte_idx in 0..total_bytes {
                let byte = dsd_chunk[byte_idx * channels + ch];
                for bit in 0..8u8 {
                    let bit_val = if self.lsb_first {
                        (byte >> bit) & 1
                    } else {
                        (byte >> (7 - bit)) & 1
                    };
                    hist.push(if bit_val == 1 { 1.0 } else { -1.0 });
                }
            }
        }
        self.total_dsd_samples += total_bytes * 8;

        // Une sortie est prête quand le dernier bit de sa fenêtre est arrivé
        // (n·D + D/2 + L/2 ≤ total) — et, comme avant, pas avant que le flux
        // ait rempli une fenêtre entière.
        let filter_len = self.filter_coeffs.len();
        let d = self.decimation_ratio;
        let reach = d / 2 + filter_len / 2;
        if self.total_dsd_samples < filter_len || self.total_dsd_samples < reach {
            return Vec::new();
        }
        let pretes = (self.total_dsd_samples - reach) / d + 1;
        let nouvelles = pretes.saturating_sub(self.output_sample_idx);
        let mut output = Vec::with_capacity(nouvelles * channels * 3);
        if nouvelles > 0 {
            self.emettre(nouvelles, &mut output);
        }
        output
    }

    /// Flush any remaining samples at the end of the stream.
    /// Produces PCM for any DSD samples that haven't been output yet
    /// (due to the FIR filter needing future samples that don't exist:
    /// those taps read zero).
    pub fn flush(&mut self) -> Vec<u8> {
        let channels = self.channels;
        if channels == 0 {
            return Vec::new();
        }
        let max_outputs = self.total_dsd_samples / self.decimation_ratio;
        let remaining = max_outputs.saturating_sub(self.output_sample_idx);
        if remaining == 0 {
            return Vec::new();
        }

        // Bourrage de fin : les taps au-delà du dernier bit lisent 0.0.
        let fin_reelle = self.hist_base + self.hist[0].len() as isize;
        let fin_voulue = self.debut_fenetre(max_outputs - 1) + self.filter_coeffs.len() as isize;
        if fin_voulue > fin_reelle {
            let longueur = (fin_voulue - self.hist_base) as usize;
            for h in &mut self.hist {
                h.resize(longueur, 0.0);
            }
        }

        let mut output = Vec::with_capacity(remaining * channels * 3);
        self.emettre(remaining, &mut output);

        // Retirer le bourrage : l'historique ne garde que des bits réels.
        let longueur_reelle = (fin_reelle - self.hist_base).max(0) as usize;
        for h in &mut self.hist {
            h.truncate(longueur_reelle);
        }
        output
    }

    /// Total number of PCM samples emitted so far (across all channels).
    pub fn total_output_samples(&self) -> usize {
        self.output_sample_idx * self.channels
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_coefficients_sum_to_one() {
        let coeffs = design_lowpass_fir(256, 16);
        let sum: f64 = coeffs.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-6,
            "filter should have unity DC gain, got {sum}"
        );
    }

    #[test]
    fn filter_is_symmetric() {
        let coeffs = design_lowpass_fir(256, 16);
        let len = coeffs.len();
        for i in 0..len / 2 {
            assert!(
                (coeffs[i] - coeffs[len - 1 - i]).abs() < 1e-12,
                "filter should be symmetric at index {i}"
            );
        }
    }

    #[test]
    fn silence_dsd_produces_silence_pcm() {
        // All-zero DSD = constant negative = DC offset, but with a proper filter
        // it should produce a constant (near-DC) output.
        // All 0x00 means all bits are 0 => all -1.0 => strong negative DC.
        // All 0xFF means all bits are 1 => all +1.0 => strong positive DC.
        // A 50/50 mix (0x55 or 0xAA) approximates silence.

        // For true silence test: alternating 0x55 pattern (01010101 in binary)
        // With LSB-first (DSF), this gives alternating -1, +1, -1, +1...
        // which should produce near-zero PCM output.
        let channels = 2;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        // Generate enough data for at least a few output samples
        // Need decimation_ratio * output_samples DSD samples per channel
        // Each byte = 8 DSD samples, decimation = 16
        // So 1 output sample needs 16/8 = 2 bytes per channel
        let output_samples = 100;
        let bytes_per_ch = (output_samples * converter.decimation_ratio) / 8 + 128;
        let total_bytes = bytes_per_ch * channels;

        let dsd_data: Vec<u8> = (0..total_bytes).map(|_| 0x55u8).collect();
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty(), "should produce PCM output");

        // All samples should be near zero (alternating DSD = ~silence)
        // Allow some filter ringing near edges
        let mid_start = pcm.len() / 4;
        let mid_end = 3 * pcm.len() / 4;
        for &s in &pcm[mid_start..mid_end] {
            assert!(
                s.abs() < 1000,
                "alternating DSD pattern should be near-silence, got {s}"
            );
        }
    }

    #[test]
    fn all_zeros_dsd_produces_negative_dc() {
        // All 0x00 = all bits 0 = all -1.0 => should produce large negative PCM values
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        let bytes_per_ch = 1024;
        let dsd_data = vec![0x00u8; bytes_per_ch];
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty());

        // Middle samples should be strongly negative
        let mid = pcm.len() / 2;
        assert!(
            pcm[mid] < -10000,
            "all-zero DSD should produce negative PCM, got {}",
            pcm[mid]
        );
    }

    /// #1638 : l'échelle SACD (+6 dB) doit être appliquée à la conversion.
    /// Un motif DSD à 62,5 % de uns a une moyenne de +0,25 dans le domaine
    /// ±1 ; sans le ×2 il sortait à ~25 % de la pleine échelle, il doit
    /// désormais sortir à ~50 %. Et un DC saturant (tous uns, +1,0 ×2) doit
    /// être écrêté proprement à la pleine échelle, pas déborder.
    #[test]
    fn sacd_scale_doubles_conversion_and_clamps() {
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        // 5 bits à 1 sur 8 => moyenne (5-3)/8 = +0,25 => ~0,5 FS après ×2.
        let dsd_data = vec![0b0001_1111u8; 4096];
        let pcm = converter.process_to_i16(&dsd_data);
        let mid = pcm[pcm.len() / 2] as f64 / 32767.0;
        assert!(
            (0.45..0.55).contains(&mid),
            "attendu ~0,5 FS (échelle SACD ×2 sur une moyenne de 0,25), obtenu {mid:.3}"
        );

        // DC saturant : clamp à la pleine échelle, sans wrap.
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let pcm = converter.process_to_i16(&vec![0xFFu8; 4096]);
        let mid = pcm[pcm.len() / 2];
        assert!(
            mid >= 32700,
            "DC +1,0 doit saturer proprement, obtenu {mid}"
        );
    }

    #[test]
    fn all_ones_dsd_produces_positive_dc() {
        // All 0xFF = all bits 1 = all +1.0 => should produce large positive PCM values
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        let bytes_per_ch = 1024;
        let dsd_data = vec![0xFFu8; bytes_per_ch];
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty());

        let mid = pcm.len() / 2;
        assert!(
            pcm[mid] > 10000,
            "all-ones DSD should produce positive PCM, got {}",
            pcm[mid]
        );
    }

    #[test]
    fn dsd128_conversion() {
        let channels = 2;
        let converter = DsdToPcmConverter::new(5_644_800, 176_400, channels, true);

        assert_eq!(converter.decimation_ratio, 32);
        assert_eq!(converter.output_rate, 176_400);

        let bytes_per_ch = 2048;
        let dsd_data: Vec<u8> = (0..bytes_per_ch * channels).map(|_| 0x55u8).collect();
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty(), "DSD128 conversion should produce output");
    }

    #[test]
    fn msb_first_dff_conversion() {
        // DFF uses MSB-first bit ordering
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, false);

        let bytes_per_ch = 1024;
        // 0xAA = 10101010 MSB-first => alternating +1/-1 => near silence
        let dsd_data: Vec<u8> = (0..bytes_per_ch).map(|_| 0xAAu8).collect();
        let pcm = converter.process_to_i16(&dsd_data);

        assert!(!pcm.is_empty());

        // Middle samples should be near zero
        let mid_start = pcm.len() / 4;
        let mid_end = 3 * pcm.len() / 4;
        for &s in &pcm[mid_start..mid_end] {
            assert!(
                s.abs() < 1000,
                "alternating DFF pattern should be near-silence, got {s}"
            );
        }
    }

    #[test]
    fn choose_output_rate_dsd64() {
        assert_eq!(choose_output_rate(2_822_400), 176_400);
    }

    #[test]
    fn choose_output_rate_dsd128() {
        assert_eq!(choose_output_rate(5_644_800), 352_800);
    }

    #[test]
    fn choose_output_rate_dsd256() {
        assert_eq!(choose_output_rate(11_289_600), 352_800);
    }

    #[test]
    fn choose_output_rate_dsd512() {
        assert_eq!(choose_output_rate(22_579_200), 352_800);
    }

    #[test]
    fn empty_input_produces_empty_output() {
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, 2, true);
        let pcm = converter.process(&[]);
        assert!(pcm.is_empty());
    }

    #[test]
    fn stereo_channel_count_preserved() {
        let channels = 2;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        // 256 bytes per channel, interleaved
        let total_bytes = 256 * channels;
        let dsd_data = vec![0x55u8; total_bytes];
        let pcm = converter.process_to_i16(&dsd_data);

        // Output sample count should be divisible by channel count
        assert_eq!(
            pcm.len() % channels,
            0,
            "output samples should be a multiple of channel count"
        );
    }

    #[test]
    fn output_depth_is_24() {
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, 2, true);
        assert_eq!(converter.output_depth, 24);
    }

    #[test]
    fn process_24bit_output_format() {
        let channels = 1;
        let converter = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);

        let dsd_data = vec![0x55u8; 512];
        let pcm_24 = converter.process(&dsd_data);

        // Each sample is 3 bytes (24-bit)
        assert_eq!(
            pcm_24.len() % 3,
            0,
            "24-bit output should be a multiple of 3 bytes"
        );
    }

    // --- DsdToPcmStreamer tests ---

    #[test]
    fn streamer_produces_output() {
        let channels = 2;
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);

        let total_bytes = 2048 * channels;
        let dsd_data: Vec<u8> = (0..total_bytes).map(|_| 0x55u8).collect();

        let pcm = streamer.feed(&dsd_data);
        let flush = streamer.flush();

        let total_pcm = pcm.len() + flush.len();
        assert!(total_pcm > 0, "streamer should produce PCM output");
        assert_eq!(total_pcm % 3, 0, "output should be 24-bit (3 bytes/sample)");
        assert_eq!(
            (total_pcm / 3) % channels,
            0,
            "output should have correct channel count"
        );
    }

    #[test]
    fn streamer_silence_pattern() {
        // Alternating 0x55 pattern = near-silence
        let channels = 1;
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);

        let dsd_data = vec![0x55u8; 4096];
        let pcm_24 = streamer.feed(&dsd_data);
        let flush = streamer.flush();

        let mut all_pcm = pcm_24;
        all_pcm.extend_from_slice(&flush);

        // Convert to i16 for easy checking
        let num_samples = all_pcm.len() / 3;
        assert!(num_samples > 10, "should produce at least 10 samples");

        let mut max_abs: i32 = 0;
        for i in num_samples / 4..3 * num_samples / 4 {
            let offset = i * 3;
            let lo = all_pcm[offset] as u32;
            let mid = all_pcm[offset + 1] as u32;
            let hi = all_pcm[offset + 2] as u32;
            let val24 = lo | (mid << 8) | (hi << 16);
            let val32 = if val24 & 0x80_0000 != 0 {
                (val24 | 0xFF00_0000) as i32
            } else {
                val24 as i32
            };
            let val16 = val32 >> 8;
            if val16.abs() > max_abs {
                max_abs = val16.abs();
            }
        }
        assert!(
            max_abs < 1000,
            "alternating DSD pattern should be near-silence, max abs = {max_abs}"
        );
    }

    #[test]
    fn streamer_chunked_matches_single_feed() {
        // Feeding data in multiple small chunks should produce the same output
        // as feeding it all at once.
        let channels = 2;
        let total_bytes = 1024 * channels;
        let dsd_data: Vec<u8> = (0..total_bytes).map(|i| (i % 256) as u8).collect();

        // Single feed
        let mut streamer1 = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let pcm1 = streamer1.feed(&dsd_data);
        let flush1 = streamer1.flush();
        let mut all1 = pcm1;
        all1.extend_from_slice(&flush1);

        // Chunked feed (feed in small chunks)
        let mut streamer2 = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let chunk_size = 64 * channels; // 64 bytes per channel per chunk
        let mut all2 = Vec::new();
        for chunk in dsd_data.chunks(chunk_size) {
            all2.extend_from_slice(&streamer2.feed(chunk));
        }
        all2.extend_from_slice(&streamer2.flush());

        assert_eq!(
            all1.len(),
            all2.len(),
            "single feed and chunked feed should produce same length"
        );
        assert_eq!(
            all1, all2,
            "single feed and chunked feed should produce identical output"
        );
    }

    #[test]
    fn streamer_dsd128() {
        let channels = 2;
        let mut streamer = DsdToPcmStreamer::new(5_644_800, 352_800, channels, true);
        assert_eq!(streamer.output_rate, 352_800);

        let dsd_data = vec![0xAAu8; 4096 * channels];
        let pcm = streamer.feed(&dsd_data);
        let flush = streamer.flush();
        let total = pcm.len() + flush.len();
        assert!(total > 0, "DSD128 streaming should produce output");
    }

    #[test]
    fn streamer_dff_msb_first() {
        let channels = 1;
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, false);

        let dsd_data = vec![0xAAu8; 4096];
        let pcm = streamer.feed(&dsd_data);
        let flush = streamer.flush();
        let total = pcm.len() + flush.len();
        assert!(total > 0, "DFF MSB-first streaming should produce output");
    }

    #[test]
    fn streamer_empty_input() {
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, 2, true);
        let pcm = streamer.feed(&[]);
        assert!(pcm.is_empty());
        let flush = streamer.flush();
        assert!(flush.is_empty());
    }

    #[test]
    fn streamer_matches_batch_stereo() {
        // The streaming converter must produce identical output to the batch
        // converter for the same input data.  This catches bugs in the ring
        // buffer indexing (e.g. overwriting channel 0's samples because
        // ring_pos didn't advance between bits within a byte).
        let channels = 2;
        let total_bytes = 512 * channels;
        // Use a deterministic non-trivial pattern (not silence or DC)
        let dsd_data: Vec<u8> = (0..total_bytes)
            .map(|i| ((i * 37 + 13) % 256) as u8)
            .collect();

        // Batch converter
        let batch = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let batch_pcm = batch.process(&dsd_data);

        // Streaming converter (single feed)
        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let stream_pcm = streamer.feed(&dsd_data);
        let stream_flush = streamer.flush();
        let mut all_stream = stream_pcm;
        all_stream.extend_from_slice(&stream_flush);

        // Both should produce the same number of samples
        assert_eq!(
            batch_pcm.len(),
            all_stream.len(),
            "batch and streaming should produce same byte count"
        );

        // Compare sample by sample (allow tiny rounding differences from f64 arithmetic)
        let num_samples = batch_pcm.len() / 3;
        let mut max_diff: i32 = 0;
        for i in 0..num_samples {
            let off = i * 3;
            let batch_val = {
                let lo = batch_pcm[off] as u32;
                let mid = batch_pcm[off + 1] as u32;
                let hi = batch_pcm[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let stream_val = {
                let lo = all_stream[off] as u32;
                let mid = all_stream[off + 1] as u32;
                let hi = all_stream[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let diff = (batch_val - stream_val).abs();
            if diff > max_diff {
                max_diff = diff;
            }
        }

        assert!(
            max_diff <= 1,
            "batch and streaming outputs should match (max sample diff = {max_diff})"
        );
    }

    #[test]
    fn streamer_fast_path_matches_batch_long() {
        // Longer input so most output samples are emitted in steady state and
        // exercise the contiguous two-slice fast path in `feed`. It must still
        // match the independent batch converter (which uses the plain indexed
        // algorithm), guarding the fast-path ring math.
        let channels = 2;
        let total_bytes = 4096 * channels; // 32768 DSD samples/ch -> ~2048 out/ch
        let dsd_data: Vec<u8> = (0..total_bytes)
            .map(|i| ((i * 101 + 7) % 256) as u8)
            .collect();

        let batch = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let batch_pcm = batch.process(&dsd_data);

        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let mut all_stream = streamer.feed(&dsd_data);
        all_stream.extend_from_slice(&streamer.flush());

        assert_eq!(batch_pcm.len(), all_stream.len());
        let num_samples = batch_pcm.len() / 3;
        assert!(
            num_samples > 2000,
            "should exercise many steady-state samples"
        );

        let read = |b: &[u8], off: usize| -> i32 {
            let v = b[off] as u32 | ((b[off + 1] as u32) << 8) | ((b[off + 2] as u32) << 16);
            if v & 0x80_0000 != 0 {
                (v | 0xFF00_0000) as i32
            } else {
                v as i32
            }
        };
        let mut max_diff = 0i32;
        for i in 0..num_samples {
            let off = i * 3;
            max_diff = max_diff.max((read(&batch_pcm, off) - read(&all_stream, off)).abs());
        }
        assert!(
            max_diff <= 1,
            "fast path must match batch (max diff = {max_diff})"
        );
    }

    #[test]
    fn streamer_matches_batch_mono() {
        // Mono should also match (mono was less affected by the original bug
        // since there's only one channel, but verify for completeness).
        let channels = 1;
        let total_bytes = 512;
        let dsd_data: Vec<u8> = (0..total_bytes)
            .map(|i| ((i * 37 + 13) % 256) as u8)
            .collect();

        let batch = DsdToPcmConverter::new(2_822_400, 176_400, channels, true);
        let batch_pcm = batch.process(&dsd_data);

        let mut streamer = DsdToPcmStreamer::new(2_822_400, 176_400, channels, true);
        let stream_pcm = streamer.feed(&dsd_data);
        let stream_flush = streamer.flush();
        let mut all_stream = stream_pcm;
        all_stream.extend_from_slice(&stream_flush);

        assert_eq!(batch_pcm.len(), all_stream.len());

        let num_samples = batch_pcm.len() / 3;
        let mut max_diff: i32 = 0;
        for i in 0..num_samples {
            let off = i * 3;
            let batch_val = {
                let lo = batch_pcm[off] as u32;
                let mid = batch_pcm[off + 1] as u32;
                let hi = batch_pcm[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let stream_val = {
                let lo = all_stream[off] as u32;
                let mid = all_stream[off + 1] as u32;
                let hi = all_stream[off + 2] as u32;
                let v = lo | (mid << 8) | (hi << 16);
                if v & 0x80_0000 != 0 {
                    (v | 0xFF00_0000) as i32
                } else {
                    v as i32
                }
            };
            let diff = (batch_val - stream_val).abs();
            if diff > max_diff {
                max_diff = diff;
            }
        }

        assert!(
            max_diff <= 1,
            "batch and streaming mono outputs should match (max sample diff = {max_diff})"
        );
    }

    // --- #4354 : le calcul par lots rend le MÊME PCM, au bit près ---

    /// Octets DSD pseudo-aléatoires déterministes (même LCG que le banc).
    fn dsd_lcg(octets: usize, graine: u32) -> Vec<u8> {
        let mut s = graine;
        (0..octets)
            .map(|_| {
                s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (s >> 24) as u8
            })
            .collect()
    }

    fn verifier_identique_a_la_reference(dsd_rate: u32, pcm_rate: u32, channels: usize, lsb: bool) {
        // Blocs alignés sur la trame : le convertisseur de référence n'a pas
        // de notion de bloc, un reliquat de trame n'aurait pas de sens ici.
        let data = dsd_lcg(6000 * channels, 0x5eed_0000 ^ dsd_rate ^ channels as u32);
        let reference = DsdToPcmConverter::new(dsd_rate, pcm_rate, channels, lsb).process(&data);

        let mut d_un_bloc = DsdToPcmStreamer::new(dsd_rate, pcm_rate, channels, lsb);
        let mut en_une_fois = d_un_bloc.feed(&data);
        en_une_fois.extend_from_slice(&d_un_bloc.flush());
        assert_eq!(
            en_une_fois, reference,
            "{dsd_rate}->{pcm_rate} {channels} canal(aux) lsb={lsb} : un seul bloc doit rendre la référence au bit près"
        );

        // Blocs irréguliers, mais chacun aligné sur la trame de `channels` octets.
        let mut st = DsdToPcmStreamer::new(dsd_rate, pcm_rate, channels, lsb);
        let mut sortie = Vec::new();
        let tailles = [1usize, 7, 4096, 3, 250, 2, 9000, 5];
        let (mut pos, mut i) = (0usize, 0usize);
        while pos < data.len() {
            let fin = (pos + tailles[i % tailles.len()] * channels).min(data.len());
            sortie.extend_from_slice(&st.feed(&data[pos..fin]));
            pos = fin;
            i += 1;
        }
        sortie.extend_from_slice(&st.flush());
        assert_eq!(
            sortie, reference,
            "{dsd_rate}->{pcm_rate} {channels} canal(aux) lsb={lsb} : le découpage en blocs ne doit rien changer"
        );
    }

    #[test]
    fn streamer_identique_au_bit_a_la_reference_dsd64() {
        verifier_identique_a_la_reference(2_822_400, 176_400, 2, true);
        verifier_identique_a_la_reference(2_822_400, 176_400, 1, false);
        verifier_identique_a_la_reference(2_822_400, 88_200, 2, true);
    }

    #[test]
    fn streamer_identique_au_bit_a_la_reference_dsd128() {
        verifier_identique_a_la_reference(5_644_800, 352_800, 2, true);
        verifier_identique_a_la_reference(5_644_800, 176_400, 2, false);
        verifier_identique_a_la_reference(5_644_800, 352_800, 6, true);
    }

    #[test]
    fn streamer_identique_au_bit_a_la_reference_dsd256_et_512() {
        verifier_identique_a_la_reference(11_289_600, 352_800, 2, true);
        verifier_identique_a_la_reference(22_579_200, 352_800, 2, false);
        verifier_identique_a_la_reference(11_289_600, 44_100, 2, true);
    }

    /// Empreinte SHA-256 du PCM rendu pour une seconde de DSD128 stéréo
    /// pseudo-aléatoire, calculée par le convertisseur d'AVANT #4354 (v0.9.157,
    /// calcul sortie par sortie). Toute modification numérique du convertisseur
    /// — ordre de sommation, fusion multiplication-addition, précision — la
    /// fait tomber : c'est le contrat « même PCM au bit près ».
    #[test]
    fn streamer_empreinte_figee_dsd128() {
        use sha2::{Digest, Sha256};
        let data = dsd_lcg(5_644_800 / 8 * 2, 0x1234_5678);
        let mut st = DsdToPcmStreamer::new(5_644_800, 352_800, 2, true);
        // Blocs de 8192 octets : un super-bloc DSF stéréo, comme en lecture.
        let mut pcm = Vec::new();
        for bloc in data.chunks(8192) {
            pcm.extend_from_slice(&st.feed(bloc));
        }
        pcm.extend_from_slice(&st.flush());
        assert_eq!(
            pcm.len(),
            352_800 * 2 * 3,
            "une seconde de 352,8 kHz stéréo 24 bits"
        );
        let empreinte: String = Sha256::digest(&pcm)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            empreinte,
            "33c0695ff1067c2d12c41e455de0e2c28d4ae1954055e6e9364672a4510436a0"
        );
    }
}
