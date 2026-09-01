//! Multichannel audio support: layout enum, device detection, downmix matrices.
//!
//! Ported from Python `tune_server/audio/formats.py` (feat/multichannel branch).
//! Supports up to 32 channels (Trinnov Altitude).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Channel layout enum
// ---------------------------------------------------------------------------

/// Standard channel layouts for audio content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    /// 1 channel
    Mono,
    /// 2 channels (L, R)
    Stereo,
    /// 6 channels (L, R, C, LFE, Ls, Rs)
    Surround51,
    /// 8 channels (L, R, C, LFE, Ls, Rs, Lb, Rb)
    Surround71,
    /// 10 channels (L, R, C, LFE, Ls, Rs, Lb, Rb, Ltf, Rtf) — 5.1.4
    Surround514,
    /// 12 channels (L, R, C, LFE, Ls, Rs, Lb, Rb, Ltf, Rtf, Ltr, Rtr) — 7.1.4 Atmos
    Surround714,
    /// 16 channels — 9.1.6 Auro-3D
    Surround916,
    /// 24 channels — 13.1.10 or custom immersive
    Immersive24,
    /// 32 channels — Trinnov Altitude / custom processor
    Immersive32,
}

impl ChannelLayout {
    /// Number of discrete channels in this layout.
    pub fn channel_count(self) -> u16 {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
            Self::Surround51 => 6,
            Self::Surround71 => 8,
            Self::Surround514 => 10,
            Self::Surround714 => 12,
            Self::Surround916 => 16,
            Self::Immersive24 => 24,
            Self::Immersive32 => 32,
        }
    }

    /// Infer layout from a raw channel count.
    pub fn from_channel_count(channels: u16) -> Self {
        match channels {
            0 | 1 => Self::Mono,
            2 => Self::Stereo,
            3..=6 => Self::Surround51,
            7 | 8 => Self::Surround71,
            9 | 10 => Self::Surround514,
            11 | 12 => Self::Surround714,
            13..=16 => Self::Surround916,
            17..=24 => Self::Immersive24,
            _ => Self::Immersive32,
        }
    }

    /// Human-readable badge string for UI display (e.g. "5.1", "7.1.4 Atmos").
    /// Returns `None` for mono/stereo (no badge needed).
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Self::Mono | Self::Stereo => None,
            Self::Surround51 => Some("5.1"),
            Self::Surround71 => Some("7.1"),
            Self::Surround514 => Some("5.1.4"),
            Self::Surround714 => Some("7.1.4 Atmos"),
            Self::Surround916 => Some("9.1.6 Auro-3D"),
            Self::Immersive24 => Some("Immersive 24ch"),
            Self::Immersive32 => Some("Immersive 32ch"),
        }
    }

    /// Returns true if this layout has more than 2 channels.
    pub fn is_multichannel(self) -> bool {
        self.channel_count() > 2
    }
}

/// Return a badge string for a given channel count.
/// Returns `None` for mono/stereo.
pub fn channel_badge(channels: u16) -> Option<&'static str> {
    ChannelLayout::from_channel_count(channels).badge()
}

// ---------------------------------------------------------------------------
// Device channel detection — heuristic from known brands
// ---------------------------------------------------------------------------

/// Known multichannel-capable device name/model patterns (case-insensitive)
/// with their maximum supported channel count.
const MULTICHANNEL_CAPABLE_PATTERNS: &[(&str, u16)] = &[
    ("trinnov", 32),
    ("altitude", 32),
    ("datasat", 32),
    ("storm audio", 24),
    ("stormaudio", 24),
    ("jbl synthesis", 16),
    ("lyngdorf", 16),
    ("marantz", 8),
    ("denon", 8),
    ("yamaha", 8),
    ("pioneer", 8),
    ("onkyo", 8),
    ("nad", 8),
    ("anthem", 8),
    ("arcam", 8),
    ("emotiva", 8),
    ("monoprice", 8),
    ("sonos arc", 6),
    ("sonos beam", 6),
    ("samsung", 6),
];

/// Heuristic: check device name/model against known multichannel-capable devices.
/// Returns the max channel count, or `None` if unknown.
pub fn detect_max_channels_from_device_name(name: &str) -> Option<u16> {
    let lower = name.to_lowercase();
    for &(pattern, channels) in MULTICHANNEL_CAPABLE_PATTERNS {
        if lower.contains(pattern) {
            return Some(channels);
        }
    }
    None
}

/// Detect max channel count by checking both device name and model strings.
pub fn detect_max_channels_from_device_info(name: &str, model: &str) -> Option<u16> {
    let combined = format!("{name} {model}").to_lowercase();
    for &(pattern, channels) in MULTICHANNEL_CAPABLE_PATTERNS {
        if combined.contains(pattern) {
            return Some(channels);
        }
    }
    None
}

/// Parse DLNA sink protocol entries for max channel count.
///
/// Protocol entries may contain `channels=N` (e.g.
/// `http-get:*:audio/flac:*;channels=6`). Returns the maximum found,
/// or 2 (stereo) if nothing is detected.
pub fn detect_max_channels_from_sink_protocols(sink_protocols: &[String]) -> u16 {
    let mut max_ch: u16 = 2;
    for entry in sink_protocols {
        let lower = entry.to_lowercase();
        // Look for "channels=N" anywhere in the protocol entry
        if let Some(pos) = lower.find("channels=") {
            let after = &lower[pos + 9..];
            let num_str: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u16>() {
                max_ch = max_ch.max(n);
            }
        }
    }
    max_ch
}

// ---------------------------------------------------------------------------
// Downmix matrix — ITU-R BS.775 coefficients with deterministic headroom
// ---------------------------------------------------------------------------

/// ITU-R BS.775 coefficient for center channel in stereo downmix.
const ITU_CENTER_COEFF: f32 = 0.707; // -3 dB (1/sqrt(2))

/// ITU-R BS.775 coefficient for surround channels in stereo downmix.
const ITU_SURROUND_COEFF: f32 = 0.707; // -3 dB

/// ITU-R BS.775 coefficient for surround channels in mono downmix.
const ITU_SURROUND_MONO_COEFF: f32 = 0.354; // -9 dB (0.5 * 0.707)

/// Build a downmix coefficient matrix for converting `source_ch` channels
/// to `target_ch` channels.
///
/// Returns `None` if no downmix is needed (source <= target).
///
/// The returned vector has `target_ch * source_ch` elements, laid out row-major:
/// `output[out_ch * source_ch + in_ch]` = coefficient to apply to input
/// channel `in_ch` when computing output channel `out_ch`.
///
/// Standard 5.1 channel order: FL, FR, FC, LFE, SL, SR
/// Standard 7.1 channel order: FL, FR, FC, LFE, SL, SR, BL, BR
///
/// Each output row is attenuated when the sum of its absolute coefficients
/// exceeds unity. Correlated full-scale inputs therefore remain in range
/// without a hard clip after the sum. Relative ITU coefficients are preserved;
/// the attenuation is the required mix headroom.
pub fn build_downmix_matrix(source_ch: u16, target_ch: u16) -> Option<Vec<f32>> {
    if source_ch <= target_ch {
        return None;
    }

    let src = source_ch as usize;
    let tgt = target_ch as usize;
    let mut matrix = vec![0.0f32; tgt * src];

    match (source_ch, target_ch) {
        // Stereo -> mono: preserve both sides instead of silently keeping L.
        (2, 1) => {
            matrix[0] = 0.5;
            matrix[1] = 0.5;
        }

        // 5.1 (6ch) -> stereo (2ch): ITU-R BS.775
        // L_out = FL + 0.707*FC + 0.707*SL
        // R_out = FR + 0.707*FC + 0.707*SR
        (6, 2) => {
            // Row 0 (left output): FL=1.0, FC=0.707, SL=0.707
            matrix[0] = 1.0; // FL
            matrix[2] = ITU_CENTER_COEFF; // FC
            matrix[4] = ITU_SURROUND_COEFF; // SL
            // Row 1 (right output): FR=1.0, FC=0.707, SR=0.707
            matrix[src + 1] = 1.0; // FR
            matrix[src + 2] = ITU_CENTER_COEFF; // FC
            matrix[src + 5] = ITU_SURROUND_COEFF; // SR
        }

        // 7.1 (8ch) -> stereo (2ch): extended ITU-R BS.775
        // L_out = FL + 0.707*FC + 0.707*SL + 0.707*BL
        // R_out = FR + 0.707*FC + 0.707*SR + 0.707*BR
        (8, 2) => {
            matrix[0] = 1.0; // FL
            matrix[2] = ITU_CENTER_COEFF; // FC
            matrix[4] = ITU_SURROUND_COEFF; // SL
            matrix[6] = ITU_SURROUND_COEFF; // BL
            matrix[src + 1] = 1.0; // FR
            matrix[src + 2] = ITU_CENTER_COEFF; // FC
            matrix[src + 5] = ITU_SURROUND_COEFF; // SR
            matrix[src + 7] = ITU_SURROUND_COEFF; // BR
        }

        // 5.1 / 7.1 -> mono. Other channel counts have no unambiguous layout
        // here and deliberately use the conservative mapping below.
        (6 | 8, 1) => {
            // Mono = 0.5*FL + 0.5*FR + 0.707*FC + 0.354*(surrounds)
            matrix[0] = 0.5; // FL
            matrix[1] = 0.5; // FR
            matrix[2] = ITU_CENTER_COEFF; // FC
            // LFE (index 3) intentionally excluded from mono downmix
            if src > 4 {
                matrix[4] = ITU_SURROUND_MONO_COEFF; // SL
            }
            if src > 5 {
                matrix[5] = ITU_SURROUND_MONO_COEFF; // SR
            }
            if src > 6 {
                matrix[6] = ITU_SURROUND_MONO_COEFF; // BL
            }
            if src > 7 {
                matrix[7] = ITU_SURROUND_MONO_COEFF; // BR
            }
        }

        // 7.1 (8ch) -> 5.1 (6ch): fold back channels into surrounds
        (8, 6) => {
            // Pass through FL, FR, FC, LFE, fold SL+BL -> SL, SR+BR -> SR
            for i in 0..6 {
                matrix[i * src + i] = 1.0; // identity for first 6
            }
            matrix[4 * src + 6] = ITU_SURROUND_COEFF; // BL -> SL
            matrix[5 * src + 7] = ITU_SURROUND_COEFF; // BR -> SR
        }

        // Conservative fallback for an unknown layout: preserve only channels
        // which have a destination at the same index. Never fold an unknown
        // channel into another one (notably LFE/surround into a front channel).
        _ => {
            for i in 0..tgt.min(src) {
                matrix[i * src + i] = 1.0;
            }
        }
    }

    // Worst-case headroom: every contributing input may be correlated and at
    // full scale. Keeping each absolute row sum <= 1 makes clipping impossible
    // for normalized PCM without changing the relative mix coefficients.
    for row in matrix.chunks_exact_mut(src) {
        let peak_gain: f32 = row.iter().map(|coefficient| coefficient.abs()).sum();
        if peak_gain > 1.0 {
            for coefficient in row {
                *coefficient /= peak_gain;
            }
        }
    }

    Some(matrix)
}

/// Replier un flux stéréo entrelacé en mono, **sur ses deux voies** — sur
/// place, sans allocation.
///
/// `M = (L + R) / 2`, puis `M` est réémis sur la voie gauche ET sur la voie
/// droite. Le nombre de canaux, la fréquence et la profondeur ne changent
/// pas : seul le contenu des échantillons change. C'est ce que demande #2362,
/// et c'est ce qui le distingue du bras `(2, 1)` de [`build_downmix_matrix`] :
/// celui-là produit UNE voie, et le serveur ne demande jamais une cible mono à
/// un DAC qui annonce deux canaux. Une seule enceinte câblée sur le canal
/// gauche n'entendrait donc jamais rien de ce qui est panné à droite.
///
/// ## Pourquoi `/2` et pas `L + R`
///
/// `|0,5·(L+R)| <= 0,5·(|L| + |R|) <= 1,0` pour du PCM normalisé : la somme
/// atténuée ne peut PAS écrêter, même sur deux voies corrélées à pleine
/// échelle. Une somme brute atteindrait `+6 dBFS` sur du contenu mono — le
/// piège nommé au point 5 de la section « Ce qui n'est PAS établi » de #2362.
/// C'est le même coefficient que le bras `(2, 1)`, pour la même raison.
///
/// ## Pourquoi la somme passe par `f64`
///
/// `0,5` est une puissance de deux, donc la multiplication est exacte ; la
/// seule erreur possible vient de l'addition. En accumulant en `f64` puis en
/// arrondissant une seule fois vers `f32`, le résultat est l'arrondi correct
/// de `(L+R)/2`. Le surcoût est de deux conversions par trame — négligeable
/// devant le reste de la chaîne, et le chemin temps réel n'alloue rien.
///
/// Une longueur impaire (trame incomplète en fin de tampon) laisse le dernier
/// échantillon intact : `chunks_exact_mut` l'ignore. C'est volontaire — la
/// trame suivante arrivera complète au prochain tampon.
#[inline]
pub fn fold_stereo_to_mono_in_place(samples: &mut [f32]) {
    for frame in samples.chunks_exact_mut(2) {
        let mono = ((f64::from(frame[0]) + f64::from(frame[1])) * 0.5) as f32;
        frame[0] = mono;
        frame[1] = mono;
    }
}

/// Adapt interleaved floating-point PCM to an exact channel count.
///
/// Mono to stereo is the only implicit duplication. Wider upmixes retain the
/// front channels and fill every absent destination with silence, so stereo
/// content is never invented in C, LFE or surrounds. Downmixes all use the
/// single matrix above.
pub fn adapt_channels_f32(
    samples: &[f32],
    source_ch: u16,
    target_ch: u16,
) -> Result<Vec<f32>, String> {
    validate_channel_adaptation(samples.len(), source_ch, target_ch)?;
    if source_ch == target_ch || samples.is_empty() {
        return Ok(samples.to_vec());
    }

    let src = source_ch as usize;
    let tgt = target_ch as usize;
    let frames = samples.len() / src;
    let mut output = Vec::with_capacity(frames.saturating_mul(tgt));

    if source_ch < target_ch {
        for frame in samples.chunks_exact(src) {
            if source_ch == 1 && target_ch >= 2 {
                output.push(frame[0]);
                output.push(frame[0]);
                output.extend(std::iter::repeat_n(0.0, tgt - 2));
            } else {
                output.extend_from_slice(frame);
                output.extend(std::iter::repeat_n(0.0, tgt - src));
            }
        }
        return Ok(output);
    }

    let matrix = build_downmix_matrix(source_ch, target_ch)
        .ok_or_else(|| format!("no downmix matrix for {source_ch} -> {target_ch} channels"))?;
    for frame in samples.chunks_exact(src) {
        for row in matrix.chunks_exact(src) {
            output.push(
                frame
                    .iter()
                    .zip(row)
                    .map(|(&sample, &coefficient)| sample * coefficient)
                    .sum(),
            );
        }
    }
    Ok(output)
}

fn validate_channel_adaptation(
    sample_count: usize,
    source_ch: u16,
    target_ch: u16,
) -> Result<(), String> {
    if source_ch == 0 || target_ch == 0 {
        return Err("channel count must be greater than zero".into());
    }
    if sample_count % source_ch as usize != 0 {
        return Err(format!(
            "PCM sample count {sample_count} is not aligned to {source_ch} source channels"
        ));
    }
    Ok(())
}

/// Adapt interleaved, right-justified integer PCM to an exact channel count.
///
/// Downmixes use [`build_downmix_matrix`]. Upmixes follow the same safe policy
/// as [`adapt_channels_f32`]: mono is duplicated to the front pair, while all
/// other absent outputs are silent. The output always contains the same number
/// of frames as the input.
pub fn adapt_channels_i32(
    samples: &[i32],
    source_ch: u16,
    target_ch: u16,
    bit_depth: u16,
) -> Result<Vec<i32>, String> {
    validate_channel_adaptation(samples.len(), source_ch, target_ch)?;
    if source_ch == target_ch || samples.is_empty() {
        return Ok(samples.to_vec());
    }

    let src = source_ch as usize;
    let tgt = target_ch as usize;
    let frames = samples.len() / src;
    let mut output = Vec::with_capacity(frames.saturating_mul(tgt));

    if source_ch < target_ch {
        for frame in samples.chunks_exact(src) {
            if source_ch == 1 && target_ch >= 2 {
                output.push(frame[0]);
                output.push(frame[0]);
                output.extend(std::iter::repeat_n(0, tgt - 2));
            } else {
                output.extend_from_slice(frame);
                output.extend(std::iter::repeat_n(0, tgt - src));
            }
        }
        return Ok(output);
    }

    let matrix = build_downmix_matrix(source_ch, target_ch)
        .ok_or_else(|| format!("no downmix matrix for {source_ch} -> {target_ch} channels"))?;
    let depth = bit_depth.clamp(8, 32);
    let min = -(1i64 << (depth - 1));
    let max = (1i64 << (depth - 1)) - 1;
    for frame in samples.chunks_exact(src) {
        for out_ch in 0..tgt {
            let row = out_ch * src;
            let sum = frame
                .iter()
                .enumerate()
                .map(|(in_ch, &sample)| sample as f64 * matrix[row + in_ch] as f64)
                .sum::<f64>();
            output.push(sum.round().clamp(min as f64, max as f64) as i32);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2362 — la sommation est JUSTE sur des échantillons connus, et elle
    /// arrive sur les DEUX voies.
    ///
    /// Mutation discriminante : garder la voie gauche (`frame[1] = frame[0]`
    /// sans somme) rendrait ce test rouge sur la deuxième trame, où la voie
    /// droite est la seule à porter du signal — exactement le cas de Nicolas
    /// Tardif, dont l'unique enceinte est câblée à gauche.
    #[test]
    fn sommation_mono_juste_sur_les_deux_voies() {
        //                     L     R  |  L     R  |   L     R
        let mut pcm = vec![0.5, 0.3, 0.0, 0.8, -0.4, 0.4];
        fold_stereo_to_mono_in_place(&mut pcm);
        assert_eq!(pcm, vec![0.4, 0.4, 0.4, 0.4, 0.0, 0.0]);
    }

    /// Deux voies à pleine échelle, corrélées : le pire cas d'écrêtage. La
    /// sortie reste EXACTEMENT à pleine échelle, jamais au-delà.
    ///
    /// Mutation discriminante : une somme brute `L + R` rendrait `±2,0`.
    #[test]
    fn pleine_echelle_correlee_ne_secrete_pas() {
        let mut pcm = vec![1.0, 1.0, -1.0, -1.0, 1.0, -1.0];
        fold_stereo_to_mono_in_place(&mut pcm);
        assert_eq!(pcm, vec![1.0, 1.0, -1.0, -1.0, 0.0, 0.0]);
        assert!(
            pcm.iter().all(|s| s.abs() <= 1.0),
            "aucun échantillon ne doit sortir de [-1, 1] : {pcm:?}"
        );
    }

    /// Une trame incomplète en fin de tampon reste intacte plutôt que d'être
    /// sommée avec un voisin qui n'existe pas.
    #[test]
    fn trame_incomplete_laissee_intacte() {
        let mut pcm = vec![0.5, 0.3, 0.9];
        fold_stereo_to_mono_in_place(&mut pcm);
        assert_eq!(pcm, vec![0.4, 0.4, 0.9]);
    }

    /// Le repli ne change ni le nombre d'échantillons ni, par conséquent, le
    /// nombre de canaux ou la cadence : seul leur CONTENU change. C'est ce qui
    /// permet de garder le contrat du DAC (deux canaux) intact.
    #[test]
    fn le_repli_ne_change_pas_le_nombre_dechantillons() {
        let mut pcm = vec![0.1f32; 1024];
        fold_stereo_to_mono_in_place(&mut pcm);
        assert_eq!(pcm.len(), 1024);
    }

    #[test]
    fn channel_layout_counts() {
        assert_eq!(ChannelLayout::Mono.channel_count(), 1);
        assert_eq!(ChannelLayout::Stereo.channel_count(), 2);
        assert_eq!(ChannelLayout::Surround51.channel_count(), 6);
        assert_eq!(ChannelLayout::Surround71.channel_count(), 8);
        assert_eq!(ChannelLayout::Surround514.channel_count(), 10);
        assert_eq!(ChannelLayout::Surround714.channel_count(), 12);
        assert_eq!(ChannelLayout::Surround916.channel_count(), 16);
    }

    #[test]
    fn channel_layout_from_count() {
        assert_eq!(ChannelLayout::from_channel_count(1), ChannelLayout::Mono);
        assert_eq!(ChannelLayout::from_channel_count(2), ChannelLayout::Stereo);
        assert_eq!(
            ChannelLayout::from_channel_count(6),
            ChannelLayout::Surround51
        );
        assert_eq!(
            ChannelLayout::from_channel_count(8),
            ChannelLayout::Surround71
        );
        assert_eq!(
            ChannelLayout::from_channel_count(12),
            ChannelLayout::Surround714
        );
    }

    #[test]
    fn channel_badges() {
        assert_eq!(channel_badge(1), None);
        assert_eq!(channel_badge(2), None);
        assert_eq!(channel_badge(6), Some("5.1"));
        assert_eq!(channel_badge(8), Some("7.1"));
        assert_eq!(channel_badge(12), Some("7.1.4 Atmos"));
        assert_eq!(channel_badge(16), Some("9.1.6 Auro-3D"));
    }

    #[test]
    fn detect_from_device_name() {
        assert_eq!(
            detect_max_channels_from_device_name("Marantz SR7009"),
            Some(8)
        );
        assert_eq!(
            detect_max_channels_from_device_name("Denon AVR-X3700H"),
            Some(8)
        );
        assert_eq!(detect_max_channels_from_device_name("Sonos Arc"), Some(6));
        assert_eq!(detect_max_channels_from_device_name("Unknown Device"), None);
    }

    #[test]
    fn detect_from_device_info() {
        assert_eq!(
            detect_max_channels_from_device_info("Living Room", "Yamaha RX-A2080"),
            Some(8)
        );
        assert_eq!(
            detect_max_channels_from_device_info("MyDevice", "CustomModel"),
            None
        );
    }

    #[test]
    fn detect_from_sink_protocols() {
        let protos = vec![
            "http-get:*:audio/flac:*".to_string(),
            "http-get:*:audio/wav:*;channels=6".to_string(),
        ];
        assert_eq!(detect_max_channels_from_sink_protocols(&protos), 6);

        let protos_8 = vec![
            "http-get:*:audio/flac:*;channels=8".to_string(),
            "http-get:*:audio/wav:*;channels=2".to_string(),
        ];
        assert_eq!(detect_max_channels_from_sink_protocols(&protos_8), 8);

        let protos_none = vec!["http-get:*:audio/flac:*".to_string()];
        assert_eq!(detect_max_channels_from_sink_protocols(&protos_none), 2);
    }

    #[test]
    fn no_downmix_when_not_needed() {
        assert!(build_downmix_matrix(2, 2).is_none());
        assert!(build_downmix_matrix(2, 6).is_none());
        assert!(build_downmix_matrix(1, 2).is_none());
    }

    #[test]
    fn downmix_51_to_stereo() {
        let matrix = build_downmix_matrix(6, 2).unwrap();
        assert_eq!(matrix.len(), 12); // 2 * 6
        let headroom = 1.0 / (1.0 + 2.0 * 0.707);
        assert!((matrix[0] - headroom).abs() < 0.001);
        assert!((matrix[2] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[4] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[7] - headroom).abs() < 0.001);
        assert!((matrix[8] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[11] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[..6].iter().sum::<f32>() - 1.0).abs() < 0.001);
        assert!((matrix[6..].iter().sum::<f32>() - 1.0).abs() < 0.001);
    }

    #[test]
    fn downmix_71_to_stereo() {
        let matrix = build_downmix_matrix(8, 2).unwrap();
        assert_eq!(matrix.len(), 16); // 2 * 8
        let headroom = 1.0 / (1.0 + 3.0 * 0.707);
        assert!((matrix[0] - headroom).abs() < 0.001);
        assert!((matrix[2] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[4] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[6] - 0.707 * headroom).abs() < 0.001);
        assert!((matrix[..8].iter().sum::<f32>() - 1.0).abs() < 0.001);
    }

    #[test]
    fn downmix_71_to_51() {
        let matrix = build_downmix_matrix(8, 6).unwrap();
        assert_eq!(matrix.len(), 48); // 6 * 8
        // First 4 channels pass through (FL, FR, FC, LFE)
        assert!((matrix[0] - 1.0).abs() < 0.001); // FL->FL
        assert!((matrix[8 + 1] - 1.0).abs() < 0.001); // FR->FR
        assert!((matrix[16 + 2] - 1.0).abs() < 0.001); // FC->FC
        assert!((matrix[24 + 3] - 1.0).abs() < 0.001); // LFE->LFE
        // BL output: BL=1.0 + SL*0.707, with worst-case headroom.
        let headroom = 1.0 / 1.707;
        assert!((matrix[32 + 4] - headroom).abs() < 0.001); // BL->BL
        assert!((matrix[32 + 6] - 0.707 * headroom).abs() < 0.001); // SL->BL
    }

    #[test]
    fn downmix_51_to_mono() {
        let matrix = build_downmix_matrix(6, 1).unwrap();
        assert_eq!(matrix.len(), 6); // 1 * 6
        let headroom = 1.0 / (0.5 + 0.5 + 0.707 + 0.354 + 0.354);
        assert!((matrix[0] - 0.5 * headroom).abs() < 0.001); // FL
        assert!((matrix[1] - 0.5 * headroom).abs() < 0.001); // FR
        assert!((matrix[2] - 0.707 * headroom).abs() < 0.001); // FC
        assert!((matrix[3]).abs() < 0.001); // LFE excluded
        assert!((matrix[4] - 0.354 * headroom).abs() < 0.001); // BL
        assert!((matrix[5] - 0.354 * headroom).abs() < 0.001); // BR
    }

    #[test]
    fn is_multichannel() {
        assert!(!ChannelLayout::Mono.is_multichannel());
        assert!(!ChannelLayout::Stereo.is_multichannel());
        assert!(ChannelLayout::Surround51.is_multichannel());
        assert!(ChannelLayout::Surround71.is_multichannel());
        assert!(ChannelLayout::Surround714.is_multichannel());
    }

    #[test]
    fn generic_fallback_downmix() {
        // 10ch -> 2ch should use generic fallback (pass through first 2)
        let matrix = build_downmix_matrix(10, 2).unwrap();
        assert_eq!(matrix.len(), 20); // 2 * 10
        assert!((matrix[0] - 1.0).abs() < 0.001); // ch0 -> out0
        assert!((matrix[11] - 1.0).abs() < 0.001); // ch1 -> out1
    }

    #[test]
    fn stereo_upmix_ne_cree_ni_centre_ni_lfe_ni_surrounds() {
        let frame = adapt_channels_f32(&[0.25, -0.5], 2, 8).unwrap();
        assert_eq!(frame, vec![0.25, -0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn mono_upmix_ne_remplit_que_la_paire_frontale() {
        let frame = adapt_channels_f32(&[0.25], 1, 6).unwrap();
        assert_eq!(frame, vec![0.25, 0.25, 0.0, 0.0, 0.0, 0.0]);
    }

    // Garde reprise de #2577. Le correctif qu'elle portait est deja sur la
    // ligne — `build_downmix_matrix(2, 1)` somme les deux voies a 0,5 — mais
    // RIEN n'empechait la troncature de revenir :
    // `stereo_vers_mono_melange_les_deux_cotes` teste un signal present des
    // DEUX cotes, donc il resterait vert si l'on ne gardait que la gauche a
    // pleine echelle. C'est exactement le trou qui avait laisse passer le
    // defaut d'origine, verrouille pendant des mois par un test vert.

    #[test]
    fn mono_ne_perd_pas_ce_qui_est_panne_a_droite() {
        // Le cas qui motive le reglage (#2362) : une seule enceinte, cablee a
        // gauche. Si le mono jette la voie droite, tout ce qui n'existe qu'a
        // droite disparait — c'est-a-dire precisement ce qu'on voulait
        // recuperer. Un signal purement droit doit donc sortir NON NUL.
        let purement_a_droite = adapt_channels_f32(&[0.0, 0.8, 0.0, -0.6], 2, 1).unwrap();
        assert_eq!(purement_a_droite.len(), 2);
        assert!(
            purement_a_droite[0].abs() > 0.01 && purement_a_droite[1].abs() > 0.01,
            "un signal panne uniquement a droite est sorti a zero : {purement_a_droite:?}"
        );
        assert!((purement_a_droite[0] - 0.4).abs() < 1e-6);
        assert!((purement_a_droite[1] + 0.3).abs() < 1e-6);

        // Symetrique : la gauche seule ne doit pas ressortir a pleine echelle,
        // ce qui trahirait une simple troncature.
        let purement_a_gauche = adapt_channels_f32(&[0.8, 0.0], 2, 1).unwrap();
        assert!(
            (purement_a_gauche[0] - 0.4).abs() < 1e-6,
            "la voie gauche ressort a pleine echelle — c'est une troncature, pas un melange : {purement_a_gauche:?}"
        );
    }

    #[test]
    fn downmix_pleine_echelle_reste_dans_la_plage_sans_ecretage() {
        for channels in [6, 8] {
            let input = vec![1.0; channels];
            let output = adapt_channels_f32(&input, channels as u16, 2).unwrap();
            assert_eq!(output.len(), 2);
            assert!(
                output
                    .iter()
                    .all(|sample| *sample <= 1.0 && *sample > 0.999)
            );
        }
    }

    #[test]
    fn impulsions_51_restent_dans_leur_cote() {
        let left = adapt_channels_f32(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0], 6, 2).unwrap();
        let right = adapt_channels_f32(&[0.0, 1.0, 0.0, 0.0, 0.0, 0.0], 6, 2).unwrap();
        assert!(left[0] > 0.0 && left[1] == 0.0);
        assert!(right[1] > 0.0 && right[0] == 0.0);
    }
}

/// Ramener un flux entrelacé à la stéréo, en entiers.
///
/// Créé pour la frontière AirPlay, qui négocie un payload RTP L16 **fixe** —
/// 44,1 kHz, 16 bits, stéréo. Il reste une garde spécialisée en plus du contrat
/// commun de `decode_to_pcm` (#2230, #2237).
///
/// Toutes les conversions passent par [`adapt_channels_i32`] : AirPlay, le
/// décodeur et la sortie locale partagent donc réellement la même matrice et la
/// même réserve de headroom.
pub fn to_stereo_i32(samples: &[i32], from_ch: u16) -> Vec<i32> {
    if from_ch == 2 || from_ch == 0 || samples.is_empty() {
        return samples.to_vec();
    }
    adapt_channels_i32(samples, from_ch, 2, 32).unwrap_or_else(|_| samples.to_vec())
}

#[cfg(test)]
mod stereo_i32_tests {
    use super::{adapt_channels_i32, to_stereo_i32};

    #[test]
    fn la_stereo_passe_telle_quelle() {
        let s = [1, -1, 2, -2];
        assert_eq!(to_stereo_i32(&s, 2), s.to_vec());
    }

    #[test]
    fn le_mono_se_dedouble_au_lieu_de_se_faire_passer_pour_de_la_stereo() {
        // La régression : un mono partait entrelacé comme du stéréo, donc une
        // trame sur deux atterrissait dans la mauvaise oreille et la durée
        // doublait.
        assert_eq!(
            to_stereo_i32(&[10, 20, 30], 1),
            vec![10, 10, 20, 20, 30, 30]
        );
    }

    #[test]
    fn le_51_est_replie_et_ne_deborde_pas() {
        // Une trame 5.1 à pleine échelle tient par la réserve de la matrice,
        // pas par un écrêtage après la somme.
        let plein = i32::MAX;
        let trame = [plein, plein, plein, 0, plein, plein];
        let out = to_stereo_i32(&trame, 6);
        assert_eq!(out.len(), 2);
        assert!(out[0] > i32::MAX - 4096);
        assert!(out[1] > i32::MAX - 4096);
    }

    #[test]
    fn un_51_modere_garde_les_proportions() {
        let trame = [1000, 2000, 0, 0, 0, 0];
        let out = to_stereo_i32(&trame, 6);
        assert!(out[0] > 0);
        assert!((out[1] - 2 * out[0]).abs() <= 1);
    }

    #[test]
    fn trois_a_cinq_canaux_gardent_la_paire_avant() {
        assert_eq!(to_stereo_i32(&[7, 8, 9], 3), vec![7, 8]);
    }

    #[test]
    fn stereo_vers_mono_melange_les_deux_cotes() {
        assert_eq!(
            adapt_channels_i32(&[10_000, -2_000, -4_000, 2_000], 2, 1, 16).unwrap(),
            vec![4_000, -1_000]
        );
    }

    #[test]
    fn mono_vers_multicanal_conserve_le_nombre_de_trames() {
        let out = adapt_channels_i32(&[1, 2], 1, 4, 16).unwrap();
        assert_eq!(out, vec![1, 1, 0, 0, 2, 2, 0, 0]);
        assert_eq!(out.len() / 4, 2);
    }

    #[test]
    fn adaptation_refuse_une_trame_incomplete() {
        assert!(adapt_channels_i32(&[1, 2, 3], 2, 1, 16).is_err());
    }
}
