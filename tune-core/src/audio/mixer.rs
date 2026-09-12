use super::channels::build_downmix_matrix;

/// Downmix interleaved PCM samples from `source_channels` to `target_channels`.
///
/// Uses ITU-R BS.775 coefficients for standard layouts (5.1->stereo, 7.1->stereo).
/// Returns the input unchanged if no downmix is needed (source <= target).
///
/// `samples` are interleaved f32 PCM: [L0, R0, C0, LFE0, BL0, BR0, L1, R1, ...].
pub fn downmix(samples: &[f32], source_channels: u16, target_channels: u16) -> Vec<f32> {
    if source_channels <= target_channels {
        return samples.to_vec();
    }

    let src = source_channels as usize;
    let tgt = target_channels as usize;

    let matrix = match build_downmix_matrix(source_channels, target_channels) {
        Some(m) => m,
        None => return samples.to_vec(),
    };

    let frame_count = samples.len() / src;
    let mut output = Vec::with_capacity(frame_count * tgt);

    for frame in 0..frame_count {
        let in_offset = frame * src;
        for out_ch in 0..tgt {
            let mut sum = 0.0f32;
            let row_offset = out_ch * src;
            for in_ch in 0..src {
                sum += samples[in_offset + in_ch] * matrix[row_offset + in_ch];
            }
            // The shared matrix already reserves worst-case headroom. Keep an
            // out-of-range source visible instead of hiding it behind a clip.
            output.push(sum);
        }
    }

    output
}

/// Downmix interleaved i16 PCM bytes from `source_channels` to `target_channels`.
///
/// Convenience wrapper that operates on raw i16 LE byte buffers.
pub fn downmix_i16_bytes(data: &[u8], source_channels: u16, target_channels: u16) -> Vec<u8> {
    if source_channels <= target_channels {
        return data.to_vec();
    }

    let src = source_channels as usize;
    let tgt = target_channels as usize;
    let bytes_per_sample = 2usize;
    let frame_size = src * bytes_per_sample;
    let frame_count = data.len() / frame_size;

    let matrix = match build_downmix_matrix(source_channels, target_channels) {
        Some(m) => m,
        None => return data.to_vec(),
    };

    let mut output = Vec::with_capacity(frame_count * tgt * bytes_per_sample);

    for frame in 0..frame_count {
        let base = frame * frame_size;
        for out_ch in 0..tgt {
            let mut sum = 0.0f64;
            let row_offset = out_ch * src;
            for in_ch in 0..src {
                let pos = base + in_ch * bytes_per_sample;
                if pos + 1 < data.len() {
                    let sample = i16::from_le_bytes([data[pos], data[pos + 1]]);
                    sum += sample as f64 * matrix[row_offset + in_ch] as f64;
                }
            }
            let clamped = sum.clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            output.extend_from_slice(&clamped.to_le_bytes());
        }
    }

    output
}

/// Pourquoi un mélange n'a pas eu lieu.
///
/// #2219, tranche R3 — jusqu'ici `PcmMixer::mix_buffers` répondait au bras `_`
/// en rendant le PREMIER tampon. En 32 bits, le second producteur était donc
/// **jeté en silence** : aucune erreur, aucune ligne de journal, un mélange qui
/// n'en est pas un. Le défaut est resté dormant parce que le mélangeur n'a
/// qu'un appelant hors essais — `playback::dj_player`, qui fixe `MIX_BIT_DEPTH`
/// à 16 — et il devient fatal au premier second producteur (tranche R4).
///
/// Une profondeur que le mélangeur ne sait pas additionner se refuse désormais
/// par ce motif nommé, au lieu de rendre une réponse fausse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MixError {
    /// Profondeur d'échantillon hors du contrat : 16, 24 ou 32 bits ENTIERS.
    ///
    /// Le PCM flottant n'est pas de ce nombre, et ce n'est pas un oubli : dans
    /// cette caisse il ne se porte JAMAIS par un `bit_depth: u16`. `downmix`
    /// ci-dessus prend des `&[f32]` ; `outputs::local` reconnaît le flottant au
    /// `WAVE_FORMAT_IEEE_FLOAT` de l'en-tête et `audio::convolver` à son
    /// `format_tag == 3`, jamais à une profondeur. `PcmMixer` ne porte aucun de
    /// ces drapeaux : lui faire dire « 32 » pour du f32 confondrait deux
    /// encodages de même largeur et rendrait du bruit. Tant qu'aucun appelant
    /// ne demande un mélange flottant, il se refuse ici.
    UnsupportedBitDepth(u16),
    /// Le tampon fourni par l'appelant ne peut pas contenir le mélange.
    OutputTooSmall { needed: usize, provided: usize },
}

impl std::fmt::Display for MixError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedBitDepth(bits) => write!(
                f,
                "profondeur non mélangeable : {bits} bits (attendu 16, 24 ou 32 entiers)"
            ),
            Self::OutputTooSmall { needed, provided } => write!(
                f,
                "tampon de sortie trop court : {needed} octets nécessaires, {provided} fournis"
            ),
        }
    }
}

impl std::error::Error for MixError {}

/// Encodage d'un échantillon PCM entier petit-boutien.
///
/// Aucun bras `_` ici : c'est `from_bit_depth` qui tranche une fois pour
/// toutes, et il RÉPOND par une erreur nommée. Le reste du chemin est
/// exhaustif, donc une quatrième profondeur ajoutée à l'énumération ne peut
/// plus se glisser sous un repli silencieux — le compilateur la réclame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SampleFormat {
    I16,
    I24,
    I32,
}

impl SampleFormat {
    fn from_bit_depth(bit_depth: u16) -> Result<Self, MixError> {
        match bit_depth {
            16 => Ok(Self::I16),
            24 => Ok(Self::I24),
            32 => Ok(Self::I32),
            other => Err(MixError::UnsupportedBitDepth(other)),
        }
    }

    const fn bytes(self) -> usize {
        match self {
            Self::I16 => 2,
            Self::I24 => 3,
            Self::I32 => 4,
        }
    }

    /// Bornes de saturation, dans l'ordre (plancher, plafond).
    const fn range(self) -> (f64, f64) {
        match self {
            Self::I16 => (i16::MIN as f64, i16::MAX as f64),
            Self::I24 => (-8_388_608.0, 8_388_607.0),
            Self::I32 => (i32::MIN as f64, i32::MAX as f64),
        }
    }

    /// Lit un échantillon. `bytes` fait AU MOINS `self.bytes()` octets.
    fn read(self, bytes: &[u8]) -> i32 {
        match self {
            Self::I16 => i16::from_le_bytes([bytes[0], bytes[1]]) as i32,
            Self::I24 => {
                let raw = ((bytes[2] as i32) << 16) | ((bytes[1] as i32) << 8) | (bytes[0] as i32);
                if raw & 0x0080_0000 != 0 {
                    raw | !0x00FF_FFFF
                } else {
                    raw
                }
            }
            Self::I32 => i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        }
    }

    /// Écrit un échantillon saturé. `out` fait AU MOINS `self.bytes()` octets.
    fn write(self, value: f64, out: &mut [u8]) {
        let (lo, hi) = self.range();
        let clamped = value.clamp(lo, hi);
        match self {
            Self::I16 => out[..2].copy_from_slice(&(clamped as i16).to_le_bytes()),
            Self::I24 => {
                let v = clamped as i32;
                out[0] = (v & 0xFF) as u8;
                out[1] = ((v >> 8) & 0xFF) as u8;
                out[2] = ((v >> 16) & 0xFF) as u8;
            }
            Self::I32 => out[..4].copy_from_slice(&(clamped as i32).to_le_bytes()),
        }
    }
}

pub struct PcmMixer {
    channels: u16,
    bit_depth: u16,
    sample_rate: u32,
}

impl PcmMixer {
    pub fn new(channels: u16, bit_depth: u16, sample_rate: u32) -> Self {
        Self {
            channels,
            bit_depth,
            sample_rate,
        }
    }

    /// Nombre d'octets que `mix_into` écrira pour ces tampons.
    ///
    /// C'est aussi la porte de la profondeur : elle refuse ici, une seule fois,
    /// ce que le reste du chemin ne saurait pas additionner.
    pub fn mixed_len(&self, buffers: &[&[u8]]) -> Result<usize, MixError> {
        let format = SampleFormat::from_bit_depth(self.bit_depth)?;
        let max_len = buffers.iter().map(|b| b.len()).max().unwrap_or(0);
        // Un échantillon incomplet en fin de tampon n'est pas interprétable :
        // on s'arrête à la dernière frontière entière, comme avant.
        Ok(max_len - (max_len % format.bytes()))
    }

    /// Mélange dans un tampon FOURNI par l'appelant, sans allouer.
    ///
    /// Rend le nombre d'octets écrits. Le rappel temps réel d'une sortie ne
    /// peut pas se permettre le `Vec<u8>` que `mix_buffers` construit à chaque
    /// appel : c'est cette variante-là qu'il doit appeler, avec un tampon
    /// dimensionné une fois par `mixed_len`.
    ///
    /// Gardé par `aucune_allocation_dans_mix_into`
    /// (`tune-core/tests/melangeur_multi_producteurs_r3.rs`).
    pub fn mix_into(
        &self,
        buffers: &[&[u8]],
        gains: &[f32],
        out: &mut [u8],
    ) -> Result<usize, MixError> {
        let format = SampleFormat::from_bit_depth(self.bit_depth)?;
        let needed = self.mixed_len(buffers)?;
        if out.len() < needed {
            return Err(MixError::OutputTooSmall {
                needed,
                provided: out.len(),
            });
        }

        let width = format.bytes();
        let sample_count = needed / width;
        for i in 0..sample_count {
            let pos = i * width;
            let mut sum = 0.0f64;
            for (buf_idx, buf) in buffers.iter().enumerate() {
                let gain = gains.get(buf_idx).copied().unwrap_or(1.0) as f64;
                if pos + width <= buf.len() {
                    sum += format.read(&buf[pos..pos + width]) as f64 * gain;
                }
            }
            format.write(sum, &mut out[pos..pos + width]);
        }

        Ok(needed)
    }

    /// Mélange en allouant le tampon de sortie.
    ///
    /// Confort pour les appels hors rappel temps réel. Le rappel, lui, appelle
    /// `mix_into`.
    pub fn mix_buffers(&self, buffers: &[&[u8]], gains: &[f32]) -> Result<Vec<u8>, MixError> {
        let needed = self.mixed_len(buffers)?;
        let mut output = vec![0u8; needed];
        let written = self.mix_into(buffers, gains, &mut output)?;
        debug_assert_eq!(written, needed);
        Ok(output)
    }

    /// Applique un gain en place.
    ///
    /// Rend une erreur nommée plutôt que de ne rien faire : un `_ => {}` ici
    /// laissait le tampon INCHANGÉ, ce qui se lit à l'oreille comme un gain
    /// ignoré et à la lecture du code comme un succès.
    pub fn apply_gain(data: &mut [u8], gain: f32, bit_depth: u16) -> Result<(), MixError> {
        let format = SampleFormat::from_bit_depth(bit_depth)?;
        let width = format.bytes();
        for chunk in data.chunks_exact_mut(width) {
            let value = format.read(chunk) as f64 * gain as f64;
            format.write(value, chunk);
        }
        Ok(())
    }

    pub fn silence(&self, duration_ms: u64) -> Vec<u8> {
        let sample_count = (self.sample_rate as u64 * self.channels as u64 * duration_ms) / 1000;
        let bytes_per_sample = (self.bit_depth / 8) as u64;
        vec![0u8; (sample_count * bytes_per_sample) as usize]
    }

    pub fn duration_ms(&self, data_len: usize) -> u64 {
        let bytes_per_sample = (self.bit_depth / 8) as u64;
        let total_samples = data_len as u64 / bytes_per_sample;
        let frames = total_samples / self.channels as u64;
        (frames * 1000) / self.sample_rate as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_two_16bit_buffers() {
        let mixer = PcmMixer::new(1, 16, 44100);
        let buf1: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        let buf2: Vec<u8> = 2000i16.to_le_bytes().to_vec();

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[1.0, 1.0]).unwrap();
        let result = i16::from_le_bytes([mixed[0], mixed[1]]);
        assert_eq!(result, 3000);
    }

    #[test]
    fn mix_with_gain() {
        let mixer = PcmMixer::new(1, 16, 44100);
        let buf1: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        let buf2: Vec<u8> = 1000i16.to_le_bytes().to_vec();

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[0.5, 0.5]).unwrap();
        let result = i16::from_le_bytes([mixed[0], mixed[1]]);
        assert_eq!(result, 1000);
    }

    #[test]
    fn clamp_on_overflow() {
        let mixer = PcmMixer::new(1, 16, 44100);
        let buf1: Vec<u8> = 30000i16.to_le_bytes().to_vec();
        let buf2: Vec<u8> = 30000i16.to_le_bytes().to_vec();

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[1.0, 1.0]).unwrap();
        let result = i16::from_le_bytes([mixed[0], mixed[1]]);
        assert_eq!(result, i16::MAX);
    }

    #[test]
    fn apply_gain_16bit() {
        let mut data: Vec<u8> = 1000i16.to_le_bytes().to_vec();
        PcmMixer::apply_gain(&mut data, 0.5, 16).unwrap();
        let result = i16::from_le_bytes([data[0], data[1]]);
        assert_eq!(result, 500);
    }

    /// Même famille que le bras `_` de `mix_buffers` : `apply_gain` ne faisait
    /// RIEN en 32 bits, ce qui se lit comme un succès.
    #[test]
    fn apply_gain_32bit() {
        let mut data: Vec<u8> = 1_000_000i32.to_le_bytes().to_vec();
        PcmMixer::apply_gain(&mut data, 0.5, 32).unwrap();
        let result = i32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        assert_eq!(result, 500_000, "le gain 32 bits ne doit pas être ignoré");
    }

    #[test]
    fn apply_gain_refuse_une_profondeur_hors_contrat() {
        let mut data = vec![1u8, 2, 3, 4];
        let avant = data.clone();
        assert_eq!(
            PcmMixer::apply_gain(&mut data, 0.5, 8),
            Err(MixError::UnsupportedBitDepth(8))
        );
        assert_eq!(data, avant, "un refus ne doit rien modifier");
    }

    #[test]
    fn silence_duration() {
        let mixer = PcmMixer::new(2, 16, 44100);
        let silence = mixer.silence(1000);
        assert_eq!(silence.len(), 44100 * 2 * 2);
        assert!(silence.iter().all(|&b| b == 0));
    }

    #[test]
    fn duration_calculation() {
        let mixer = PcmMixer::new(2, 16, 44100);
        let data_len = 44100 * 2 * 2;
        assert_eq!(mixer.duration_ms(data_len), 1000);
    }

    #[test]
    fn empty_mix() {
        let mixer = PcmMixer::new(2, 16, 44100);
        let result = mixer.mix_buffers(&[], &[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn mix_24bit_basic() {
        let mixer = PcmMixer::new(1, 24, 44100);
        let buf1 = vec![0x00u8, 0x10, 0x00]; // 4096
        let buf2 = vec![0x00u8, 0x10, 0x00]; // 4096

        let mixed = mixer.mix_buffers(&[&buf1, &buf2], &[1.0, 1.0]).unwrap();
        let val = (mixed[0] as i32) | ((mixed[1] as i32) << 8) | ((mixed[2] as i32) << 16);
        assert_eq!(val, 8192);
    }

    #[test]
    fn downmix_passthrough_when_not_needed() {
        let samples = vec![0.5f32, -0.3, 0.1, 0.2];
        let result = downmix(&samples, 2, 2);
        assert_eq!(result, samples);
    }

    #[test]
    fn downmix_passthrough_upmix() {
        let samples = vec![0.5f32, -0.3];
        let result = downmix(&samples, 1, 2);
        assert_eq!(result, samples);
    }

    #[test]
    fn downmix_51_to_stereo() {
        // One frame of 5.1: FL=0.5, FR=-0.5, FC=0.3, LFE=0.0, BL=0.1, BR=-0.1
        let samples = vec![0.5, -0.5, 0.3, 0.0, 0.1, -0.1];
        let result = downmix(&samples, 6, 2);
        assert_eq!(result.len(), 2);
        let headroom = 1.0 / (1.0 + 2.0 * 0.707);
        assert!((result[0] - 0.7828 * headroom).abs() < 0.01);
        assert!((result[1] - (-0.3586) * headroom).abs() < 0.01);
    }

    #[test]
    fn downmix_i16_bytes_passthrough() {
        let data: Vec<u8> = 1000i16
            .to_le_bytes()
            .iter()
            .chain(2000i16.to_le_bytes().iter())
            .copied()
            .collect();
        let result = downmix_i16_bytes(&data, 2, 2);
        assert_eq!(result, data);
    }

    #[test]
    fn downmix_i16_bytes_51_to_stereo() {
        // One frame: FL=10000, FR=-10000, FC=5000, LFE=0, BL=2000, BR=-2000
        let samples: Vec<i16> = vec![10000, -10000, 5000, 0, 2000, -2000];
        let mut data = Vec::new();
        for s in &samples {
            data.extend_from_slice(&s.to_le_bytes());
        }
        let result = downmix_i16_bytes(&data, 6, 2);
        assert_eq!(result.len(), 4); // 2 channels * 2 bytes
        let left = i16::from_le_bytes([result[0], result[1]]);
        let right = i16::from_le_bytes([result[2], result[3]]);
        let headroom = 1.0 / (1.0 + 2.0 * 0.707);
        assert!((left as f64 - 14949.0 * headroom).abs() < 10.0);
        assert!((right as f64 - (-7879.0) * headroom).abs() < 10.0);
    }
}
