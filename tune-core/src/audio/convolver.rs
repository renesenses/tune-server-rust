use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use std::collections::VecDeque;
use std::sync::Arc;

/// Réponse impulsionnelle en domaine temporel, indépendante d'un flux.
///
/// La configuration conserve volontairement les taps et leur cadence source :
/// une instance [`Convolver`] est un état de traitement lié à un nombre de
/// canaux et doit être reconstruite quand le format du flux change (#2210).
#[derive(Clone)]
pub struct ConvolverConfig {
    impulse_response: Arc<[Vec<f32>]>,
    sample_rate: u32,
}

impl ConvolverConfig {
    pub fn new(impulse_response: Vec<Vec<f32>>, sample_rate: u32) -> Result<Self, String> {
        if sample_rate == 0 {
            return Err("la cadence de la réponse impulsionnelle doit être positive".into());
        }
        if impulse_response.is_empty() || impulse_response.iter().any(Vec::is_empty) {
            return Err("la réponse impulsionnelle doit contenir des taps sur chaque canal".into());
        }
        Ok(Self {
            impulse_response: impulse_response.into(),
            sample_rate,
        })
    }

    pub fn from_wav(path: &str) -> Result<Self, String> {
        let (impulse_response, sample_rate) = Convolver::read_wav_ir(path)?;
        Self::new(impulse_response, sample_rate)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn source_channels(&self) -> usize {
        self.impulse_response.len()
    }

    /// Bâtir le moteur pour le format réellement négocié.
    ///
    /// Aucun rééchantillonnage silencieux : l'utilisateur reçoit la cadence à
    /// laquelle réexporter son filtre. Une IR mono est explicitement dupliquée
    /// sur tous les canaux ; tout autre écart de layout est refusé.
    pub fn build_for(
        &self,
        block_size: usize,
        target_sample_rate: u32,
        target_channels: usize,
    ) -> Result<Convolver, String> {
        if target_channels == 0 {
            return Err("le flux ne contient aucun canal audio".into());
        }
        if self.sample_rate != target_sample_rate {
            return Err(format!(
                "Réponse impulsionnelle à {} Hz incompatible avec le flux à {target_sample_rate} Hz ; réexportez le filtre à {target_sample_rate} Hz",
                self.sample_rate
            ));
        }
        let adapted = if self.impulse_response.len() == target_channels {
            self.impulse_response.to_vec()
        } else if self.impulse_response.len() == 1 {
            vec![self.impulse_response[0].clone(); target_channels]
        } else {
            return Err(format!(
                "Réponse impulsionnelle à {} canaux incompatible avec le flux à {target_channels} canaux ; fournissez une IR mono ou une IR à {target_channels} canaux",
                self.impulse_response.len()
            ));
        };
        Ok(Convolver::new(&adapted, block_size))
    }
}

/// Partitioned overlap-save FFT convolver for real-time FIR filtering.
///
/// Processes audio in fixed-size blocks using FFT convolution.
/// Supports arbitrary-length impulse responses by partitioning them
/// into segments and accumulating the results.
pub struct Convolver {
    block_size: usize,
    fft_size: usize,
    channels: usize,
    /// FFT of each partition of the impulse response, per channel.
    ir_partitions: Vec<Vec<Vec<Complex<f32>>>>,
    /// Input buffer per channel (collects samples until block_size is reached).
    input_buf: Vec<Vec<f32>>,
    /// Frequency-domain delay line per channel (one slot per partition).
    fdl: Vec<Vec<Vec<Complex<f32>>>>,
    fdl_pos: usize,
    /// Overlap buffer per channel (tail from previous block).
    overlap: Vec<Vec<f32>>,
    /// File de sortie par canal, amorcée de `block_size` zéros.
    ///
    /// Une convolution par blocs ne peut pas rendre un échantillon avant
    /// d'avoir vu le bloc qui le contient : elle a une latence, et c'est
    /// `block_size`. L'ancien code la niait — il réécrivait le bloc traité
    /// À REBOURS dans le tampon d'entrée, ce qui suppose que le bloc s'y
    /// trouve en entier. Dès qu'un bloc partiel était reporté d'un appel au
    /// suivant, `frame + 1 - block_size` sous-débordait (#2209).
    ///
    /// Avec une file amorcée, chaque trame entrée rend une trame sortie, et le
    /// résultat ne dépend plus du découpage des appels.
    output_buf: Vec<VecDeque<f32>>,
    /// Tampon de travail d'un bloc, réutilisé — il était alloué à chaque bloc.
    scratch: Vec<Vec<f32>>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
}

impl Convolver {
    pub fn new(impulse_response: &[Vec<f32>], block_size: usize) -> Self {
        let channels = impulse_response.len();
        assert!(channels > 0, "IR must have at least one channel");
        let fft_size = (block_size * 2).next_power_of_two();
        let spectrum_len = fft_size / 2 + 1;

        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(fft_size);
        let inv = planner.plan_fft_inverse(fft_size);

        let mut ir_partitions = Vec::with_capacity(channels);
        let mut num_partitions = 0;

        for ch_ir in impulse_response {
            let n_parts = (ch_ir.len() + block_size - 1) / block_size;
            num_partitions = num_partitions.max(n_parts);
            let mut ch_parts = Vec::with_capacity(n_parts);

            for p in 0..n_parts {
                let start = p * block_size;
                let end = (start + block_size).min(ch_ir.len());
                let mut padded = vec![0.0f32; fft_size];
                padded[..end - start].copy_from_slice(&ch_ir[start..end]);
                let mut spectrum = fwd.make_output_vec();
                fwd.process(&mut padded, &mut spectrum).unwrap();
                ch_parts.push(spectrum);
            }
            ir_partitions.push(ch_parts);
        }

        let zero_spectrum = vec![Complex::new(0.0, 0.0); spectrum_len];
        let fdl: Vec<Vec<Vec<_>>> = (0..channels)
            .map(|_| vec![zero_spectrum.clone(); num_partitions])
            .collect();

        let input_buf = vec![Vec::with_capacity(block_size); channels];
        let overlap = vec![vec![0.0f32; fft_size - block_size]; channels];
        // Amorçage : la latence de la convolution, rendue explicite.
        let output_buf = vec![VecDeque::from(vec![0.0f32; block_size]); channels];
        let scratch = vec![vec![0.0f32; block_size]; channels];

        Self {
            block_size,
            fft_size,
            channels,
            ir_partitions,
            input_buf,
            fdl,
            fdl_pos: 0,
            overlap,
            output_buf,
            scratch,
            fwd,
            inv,
        }
    }

    /// Parse a WAV impulse response into per-channel f32 taps + its sample
    /// rate. Walks the RIFF chunks so extra chunks before `data` don't break it.
    fn read_wav_ir(path: &str) -> Result<(Vec<Vec<f32>>, u32), String> {
        let data = std::fs::read(path).map_err(|e| format!("read IR: {e}"))?;
        if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
            return Err("not a RIFF/WAVE file".into());
        }

        // Walk the RIFF chunks to locate `fmt ` and `data` at their REAL
        // offsets. Impulse WAVs exported by REW / rePhase / Dirac often carry
        // extra chunks (fact, PEAK, LIST/INFO, cue…) before `data`, so a fixed
        // 44-byte header offset misreads them into noise.
        let mut format_tag: u16 = 1; // 1 = PCM int, 3 = IEEE float
        let mut channels: usize = 0;
        let mut sample_rate: u32 = 0;
        let mut bits: usize = 0;
        let mut data_range: Option<(usize, usize)> = None;
        let mut pos = 12usize;
        while pos + 8 <= data.len() {
            let id = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
            let declared =
                u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                    as usize;
            let body = pos + 8;
            // Clamp to what's actually present so a truncated final chunk can't
            // index out of bounds.
            let size = declared.min(data.len() - body);
            match &id {
                b"fmt " if size >= 16 => {
                    format_tag = u16::from_le_bytes([data[body], data[body + 1]]);
                    channels = u16::from_le_bytes([data[body + 2], data[body + 3]]) as usize;
                    sample_rate = u32::from_le_bytes([
                        data[body + 4],
                        data[body + 5],
                        data[body + 6],
                        data[body + 7],
                    ]);
                    bits = u16::from_le_bytes([data[body + 14], data[body + 15]]) as usize;
                    // WAVE_FORMAT_EXTENSIBLE: the effective format tag lives in
                    // the first 2 bytes of the sub-format GUID.
                    if format_tag == 0xFFFE && size >= 40 {
                        format_tag = u16::from_le_bytes([data[body + 24], data[body + 25]]);
                    }
                }
                b"data" => data_range = Some((body, body + size)),
                _ => {}
            }
            // Chunks are word-aligned: an odd size carries a trailing pad byte.
            pos = body + size + (size & 1);
        }

        let (dstart, dend) = data_range.ok_or("missing data chunk")?;
        if channels == 0 {
            return Err("missing or invalid fmt chunk".into());
        }
        let bytes_per_sample = bits / 8;
        if bytes_per_sample == 0 {
            return Err(format!("unsupported bit depth: {bits}"));
        }
        let is_float = format_tag == 3;

        let total_samples = (dend - dstart) / bytes_per_sample;
        let samples_per_channel = total_samples / channels;
        if samples_per_channel == 0 {
            return Err("IR has no samples".into());
        }

        tracing::info!(
            path,
            channels,
            sample_rate,
            bits,
            is_float,
            samples_per_channel,
            "convolver_ir_loaded"
        );

        let mut ir = vec![Vec::with_capacity(samples_per_channel); channels];
        for i in 0..samples_per_channel {
            for ch in 0..channels {
                let o = dstart + (i * channels + ch) * bytes_per_sample;
                let sample = match (bits, is_float) {
                    (16, false) => i16::from_le_bytes([data[o], data[o + 1]]) as f32 / 32768.0,
                    (24, false) => {
                        // 24-bit LE placed in the top 3 bytes of an i32 for
                        // correct sign extension, then scaled by 2^31.
                        let v = i32::from_le_bytes([0, data[o], data[o + 1], data[o + 2]]);
                        v as f32 / 2147483648.0
                    }
                    (32, false) => {
                        let v =
                            i32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
                        v as f32 / 2147483648.0
                    }
                    (32, true) => {
                        f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
                    }
                    (64, true) => f64::from_le_bytes([
                        data[o],
                        data[o + 1],
                        data[o + 2],
                        data[o + 3],
                        data[o + 4],
                        data[o + 5],
                        data[o + 6],
                        data[o + 7],
                    ]) as f32,
                    _ => {
                        return Err(format!(
                            "unsupported WAV sample format: {bits}-bit {}",
                            if is_float { "float" } else { "int" }
                        ));
                    }
                };
                ir[ch].push(sample);
            }
        }

        Ok((ir, sample_rate))
    }

    /// Load an impulse response from a WAV file (any sample rate / channels).
    pub fn from_wav(path: &str, block_size: usize) -> Result<Self, String> {
        let config = ConvolverConfig::from_wav(path)?;
        Ok(Self::new(&config.impulse_response, block_size))
    }

    /// Read the raw taps of a WAV impulse response (per channel) plus its
    /// sample rate, WITHOUT building a convolver. The running [`Convolver`]
    /// only keeps the IR in FFT-partitioned form, so anything that needs the
    /// time-domain coefficients again (e.g. the `/convolver/response`
    /// visualisation endpoint) re-reads them from the persisted file.
    pub fn read_ir_taps(path: &str) -> Result<(Vec<Vec<f32>>, u32), String> {
        Self::read_wav_ir(path)
    }

    /// Combiner deux reponses impulsionnelles MONO — gauche et droite — en un
    /// seul WAV stereo, et rendre sa cadence.
    ///
    /// Le moteur sait deja convoluer un canal par reponse : `Convolver::new`
    /// prend un `&[Vec<f32>]`, et un WAV stereo donne bien deux corrections
    /// differentes. Ce qui manquait, c'est le CHEMIN D'ENTREE : les outils de
    /// correction de piece — REW, Acourate, Audiolense — exportent DEUX
    /// fichiers mono, `filter_L.wav` et `filter_R.wav`, jamais un stereo.
    /// L'utilisateur devait donc les fusionner lui-meme dans un editeur audio
    /// (Daniel, 24/08/2026).
    ///
    /// On ecrit le resultat au chemin que les consommateurs lisent DEJA, plutot
    /// que d'ajouter un second reglage : la sortie locale, le chemin de
    /// transcodage vers les renderers reseau et la visualisation
    /// `/convolver/response` continuent de ne connaitre qu'un fichier.
    ///
    /// Les deux reponses doivent partager leur cadence — convoluer a des
    /// cadences differentes decalerait un canal par rapport a l'autre. La plus
    /// courte est completee de zeros : c'est neutre pour une convolution.
    pub fn combiner_en_stereo(
        chemin_gauche: &str,
        chemin_droite: &str,
        chemin_sortie: &str,
    ) -> Result<u32, String> {
        let (ir_g, sr_g) = Self::read_wav_ir(chemin_gauche)?;
        let (ir_d, sr_d) = Self::read_wav_ir(chemin_droite)?;
        if sr_g != sr_d {
            return Err(format!(
                "les deux filtres doivent partager leur cadence : {sr_g} Hz a gauche, {sr_d} Hz a droite"
            ));
        }
        // Un fichier stereo passe aussi : on prend son canal correspondant, ce
        // qui evite un refus incomprehensible si l'outil a exporte deux stereo.
        let gauche = ir_g.first().ok_or("le filtre gauche est vide")?;
        let droite = ir_d
            .get(1)
            .or_else(|| ir_d.first())
            .ok_or("le filtre droit est vide")?;
        if gauche.is_empty() || droite.is_empty() {
            return Err("un des deux filtres ne contient aucun echantillon".into());
        }

        let n = gauche.len().max(droite.len());
        let mut entrelace = Vec::with_capacity(n * 2);
        for i in 0..n {
            entrelace.push(gauche.get(i).copied().unwrap_or(0.0));
            entrelace.push(droite.get(i).copied().unwrap_or(0.0));
        }
        ecrire_wav_float32(chemin_sortie, &entrelace, 2, sr_g)?;
        tracing::info!(
            gauche = chemin_gauche,
            droite = chemin_droite,
            sortie = chemin_sortie,
            sample_rate = sr_g,
            taps_gauche = gauche.len(),
            taps_droite = droite.len(),
            "convolver_ir_stereo_combinee"
        );
        Ok(sr_g)
    }

    /// Load an IR for a specific stream rate + channel count. Requires the IR's
    /// sample rate to match (resampling is a follow-up); a mono IR is duplicated
    /// to the stream's channel count. Used by the transcode path so the FIR can
    /// apply to network renderers, not just the local output.
    pub fn from_wav_for(
        path: &str,
        block_size: usize,
        target_sr: u32,
        target_channels: usize,
    ) -> Result<Self, String> {
        ConvolverConfig::from_wav(path)?.build_for(block_size, target_sr, target_channels)
    }

    /// Remettre le moteur à zéro : plus rien de la piste précédente.
    ///
    /// Le convolveur est installé une fois (`set_convolver_ir`) et vit aussi
    /// longtemps que la sortie. Sans cette remise à zéro, la file de sortie, la
    /// ligne à retard et l'overlap portent la queue de la piste d'avant : elle
    /// repart dans la suivante, et un seek ou un arrêt n'établit aucune
    /// frontière (JP Robbe, revue de #2268).
    pub fn reset(&mut self) {
        for c in 0..self.channels {
            self.input_buf[c].clear();
            self.output_buf[c].clear();
            // Ré-amorcer la latence : sans ça, la première trame de la piste
            // suivante sortirait du néant.
            self.output_buf[c].extend(std::iter::repeat_n(0.0f32, self.block_size));
            self.overlap[c].iter_mut().for_each(|v| *v = 0.0);
            for slot in self.fdl[c].iter_mut() {
                slot.iter_mut().for_each(|v| *v = Complex::new(0.0, 0.0));
            }
        }
        self.fdl_pos = 0;
    }

    /// Rendre ce que le moteur retient encore, en fin de piste.
    ///
    /// Une convolution par blocs garde `latency_frames()` trames en réserve —
    /// c'est le prix de sa latence. Sans ce drainage, ces trames ne partent
    /// jamais au périphérique : la fin de chaque piste était tronquée
    /// (JP Robbe, revue de #2268).
    ///
    /// Rend des échantillons ENTRELACÉS, prêts à suivre le même chemin que le
    /// reste. Le moteur est remis à zéro après : la piste est finie.
    pub fn flush(&mut self) -> Vec<f32> {
        let ch = self.channels;
        if ch == 0 {
            return Vec::new();
        }
        // Nourrir du silence pour pousser la queue hors du moteur, puis
        // recueillir exactement ce qui restait.
        let latence = self.latency_frames();
        let mut queue = vec![0.0f32; latence * ch];
        self.process_interleaved(&mut queue);
        self.reset();
        queue
    }

    /// Latence introduite par la convolution, en trames.
    ///
    /// Une convolution par blocs ne peut rien rendre avant d'avoir vu un bloc
    /// entier. La déclarer permet aux appelants qui ont besoin d'un alignement
    /// exact — `process_offline` — de la compenser.
    pub fn latency_frames(&self) -> usize {
        self.block_size
    }

    /// Process interleaved f32 samples in-place.
    ///
    /// Le résultat ne dépend PAS du découpage des appels : nourrir 1024 trames
    /// d'un coup, ou 100 puis 924, ou 1 par 1, rend exactement la même suite,
    /// décalée de `latency_frames()`.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        let ch = self.channels;
        if ch == 0 {
            return;
        }
        let frame_count = samples.len() / ch;

        for frame in 0..frame_count {
            for c in 0..ch {
                self.input_buf[c].push(samples[frame * ch + c]);
            }

            if self.input_buf[0].len() >= self.block_size {
                let mut sortie = std::mem::take(&mut self.scratch);
                self.process_block(&mut sortie);
                for c in 0..ch {
                    self.output_buf[c].extend(sortie[c].iter().copied());
                    self.input_buf[c].drain(..self.block_size);
                }
                self.scratch = sortie;
            }

            // Une trame entrée, une trame sortie. La file est amorcée, donc
            // elle n'est jamais vide — le `unwrap_or` ne couvre qu'un canal
            // dont l'IR aurait zéro partition.
            for c in 0..ch {
                samples[frame * ch + c] = self.output_buf[c].pop_front().unwrap_or(0.0);
            }
        }
    }

    fn process_block(&mut self, output: &mut [Vec<f32>]) {
        let spectrum_len = self.fft_size / 2 + 1;
        // La ligne à retard est dimensionnée sur le MAXIMUM de partitions parmi
        // les canaux. Prendre `ir_partitions[0].len()` ignorait la fin d'une IR
        // plus longue sur un autre canal — une IR stéréo aux deux canaux de
        // longueurs différentes perdait sa queue (#2209).
        let num_partitions = self.fdl[0].len();

        for ch in 0..self.channels {
            let mut padded = vec![0.0f32; self.fft_size];
            let n = self.input_buf[ch].len().min(self.block_size);
            padded[..n].copy_from_slice(&self.input_buf[ch][..n]);

            let mut input_spectrum = self.fwd.make_output_vec();
            self.fwd.process(&mut padded, &mut input_spectrum).unwrap();

            self.fdl[ch][self.fdl_pos] = input_spectrum;

            let mut acc = vec![Complex::new(0.0, 0.0); spectrum_len];
            for p in 0..num_partitions {
                let fdl_idx = (self.fdl_pos + num_partitions - p) % num_partitions;
                if p < self.ir_partitions[ch].len() {
                    for k in 0..spectrum_len {
                        acc[k] += self.fdl[ch][fdl_idx][k] * self.ir_partitions[ch][p][k];
                    }
                }
            }

            let mut time_out = self.inv.make_output_vec();
            self.inv.process(&mut acc, &mut time_out).unwrap();

            let scale = 1.0 / self.fft_size as f32;
            for i in 0..self.block_size {
                output[ch][i] = time_out[i] * scale + self.overlap[ch][i];
            }
            let overlap_len = self.fft_size - self.block_size;
            for i in 0..overlap_len {
                self.overlap[ch][i] = time_out[self.block_size + i] * scale;
            }
        }

        self.fdl_pos = (self.fdl_pos + 1) % num_partitions.max(1);
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Convolve a whole interleaved buffer offline (one-shot), in place and at
    /// the same length as the input. `process_interleaved` leaves the final
    /// partial (< block_size) frames unprocessed (they'd stay dry → an audible
    /// click at the buffer end); pad up to the next block boundary so that last
    /// block is flushed too, then truncate back. The IR decay *past* the buffer
    /// end is dropped (a few ms of tail) — acceptable for room correction.
    ///
    /// Use a FRESH Convolver per buffer (no carried-over state across tracks).
    pub fn process_offline(&mut self, samples: &mut Vec<f32>) {
        let ch = self.channels;
        if ch == 0 || samples.is_empty() {
            return;
        }
        let orig_len = samples.len();
        let frames = orig_len / ch;
        let rem = frames % self.block_size;
        if rem != 0 {
            let pad = (self.block_size - rem) * ch;
            samples.extend(std::iter::repeat(0.0).take(pad));
        }
        // Compenser la latence : `process_interleaved` rend la trame `i` à la
        // position `i + latency`. On nourrit donc `latency` trames de silence
        // en plus, et on jette autant de trames en tête.
        let latence = self.latency_frames();
        samples.extend(std::iter::repeat(0.0).take(latence * ch));
        self.process_interleaved(samples);
        samples.drain(..latence * ch);
        samples.truncate(orig_len);
    }

    /// Apply the convolver to interleaved integer PCM bytes in place, matching
    /// the layout the transcode pipeline hands to `EqProcessor::process_pcm`
    /// (little-endian, `bit_depth` of 16/24/32). Decodes to f32, convolves
    /// offline, and writes the samples back with soft clamping.
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16) {
        let ch = self.channels;
        if ch == 0 || pcm.is_empty() {
            return;
        }
        let bps = (bit_depth / 8) as usize;
        if bps == 0 {
            return;
        }
        let total = pcm.len() / bps;
        let mut buf: Vec<f32> = Vec::with_capacity(total);
        for i in 0..total {
            let o = i * bps;
            let s = match bit_depth {
                16 => i16::from_le_bytes([pcm[o], pcm[o + 1]]) as f32 / 32768.0,
                24 => {
                    let v = i32::from_le_bytes([0, pcm[o], pcm[o + 1], pcm[o + 2]]);
                    v as f32 / 2147483648.0
                }
                32 => {
                    let v = i32::from_le_bytes([pcm[o], pcm[o + 1], pcm[o + 2], pcm[o + 3]]);
                    v as f32 / 2147483648.0
                }
                _ => return,
            };
            buf.push(s);
        }
        self.process_offline(&mut buf);
        for i in 0..total {
            let o = i * bps;
            let s = buf.get(i).copied().unwrap_or(0.0).clamp(-1.0, 1.0);
            match bit_depth {
                16 => {
                    let v = (s * 32767.0).round() as i16;
                    pcm[o..o + 2].copy_from_slice(&v.to_le_bytes());
                }
                24 => {
                    let v = (s * 8_388_607.0).round() as i32;
                    let b = v.to_le_bytes();
                    pcm[o] = b[0];
                    pcm[o + 1] = b[1];
                    pcm[o + 2] = b[2];
                }
                32 => {
                    let v = (s * 2_147_483_647.0).round() as i32;
                    pcm[o..o + 4].copy_from_slice(&v.to_le_bytes());
                }
                _ => {}
            }
        }
    }
}

/// One point of a FIR frequency response, as served by the
/// `/zones/{id}/convolver/response` endpoint.
#[derive(Debug, Clone, Copy)]
pub struct ResponsePoint {
    pub freq_hz: f64,
    pub magnitude_db: f64,
    pub phase_deg: f64,
}

/// Log-spaced frequency grid (`n` points from `f_lo` to `f_hi` inclusive).
pub fn log_freq_grid(n: usize, f_lo: f64, f_hi: f64) -> Vec<f64> {
    if n == 0 || f_lo <= 0.0 || f_hi <= f_lo {
        return Vec::new();
    }
    if n == 1 {
        return vec![f_lo];
    }
    let ratio = (f_hi / f_lo).ln();
    (0..n)
        .map(|i| f_lo * (ratio * i as f64 / (n - 1) as f64).exp())
        .collect()
}

/// Evaluate the frequency response of a FIR filter at arbitrary frequencies by
/// DIRECT summation: H(f) = Σ h[n]·e^(−j2πfn/fs), accumulated in f64. No FFT —
/// the target grid is log-spaced, which an FFT bin grid can't serve without
/// interpolation, and ~200 freqs × 128k taps stays a few tens of millions of
/// f64 ops. The unit phasor is advanced by complex rotation (one sin/cos pair
/// per frequency); f64 rotation drift over 128k steps is ~1e-11, negligible.
///
/// Magnitude is 20·log10(|H|) floored at −200 dB (a true zero would be −inf,
/// which JSON can't carry); phase is the principal atan2 value in degrees.
/// Ecrire un WAV flottant 32 bits — le format que `read_wav_ir` relit sans
/// perte (`format_tag = 3`).
///
/// Volontairement minimal : un `fmt ` et un `data`, rien d'autre. Une reponse
/// impulsionnelle n'a que faire d'un `LIST/INFO`, et moins il y a de chunks,
/// moins il y a de facons de se tromper en les relisant.
fn ecrire_wav_float32(
    chemin: &str,
    entrelace: &[f32],
    canaux: u16,
    sample_rate: u32,
) -> Result<(), String> {
    let bits: u16 = 32;
    let bloc = canaux * bits / 8;
    let octets_par_seconde = sample_rate * bloc as u32;
    let taille_data = (entrelace.len() * 4) as u32;

    let mut w = Vec::with_capacity(44 + taille_data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + taille_data).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    w.extend_from_slice(&canaux.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&octets_par_seconde.to_le_bytes());
    w.extend_from_slice(&bloc.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&taille_data.to_le_bytes());
    for e in entrelace {
        w.extend_from_slice(&e.to_le_bytes());
    }
    std::fs::write(chemin, &w).map_err(|e| format!("ecriture du filtre combine : {e}"))
}

pub fn fir_frequency_response(
    taps: &[f32],
    sample_rate: u32,
    freqs_hz: &[f64],
) -> Vec<ResponsePoint> {
    let fs = sample_rate as f64;
    if taps.is_empty() || fs <= 0.0 {
        return Vec::new();
    }
    freqs_hz
        .iter()
        .map(|&f| {
            let theta = -2.0 * std::f64::consts::PI * f / fs;
            let (wi, wr) = theta.sin_cos();
            // Unit phasor e^(−j2πfn/fs), advanced by multiplication with w.
            let (mut cr, mut ci) = (1.0f64, 0.0f64);
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for &h in taps {
                let h = h as f64;
                re += h * cr;
                im += h * ci;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
            let mag = (re * re + im * im).sqrt();
            let magnitude_db = if mag > 1e-10 {
                20.0 * mag.log10()
            } else {
                -200.0
            };
            ResponsePoint {
                freq_hz: f,
                magnitude_db,
                phase_deg: im.atan2(re).to_degrees(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une convolution par blocs a une latence : elle ne peut rendre un
    /// echantillon avant d'avoir vu le bloc qui le contient. Ce test lisait
    /// autrefois la sortie DANS le meme appel, ce qui n'est vrai que si la
    /// frontiere du bloc coincide avec celle de l'appel — l'hypothese meme qui
    /// faisait sous-deborder l'ancien code (#2209). On nourrit donc la latence
    /// et on lit decale.
    /// Convolution directe de reference — la definition, sans FFT ni blocs.
    fn convolution_directe(x: &[f32], h: &[f32]) -> Vec<f32> {
        let mut y = vec![0.0f32; x.len()];
        for (n, yn) in y.iter_mut().enumerate() {
            for (k, hk) in h.iter().enumerate() {
                if n >= k {
                    *yn += x[n - k] * hk;
                }
            }
        }
        y
    }

    /// Nourrit un convolveur neuf par lots de tailles donnees et rend la
    /// sortie complete, latence comprise.
    fn passer_par_lots(ir: &[Vec<f32>], block: usize, x: &[f32], lots: &[usize]) -> Vec<f32> {
        let mut conv = Convolver::new(ir, block);
        let mut entree = x.to_vec();
        entree.extend(std::iter::repeat(0.0).take(conv.latency_frames()));
        let mut sortie = Vec::with_capacity(entree.len());
        let mut i = 0;
        let mut t = 0;
        while i < entree.len() {
            let n = lots[t % lots.len()].min(entree.len() - i);
            t += 1;
            let mut morceau = entree[i..i + n].to_vec();
            conv.process_interleaved(&mut morceau);
            sortie.extend_from_slice(&morceau);
            i += n;
        }
        sortie
    }

    /// LE test que ce correctif demandait. Le decoupage des appels ne doit rien
    /// changer au resultat — c'est toute la difference entre un convolveur et
    /// un convolveur qui suppose que ses blocs arrivent alignes.
    ///
    /// Le cas `[100, 924]` est celui du ticket : bloc 1024, un premier appel de
    /// 100 trames, un second de 924. L'ancien code calculait
    /// `frame + 1 - block_size` = `924 - 1024` sur des `usize`.
    #[test]
    fn le_decoupage_des_appels_ne_change_pas_le_resultat() {
        let block = 1024;
        let ir = vec![
            (0..200)
                .map(|k| 1.0 / (1.0 + k as f32))
                .collect::<Vec<f32>>(),
        ];
        let x: Vec<f32> = (0..5000)
            .map(|n| ((n as f32) * 0.037).sin() * 0.8)
            .collect();

        let reference = passer_par_lots(&ir, block, &x, &[block]);
        for lots in [
            vec![1usize],
            vec![100, 924],
            vec![1024],
            vec![1500],
            vec![7, 3, 991, 64, 1500, 1],
        ] {
            let obtenu = passer_par_lots(&ir, block, &x, &lots);
            assert_eq!(obtenu.len(), reference.len(), "lots {lots:?}");
            for (i, (o, r)) in obtenu.iter().zip(reference.iter()).enumerate() {
                assert!(
                    (o - r).abs() < 1e-4,
                    "lots {lots:?}, trame {i} : {o} au lieu de {r}"
                );
            }
        }
    }

    /// Et le resultat doit etre LA convolution, pas seulement une valeur
    /// stable : on compare a la definition directe.
    #[test]
    fn le_resultat_est_la_convolution_directe() {
        let block = 64;
        let h: Vec<f32> = (0..40)
            .map(|k| ((k as f32) * 0.3).cos() / (1.0 + k as f32))
            .collect();
        let x: Vec<f32> = (0..1000).map(|n| ((n as f32) * 0.11).sin()).collect();

        let attendu = convolution_directe(&x, &h);
        let sortie = passer_par_lots(&[h.clone()], block, &x, &[37]);
        let latence = block;
        for (n, a) in attendu.iter().enumerate() {
            let obtenu = sortie[latence + n];
            assert!(
                (obtenu - a).abs() < 1e-3,
                "trame {n} : {obtenu} au lieu de {a}"
            );
        }
    }

    /// Une IR stereo dont les canaux n'ont pas la meme longueur : la queue du
    /// canal le plus long etait ignoree, `process_block` bornant la boucle sur
    /// le nombre de partitions du canal 0.
    #[test]
    fn la_queue_dune_ir_plus_longue_sur_lautre_canal_nest_pas_perdue() {
        let block = 16;
        let court = vec![1.0f32];
        // Le canal droit porte une impulsion au-dela de la premiere partition.
        let mut long = vec![0.0f32; 40];
        long[0] = 1.0;
        long[33] = 0.5;
        let ir = vec![court, long];

        let mut conv = Convolver::new(&ir, block);
        let latence = conv.latency_frames();
        let trames = 128;
        let mut x = vec![0.0f32; trames * 2];
        x[0] = 1.0; // impulsion a gauche
        x[1] = 1.0; // impulsion a droite
        x.extend(std::iter::repeat(0.0).take(latence * 2));
        conv.process_interleaved(&mut x);

        let droite = |n: usize| x[(latence + n) * 2 + 1];
        assert!((droite(0) - 1.0).abs() < 1e-3, "premiere partition perdue");
        assert!(
            (droite(33) - 0.5).abs() < 1e-3,
            "queue au-dela de la premiere partition perdue : {}",
            droite(33)
        );
    }

    #[test]
    fn identity_ir() {
        let ir = vec![vec![1.0, 0.0, 0.0, 0.0]];
        let mut conv = Convolver::new(&ir, 4);
        let latence = conv.latency_frames();
        let attendu = [1.0f32, 0.5, 0.25, 0.125];
        let mut samples = attendu.to_vec();
        samples.extend(std::iter::repeat(0.0).take(latence));
        conv.process_interleaved(&mut samples);
        for (i, &a) in attendu.iter().enumerate() {
            let s = samples[latence + i];
            assert!((s - a).abs() < 0.001, "sample {i}: {s}");
        }
    }

    #[test]
    fn stereo_ir() {
        let ir = vec![vec![1.0, 0.0]; 2];
        let mut conv = Convolver::new(&ir, 4);
        let latence = conv.latency_frames();
        let mut samples = vec![1.0, 0.5, 0.25, 0.125, 1.0, 0.5, 0.25, 0.125];
        samples.extend(std::iter::repeat(0.0).take(latence * 2));
        conv.process_interleaved(&mut samples);
        assert!((samples[latence * 2] - 1.0).abs() < 0.01);
        assert!((samples[latence * 2 + 1] - 0.5).abs() < 0.01);
    }

    /// #2210 — la cadence n'est plus une métadonnée jetée après lecture du
    /// WAV. Chacune des cadences réellement livrées doit produire un moteur
    /// seulement pour un flux de même cadence.
    #[test]
    fn la_configuration_est_liee_aux_cadences_441_48_96_et_192_khz() {
        for sample_rate in [44_100, 48_000, 96_000, 192_000] {
            let config = ConvolverConfig::new(vec![vec![1.0, 0.5]], sample_rate).unwrap();
            let convolver = config
                .build_for(4, sample_rate, 2)
                .expect("une IR mono de même cadence doit être dupliquée");
            assert_eq!(convolver.channels(), 2);

            let other_rate = if sample_rate == 48_000 {
                96_000
            } else {
                48_000
            };
            let error = config
                .build_for(4, other_rate, 2)
                .err()
                .expect("une cadence différente doit être refusée");
            assert!(error.contains(&sample_rate.to_string()), "{error}");
            assert!(error.contains(&other_rate.to_string()), "{error}");
            assert!(error.contains("réexportez"), "{error}");
        }
    }

    #[test]
    fn seule_une_ir_mono_est_dupliquee_implicitement() {
        let mono = ConvolverConfig::new(vec![vec![1.0]], 48_000).unwrap();
        assert_eq!(mono.build_for(4, 48_000, 8).unwrap().channels(), 8);

        let stereo = ConvolverConfig::new(vec![vec![1.0], vec![0.5]], 48_000).unwrap();
        let error = stereo
            .build_for(4, 48_000, 6)
            .err()
            .expect("un layout non mono doit correspondre exactement");
        assert!(
            error.contains("2 canaux") && error.contains("6 canaux"),
            "{error}"
        );
    }

    #[test]
    fn from_wav_skips_extra_chunks_before_data() {
        // Identity IR (4 taps, 16-bit mono @48k) with a spurious LIST chunk
        // inserted before `data` — exactly what the old fixed-44-byte parser
        // misread into noise. The chunk-walking parser must still find `data`.
        let ir_i16: [i16; 4] = [32767, 0, 0, 0];
        let mut wav: Vec<u8> = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&[0, 0, 0, 0]); // RIFF size — patched below
        wav.extend_from_slice(b"WAVE");
        // fmt  (PCM)
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // format = PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // channels
        wav.extend_from_slice(&48_000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&(48_000u32 * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits
        // spurious chunk before data
        wav.extend_from_slice(b"LIST");
        wav.extend_from_slice(&4u32.to_le_bytes());
        wav.extend_from_slice(b"INFO");
        // data
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&((ir_i16.len() * 2) as u32).to_le_bytes());
        for s in ir_i16 {
            wav.extend_from_slice(&s.to_le_bytes());
        }
        let riff_size = (wav.len() - 8) as u32;
        wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

        let ir_file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let path = ir_file.path().to_path_buf();
        std::fs::write(&path, &wav).unwrap();
        let mut conv = Convolver::from_wav(path.to_str().unwrap(), 4).unwrap();

        let latence = conv.latency_frames();
        let attendu = [1.0f32, 0.5, 0.25, 0.125];
        let mut samples = attendu.to_vec();
        samples.extend(std::iter::repeat(0.0).take(latence));
        conv.process_interleaved(&mut samples);
        for (i, &a) in attendu.iter().enumerate() {
            let s = samples[latence + i];
            assert!((s - a).abs() < 0.01, "sample {i}: {s}");
        }
    }

    #[test]
    fn process_offline_flushes_last_partial_block() {
        // IR = [1.0, 0.5], block 4. A 6-frame buffer is not a block multiple,
        // so the streaming path alone leaves frames 4-5 dry; process_offline
        // must flush them. Convolving an impulse yields the IR then silence.
        let ir = vec![vec![1.0f32, 0.5]];
        let mut conv = Convolver::new(&ir, 4);
        let mut samples = vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        conv.process_offline(&mut samples);
        assert_eq!(samples.len(), 6, "length must be preserved");
        let expected = [1.0, 0.5, 0.0, 0.0, 0.0, 0.0];
        for (i, &e) in expected.iter().enumerate() {
            assert!(
                (samples[i] - e).abs() < 0.001,
                "frame {i}: {} != {e}",
                samples[i]
            );
        }
    }

    #[test]
    fn process_pcm_16bit_round_trip() {
        let ir = vec![vec![1.0f32, 0.5]];
        let mut conv = Convolver::new(&ir, 4);
        let frames: [i16; 6] = [32767, 0, 0, 0, 0, 0];
        let mut pcm: Vec<u8> = Vec::new();
        for f in frames {
            pcm.extend_from_slice(&f.to_le_bytes());
        }
        conv.process_pcm(&mut pcm, 16);
        let out: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert!((out[0] as i32 - 32767).abs() < 4, "out0={}", out[0]);
        assert!((out[1] as i32 - 16383).abs() < 4, "out1={}", out[1]); // 0.5 * full-scale
        for (i, &v) in out.iter().enumerate().skip(2) {
            assert!(v.abs() < 4, "out{i}={v}");
        }
    }

    #[test]
    fn frequency_response_unit_impulse_is_flat_0db() {
        // δ[n] → H(f) = 1 everywhere: 0 dB, 0° phase.
        let taps = vec![1.0f32, 0.0, 0.0, 0.0];
        let freqs = log_freq_grid(50, 20.0, 20_000.0);
        assert_eq!(freqs.len(), 50);
        assert!((freqs[0] - 20.0).abs() < 1e-9);
        assert!((freqs[49] - 20_000.0).abs() < 1e-6);
        for p in fir_frequency_response(&taps, 48_000, &freqs) {
            assert!(
                p.magnitude_db.abs() < 1e-6,
                "{} Hz: {} dB",
                p.freq_hz,
                p.magnitude_db
            );
            assert!(
                p.phase_deg.abs() < 1e-6,
                "{} Hz: {}°",
                p.freq_hz,
                p.phase_deg
            );
        }
    }

    #[test]
    fn frequency_response_pure_delay_flat_linear_phase() {
        // δ[n−1] → |H| = 1 (0 dB), phase = −360·f/fs degrees (no wrap below
        // fs/2 for a 1-sample delay).
        let taps = vec![0.0f32, 1.0];
        let fs = 48_000u32;
        let freqs = log_freq_grid(50, 20.0, 20_000.0);
        for p in fir_frequency_response(&taps, fs, &freqs) {
            assert!(
                p.magnitude_db.abs() < 1e-6,
                "{} Hz: {} dB",
                p.freq_hz,
                p.magnitude_db
            );
            let expected = -360.0 * p.freq_hz / fs as f64;
            assert!(
                (p.phase_deg - expected).abs() < 1e-6,
                "{} Hz: {}° != {expected}°",
                p.freq_hz,
                p.phase_deg
            );
        }
    }

    #[test]
    fn frequency_response_two_tap_average_lowpass() {
        // h = [0.5, 0.5] → |H(f)| = cos(πf/fs): −3.01 dB at fs/4, plunging
        // toward −∞ near fs/2 (floored at −200 dB by the implementation).
        let taps = vec![0.5f32, 0.5];
        let fs = 48_000u32;
        let pts = fir_frequency_response(&taps, fs, &[12_000.0, 23_990.0]);
        assert!(
            (pts[0].magnitude_db - (-3.0103)).abs() < 0.01,
            "fs/4: {} dB",
            pts[0].magnitude_db
        );
        assert!(
            pts[1].magnitude_db < -60.0,
            "near fs/2: {} dB",
            pts[1].magnitude_db
        );
    }

    /// Ecrire un WAV mono flottant pour les tests de combinaison.
    fn ecrire_ir_mono(chemin: &std::path::Path, taps: &[f32], sr: u32) {
        super::ecrire_wav_float32(chemin.to_str().unwrap(), taps, 1, sr).unwrap();
    }

    /// Deux filtres MONO, gauche et droite, deviennent un WAV stereo dont
    /// chaque canal porte SON filtre.
    ///
    /// C'est le chemin d'entree qui manquait : le moteur savait deja convoluer
    /// un canal par reponse, mais REW, Acourate et Audiolense exportent deux
    /// fichiers mono, jamais un stereo (Daniel, 24/08/2026).
    #[test]
    fn deux_filtres_mono_deviennent_un_stereo_par_canal() {
        let dir = std::env::temp_dir().join(format!("tune-fir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = dir.join("gauche.wav");
        let d = dir.join("droite.wav");
        let out = dir.join("combine.wav");

        // Deux filtres RECONNAISSABLES et de longueurs differentes.
        ecrire_ir_mono(&g, &[1.0, 0.5, 0.25], 48_000);
        ecrire_ir_mono(&d, &[-1.0, -0.5], 48_000);

        let sr = Convolver::combiner_en_stereo(
            g.to_str().unwrap(),
            d.to_str().unwrap(),
            out.to_str().unwrap(),
        )
        .expect("la combinaison doit reussir");
        assert_eq!(sr, 48_000);

        let (ir, sr_relu) = Convolver::read_ir_taps(out.to_str().unwrap()).unwrap();
        assert_eq!(sr_relu, 48_000);
        assert_eq!(ir.len(), 2, "le fichier combine doit etre STEREO");
        assert_eq!(
            ir[0],
            vec![1.0, 0.5, 0.25],
            "le canal gauche doit porter le filtre gauche, intact"
        );
        assert_eq!(
            ir[1],
            vec![-1.0, -0.5, 0.0],
            "le canal droit doit porter le filtre droit, complete de zeros — un \
             zero est neutre pour une convolution, contrairement a une repetition"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Des cadences differentes sont un refus, pas un rattrapage silencieux :
    /// convoluer a deux cadences decalerait un canal par rapport a l'autre.
    #[test]
    fn deux_cadences_differentes_sont_refusees() {
        let dir = std::env::temp_dir().join(format!("tune-fir-sr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = dir.join("g.wav");
        let d = dir.join("d.wav");
        let out = dir.join("o.wav");
        ecrire_ir_mono(&g, &[1.0, 0.0], 48_000);
        ecrire_ir_mono(&d, &[1.0, 0.0], 44_100);

        let e = Convolver::combiner_en_stereo(
            g.to_str().unwrap(),
            d.to_str().unwrap(),
            out.to_str().unwrap(),
        )
        .expect_err("deux cadences differentes doivent etre refusees");
        assert!(
            e.contains("48000") && e.contains("44100"),
            "le refus doit NOMMER les deux cadences, sinon l'utilisateur ne sait \
             pas lequel de ses deux fichiers reexporter — recu : {e}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Le fichier combine doit vraiment traverser le convolveur : deux
    /// impulsions de signes opposes doivent ressortir de signes opposes.
    #[test]
    fn le_stereo_combine_convolue_chaque_canal_avec_son_filtre() {
        let dir = std::env::temp_dir().join(format!("tune-fir-conv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let g = dir.join("g.wav");
        let d = dir.join("d.wav");
        let out = dir.join("o.wav");
        // Gauche : gain 1. Droite : gain -1 (phase inversee).
        ecrire_ir_mono(&g, &[1.0], 48_000);
        ecrire_ir_mono(&d, &[-1.0], 48_000);
        Convolver::combiner_en_stereo(
            g.to_str().unwrap(),
            d.to_str().unwrap(),
            out.to_str().unwrap(),
        )
        .unwrap();

        let mut conv = Convolver::from_wav(out.to_str().unwrap(), 4).unwrap();
        let latence = conv.latency_frames();
        // Une impulsion identique sur les deux canaux, entrelacee.
        let mut sortie = vec![0.0f32; (latence + 4) * 2];
        sortie[0] = 1.0;
        sortie[1] = 1.0;
        // Traitement EN PLACE : la tranche porte l'entree puis la sortie.
        conv.process_interleaved(&mut sortie);

        let i = latence * 2;
        assert!(
            (sortie[i] - 1.0).abs() < 1e-4,
            "le canal gauche doit sortir en phase : {}",
            sortie[i]
        );
        assert!(
            (sortie[i + 1] + 1.0).abs() < 1e-4,
            "le canal droit doit sortir en phase INVERSEE — s'il sort comme le \
             gauche, les deux canaux partagent le meme filtre : {}",
            sortie[i + 1]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
