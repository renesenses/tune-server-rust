// Code audio (décodage, analyse, traitement du signal) : les boucles indexées
// et les découpes par `chunks_exact` y sont gardées telles quelles. Les récrire
// (`as_chunks`, itérateurs, `repeat_n`) ne changerait rien au son mais toucherait
// la logique audio pour un gain de forme (clippy 1.98).
#![allow(clippy::chunks_exact_to_as_chunks)]

use lofty::file::AudioFile;

use tracing::{debug, info, warn};

/// Decode audio file to raw PCM (i16 LE interleaved).
///
/// Uses native Rust decoders for all supported formats (FLAC, MP3, WAV, AAC,
/// ALAC, OGG, AIFF, DSF, DFF, WavPack, APE).
pub async fn decode_pcm(
    file_path: &str,
    sample_rate: u32,
    channels: u32,
    seek_s: f64,
    duration_s: f64,
) -> Result<Vec<u8>, String> {
    let path = file_path.to_string();
    let result = tokio::task::spawn_blocking(move || {
        super::decode::decode_to_pcm(&path, Some(sample_rate), Some(channels), seek_s, duration_s)
    })
    .await
    .map_err(|e| format!("join: {e}"))?;

    match result {
        Ok(decoded) => {
            debug!(
                file = file_path,
                samples = decoded.samples_i32.len(),
                sample_rate = decoded.sample_rate,
                channels = decoded.channels,
                source_bit_depth = decoded.bit_depth,
                output_bit_depth = 16,
                "decoded_analyzer_contract"
            );
            // Analyzer consumers parse two bytes per sample. Returning native
            // 24/32-bit bytes under an i16 contract shifted frame boundaries
            // and corrupted BPM/waveform measurements (#2230).
            let bytes =
                super::decode::convert_pcm_bit_depth(&decoded.samples_i32, decoded.bit_depth, 16);
            Ok(bytes)
        }
        Err(e) => {
            warn!(file = file_path, error = %e, "native_decode_failed");
            Err(e)
        }
    }
}

pub async fn get_duration(file_path: &str) -> Result<f64, String> {
    let path = file_path.to_string();
    tokio::task::spawn_blocking(move || {
        let tagged = lofty::read_from_path(&path).map_err(|e| format!("lofty duration: {e}"))?;
        let duration = tagged.properties().duration();
        Ok(duration.as_secs_f64())
    })
    .await
    .map_err(|e| format!("join: {e}"))?
}

// ---------------------------------------------------------------------------
// EBU R128 loudness measurement (pure Rust)
// ---------------------------------------------------------------------------

/// Transposed direct-form II biquad filter.
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0,
            b1,
            b2,
            a1,
            a2,
            z1: 0.0,
            z2: 0.0,
        }
    }

    #[cfg(test)]
    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Process one sample (transposed direct-form II).
    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

/// Compute K-weighting biquad coefficients for the given sample rate.
///
/// Returns (stage1, stage2) where:
/// - stage1 = pre-filter (high-shelf modelling head acoustics)
/// - stage2 = RLB weighting (high-pass ~38 Hz)
///
/// Reference: ITU-R BS.1770-4, Table 1.
fn k_weighting_coefficients(fs: f64) -> (Biquad, Biquad) {
    // --- Stage 1: Pre-filter (high-shelf) ---
    // Design parameters (from ITU-R BS.1770-4)
    let db = 3.999843853973347;
    let f0 = 1681.974450955533;
    let q = 0.7071752369554196;

    let k = (std::f64::consts::PI * f0 / fs).tan();
    let vh = 10.0_f64.powf(db / 20.0);
    let vb = vh.powf(0.4996667741545416);

    let a0 = 1.0 + k / q + k * k;
    let s1_b0 = (vh + vb * k / q + k * k) / a0;
    let s1_b1 = 2.0 * (k * k - vh) / a0;
    let s1_b2 = (vh - vb * k / q + k * k) / a0;
    let s1_a1 = 2.0 * (k * k - 1.0) / a0;
    let s1_a2 = (1.0 - k / q + k * k) / a0;

    // --- Stage 2: RLB weighting (high-pass) ---
    let f0_hp = 38.13547087602444;
    let q_hp = 0.5003270373238773;

    let k2 = (std::f64::consts::PI * f0_hp / fs).tan();
    let a0_hp = 1.0 + k2 / q_hp + k2 * k2;
    let s2_b0 = 1.0 / a0_hp;
    let s2_b1 = -2.0 / a0_hp;
    let s2_b2 = 1.0 / a0_hp;
    let s2_a1 = 2.0 * (k2 * k2 - 1.0) / a0_hp;
    let s2_a2 = (1.0 - k2 / q_hp + k2 * k2) / a0_hp;

    (
        Biquad::new(s1_b0, s1_b1, s1_b2, s1_a1, s1_a2),
        Biquad::new(s2_b0, s2_b1, s2_b2, s2_a1, s2_a2),
    )
}

/// Plage dynamique (« DR ») d'une piste, en flux — algorithme TT Dynamic Range.
///
/// Bertrand, 09/09/2026 : « Lance le calcul des DR ». Il n'y avait rien à
/// lancer — Tune LISAIT le tag Vorbis `ALBUM DYNAMIC RANGE` / `DYNAMIC RANGE`
/// (`metadata/mod.rs`) et ne calculait rien. Mesuré le même jour sur TROIS
/// bibliothèques (.18, .15, .42) : zéro DR partout, parce qu'aucun de ces
/// fichiers ne porte le tag. Cinq surfaces d'interface — badge de fiche, tri,
/// tranche, facette Oxygen, colonne de titres — étaient construites et vides.
///
/// ## L'algorithme, et pourquoi ces constantes-là
///
/// Pour chaque canal :
///  1. découper en blocs de **3 s** ;
///  2. par bloc, le RMS *référencé sinus* `sqrt(2 · moyenne(x²))` et le pic ;
///  3. garder les **20 %** de blocs au RMS le plus fort, et en faire le RMS
///     quadratique moyen ;
///  4. `DR = 20·log₁₀(pic₂ / RMS₂₀%)`, où `pic₂` est le DEUXIÈME plus grand pic
///     de bloc du canal.
///
/// Puis la moyenne sur les canaux, arrondie.
///
/// 🔴 Le facteur 2 du RMS n'est pas décoratif : il fait qu'un sinus pur rend
/// `RMS = amplitude`, donc `DR = 0`. Sans lui tout le barème glisse de 3 dB et
/// les valeurs ne se compareraient plus à celles publiées par les autres
/// mesureurs. C'est ce que vérifie le premier témoin.
///
/// 🔴 `pic₂` et non le pic maximum : un unique échantillon aberrant — un clic,
/// une erreur d'encodage — gonflerait le DR de toute la piste. Prendre le
/// second est ce qui rend la mesure robuste, et c'est le choix de l'outil de
/// référence.
///
/// ## Ce que cette mesure N'EST PAS
///
/// Elle ne remplace pas le tag du fichier : un disque qui porte
/// `DYNAMIC RANGE` garde SA valeur, celle que son producteur a mesurée. Le
/// calcul ne sert qu'aux fichiers qui n'en ont pas — voir `dr_source`.
///
/// ⚠️ Un écart de ±1 avec une valeur publiée est normal : le découpage des
/// blocs aux bords et l'arrondi diffèrent d'une implémentation à l'autre. On
/// ne prétend pas à l'égalité au dixième.
///
/// Mémoire bornée comme son voisin : un `f64` par bloc de 3 s et par canal —
/// une piste de dix minutes en stéréo en garde 400.
struct DrAccumulator {
    channels: usize,
    block_frames: usize,
    /// Somme des carrés du bloc en cours, par canal.
    sum_sq: Vec<f64>,
    /// Pic du bloc en cours, par canal.
    peak_courant: Vec<f64>,
    /// Trames déjà entrées dans le bloc en cours (commun aux canaux).
    frames_du_bloc: usize,
    /// RMS de chaque bloc terminé, par canal.
    rms_des_blocs: Vec<Vec<f64>>,
    /// Les DEUX plus grands pics de bloc, par canal, en ordre décroissant.
    deux_pics: Vec<[f64; 2]>,
}

impl DrAccumulator {
    /// Le bloc de 3 s de l'algorithme TT.
    const BLOC_SECONDES: f64 = 3.0;
    /// La part des blocs les plus forts retenue pour le RMS.
    const PART_FORTE: f64 = 0.20;

    fn new(sample_rate: usize, channels: usize) -> Self {
        Self {
            channels,
            block_frames: ((sample_rate as f64) * Self::BLOC_SECONDES) as usize,
            sum_sq: vec![0.0; channels],
            peak_courant: vec![0.0; channels],
            frames_du_bloc: 0,
            rms_des_blocs: vec![Vec::new(); channels],
            deux_pics: vec![[0.0; 2]; channels],
        }
    }

    /// Échantillons ENTRELACÉS et normalisés, en tranches quelconques : l'état
    /// du bloc en cours traverse les appels, exactement comme le fait le
    /// filtre du voisin. Découper autrement ne change pas le résultat.
    fn feed(&mut self, samples: &[f64]) {
        if self.channels == 0 || self.block_frames == 0 {
            return;
        }
        for trame in samples.chunks_exact(self.channels) {
            for (c, &x) in trame.iter().enumerate() {
                self.sum_sq[c] += x * x;
                let a = x.abs();
                if a > self.peak_courant[c] {
                    self.peak_courant[c] = a;
                }
            }
            self.frames_du_bloc += 1;
            if self.frames_du_bloc >= self.block_frames {
                self.fermer_le_bloc();
            }
        }
    }

    fn fermer_le_bloc(&mut self) {
        if self.frames_du_bloc == 0 {
            return;
        }
        let n = self.frames_du_bloc as f64;
        for c in 0..self.channels {
            // 🔴 RMS RÉFÉRENCÉ SINUS : `sqrt(2 · moyenne)`. Voir la doc.
            let rms = (2.0 * self.sum_sq[c] / n).sqrt();
            self.rms_des_blocs[c].push(rms);
            let p = self.peak_courant[c];
            if p > self.deux_pics[c][0] {
                self.deux_pics[c][1] = self.deux_pics[c][0];
                self.deux_pics[c][0] = p;
            } else if p > self.deux_pics[c][1] {
                self.deux_pics[c][1] = p;
            }
            self.sum_sq[c] = 0.0;
            self.peak_courant[c] = 0.0;
        }
        self.frames_du_bloc = 0;
    }

    /// La valeur DR, arrondie. `None` sur du silence ou une entrée trop courte.
    fn finish(mut self) -> Option<u32> {
        // Le reliquat compte : une piste de 4 s n'a qu'un bloc plein, et le
        // second porte l'essentiel de sa fin. L'ignorer perdrait les pistes
        // courtes — et une piste de moins de 3 s n'aurait AUCUN bloc.
        self.fermer_le_bloc();

        let mut total = 0.0_f64;
        let mut comptes = 0usize;
        for c in 0..self.channels {
            let mut rms = std::mem::take(&mut self.rms_des_blocs[c]);
            if rms.is_empty() {
                continue;
            }
            // Un canal muet (une piste mono servie en stéréo, une voie de
            // remplissage) ne doit pas tirer la moyenne : il n'a pas de plage
            // dynamique, il n'a rien du tout.
            let pic2 = self.deux_pics[c][1];
            if pic2 <= 0.0 {
                continue;
            }
            rms.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            // Au moins UN bloc, même sur une piste très courte.
            let n = ((rms.len() as f64) * Self::PART_FORTE).round().max(1.0) as usize;
            let n = n.min(rms.len());
            let somme: f64 = rms[..n].iter().map(|r| r * r).sum();
            let rms20 = (somme / n as f64).sqrt();
            if rms20 <= 0.0 {
                continue;
            }
            total += 20.0 * (pic2 / rms20).log10();
            comptes += 1;
        }
        if comptes == 0 {
            return None;
        }
        // Négatif possible sur un signal carré (RMS > pic) : le DR est une
        // ÉCHELLE qui commence à zéro, on ne publie pas de valeur négative.
        Some((total / comptes as f64).round().max(0.0) as u32)
    }
}

/// Somme des carrés des `n` premiers échantillons de la file, DANS L'ORDRE.
///
/// #5519 — même valeur, au bit près, que
/// `buf.iter().take(n).map(|s| s * s).sum::<f64>()` : la même suite
/// d'additions, de gauche à droite, sur les deux tranches contiguës de la file
/// au lieu de son itérateur. Seul le coût change.
fn somme_des_carres(buf: &std::collections::VecDeque<f64>, n: usize) -> f64 {
    let (a, b) = buf.as_slices();
    let (a, b) = if a.len() >= n {
        (&a[..n], &b[..0])
    } else {
        (a, &b[..(n - a.len()).min(b.len())])
    };
    // `-0.0`, comme `Iterator::sum` pour `f64` : sans quoi une file vide
    // rendrait `+0.0` au lieu de `-0.0`, un bit de différence.
    let mut somme = -0.0_f64;
    for s in a {
        somme += s * s;
    }
    for s in b {
        somme += s * s;
    }
    somme
}

/// Streaming EBU R128 (BS.1770-4) integrated-loudness + sample-peak accumulator.
///
/// Feed interleaved, normalized (`[-1, 1]`) f64 samples in any chunking — the
/// K-weighting filter state is continuous across `feed` calls, so feeding a
/// signal all at once or in chunks yields the **same** value as the previous
/// whole-track code. Memory is bounded by one 400 ms block regardless of track
/// length: only one `f64` per 100 ms block (`block_powers`) is retained. This is
/// what lets `measure_loudness_and_peak` analyse multi-GB hi-res tracks without
/// materialising them in RAM (fixes the OOM crash-loop, #1109).
struct LoudnessAccumulator {
    channels: usize,
    block_frames: usize,
    step_frames: usize,
    /// Per-channel K-weighting biquads (state carried across `feed`).
    filters: Vec<(Biquad, Biquad)>,
    /// Per-channel K-weighted samples from the current block start onward.
    bufs: Vec<std::collections::VecDeque<f64>>,
    /// Mean-square power per 400 ms block (channel-summed).
    block_powers: Vec<f64>,
    /// Running linear sample peak on the *un-weighted* samples.
    peak: f64,
    /// Crête vraie (ITU-R BS.1770, annexe 2) sur les échantillons NON
    /// pondérés — voir [`super::crete_vraie`] (#2713). Son histoire traverse
    /// les appels `feed`, comme l'état du filtre de pondération K.
    crete_vraie: super::crete_vraie::CreteVraie,
    total_frames: usize,
}

impl LoudnessAccumulator {
    fn new(sample_rate: usize, channels: usize) -> Self {
        let fs = sample_rate as f64;
        Self {
            channels,
            block_frames: (fs * 0.4) as usize,
            step_frames: (fs * 0.1) as usize,
            filters: (0..channels)
                .map(|_| k_weighting_coefficients(fs))
                .collect(),
            bufs: (0..channels)
                .map(|_| std::collections::VecDeque::new())
                .collect(),
            block_powers: Vec::new(),
            peak: 0.0,
            crete_vraie: super::crete_vraie::CreteVraie::new(sample_rate, channels),
            total_frames: 0,
        }
    }

    /// Feed interleaved normalized samples. Emits every complete 400 ms block
    /// aligned on the 100 ms step (identical alignment to the batch loop
    /// `while start + block_frames <= num_frames { start += step_frames }`).
    fn feed(&mut self, interleaved: &[f64]) {
        if self.channels == 0 {
            return;
        }
        let frames = interleaved.len() / self.channels;
        self.crete_vraie.nourrir(interleaved);
        for f in 0..frames {
            for c in 0..self.channels {
                let raw = interleaved[f * self.channels + c];
                self.peak = self.peak.max(raw.abs());
                let (s1, s2) = &mut self.filters[c];
                self.bufs[c].push_back(s2.process(s1.process(raw)));
            }
            self.total_frames += 1;
        }
        if self.block_frames == 0 || self.step_frames == 0 {
            return;
        }
        while self.bufs[0].len() >= self.block_frames {
            let mut power_sum = 0.0;
            for c in 0..self.channels {
                let ms: f64 =
                    somme_des_carres(&self.bufs[c], self.block_frames) / self.block_frames as f64;
                power_sum += ms; // channel weight = 1.0 (mono/stereo)
            }
            self.block_powers.push(power_sum);
            for c in 0..self.channels {
                self.bufs[c].drain(..self.step_frames);
            }
        }
    }

    /// Integrated loudness (LUFS, rounded to 0.1) + sample peak (clamped to
    /// 1.0) + TRUE peak (BS.1770 annexe 2, #2713 ; volontairement NON borné
    /// à 1.0 : les overs inter-échantillons au-dessus de 0 dBFS sont
    /// précisément l'information que `prevent_clipping` doit voir, #1694). `None` for silence /
    /// below-threshold / empty input.
    fn finish(self) -> Option<(f64, f64, f64)> {
        let peak = self.peak.min(1.0);
        // Le vrai pic englobe le sample peak par construction (chaque
        // échantillon brut y participe) ; on le republie tel quel.
        let true_peak = self.crete_vraie.crete();

        // Too short for even one 400 ms block: simple loudness over all samples
        // (nothing was drained, so the buffers still hold the whole signal).
        if self.block_powers.is_empty() {
            if self.total_frames == 0 {
                return None;
            }
            let mut power_sum = 0.0;
            for buf in &self.bufs {
                if buf.is_empty() {
                    return None;
                }
                power_sum += buf.iter().map(|s| s * s).sum::<f64>() / buf.len() as f64;
            }
            if power_sum <= 0.0 {
                return None;
            }
            let lufs = -0.691 + 10.0 * power_sum.log10();
            return Some(((lufs * 10.0).round() / 10.0, peak, true_peak));
        }

        // Absolute gating: keep blocks above -70 LUFS.
        let abs_threshold = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
        let gated_abs: Vec<f64> = self
            .block_powers
            .iter()
            .copied()
            .filter(|&p| p > abs_threshold)
            .collect();
        if gated_abs.is_empty() {
            return None;
        }
        // Relative threshold = mean of abs-gated blocks - 10 dB.
        let mean_abs: f64 = gated_abs.iter().sum::<f64>() / gated_abs.len() as f64;
        let rel_threshold = mean_abs * 10.0_f64.powf(-10.0 / 10.0);
        let gated_rel: Vec<f64> = self
            .block_powers
            .iter()
            .copied()
            .filter(|&p| p > rel_threshold)
            .collect();
        if gated_rel.is_empty() {
            return None;
        }
        let mean_rel: f64 = gated_rel.iter().sum::<f64>() / gated_rel.len() as f64;
        if mean_rel <= 0.0 {
            return None;
        }
        let lufs = -0.691 + 10.0 * mean_rel.log10();
        Some(((lufs * 10.0).round() / 10.0, peak, true_peak))
    }
}

/// i32 → normalized f64 scale for a given bit depth.
fn pcm_scale(bit_depth: u16) -> f64 {
    match bit_depth {
        24 => (1i64 << 23) as f64,
        32 => (1i64 << 31) as f64,
        _ => 32768.0,
    }
}

/// Integrated loudness (LUFS) from already-normalized interleaved samples.
///
/// Enveloppe d'un seul appel autour de [`LoudnessAccumulator`], utilisée par le
/// seul test d'équivalence « une passe = par morceaux » : le chemin fichier
/// pilote l'accumulateur lui-même, par segments bornés (#1109). Portée `test`
/// pour le dire — hors test, plus personne n'appelle par ici.
#[cfg(test)]
fn integrated_loudness_from_samples(
    samples: &[f64],
    sample_rate: usize,
    channels: usize,
) -> Option<f64> {
    let mut acc = LoudnessAccumulator::new(sample_rate, channels);
    acc.feed(samples);
    acc.finish().map(|(lufs, _, _)| lufs)
}

/// Measure EBU R128 integrated loudness (in LUFS) using native decoding.
///
/// Implements ITU-R BS.1770-4:
/// 1. K-frequency weighting (2-stage biquad)
/// 2. Mean-square per 400ms blocks (75% overlap)
/// 3. Absolute gating at -70 LUFS
/// 4. Relative gating at mean - 10 dB
pub async fn measure_loudness(file_path: &str) -> Option<f64> {
    measure_loudness_and_peak(file_path)
        .await
        .map(|(lufs, _, _)| lufs)
}

/// Measure the EBU R128 integrated loudness (LUFS), the linear sample peak
/// (0.0–1.0) and the linear TRUE peak (4× inter-sample, may exceed 1.0) in a
/// SINGLE decode pass — used by the ReplayGain analysis to derive
/// `rg_track_gain` (reference − LUFS), `rg_track_peak` and
/// `rg_track_true_peak` (#1694) without decoding the file twice.
pub async fn measure_loudness_and_peak(file_path: &str) -> Option<(f64, f64, f64)> {
    mesurer_intensite_et_plage(file_path)
        .await
        .map(|(lufs, peak, tp, _dr)| (lufs, peak, tp))
}

/// Intensité, pics ET plage dynamique — UN SEUL décodage.
///
/// 🔴 C'est toute l'économie du calcul de DR : il voyage avec le décodage que
/// la passe ReplayGain paie déjà. Un balayage séparé relirait chaque fichier
/// une seconde fois, pour une bibliothèque qui compte des milliers d'heures
/// d'audio (46 877 pistes mesurées sur le .18 le 09/09/2026).
///
/// Le quatrième membre est `None` quand la piste est trop courte, muette, ou
/// que ses canaux n'ont pas deux pics de bloc distincts — jamais une valeur
/// inventée.
pub async fn mesurer_intensite_et_plage(file_path: &str) -> Option<(f64, f64, f64, Option<u32>)> {
    mesurer_a_partir_de(file_path, None).await
}

/// Une mesure, suivie de l'empreinte que son décodage a permis de tirer.
pub struct MesureEtEmpreinte {
    /// Ce que rendrait [`mesurer_intensite_et_plage`], au bit près.
    pub mesure: Option<(f64, f64, f64, Option<u32>)>,
    /// Ce que rendrait `empreinte::empreinte_du_fichier`, au bit près — ou
    /// `None` quand ce décodage ne permet pas de la tirer (format hors du
    /// chemin partagé, tâche interrompue) : l'appelant la calcule alors à part,
    /// comme avant.
    pub empreinte: Option<Result<Option<super::empreinte::Empreinte>, String>>,
}

/// Les formats que décode `decode_symphonia`, dont le décodage de tête est un
/// PRÉFIXE exact d'un décodage plus long (même paquets, troncature au même
/// échantillon). Mesuré sur 40 FLAC réels (#5519, banc étage E : préfixe exact
/// 40/40). Tout le reste (DSD, dont le décodeur vise la cadence demandée ;
/// AIFF, APE, WavPack, Opus, Ogg, Matroska…) garde ses deux décodages.
fn empreinte_partageable(file_path: &str) -> bool {
    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(ext.as_str(), "flac" | "wav" | "mp3" | "m4a")
}

/// Les `secondes` premières d'un décodage natif, découpées EXACTEMENT comme
/// `decode_symphonia` tronque un décodage borné à `secondes`.
fn tete_du_decodage(
    natif: &super::decode::DecodedAudio,
    secondes: f64,
) -> super::decode::DecodedAudio {
    let max =
        super::decode::echantillons_de_la_fenetre(secondes, natif.sample_rate, natif.channels);
    let samples = natif.samples_i32[..max.min(natif.samples_i32.len())].to_vec();
    let frames = samples.len() as f64 / natif.channels.max(1) as f64;
    super::decode::DecodedAudio {
        duration_s: if natif.sample_rate > 0 {
            frames / natif.sample_rate as f64
        } else {
            0.0
        },
        samples_i32: samples,
        bit_depth: natif.bit_depth,
        sample_rate: natif.sample_rate,
        channels: natif.channels,
        integrite: Default::default(),
    }
}

/// [`mesurer_intensite_et_plage`] ET l'empreinte du contenu, sur UN décodage
/// de tête au lieu de deux (#5519).
///
/// La passe ReplayGain décodait la tête de chaque fichier deux fois : 30 s pour
/// le premier segment de la mesure, puis 90 s pour l'empreinte. Ici, la tête
/// de 90 s est décodée UNE fois : l'empreinte en est tirée exactement comme
/// `empreinte_du_fichier` la tirerait, et le premier segment de la mesure en
/// est le préfixe de 30 s. Les segments suivants, eux, sont décodés comme
/// avant, par `seek` : ils ne sont PAS la suite exacte d'un décodage d'un seul
/// tenant (seek grossier, banc étage E : 0/40), les prendre dans la tête
/// changerait le gain.
///
/// Gardé par `la_mesure_partagee_rend_les_memes_valeurs_au_bit_pres`.
pub async fn mesurer_intensite_plage_et_empreinte(file_path: &str) -> MesureEtEmpreinte {
    const SEG_SECONDS: f64 = 30.0;
    if !empreinte_partageable(file_path) {
        return MesureEtEmpreinte {
            mesure: mesurer_intensite_et_plage(file_path).await,
            empreinte: None,
        };
    }
    let path = file_path.to_string();
    let tete = tokio::task::spawn_blocking(move || {
        // #4681 — E/S basses si une zone joue (segment déjà parti).
        let _basse = crate::taches_de_fond::priorite::politique::baisser_pendant_la_lecture();
        let natif = super::decode::decode_natif(
            &path,
            Some(super::empreinte::TAUX),
            Some(1),
            0.0,
            super::empreinte::FENETRE_DECODEE_S,
        );
        match natif {
            Err(e) => (Err(e.clone()), Err(e)),
            Ok(natif) => {
                let segment = super::decode::adapter_pcm(
                    tete_du_decodage(&natif, SEG_SECONDS),
                    None,
                    Some(2),
                );
                let empreinte = super::empreinte::empreinte_d_un_decodage_natif(natif);
                (segment, empreinte)
            }
        }
    })
    .await;
    let Ok((segment, empreinte)) = tete else {
        // Tâche interrompue : ni mesure ni empreinte de ce décodage-ci.
        return MesureEtEmpreinte {
            mesure: None,
            empreinte: None,
        };
    };
    let mesure = match segment {
        Ok(premier) => mesurer_a_partir_de(file_path, Some(premier)).await,
        // Même issue que `mesurer_intensite_et_plage` sur un premier segment
        // qui ne se décode pas.
        Err(_) => None,
    };
    MesureEtEmpreinte {
        mesure,
        empreinte: Some(empreinte),
    }
}

/// Les accumulateurs d'une mesure en cours, d'un segment au suivant.
///
/// Ils voyagent DANS la tâche bloquante de chaque segment, puis en reviennent
/// (#5519) : voir [`mesurer_a_partir_de`].
#[derive(Default)]
struct MesureEnCours {
    acc: Option<LoudnessAccumulator>,
    dr: Option<DrAccumulator>,
    /// #2713 — mesure de la SEULE crête vraie ([`mesurer_la_crete_vraie`]) :
    /// ni sonie ni plage dynamique, seulement `crete`.
    crete_seule: bool,
    crete: Option<super::crete_vraie::CreteVraie>,
}

/// Ce qu'a donné UN segment de la mesure.
enum Segment {
    /// Le décodage a échoué : la mesure entière échoue, comme avant.
    Echec,
    /// Fin du fichier (segment vide ou plus court que demandé).
    Fin,
    /// Segment complet : le prochain commence `avance` secondes plus loin.
    Suite(f64),
}

impl MesureEnCours {
    /// Nourrir les deux accumulateurs d'un segment décodé. Synchrone, et
    /// appelé sur le pool bloquant, jamais sur un fil de l'exécuteur.
    fn nourrir(&mut self, decoded: super::decode::DecodedAudio, seg_seconds: f64) -> Segment {
        #[cfg(test)]
        tests::FILS_QUI_ONT_NOURRI
            .lock()
            .unwrap()
            .push(std::thread::current().id());
        let sample_rate = decoded.sample_rate as usize;
        let channels = decoded.channels as usize;
        if sample_rate == 0 || channels == 0 || decoded.samples_i32.is_empty() {
            return Segment::Fin; // EOF (or unreadable): done.
        }

        let scale = pcm_scale(decoded.bit_depth);
        let samples: Vec<f64> = decoded
            .samples_i32
            .iter()
            .map(|&s| s as f64 / scale)
            .collect();
        if self.crete_seule {
            // Les MÊMES échantillons que la mesure complète : la crête rendue
            // est celle de `mesurer_intensite_et_plage`, au bit près.
            self.crete
                .get_or_insert_with(|| super::crete_vraie::CreteVraie::new(sample_rate, channels))
                .nourrir(&samples);
        } else {
            self.nourrir_les_accumulateurs(&samples, sample_rate, channels);
        }

        // A segment shorter than requested means we reached the end. Advance the
        // seek by the actual decoded duration so segments stay contiguous even if
        // the decoder rounds the boundary.
        let frames = decoded.samples_i32.len() / channels;
        if (frames as f64) < seg_seconds * sample_rate as f64 {
            return Segment::Fin;
        }
        Segment::Suite(frames as f64 / sample_rate as f64)
    }

    /// Sonie, pics et plage dynamique d'un segment déjà normalisé.
    fn nourrir_les_accumulateurs(&mut self, samples: &[f64], sample_rate: usize, channels: usize) {
        self.acc
            .get_or_insert_with(|| LoudnessAccumulator::new(sample_rate, channels))
            .feed(samples);
        // LES MÊMES échantillons, déjà décodés et déjà normalisés : la plage
        // dynamique ne coûte que son arithmétique.
        self.dr
            .get_or_insert_with(|| DrAccumulator::new(sample_rate, channels))
            .feed(samples);
    }

    fn finir(self) -> Option<(f64, f64, f64, Option<u32>)> {
        let (lufs, peak, true_peak) = self.acc?.finish()?;
        // La plage dynamique est FACULTATIVE : une piste dont l'intensité se
        // mesure mais dont la plage ne se calcule pas (trop courte, un seul pic)
        // ne doit pas faire échouer toute la mesure — ReplayGain en dépend.
        Some((lufs, peak, true_peak, self.dr.and_then(|d| d.finish())))
    }
}

/// Le corps de [`mesurer_intensite_et_plage`]. `premier` : le premier segment
/// déjà décodé (seek 0, 30 s, stéréo), ou `None` pour le décoder ici.
///
/// # #5519 — le calcul part sur le pool bloquant AVEC le décodage
///
/// Seul le décodage partait en `spawn_blocking` ; la conversion en `f64`, la
/// pondération K, la crête vraie et la plage dynamique de chaque segment
/// tournaient sur le fil de l'exécuteur qui attendait ce décodage. C'était le
/// plus gros poste de la passe (0,67 s par piste sur 1,94, banc Shrek du
/// 30/09). Or la passe lance ses fichiers « à plusieurs » DANS UNE SEULE tâche
/// (`en_parallele_borne`) : ce calcul-là ne se parallélisait donc pas. À quatre
/// fichiers à la fois, la passe n'allait que 1,6 fois plus vite qu'à un seul
/// (banc étage B), et le fil de l'exécuteur restait occupé à plein — le
/// `tokio-rt-worker` à 100 % du .18 (23/09).
///
/// Désormais chaque segment est décodé PUIS accumulé dans la même tâche
/// bloquante ; les accumulateurs y entrent et en reviennent. Mêmes opérations,
/// dans le même ordre : le résultat est identique au bit près. La frontière où
/// la passe peut céder à la lecture (#2495) reste le segment.
async fn mesurer_a_partir_de(
    file_path: &str,
    premier: Option<super::decode::DecodedAudio>,
) -> Option<(f64, f64, f64, Option<u32>)> {
    let mesure = parcourir_les_segments(file_path, premier, MesureEnCours::default()).await?;
    tokio::task::spawn_blocking(move || mesure.finir())
        .await
        .ok()?
}

/// La SEULE crête vraie d'un fichier (#2713), linéaire : la valeur que
/// [`mesurer_intensite_et_plage`] rendrait en troisième position, au bit près
/// — mêmes segments, mêmes échantillons, même accumulateur — sans la sonie ni
/// la plage dynamique.
///
/// Sert au rattrapage des crêtes mesurées par l'ancien algorithme : les gains
/// déjà calculés restent valides, seul le pic est à refaire. `None` pour un
/// fichier illisible, vide ou muet.
pub async fn mesurer_la_crete_vraie(file_path: &str) -> Option<f64> {
    let mesure = parcourir_les_segments(
        file_path,
        None,
        MesureEnCours {
            crete_seule: true,
            ..MesureEnCours::default()
        },
    )
    .await?;
    let crete = mesure.crete?.crete();
    (crete > 0.0).then_some(crete)
}

/// Décoder le fichier segment par segment et nourrir `mesure`. `None` si un
/// segment ne se décode pas ou si la tâche est interrompue.
async fn parcourir_les_segments(
    file_path: &str,
    mut premier: Option<super::decode::DecodedAudio>,
    mut mesure: MesureEnCours,
) -> Option<MesureEnCours> {
    // Analyse in bounded time segments and stream them through the accumulator,
    // so memory never scales with track length. Decoding a whole long 24/192
    // track into RAM cost several GB and OOM-killed the server in a crash-loop
    // (#1109). K-weighting state is continuous across segments, so the result is
    // identical to a single whole-track pass. We decode at the native rate,
    // stereo (the native decoder does not resample), same as before.
    const SEG_SECONDS: f64 = 30.0;
    // Defense in depth (#1277): even if a decoder ever ignores a failed seek and
    // keeps returning the head of the track, no analysis loop may run forever.
    // 24h is far beyond any real track, so this never truncates legitimate input
    // — it only fires on a non-progressing decoder.
    const MAX_ANALYSIS_SECONDS: f64 = 24.0 * 3600.0;

    let mut seek = 0.0_f64;

    loop {
        if seek > MAX_ANALYSIS_SECONDS {
            warn!(file = file_path, seek, "loudness_analysis_seek_cap_hit");
            break;
        }
        let path = file_path.to_string();
        let deja_decode = premier.take();
        let (rendue, segment) = tokio::task::spawn_blocking(move || {
            // #4681 — E/S basses si une zone joue (segment déjà parti).
            let _basse = crate::taches_de_fond::priorite::politique::baisser_pendant_la_lecture();
            let decoded = match deja_decode {
                Some(d) => d,
                None => match super::decode::decode_to_pcm(&path, None, Some(2), seek, SEG_SECONDS)
                {
                    Ok(d) => d,
                    Err(_) => return (mesure, Segment::Echec),
                },
            };
            let segment = mesure.nourrir(decoded, SEG_SECONDS);
            (mesure, segment)
        })
        .await
        .ok()?;
        mesure = rendue;
        match segment {
            Segment::Echec => return None,
            Segment::Fin => break,
            Segment::Suite(avance) => seek += avance,
        }
    }
    Some(mesure)
}

// ---------------------------------------------------------------------------
// Trailing silence detection (pure Rust)
// ---------------------------------------------------------------------------

/// Detect trailing silence duration in seconds.
///
/// Scans backwards from the end of the file to find the last sample whose
/// absolute amplitude exceeds `threshold_db` (a negative dB value, e.g. -50).
pub async fn detect_trailing_silence(file_path: &str, threshold_db: f64) -> f64 {
    // Streamed in segments so a long track is never decoded into RAM at once
    // (same OOM class as the loudness pass, #1109). Forward scan tracking the
    // index of the last sample above threshold — equivalent to the old backward
    // scan over the whole buffer.
    const SEG_SECONDS: f64 = 30.0;
    // Defense in depth (#1277): mirror the loudness pass — never let a
    // non-progressing decoder spin this loop forever. See measure_loudness_and_peak.
    const MAX_ANALYSIS_SECONDS: f64 = 24.0 * 3600.0;
    let threshold_linear = 10.0_f64.powf(threshold_db / 20.0);

    let mut sample_rate = 0.0_f64;
    let mut total: usize = 0;
    let mut last_loud: Option<usize> = None;
    let mut seek = 0.0_f64;

    loop {
        if seek > MAX_ANALYSIS_SECONDS {
            warn!(file = file_path, seek, "trailing_silence_seek_cap_hit");
            break;
        }
        let path = file_path.to_string();
        let decoded = match tokio::task::spawn_blocking(move || {
            super::decode::decode_to_pcm(&path, None, Some(1), seek, SEG_SECONDS)
        })
        .await
        {
            Ok(Ok(d)) => d,
            _ => break,
        };

        let sr = decoded.sample_rate as usize;
        if sr == 0 || decoded.samples_i32.is_empty() {
            break;
        }
        sample_rate = sr as f64;
        // Le contrat #2230 rend normalement du mono. On compte néanmoins des
        // trames à partir des métadonnées réellement rendues : une régression
        // de l'adaptation ne doit jamais redoubler silencieusement la durée.
        let canaux = decoded.channels.max(1) as usize;
        let scale = pcm_scale(decoded.bit_depth);
        for (trame, bloc) in decoded.samples_i32.chunks(canaux).enumerate() {
            // Une trame est sonore dès qu'UN de ses canaux l'est : un silence
            // sur le seul canal gauche n'est pas un silence.
            if bloc
                .iter()
                .any(|&s| (s as f64 / scale).abs() > threshold_linear)
            {
                last_loud = Some(total + trame);
            }
        }
        let frames = decoded.samples_i32.len() / canaux;
        total += frames;
        if (frames as f64) < SEG_SECONDS * sr as f64 {
            break;
        }
        seek += frames as f64 / sr as f64;
    }

    if sample_rate <= 0.0 || total == 0 {
        return 0.0;
    }
    match last_loud {
        Some(pos) => (total - 1 - pos) as f64 / sample_rate,
        None => total as f64 / sample_rate, // entire file is silent
    }
}

pub async fn detect_bpm(file_path: &str) -> Option<f64> {
    let sample_rate: u32 = 22050;
    let duration = 30;

    let file_duration = get_duration(file_path).await.ok()?;
    if file_duration <= 0.0 {
        return None;
    }

    let start = (file_duration / 2.0 - duration as f64 / 2.0).max(0.0);
    let pcm = decode_pcm(file_path, sample_rate, 1, start, duration as f64)
        .await
        .ok()?;

    if pcm.len() < (sample_rate as usize * 2 * 2) {
        warn!(file = file_path, "bpm_too_short");
        return None;
    }

    let samples: Vec<f64> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64)
        .collect();

    // Energy envelope via moving average
    let window = 2048_usize;
    let envelope: Vec<f64> = samples.iter().map(|s| s.abs()).collect();
    let mut running_sum: f64 = envelope[..window.min(envelope.len())].iter().sum();
    let len = envelope.len();
    let mut smoothed = vec![0.0_f64; len];
    for i in 0..len {
        smoothed[i] = running_sum / window as f64;
        if i + window < len {
            running_sum += envelope[i + window];
        }
        if i >= window {
            running_sum -= envelope[i - window];
        }
    }
    let mut envelope = smoothed;

    // Remove DC offset
    let mean: f64 = envelope.iter().sum::<f64>() / envelope.len() as f64;
    for v in &mut envelope {
        *v -= mean;
    }

    // Autocorrelation for BPM range 60-200
    let min_lag = (60 * sample_rate as usize) / 200; // 200 BPM
    let max_lag = ((60 * sample_rate as usize) / 60).min(envelope.len() - 1); // 60 BPM
    if min_lag >= max_lag {
        return None;
    }

    let mut best_lag = min_lag;
    let mut best_corr = f64::NEG_INFINITY;
    for lag in min_lag..max_lag {
        let mut corr = 0.0_f64;
        let count = envelope.len() - lag;
        for i in 0..count {
            corr += envelope[i] * envelope[i + lag];
        }
        if corr > best_corr {
            best_corr = corr;
            best_lag = lag;
        }
    }

    let bpm = (60.0 * sample_rate as f64 / best_lag as f64).round();
    if !(40.0..=220.0).contains(&bpm) {
        debug!(file = file_path, bpm, "bpm_out_of_range");
        return None;
    }

    info!(file = file_path, bpm, "bpm_detected");
    Some(bpm)
}

pub async fn generate_waveform(file_path: &str, points: usize) -> Vec<f32> {
    let sample_rate = 22050_u32;

    let pcm = match decode_pcm(file_path, sample_rate, 1, 0.0, 0.0).await {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    let samples: Vec<f64> = pcm
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64)
        .collect();

    if samples.len() < points {
        return Vec::new();
    }

    let frame_size = samples.len() / points;
    let mut rms_values: Vec<f64> = (0..points)
        .map(|i| {
            let start = i * frame_size;
            let end = start + frame_size;
            let frame = &samples[start..end];
            let mean_sq = frame.iter().map(|s| s * s).sum::<f64>() / frame.len() as f64;
            mean_sq.sqrt()
        })
        .collect();

    let max_rms = rms_values.iter().cloned().fold(0.0_f64, f64::max);
    if max_rms > 0.0 {
        for v in &mut rms_values {
            *v /= max_rms;
        }
    }

    rms_values
        .iter()
        .map(|v| (*v as f32 * 10000.0).round() / 10000.0)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un WAV 16 bits stéréo à 44,1 kHz dont l'enveloppe varie (l'empreinte
    /// a donc quelque chose à dire), plus long que la fenêtre d'empreinte.
    fn ecrire_wav_module(path: &std::path::Path, secondes: usize) {
        const HZ: usize = 44_100;
        let frames = HZ * secondes;
        let mut donnees = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let t = i as f64 / HZ as f64;
            let enveloppe =
                0.2 + 0.7 * (0.5 + 0.5 * (t * 1.3).sin()) * (0.5 + 0.5 * (t * 0.17).cos());
            let g = (enveloppe * (t * 440.0 * std::f64::consts::TAU).sin() * 30_000.0) as i16;
            let d = (enveloppe * (t * 660.0 * std::f64::consts::TAU).sin() * 20_000.0) as i16;
            donnees.extend_from_slice(&g.to_le_bytes());
            donnees.extend_from_slice(&d.to_le_bytes());
        }
        let mut w = Vec::with_capacity(donnees.len() + 44);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&(HZ as u32).to_le_bytes());
        w.extend_from_slice(&((HZ * 4) as u32).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(&donnees);
        std::fs::write(path, w).unwrap();
    }

    fn bits(m: Option<(f64, f64, f64, Option<u32>)>) -> Option<(u64, u64, u64, Option<u32>)> {
        m.map(|(a, b, c, d)| (a.to_bits(), b.to_bits(), c.to_bits(), d))
    }

    /// #5519 — la mesure partagée (un décodage de tête pour la mesure ET
    /// l'empreinte) rend les MÊMES valeurs, au bit près, que les deux appels
    /// séparés d'avant — sur une piste plus longue que la fenêtre d'empreinte
    /// (le préfixe et les segments suivants comptent) et sur une piste plus
    /// courte qu'un segment.
    #[tokio::test]
    async fn la_mesure_partagee_rend_les_memes_valeurs_au_bit_pres() {
        let dir = tempfile::TempDir::new().unwrap();
        for secondes in [95usize, 20] {
            let f = dir.path().join(format!("module_{secondes}.wav"));
            ecrire_wav_module(&f, secondes);
            let chemin = f.to_str().unwrap();

            let avant_mesure = mesurer_intensite_et_plage(chemin).await;
            let avant_empreinte = crate::audio::empreinte::empreinte_du_fichier(chemin);
            assert!(
                avant_mesure.is_some(),
                "{secondes} s : la mesure de référence existe"
            );
            assert!(
                matches!(avant_empreinte, Ok(Some(_))),
                "{secondes} s : l'empreinte de référence existe"
            );

            let partagee = mesurer_intensite_plage_et_empreinte(chemin).await;
            assert_eq!(
                bits(partagee.mesure),
                bits(avant_mesure),
                "{secondes} s : la mesure partagée diffère de la mesure séparée"
            );
            assert_eq!(
                partagee.empreinte,
                Some(avant_empreinte),
                "{secondes} s : l'empreinte partagée diffère de empreinte_du_fichier"
            );
        }
    }

    /// Les fils sur lesquels [`MesureEnCours::nourrir`] a tourné (#5519).
    pub(super) static FILS_QUI_ONT_NOURRI: std::sync::Mutex<Vec<std::thread::ThreadId>> =
        std::sync::Mutex::new(Vec::new());

    /// #5519 — le calcul de la mesure (conversion, pondération K, crête vraie,
    /// plage dynamique) tourne sur le pool BLOQUANT, jamais sur le fil de
    /// l'exécuteur qui attend le décodage.
    ///
    /// Sur un exécuteur `current_thread`, tout ce qui est `async` tourne sur
    /// le fil du test ; `spawn_blocking` part ailleurs. Si le calcul revenait
    /// dans le corps `async`, il tournerait sur ce fil-ci — et la passe, qui
    /// lance ses fichiers « à plusieurs » dans UNE tâche, ne paralléliserait
    /// plus que le décodage (1,6× à quatre fichiers, banc du 30/09).
    #[tokio::test(flavor = "current_thread")]
    async fn le_calcul_de_la_mesure_ne_tourne_pas_sur_le_fil_de_l_executeur() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("module_75.wav");
        ecrire_wav_module(&f, 75);
        let chemin = f.to_str().unwrap();
        let ici = std::thread::current().id();

        let mesure = mesurer_intensite_et_plage(chemin).await;
        let partagee = mesurer_intensite_plage_et_empreinte(chemin).await;
        assert!(
            mesure.is_some() && partagee.mesure.is_some(),
            "les deux mesures existent"
        );
        assert_eq!(bits(partagee.mesure), bits(mesure));

        let fils = FILS_QUI_ONT_NOURRI.lock().unwrap().clone();
        assert!(
            !fils.is_empty(),
            "le témoin doit avoir vu des segments nourrir les accumulateurs"
        );
        assert!(
            !fils.contains(&ici),
            "#5519 : le calcul d'un segment a tourné sur le fil de l'exécuteur \
             ({} segments sur {} nourris ici) — il doit partir sur le pool bloquant \
             avec le décodage",
            fils.iter().filter(|t| **t == ici).count(),
            fils.len()
        );
    }

    /// Un format hors du chemin partagé garde ses deux décodages : pas
    /// d'empreinte tirée d'ici, l'appelant la calcule à part.
    #[test]
    fn le_dsd_n_est_pas_sur_le_chemin_partage() {
        assert!(!empreinte_partageable("/m/a.dsf"));
        assert!(!empreinte_partageable("/m/a.ape"));
        assert!(!empreinte_partageable("/m/a.opus"));
        assert!(empreinte_partageable("/m/a.FLAC"));
    }

    /// #5519 — la somme des carrés par tranches rend la valeur de l'ancienne
    /// forme AU BIT PRÈS, y compris quand la file fait le tour de son tampon
    /// (deux tranches) et quand `n` ne prend qu'une partie de la seconde.
    /// Une accélération qui changerait le dernier bit changerait, sur une
    /// valeur arrondie au dixième, le gain d'une piste de temps en temps.
    #[test]
    fn la_somme_des_carres_par_tranches_est_identique_au_bit_pres() {
        let mut graine: u64 = 0x5519;
        let mut suivant = || {
            graine = graine
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((graine >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        let mut file: std::collections::VecDeque<f64> =
            std::collections::VecDeque::with_capacity(64);
        let mut tours_avec_deux_tranches = 0;
        for tour in 0..2_000 {
            file.push_back(suivant());
            if file.len() > 48 {
                file.drain(..(tour % 7 + 1).min(file.len()));
            }
            if !file.as_slices().1.is_empty() {
                tours_avec_deux_tranches += 1;
            }
            for n in [0, 1, file.len() / 2, file.len()] {
                let attendu: f64 = file.iter().take(n).map(|s| s * s).sum::<f64>();
                let obtenu = somme_des_carres(&file, n);
                assert_eq!(
                    attendu.to_bits(),
                    obtenu.to_bits(),
                    "tour {tour}, n = {n} : {attendu:e} ≠ {obtenu:e}"
                );
            }
        }
        assert!(
            tours_avec_deux_tranches > 100,
            "la file doit avoir fait le tour de son tampon, sinon la seconde tranche n'est pas éprouvée"
        );
    }

    // -----------------------------------------------------------------------
    // Biquad filter tests
    // -----------------------------------------------------------------------

    #[test]
    fn biquad_passthrough() {
        // Unity filter: b0=1, b1=b2=a1=a2=0 → output = input
        let mut bq = Biquad::new(1.0, 0.0, 0.0, 0.0, 0.0);
        assert!((bq.process(1.0) - 1.0).abs() < 1e-12);
        assert!((bq.process(0.5) - 0.5).abs() < 1e-12);
        assert!((bq.process(-0.3) - (-0.3)).abs() < 1e-12);
    }

    #[test]
    fn biquad_impulse_response() {
        // Simple 1-sample delay: b0=0, b1=1, rest=0 → y[n] = x[n-1]
        let mut bq = Biquad::new(0.0, 1.0, 0.0, 0.0, 0.0);
        assert!((bq.process(1.0) - 0.0).abs() < 1e-12);
        assert!((bq.process(0.0) - 1.0).abs() < 1e-12);
        assert!((bq.process(0.0) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn biquad_reset() {
        let mut bq = Biquad::new(0.5, 0.3, 0.1, -0.2, 0.1);
        bq.process(1.0);
        bq.process(0.5);
        bq.reset();
        assert_eq!(bq.z1, 0.0);
        assert_eq!(bq.z2, 0.0);
    }

    // -----------------------------------------------------------------------
    // K-weighting coefficient tests
    // -----------------------------------------------------------------------

    #[test]
    fn k_weighting_48khz_matches_reference() {
        // Verify that our coefficient computation for 48 kHz matches the
        // published ITU-R BS.1770-4 reference values (within tolerance).
        let (s1, s2) = k_weighting_coefficients(48000.0);

        // Stage 1 reference (from ITU-R BS.1770-4 Table 1)
        assert!((s1.b0 - 1.53512485958697).abs() < 1e-6, "s1.b0={}", s1.b0);
        assert!(
            (s1.b1 - (-2.69169618940638)).abs() < 1e-6,
            "s1.b1={}",
            s1.b1
        );
        assert!((s1.b2 - 1.19839281085285).abs() < 1e-6, "s1.b2={}", s1.b2);
        assert!(
            (s1.a1 - (-1.69065929318241)).abs() < 1e-6,
            "s1.a1={}",
            s1.a1
        );
        assert!((s1.a2 - 0.73248077421585).abs() < 1e-6, "s1.a2={}", s1.a2);

        // Stage 2 reference (ITU-R table lists unnormalized b; we normalize by a0)
        // a0 = 1 + k/Q + k^2 for 48 kHz ≈ 1.004993
        // So b0_norm = 1/a0, b1_norm = -2/a0, b2_norm = 1/a0
        // a1_norm and a2_norm match the table directly.
        let a0_s2 = 1.0 / s2.b0; // recover a0 from normalized b0 = 1/a0
        assert!((a0_s2 * s2.b0 - 1.0).abs() < 1e-10, "b0 * a0 should be 1.0");
        assert!(
            (a0_s2 * s2.b1 - (-2.0)).abs() < 1e-6,
            "unnormalized b1 should be -2.0, got {}",
            a0_s2 * s2.b1
        );
        assert!(
            (a0_s2 * s2.b2 - 1.0).abs() < 1e-6,
            "unnormalized b2 should be 1.0, got {}",
            a0_s2 * s2.b2
        );
        assert!(
            (s2.a1 - (-1.99004745483398)).abs() < 1e-6,
            "s2.a1={}",
            s2.a1
        );
        assert!((s2.a2 - 0.99007225036621).abs() < 1e-6, "s2.a2={}", s2.a2);
    }

    #[test]
    fn k_weighting_44100_produces_valid_coefficients() {
        let (s1, s2) = k_weighting_coefficients(44100.0);
        // Coefficients should be finite and reasonable
        assert!(s1.b0.is_finite() && s1.b0 > 0.0);
        assert!(s2.b0.is_finite() && s2.b0 > 0.0);
        // a2 should be < 1 for stability
        assert!(s1.a2.abs() < 2.0);
        assert!(s2.a2.abs() < 2.0);
    }

    // -----------------------------------------------------------------------
    // Integrated loudness tests (synthetic signals)
    // -----------------------------------------------------------------------

    #[test]
    fn loudness_of_silence_is_none() {
        // Silence should gate out entirely → None
        let samples = vec![0i16; 48000 * 2]; // 0.5s stereo silence at 48kHz
        let result = compute_loudness_from_samples(&samples, 48000, 2);
        assert!(
            result.is_none(),
            "pure silence should return None, got {:?}",
            result
        );
    }

    #[test]
    fn loudness_of_full_scale_sine() {
        // A full-scale 1 kHz sine at 48 kHz, 2 channels, 2 seconds.
        //
        // Per EBU R128 / ITU-R BS.1770-4:
        // - Each channel: RMS^2 of sine = 0.5, K-weighting gain at 1 kHz ≈ 0 dB
        // - Stereo sum: G_L * z_L + G_R * z_R = 1.0 * 0.5 + 1.0 * 0.5 = 1.0
        // - LUFS = -0.691 + 10*log10(1.0) = -0.691 ≈ -0.7 LUFS
        let sr = 48000_usize;
        let duration_s = 2.0;
        let num_frames = (sr as f64 * duration_s) as usize;
        let freq = 1000.0;

        let mut samples = Vec::with_capacity(num_frames * 2);
        for i in 0..num_frames {
            let t = i as f64 / sr as f64;
            let val = (2.0 * std::f64::consts::PI * freq * t).sin();
            let s = (val * 32767.0) as i16;
            samples.push(s); // L
            samples.push(s); // R
        }

        let lufs = compute_loudness_from_samples(&samples, sr, 2);
        assert!(lufs.is_some(), "should produce a loudness value");
        let lufs = lufs.unwrap();
        // Dual-mono 0 dBFS sine → ~-0.7 LUFS (two channels summed)
        // Allow ±1.0 dB tolerance for quantization and edge effects
        assert!(
            lufs > -2.0 && lufs < 0.5,
            "expected ~-0.7 LUFS for dual-mono 0dBFS sine, got {}",
            lufs
        );
    }

    #[test]
    fn loudness_decreases_with_amplitude() {
        let sr = 48000_usize;
        let num_frames = sr * 2; // 2 seconds

        let make_sine = |amplitude: f64| -> Vec<i16> {
            let mut samples = Vec::with_capacity(num_frames * 2);
            for i in 0..num_frames {
                let t = i as f64 / sr as f64;
                let val = (2.0 * std::f64::consts::PI * 1000.0 * t).sin() * amplitude;
                let s = (val * 32767.0) as i16;
                samples.push(s);
                samples.push(s);
            }
            samples
        };

        let loud = compute_loudness_from_samples(&make_sine(1.0), sr, 2).unwrap();
        let quiet = compute_loudness_from_samples(&make_sine(0.1), sr, 2).unwrap();

        assert!(
            quiet < loud,
            "quieter signal should have lower LUFS: loud={}, quiet={}",
            loud,
            quiet
        );
        // 20 dB amplitude difference → ~20 dB loudness difference
        let diff = loud - quiet;
        assert!(
            diff > 15.0 && diff < 25.0,
            "expected ~20 dB difference, got {}",
            diff
        );
    }

    /// Helper: compute integrated loudness from raw i16 interleaved samples.
    /// Used by tests to avoid needing actual audio files.
    fn compute_loudness_from_samples(
        raw_samples: &[i16],
        sample_rate: usize,
        channels: usize,
    ) -> Option<f64> {
        if sample_rate == 0 || channels == 0 || raw_samples.is_empty() {
            return None;
        }

        let samples: Vec<f64> = raw_samples.iter().map(|&s| s as f64 / 32768.0).collect();
        let num_frames = samples.len() / channels;

        let mut ch_bufs: Vec<Vec<f64>> = (0..channels)
            .map(|c| (0..num_frames).map(|f| samples[f * channels + c]).collect())
            .collect();

        let fs = sample_rate as f64;
        for ch in &mut ch_bufs {
            let (mut stage1, mut stage2) = k_weighting_coefficients(fs);
            for s in ch.iter_mut() {
                *s = stage1.process(*s);
                *s = stage2.process(*s);
            }
        }

        let block_frames = (sample_rate as f64 * 0.4) as usize;
        let step_frames = (sample_rate as f64 * 0.1) as usize;

        if block_frames == 0 || step_frames == 0 || num_frames < block_frames {
            let mut power_sum = 0.0;
            for ch in &ch_bufs {
                let ms: f64 = ch.iter().map(|s| s * s).sum::<f64>() / ch.len() as f64;
                power_sum += ms;
            }
            if power_sum <= 0.0 {
                return None;
            }
            let lufs = -0.691 + 10.0 * power_sum.log10();
            return Some((lufs * 10.0).round() / 10.0);
        }

        let mut block_powers: Vec<f64> = Vec::new();
        let mut start = 0;
        while start + block_frames <= num_frames {
            let mut power_sum = 0.0;
            for ch in &ch_bufs {
                let block = &ch[start..start + block_frames];
                let ms: f64 = block.iter().map(|s| s * s).sum::<f64>() / block_frames as f64;
                power_sum += ms;
            }
            block_powers.push(power_sum);
            start += step_frames;
        }

        if block_powers.is_empty() {
            return None;
        }

        let abs_threshold = 10.0_f64.powf((-70.0 + 0.691) / 10.0);
        let gated_abs: Vec<f64> = block_powers
            .iter()
            .copied()
            .filter(|&p| p > abs_threshold)
            .collect();

        if gated_abs.is_empty() {
            return None;
        }

        let mean_abs: f64 = gated_abs.iter().sum::<f64>() / gated_abs.len() as f64;
        let rel_threshold = mean_abs * 10.0_f64.powf(-10.0 / 10.0);

        let gated_rel: Vec<f64> = block_powers
            .iter()
            .copied()
            .filter(|&p| p > rel_threshold)
            .collect();

        if gated_rel.is_empty() {
            return None;
        }

        let mean_rel: f64 = gated_rel.iter().sum::<f64>() / gated_rel.len() as f64;
        if mean_rel <= 0.0 {
            return None;
        }

        let lufs = -0.691 + 10.0 * mean_rel.log10();
        Some((lufs * 10.0).round() / 10.0)
    }

    /// The streaming `LoudnessAccumulator` must (a) match the reference batch math
    /// exactly when fed in one shot, and (b) be invariant to chunking — the two
    /// guarantees that let the file path stream in bounded memory (#1109) without
    /// changing any ReplayGain value.
    #[test]
    fn accumulator_matches_reference_and_is_chunk_invariant() {
        let sr = 48_000usize;
        let n = sr * 3; // 3 s → many 400 ms / 100 ms blocks
        let mut i16s: Vec<i16> = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            // Loud for 2 s then quiet, to exercise absolute + relative gating.
            let a = if t < 2.0 { 0.5 } else { 0.02 };
            let v = (a * (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 32767.0) as i16;
            i16s.push(v); // L
            i16s.push(v); // R
        }
        let f64s: Vec<f64> = i16s.iter().map(|&s| s as f64 / 32768.0).collect();

        let reference = compute_loudness_from_samples(&i16s, sr, 2).unwrap();

        let one_shot = integrated_loudness_from_samples(&f64s, sr, 2).unwrap();
        assert!(
            (one_shot - reference).abs() < 1e-9,
            "one-shot {one_shot} != reference {reference}"
        );

        // Feed in odd, frame-aligned chunks that cross block/step boundaries.
        let mut acc = LoudnessAccumulator::new(sr, 2);
        for chunk in f64s.chunks(777 * 2) {
            acc.feed(chunk);
        }
        let (chunked, _, _) = acc.finish().unwrap();
        assert!(
            (chunked - reference).abs() < 1e-9,
            "chunked {chunked} != reference {reference}"
        );
    }

    // -----------------------------------------------------------------------
    // #1694 — true peak inter-échantillons ; BS.1770 annexe 2 depuis #2713
    // -----------------------------------------------------------------------

    /// Le cas d'école de l'over inter-échantillons : une sinusoïde à fs/4
    /// déphasée de π/4 n'est échantillonnée QUE sur ±0,707 alors que le
    /// signal continu culmine à 1,0. Le sample peak la sous-estime de 3 dB ;
    /// le true peak doit retrouver la crête manquée, à 0,1 dB près depuis
    /// #2713 (l'ancien Catmull-Rom 4× n'en retrouvait que ~0,88, −1,1 dB).
    /// La conformité détaillée est gardée par `crete_vraie_tests.rs`.
    #[test]
    fn true_peak_sees_the_inter_sample_over_that_sample_peak_misses() {
        let sr = 48_000usize;
        let n = sr; // 1 s
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = (std::f64::consts::PI / 4.0
                + 2.0 * std::f64::consts::PI * (sr as f64 / 4.0) * i as f64 / sr as f64)
                .sin();
            samples.push(v); // L
            samples.push(v); // R
        }
        let mut acc = LoudnessAccumulator::new(sr, 2);
        acc.feed(&samples);
        let (_, peak, true_peak) = acc.finish().unwrap();

        // 1/√2 : c'est EXACTEMENT ce que le test veut dire — une sinusoïde à
        // fs/4 déphasée de π/4 n'est échantillonnée que sur ±1/√2, soit
        // −3 dB sous le vrai maximum du signal continu.
        assert!(
            (peak - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "sample peak {peak}"
        );
        assert!(
            true_peak > 10f64.powf(-0.1 / 20.0),
            "le true peak doit retrouver la crête à 0,1 dB près : {true_peak}"
        );
        assert!(
            true_peak >= peak,
            "le true peak englobe le sample peak par construction"
        );
    }

    /// L'histoire d'interpolation traverse les appels `feed` : nourrir le
    /// même signal en morceaux impairs doit rendre EXACTEMENT le même true
    /// peak qu'en un seul passage — même garantie que la sonie (#1109).
    #[test]
    fn true_peak_is_chunk_invariant() {
        let sr = 48_000usize;
        let n = sr / 2;
        let mut samples = Vec::with_capacity(n * 2);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = 0.9 * (2.0 * std::f64::consts::PI * 11_987.0 * t).sin();
            samples.push(v);
            samples.push(v * 0.5);
        }
        let mut one = LoudnessAccumulator::new(sr, 2);
        one.feed(&samples);
        let (_, _, tp_one) = one.finish().unwrap();

        let mut chunked = LoudnessAccumulator::new(sr, 2);
        for chunk in samples.chunks(101 * 2) {
            chunked.feed(chunk);
        }
        let (_, _, tp_chunked) = chunked.finish().unwrap();

        assert!(
            (tp_one - tp_chunked).abs() < 1e-12,
            "one-shot {tp_one} != chunked {tp_chunked}"
        );
    }

    // -----------------------------------------------------------------------
    // Trailing silence detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn trailing_silence_all_silent() {
        // All zeros → entire duration is silence
        let samples = vec![0i16; 44100]; // 1s mono
        let threshold_linear = 10.0_f64.powf(-50.0 / 20.0);
        let last_loud = samples
            .iter()
            .rposition(|&s| (s as f64 / 32768.0).abs() > threshold_linear);
        assert!(last_loud.is_none());
    }

    #[test]
    fn trailing_silence_no_silence() {
        // Signal everywhere → 0 trailing silence
        let sr = 44100;
        let samples: Vec<i16> = (0..sr)
            .map(|i| {
                let t = i as f64 / sr as f64;
                ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16000.0) as i16
            })
            .collect();

        let threshold_linear = 10.0_f64.powf(-50.0 / 20.0);
        let last_loud = samples
            .iter()
            .rposition(|&s| (s as f64 / 32768.0).abs() > threshold_linear);

        assert!(last_loud.is_some());
        let silence_frames = samples.len() - 1 - last_loud.unwrap();
        let silence_s = silence_frames as f64 / sr as f64;
        assert!(
            silence_s < 0.01,
            "should have negligible trailing silence, got {}",
            silence_s
        );
    }

    #[test]
    fn trailing_silence_half_second() {
        // 0.5s of signal + 0.5s of silence = 0.5s trailing silence
        let sr = 44100_usize;
        let mut samples: Vec<i16> = Vec::with_capacity(sr);

        // First half: signal
        for i in 0..sr / 2 {
            let t = i as f64 / sr as f64;
            let val = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 16000.0;
            samples.push(val as i16);
        }
        // Second half: silence
        samples.extend(vec![0i16; sr / 2]);

        let threshold_linear = 10.0_f64.powf(-50.0 / 20.0);
        let last_loud = samples
            .iter()
            .rposition(|&s| (s as f64 / 32768.0).abs() > threshold_linear);

        assert!(last_loud.is_some());
        let silence_s = (samples.len() - 1 - last_loud.unwrap()) as f64 / sr as f64;
        assert!(
            (silence_s - 0.5).abs() < 0.02,
            "expected ~0.5s trailing silence, got {}",
            silence_s
        );
    }

    // -----------------------------------------------------------------------
    // Existing tests (preserved)
    // -----------------------------------------------------------------------

    #[test]
    fn waveform_normalize() {
        let rms = [0.5_f64, 1.0, 0.25];
        let max = rms.iter().cloned().fold(0.0_f64, f64::max);
        let normalized: Vec<f32> = rms.iter().map(|v| (v / max) as f32).collect();
        assert!((normalized[0] - 0.5).abs() < 0.01);
        assert!((normalized[1] - 1.0).abs() < 0.01);
        assert!((normalized[2] - 0.25).abs() < 0.01);
    }

    #[test]
    fn bpm_range_validation() {
        assert!((40.0..=220.0).contains(&120.0));
        assert!(!(40.0..=220.0).contains(&300.0));
        assert!(!(40.0..=220.0).contains(&10.0));
    }

    #[test]
    fn pcm_format_parse() {
        let bytes: [u8; 4] = [0x00, 0x40, 0x00, 0xC0]; // 16384, -16384
        let samples: Vec<i16> = bytes
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(samples, vec![16384, -16384]);
    }

    #[tokio::test]
    async fn decode_pcm_rend_toujours_des_trames_i16() {
        let wav_file = tempfile::Builder::new().suffix(".wav").tempfile().unwrap();
        let path = wav_file.path().to_path_buf();
        let source = [0x7f_ffffi32, -0x80_0000i32, 0x12_3456i32, -0x12_3456i32];
        let mut data = Vec::new();
        for sample in source {
            data.extend_from_slice(&sample.to_le_bytes()[..3]);
        }
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36u32 + data.len() as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&22_050u32.to_le_bytes());
        wav.extend_from_slice(&(22_050u32 * 3).to_le_bytes());
        wav.extend_from_slice(&3u16.to_le_bytes());
        wav.extend_from_slice(&24u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
        wav.extend_from_slice(&data);
        std::fs::write(&path, wav).unwrap();

        let pcm = decode_pcm(path.to_str().unwrap(), 22_050, 1, 0.0, 0.0)
            .await
            .unwrap();
        let samples: Vec<i16> = pcm
            .chunks_exact(2)
            .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();

        assert_eq!(pcm.len(), 8, "quatre trames mono i16 = huit octets");
        assert_eq!(samples, vec![32_767, -32_768, 0x1234, -0x1235]);
    }

    #[test]
    fn moving_average_smoothing() {
        let data = [0.0, 0.0, 10.0, 0.0, 0.0];
        let window = 3_usize;
        let smoothed: Vec<f64> = (0..data.len())
            .map(|i| {
                let start = i.saturating_sub(window / 2);
                let end = (i + window / 2 + 1).min(data.len());
                let slice = &data[start..end];
                slice.iter().sum::<f64>() / slice.len() as f64
            })
            .collect();
        assert!(smoothed[2] < 10.0);
        assert!(smoothed[2] > 0.0);
    }

    // ── Plage dynamique (DR) ───────────────────────────────────────────────
    //
    // 🔴 CES TÉMOINS ONT DES RÉPONSES CONNUES D'AVANCE, calculées à la main
    // depuis la définition de l'algorithme — pas relevées sur la sortie du
    // code. Un témoin qui inscrit ce que le code produit ne garde rien : il
    // resterait vert si l'algorithme dérivait de 3 dB, ce qui est exactement
    // l'erreur qu'on risque ici (le facteur 2 du RMS référencé sinus).

    /// Un signal de synthèse : `n` secondes à `sr` Hz, stéréo entrelacé.
    fn signal(sr: usize, secondes: f64, mut f: impl FnMut(f64) -> f64) -> Vec<f64> {
        let trames = (sr as f64 * secondes) as usize;
        let mut v = Vec::with_capacity(trames * 2);
        for i in 0..trames {
            let x = f(i as f64 / sr as f64);
            v.push(x);
            v.push(x);
        }
        v
    }

    fn dr_de(sr: usize, samples: &[f64]) -> Option<u32> {
        let mut acc = DrAccumulator::new(sr, 2);
        acc.feed(samples);
        acc.finish()
    }

    /// 🔴 LE TÉMOIN QUI TIENT LE FACTEUR 2.
    ///
    /// Un sinus pur a `RMS = A/√2` ; le RMS *référencé sinus* le multiplie par
    /// √2 et rend `A`, soit exactement son pic — donc **DR = 0**. Sans ce
    /// facteur le résultat serait 3, et TOUT le barème glisserait de 3 dB par
    /// rapport aux valeurs publiées par les autres mesureurs.
    #[test]
    fn un_sinus_pur_a_une_plage_dynamique_nulle() {
        let sr = 8_000;
        let s = signal(sr, 30.0, |t| (2.0 * std::f64::consts::PI * 440.0 * t).sin());
        assert_eq!(dr_de(sr, &s), Some(0));
    }

    /// 🔴 LA PROPRIÉTÉ CENTRALE : le DR ne mesure PAS le volume.
    ///
    /// Le même sinus vingt décibels plus bas garde la même plage dynamique.
    /// Un code qui confondrait plage et niveau passerait le témoin précédent
    /// et échouerait ici — c'est la contre-épreuve du premier.
    #[test]
    fn la_plage_dynamique_ne_depend_pas_du_volume() {
        let sr = 8_000;
        let fort = signal(sr, 30.0, |t| (2.0 * std::f64::consts::PI * 440.0 * t).sin());
        let faible = signal(sr, 30.0, |t| {
            0.1 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        });
        assert_eq!(dr_de(sr, &fort), dr_de(sr, &faible));
        assert_eq!(dr_de(sr, &faible), Some(0));
    }

    /// 🔴 CE QUE LE DR MESURE VRAIMENT — le FACTEUR DE CRÊTE, pas le contraste.
    ///
    /// Première version de ce témoin : 6 s à pleine échelle puis 24 s à
    /// −40 dB, en attendant « une grande plage ». Il rendait **0**, et le code
    /// avait raison. Les 20 % de blocs retenus sont les blocs FORTS ; dans ces
    /// blocs-là le signal est un sinus pur, donc pic = RMS, donc DR nul. Un
    /// disque bruyamment compressé suivi d'un passage calme n'a pas une grande
    /// plage dynamique : il a deux volumes.
    ///
    /// Ce que le DR voit, c'est l'écart entre les crêtes et le corps du son À
    /// L'INTÉRIEUR des passages les plus forts — une frappe de batterie qui
    /// dépasse le lit sonore. D'où ce signal : 1 % de pleine échelle, le reste
    /// à −40 dB, DANS CHAQUE bloc.
    ///
    /// La valeur attendue se calcule à la main :
    ///   moyenne(x²) ≈ 0,01·0,5 + 0,99·(0,01²/2) ≈ 0,00505
    ///   RMS = √(2 · 0,00505) ≈ 0,1005    pic₂ = 1
    ///   DR = 20·log₁₀(1 / 0,1005) ≈ 20
    #[test]
    fn le_facteur_de_crete_donne_une_grande_plage() {
        let sr = 8_000;
        let bloc = 3.0;
        let s = signal(sr, 30.0, |t| {
            // Position dans le bloc de 3 s courant.
            let dans_le_bloc = t % bloc;
            let a = if dans_le_bloc < bloc * 0.01 {
                1.0
            } else {
                0.01
            };
            a * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        });
        let dr = dr_de(sr, &s).expect("une plage doit se mesurer");
        // ±2 autour de 20 : le découpage aux bords des blocs et l'arrondi
        // déplacent la valeur, l'ordre de grandeur est ce qui compte.
        assert!(
            (18..=22).contains(&dr),
            "plage mesurée {dr}, attendue autour de 20 (calcul à la main)"
        );
    }

    /// 🔴 LE DÉCOUPAGE DES TRANCHES NE CHANGE RIEN.
    ///
    /// Le décodeur rend des segments de 30 s ; l'accumulateur doit donner le
    /// MÊME résultat qu'une passe d'un seul tenant, sinon la valeur dépendrait
    /// de la façon dont le fichier a été lu. Même garantie que son voisin
    /// `LoudnessAccumulator`, et c'est ce qui autorise le flux.
    #[test]
    fn le_decoupage_en_tranches_ne_change_pas_la_valeur() {
        let sr = 8_000;
        let s = signal(sr, 30.0, |t| {
            let a = if t < 6.0 { 1.0 } else { 0.01 };
            a * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        });
        let entier = dr_de(sr, &s);

        let mut acc = DrAccumulator::new(sr, 2);
        // Des tranches VOLONTAIREMENT irrégulières, et jamais alignées sur les
        // blocs de 3 s : un découpage complaisant ne prouverait rien.
        let mut i = 0usize;
        for taille in [1_000usize, 7_777, 33_333, 101, 250_000].iter().cycle() {
            if i >= s.len() {
                break;
            }
            let fin = (i + taille * 2).min(s.len());
            acc.feed(&s[i..fin]);
            i = fin;
        }
        assert_eq!(acc.finish(), entier);
    }

    /// Le silence n'a pas de plage dynamique — et n'en invente pas une.
    #[test]
    fn le_silence_ne_rend_aucune_plage() {
        let sr = 8_000;
        assert_eq!(dr_de(sr, &signal(sr, 10.0, |_| 0.0)), None);
    }

    /// Une piste plus courte qu'un bloc rend quand même une valeur.
    ///
    /// Le reliquat est fermé à la fin : sans cela, tout ce qui dure moins de
    /// 3 s n'aurait AUCUN bloc et sortirait sans plage. Deux pics de bloc sont
    /// nécessaires, d'où le second bloc court.
    #[test]
    fn une_piste_tres_courte_rend_quand_meme_une_valeur() {
        let sr = 8_000;
        let s = signal(sr, 4.0, |t| (2.0 * std::f64::consts::PI * 440.0 * t).sin());
        assert_eq!(dr_de(sr, &s), Some(0));
    }

    /// 🔴 UN CLIC ISOLÉ NE GONFLE PAS LA PLAGE — c'est le rôle du pic₂.
    ///
    /// On ajoute UN échantillon à pleine échelle sur un sinus faible. Prendre
    /// le pic MAXIMUM ferait bondir la valeur ; prendre le second la laisse
    /// où elle est. Sans ce choix, tout défaut d'encodage se lirait comme un
    /// disque très dynamique.
    #[test]
    fn un_clic_isole_ne_gonfle_pas_la_plage() {
        let sr = 8_000;
        let propre = signal(sr, 30.0, |t| {
            0.1 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
        });
        let mut avec_clic = propre.clone();
        // Un seul échantillon, dans UN seul bloc, à pleine échelle.
        avec_clic[sr * 2 * 5] = 1.0;
        avec_clic[sr * 2 * 5 + 1] = 1.0;
        assert_eq!(dr_de(sr, &avec_clic), dr_de(sr, &propre));
    }

    /// Un canal MUET ne tire pas la moyenne vers le bas.
    ///
    /// Une piste mono servie sur deux voies, ou une voie de remplissage :
    /// elle n'a pas une plage nulle, elle n'a pas de plage du tout.
    #[test]
    fn un_canal_muet_est_ecarte_de_la_moyenne() {
        let sr = 8_000;
        let trames = sr * 30;
        let mut s = Vec::with_capacity(trames * 2);
        for i in 0..trames {
            let t = i as f64 / sr as f64;
            let a = if t < 6.0 { 1.0 } else { 0.01 };
            s.push(a * (2.0 * std::f64::consts::PI * 440.0 * t).sin());
            s.push(0.0); // canal droit muet
        }
        let mut acc = DrAccumulator::new(sr, 2);
        acc.feed(&s);
        let deux_canaux = acc.finish().expect("le canal gauche porte une plage");

        // La même chose en mono : ce doit être la MÊME valeur.
        let mono: Vec<f64> = s.iter().step_by(2).copied().collect();
        let mut acc1 = DrAccumulator::new(sr, 1);
        acc1.feed(&mono);
        assert_eq!(Some(deux_canaux), acc1.finish());
    }

    /// Pic de mémoire résidente du processus (`VmHWM`, Linux), en Kio.
    #[cfg(target_os = "linux")]
    fn pic_residente_kio() -> u64 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmHWM:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse().ok())
            })
            .unwrap_or(0)
    }

    /// WAV 24/192 stéréo, écrit au fil de l'eau : un sinus à 997 Hz.
    #[cfg(target_os = "linux")]
    fn ecrire_wav_24_192(chemin: &std::path::Path, secondes: u64) {
        use std::io::Write;
        let rate = 192_000u64;
        let data = secondes * rate * 6;
        let mut f = std::io::BufWriter::new(std::fs::File::create(chemin).unwrap());
        f.write_all(b"RIFF").unwrap();
        f.write_all(&((36 + data) as u32).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        for v in [16u32] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        for v in [1u16, 2] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        for v in [rate as u32, rate as u32 * 6] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        for v in [6u16, 24] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        f.write_all(b"data").unwrap();
        f.write_all(&(data as u32).to_le_bytes()).unwrap();
        for i in 0..secondes * rate {
            let a = 0.1 + 0.4 * ((i / (rate * 7)) % 2) as f64;
            let v = (a
                * (i as f64 * 997.0 * std::f64::consts::TAU / rate as f64).sin()
                * 8_388_607.0) as i32;
            let b = v.to_le_bytes();
            f.write_all(&b[..3]).unwrap();
            f.write_all(&b[..3]).unwrap();
        }
        f.flush().unwrap();
    }

    /// DSF DSD64 stéréo, octets pseudo-aléatoires, écrit au fil de l'eau.
    #[cfg(target_os = "linux")]
    fn ecrire_dsf_64(chemin: &std::path::Path, secondes: u64) {
        use std::io::Write;
        const BLOC: u64 = 4096;
        let total = secondes * 2_822_400;
        let blocs = (total / 8).div_ceil(BLOC);
        let data = blocs * BLOC * 2;
        let mut f = std::io::BufWriter::new(std::fs::File::create(chemin).unwrap());
        f.write_all(b"DSD ").unwrap();
        for v in [28u64, 28 + 52 + 12 + data, 0] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        f.write_all(b"fmt ").unwrap();
        f.write_all(&52u64.to_le_bytes()).unwrap();
        for v in [1u32, 0, 2, 2, 2_822_400, 1] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        f.write_all(&total.to_le_bytes()).unwrap();
        for v in [BLOC as u32, 0] {
            f.write_all(&v.to_le_bytes()).unwrap();
        }
        f.write_all(b"data").unwrap();
        f.write_all(&(12 + data).to_le_bytes()).unwrap();
        let mut graine = 0xb209u32;
        let mut bloc = vec![0u8; BLOC as usize];
        for _ in 0..blocs * 2 {
            for o in bloc.iter_mut() {
                graine = graine.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *o = (graine >> 24) as u8;
            }
            f.write_all(&bloc).unwrap();
        }
        f.flush().unwrap();
    }

    /// plafond-analyse — la mémoire de l'analyse ne dépend PAS de la durée.
    ///
    /// Mesuré par le PIC de mémoire résidente du processus : la piste courte
    /// d'abord, la longue ensuite ; le pic ne doit presque pas bouger entre
    /// les deux. Avant la reprise au bloc des DSF, chaque segment de 30 s
    /// re-décodait la piste depuis son début : 10 min de DSD64 montaient le
    /// pic de plus de 600 Mio au-dessus de 2 min.
    ///
    /// Ignoré par défaut : il écrit ~2,3 Go de fichiers temporaires et dure
    /// plusieurs minutes. À lancer SEUL, pour que le pic soit le sien :
    /// `cargo test --release -p tune-core --lib -- --ignored --exact
    /// audio::analyzer::tests::la_memoire_de_l_analyse_ne_depend_pas_de_la_duree`
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn la_memoire_de_l_analyse_ne_depend_pas_de_la_duree() {
        const MARGE_KIO: u64 = 64 * 1024;
        let dir = tempfile::TempDir::new().unwrap();
        for (nom, court, long) in [("dsf", 120u64, 600u64), ("wav", 120, 720)] {
            let c = dir.path().join(format!("court.{nom}"));
            let l = dir.path().join(format!("long.{nom}"));
            if nom == "wav" {
                ecrire_wav_24_192(&c, court);
                ecrire_wav_24_192(&l, long);
            } else {
                ecrire_dsf_64(&c, court);
                ecrire_dsf_64(&l, long);
            }
            let m = mesurer_intensite_plage_et_empreinte(c.to_str().unwrap()).await;
            assert!(m.mesure.is_some(), "{nom} court : mesure attendue");
            let apres_court = pic_residente_kio();
            let m = mesurer_intensite_plage_et_empreinte(l.to_str().unwrap()).await;
            assert!(m.mesure.is_some(), "{nom} long : mesure attendue");
            let apres_long = pic_residente_kio();
            eprintln!(
                "{nom} : pic après {court} s = {apres_court} Kio, après {long} s = {apres_long} Kio"
            );
            assert!(
                apres_long <= apres_court + MARGE_KIO,
                "{nom} : la mémoire de l'analyse croît avec la durée — pic {apres_court} Kio \
                 pour {court} s, {apres_long} Kio pour {long} s"
            );
            let _ = std::fs::remove_file(&c);
            let _ = std::fs::remove_file(&l);
        }
    }

    // -----------------------------------------------------------------------
    // Vrai pic d'un FLAC analysé par segments (lot truepeak-flac-b209)
    // -----------------------------------------------------------------------

    const HZ_TP: u32 = 44_100;

    /// Stéréo 16 bits entrelacé : un sinus dont la fréquence dérive (la
    /// phase n'est donc jamais la même d'une trame FLAC à l'autre), crête
    /// 0,45 — loin de toute saturation.
    fn signal_tp(secondes: usize) -> Vec<i16> {
        let frames = HZ_TP as usize * secondes;
        let mut v = Vec::with_capacity(frames * 2);
        let mut phase = 0.0f64;
        for i in 0..frames {
            let t = i as f64 / HZ_TP as f64;
            let f = 440.0 + 220.0 * (t * 0.05).sin();
            phase += std::f64::consts::TAU * f / HZ_TP as f64;
            let x = 0.45 * phase.sin();
            v.push((x * 32_767.0) as i16);
            v.push((x * 0.8 * 32_767.0) as i16);
        }
        v
    }

    fn pcm_octets(pcm: &[i16]) -> Vec<u8> {
        pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn ecrire_wav_tp(chemin: &std::path::Path, pcm: &[i16]) {
        let donnees = pcm_octets(pcm);
        let mut w = Vec::with_capacity(donnees.len() + 44);
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36u32 + donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&2u16.to_le_bytes());
        w.extend_from_slice(&HZ_TP.to_le_bytes());
        w.extend_from_slice(&(HZ_TP * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(donnees.len() as u32).to_le_bytes());
        w.extend_from_slice(&donnees);
        std::fs::write(chemin, w).unwrap();
    }

    /// FLAC de l'encodeur maison : blocs fixes de 4 096 trames, AUCUNE
    /// SEEKTABLE (STREAMINFO puis VORBIS_COMMENT).
    fn flac_tp(pcm: &[i16]) -> Vec<u8> {
        let mut enc = crate::audio::encoder::AudioEncoder::new("flac", HZ_TP, 16, 2);
        enc.start_sync().unwrap();
        enc.write_sync(&pcm_octets(pcm)).unwrap();
        enc.finish_sync().unwrap()
    }

    fn crc8_flac(octets: &[u8]) -> u8 {
        octets.iter().fold(0u8, |mut crc, &b| {
            crc ^= b;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 {
                    (crc << 1) ^ 0x07
                } else {
                    crc << 1
                };
            }
            crc
        })
    }

    /// Le même FLAC, AVEC une SEEKTABLE (un point toutes les 10 s) insérée
    /// juste après STREAMINFO. Les décalages sont ceux des en-têtes de trame
    /// réels, retrouvés par leur octet exact (synchro, codes, numéro UTF-8,
    /// CRC-8) : l'index est juste, comme celui que pose `flac`.
    fn avec_seektable(flac: &[u8]) -> Vec<u8> {
        // fLaC (4) + en-tête STREAMINFO (4) + STREAMINFO (34) ; puis
        // VORBIS_COMMENT, dernier bloc : les trames suivent.
        let apres_streaminfo = 4 + 4 + 34;
        let lg_vc = u32::from_be_bytes([0, flac[43], flac[44], flac[45]]) as usize;
        let premiere_trame = apres_streaminfo + 4 + lg_vc;
        let en_tete = |k: u32| {
            // 4 096 trames (12), 44,1 kHz (9), stéréo G/D (1), 16 bits (4).
            let mut h = vec![0xFF, 0xF8, 0xC9, 0x18];
            let mut tampon = [0u8; 4];
            h.extend_from_slice(
                char::from_u32(k)
                    .unwrap()
                    .encode_utf8(&mut tampon)
                    .as_bytes(),
            );
            h.push(crc8_flac(&h));
            h
        };
        let trames_par_point = (10 * HZ_TP as usize) / 4096;
        let mut points = Vec::new();
        let mut depuis = premiere_trame;
        let mut k = 0u32;
        loop {
            let h = en_tete(k);
            let Some(pos) = flac[depuis..]
                .windows(h.len())
                .position(|w| w == h.as_slice())
            else {
                break;
            };
            let off = depuis + pos;
            points.push(((k as u64) * 4096, (off - premiere_trame) as u64));
            depuis = off + 1;
            k += trames_par_point as u32;
        }
        assert!(
            points.len() >= 3,
            "seektable : trop peu de points ({})",
            points.len()
        );
        let mut bloc = Vec::new();
        for (echantillon, decalage) in &points {
            bloc.extend_from_slice(&echantillon.to_be_bytes());
            bloc.extend_from_slice(&decalage.to_be_bytes());
            bloc.extend_from_slice(&4096u16.to_be_bytes());
        }
        let mut out = flac[..apres_streaminfo].to_vec();
        out.push(3); // SEEKTABLE, pas le dernier bloc
        out.extend_from_slice(&(bloc.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(&bloc);
        out.extend_from_slice(&flac[apres_streaminfo..]);
        out
    }

    /// La mesure de VÉRITÉ : tout le PCM d'un seul tenant, sans décodeur ni
    /// seek, par les mêmes accumulateurs.
    fn mesure_d_un_seul_tenant(pcm: &[i16]) -> Option<(f64, f64, f64, Option<u32>)> {
        let mut m = MesureEnCours::default();
        let d = crate::audio::decode::DecodedAudio {
            samples_i32: pcm.iter().map(|&s| s as i32).collect(),
            bit_depth: 16,
            sample_rate: HZ_TP,
            channels: 2,
            duration_s: pcm.len() as f64 / 2.0 / HZ_TP as f64,
            integrite: Default::default(),
        };
        let _ = m.nourrir(d, f64::MAX);
        m.finir()
    }

    /// Un FLAC (avec ou sans SEEKTABLE) et le WAV du même PCM rendent le MÊME
    /// vrai pic, le même pic, la même sonie et la même plage dynamique que le
    /// PCM mesuré d'un seul tenant.
    ///
    /// La mesure découpe la piste en segments de 30 s, chacun décodé par un
    /// `seek`. Le démultiplexeur se pose au début du paquet qui contient la
    /// cible (trame FLAC de 4 096, paquet simulé du WAV) ; tant que
    /// `decode_symphonia` ne rognait pas ce résidu, chaque jonction rejouait
    /// la fin du segment précédent — et le saut de phase qui en résulte, le
    /// suréchantillonnage 4× le lisait comme un over (0,507 au lieu de 0,455
    /// sur un FLAC de 20 min).
    #[tokio::test(flavor = "multi_thread")]
    async fn le_vrai_pic_d_un_flac_par_segments_est_celui_du_wav() {
        let dir = tempfile::TempDir::new().unwrap();
        // 65 s : trois segments, deux jonctions ; 30 s × 44 100 n'est pas un
        // multiple de 4 096, la jonction tombe au milieu d'une trame FLAC.
        let pcm = signal_tp(65);
        let verite = mesure_d_un_seul_tenant(&pcm).expect("mesure de référence");

        let wav = dir.path().join("tp.wav");
        ecrire_wav_tp(&wav, &pcm);
        let brut = flac_tp(&pcm);
        let sans = dir.path().join("tp_sans_seektable.flac");
        std::fs::write(&sans, &brut).unwrap();
        let avec = dir.path().join("tp_avec_seektable.flac");
        std::fs::write(&avec, avec_seektable(&brut)).unwrap();

        for f in [&wav, &sans, &avec] {
            let nom = f.file_name().unwrap().to_string_lossy().into_owned();
            let (lufs, pic, tp, dr) = mesurer_intensite_et_plage(f.to_str().unwrap())
                .await
                .unwrap_or_else(|| panic!("{nom} : mesure attendue"));
            eprintln!("{nom} : tp={tp:.6} pic={pic:.6} lufs={lufs} dr={dr:?} (vérité {verite:?})");
            assert!(
                (tp - verite.2).abs() <= 1e-3,
                "{nom} : vrai pic {tp:.6}, {:.6} d'un seul tenant",
                verite.2
            );
            assert!(
                (pic - verite.1).abs() <= 1e-3,
                "{nom} : pic {pic:.6}, {:.6} d'un seul tenant",
                verite.1
            );
            assert_eq!(lufs, verite.0, "{nom} : sonie");
            assert_eq!(dr, verite.3, "{nom} : plage dynamique");
            // La passe nominale (tête partagée avec l'empreinte) aussi.
            let passe = mesurer_intensite_plage_et_empreinte(f.to_str().unwrap())
                .await
                .mesure
                .unwrap_or_else(|| panic!("{nom} : mesure de la passe attendue"));
            assert!(
                (passe.2 - verite.2).abs() <= 1e-3,
                "{nom} : vrai pic de la passe {:.6}, {:.6} d'un seul tenant",
                passe.2,
                verite.2
            );
            // #2713 — la mesure de la SEULE crête vraie, celle du rattrapage,
            // rend la troisième valeur de la mesure complète au bit près.
            let seule = mesurer_la_crete_vraie(f.to_str().unwrap())
                .await
                .unwrap_or_else(|| panic!("{nom} : crête seule attendue"));
            assert_eq!(seule.to_bits(), tp.to_bits(), "{nom} : {seule} != {tp}");
        }
    }

    /// #2713 — le DSD converti en PCM (176,4 kHz pour du DSD64, donc 4×) et le
    /// multicanal (5.1, ramené en stéréo par le décodage de l'analyse comme
    /// pour la sonie) : la crête seule du rattrapage est celle de la mesure
    /// complète, au bit près, et englobe le pic d'échantillon.
    #[tokio::test(flavor = "multi_thread")]
    async fn la_crete_vraie_du_dsd_et_du_multicanal() {
        let dossier = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsd");
        for nom in [
            "ref_dsd64_stereo.dsf",
            "ref_dsd64_stereo.dff",
            "ref_dsd64_5v1.dff",
        ] {
            let f = dossier.join(nom);
            let f = f.to_str().unwrap();
            let (_lufs, pic, tp, _dr) = mesurer_intensite_et_plage(f)
                .await
                .unwrap_or_else(|| panic!("{nom} : mesure attendue"));
            let seule = mesurer_la_crete_vraie(f)
                .await
                .unwrap_or_else(|| panic!("{nom} : crête seule attendue"));
            eprintln!("{nom} : pic {pic:.6}, crête vraie {tp:.6}");
            assert_eq!(seule.to_bits(), tp.to_bits(), "{nom} : {seule} != {tp}");
            assert!(
                tp.is_finite() && tp >= pic && tp > 0.0,
                "{nom} : {tp} < {pic}"
            );
        }
    }
}
