use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

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
    /// Fréquence centrale RÉELLE de chaque bande, en Hz — voir
    /// [`band_center_frequencies`]. Même longueur que `spectrum`.
    ///
    /// Sans elle, `spectrum` n'est qu'une suite de nombres anonymes : rien
    /// dans l'événement ne dit à quelle fréquence répond la barre n° 12. Un
    /// client ne pouvait graduer son analyseur qu'en RECOPIANT le découpage
    /// d'ici — y compris ses arrondis — et en devinant la fréquence
    /// d'échantillonnage depuis les métadonnées de la piste (#2081).
    ///
    /// `Arc` et non `Vec` : la table ne dépend que du format, elle est
    /// calculée une fois puis partagée par toutes les trames.
    pub spectrum_hz: Arc<[f32]>,
    /// Taille de FFT réellement employée pour cette trame.
    ///
    /// Elle suit la fenêtre : 2048 à 44,1 et 48 kHz, 4096 à 96 kHz, 8192 à
    /// 192 kHz (#2866) — et moins sur une fenêtre courte. Un client qui suppose
    /// 2048 se trompe d'axe sur tout ce qui dépasse 48 kHz.
    pub spectrum_fft_size: usize,
    /// Nombre de trames de signal RÉEL entrées dans la FFT (avant zéro-padding).
    pub spectrum_frames: usize,
    /// Résolution VRAIE de l'analyse, en Hz — voir [`spectrum_resolution_hz`].
    ///
    /// Ce n'est PAS `sample_rate / spectrum_fft_size` : le zéro-padding
    /// resserre les raies sans ajouter d'information.
    pub spectrum_resolution_hz: f32,
    /// Pour chaque bande : l'analyse la sépare-t-elle vraiment de ses voisines ?
    /// Voir [`band_resolved`]. Même longueur que `spectrum`.
    pub spectrum_resolved: Arc<[bool]>,
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

/// Nombre de bandes du spectre émis sur `playback.audio_levels`.
pub const SPECTRUM_BANDS: usize = 32;

/// Taille de FFT NOMINALE, employée quand la fenêtre n'apprend rien (trame
/// vide) — et taille effective aux débits où 40 ms tiennent dans 2048 trames
/// (44,1 et 48 kHz).
pub const SPECTRUM_FFT_SIZE: usize = 2048;

/// Plafond de la FFT d'analyse, en échantillons (#2866).
///
/// Le plafond valait 2048 quel que soit le débit. Une fenêtre de 40 ms en
/// porte pourtant 3840 à 96 kHz et 7680 à 192 kHz : le reste partait à la
/// poubelle et la résolution s'effondrait à 46,9 Hz puis 93,75 Hz, contre
/// 25 Hz à 44,1 kHz. Le plafond à 8192 rend la résolution CONSTANTE (~25 Hz)
/// sur toute la gamme, **sans allonger la fenêtre** : les échantillons étaient
/// déjà là. Coût mesuré : voir `docs/` de la PR — la table de twiddles
/// ci-dessous paie plus que le surcoût.
pub const SPECTRUM_FFT_MAX: usize = 8192;

/// Bas de l'axe fréquentiel de l'analyseur, en Hz.
pub const SPECTRUM_FREQ_MIN_HZ: f64 = 20.0;

/// Haut de l'axe, avant bornage par le Nyquist du flux, en Hz.
pub const SPECTRUM_FREQ_MAX_HZ: f64 = 20_000.0;

/// Première raie EXPLOITABLE de la FFT.
///
/// La raie 0 est le CONTINU (0 Hz), pas du son. Le découpage la donnait à la
/// bande la plus grave, qui annonçait alors une fréquence centrale de 10,8 Hz
/// à 44,1 kHz — sous la borne d'axe de 20 Hz, et sous la première raie
/// mesurable : un nombre inventé. Pire, un offset continu (fréquent sur les
/// numérisations) allumait cette barre en permanence et, `spectrum` étant
/// normalisé par la bande la plus forte, écrasait toutes les autres (#2866).
const FIRST_BIN: usize = 1;

/// Raies FFT `[f_low, f_high)` agrégées par la bande `b`.
///
/// Point de vérité UNIQUE du découpage : l'analyse et la table de fréquences
/// l'appellent toutes les deux, donc le repère annoncé ne peut pas dériver de
/// la barre mesurée. `half` doit valoir au moins 2 ([`FIRST_BIN`] doit exister).
#[inline]
fn band_bin_range(
    b: usize,
    bins: usize,
    log_ratio: f64,
    nyquist: f64,
    half: usize,
) -> (usize, usize) {
    let hz_low = SPECTRUM_FREQ_MIN_HZ * log_ratio.powf(b as f64 / bins as f64);
    let hz_high = SPECTRUM_FREQ_MIN_HZ * log_ratio.powf((b + 1) as f64 / bins as f64);
    let f_low = (((hz_low / nyquist) * half as f64) as usize).clamp(FIRST_BIN, half - 1);
    let f_high = (((hz_high / nyquist) * half as f64) as usize)
        .max(f_low + 1)
        .min(half);
    (f_low, f_high)
}

/// Largeur ANNONCÉE de la bande `b`, en Hz — bornes logarithmiques, avant
/// ramenage aux raies FFT.
#[inline]
fn band_width_hz(b: usize, bins: usize, log_ratio: f64) -> f64 {
    let hz_low = SPECTRUM_FREQ_MIN_HZ * log_ratio.powf(b as f64 / bins as f64);
    let hz_high = SPECTRUM_FREQ_MIN_HZ * log_ratio.powf((b + 1) as f64 / bins as f64);
    hz_high - hz_low
}

type BandTables = RwLock<HashMap<(usize, u32, usize), Arc<[f32]>>>;

fn band_tables() -> &'static BandTables {
    static TABLES: OnceLock<BandTables> = OnceLock::new();
    TABLES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Fréquence centrale RÉELLE de chaque bande, en Hz.
///
/// « Réelle » et non « annoncée » : les bornes de bande sont logarithmiques,
/// mais elles sont ramenées à des raies FFT entières par une TRONCATURE. Dans
/// le grave, plusieurs bandes annoncées sont plus étroites que la résolution
/// de la FFT (21,5 Hz à 44,1 kHz, 46,9 Hz à 96 kHz) et retombent sur les mêmes
/// raies. Le centre rendu ici est celui de l'intervalle de fréquences
/// effectivement agrégé, donc **ce que la barre montre vraiment**.
///
/// Conséquence exploitable côté client : deux bandes voisines qui portent la
/// même valeur lisent les mêmes raies — l'analyseur ne sait pas les
/// distinguer, et y poser deux repères différents serait une invention.
///
/// Chemin temps réel : la table ne dépend que de `(bins, sample_rate,
/// fft_size)`, elle est mémorisée et rendue par `Arc` — aucune allocation par
/// trame une fois le format vu une première fois.
pub fn band_center_frequencies(bins: usize, sample_rate: u32, fft_size: usize) -> Arc<[f32]> {
    let key = (bins, sample_rate, fft_size);
    if let Ok(tables) = band_tables().read() {
        if let Some(table) = tables.get(&key) {
            return Arc::clone(table);
        }
    }

    let table = compute_band_center_frequencies(bins, sample_rate, fft_size);
    if let Ok(mut tables) = band_tables().write() {
        // Une poignée de fréquences d'échantillonnage existent ; la borne
        // empêche qu'un flux malformé fasse enfler le cache indéfiniment.
        if tables.len() < 64 {
            tables.insert(key, Arc::clone(&table));
        }
    }
    table
}

fn compute_band_center_frequencies(bins: usize, sample_rate: u32, fft_size: usize) -> Arc<[f32]> {
    if bins == 0 {
        return Arc::from(Vec::new());
    }
    let half = fft_size / 2;
    let nyquist = sample_rate as f64 / 2.0;
    let freq_max = nyquist.min(SPECTRUM_FREQ_MAX_HZ);
    // Sans Nyquist utilisable — ou sans la moindre raie au-dessus du continu —
    // aucune fréquence ne peut être annoncée : on rend des zéros plutôt qu'un
    // axe inventé.
    if half < FIRST_BIN + 1 || freq_max <= SPECTRUM_FREQ_MIN_HZ {
        return Arc::from(vec![0.0f32; bins]);
    }

    let log_ratio = freq_max / SPECTRUM_FREQ_MIN_HZ;
    let resolution = sample_rate as f64 / fft_size as f64;
    let mut out = Vec::with_capacity(bins);
    for b in 0..bins {
        let (f_low, f_high) = band_bin_range(b, bins, log_ratio, nyquist, half);
        // Centre de l'intervalle [f_low, f_high) × résolution.
        out.push((((f_low + f_high) as f64 / 2.0) * resolution) as f32);
    }
    Arc::from(out)
}

type ResolvedTables = RwLock<HashMap<(usize, u32, usize), Arc<[bool]>>>;

fn resolved_tables() -> &'static ResolvedTables {
    static TABLES: OnceLock<ResolvedTables> = OnceLock::new();
    TABLES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Résolution VRAIE de l'analyse, en Hz : `sample_rate / trames RÉELLEMENT
/// analysées` — pas `sample_rate / fft_size` (#2866).
///
/// Le zéro-padding porte la FFT à la puissance de deux supérieure et resserre
/// l'espacement des raies, mais il **n'ajoute aucune information** : deux
/// composantes plus proches que `sample_rate / frames` restent indiscernables.
/// À 44,1 kHz, 1764 trames de signal poussées dans une FFT de 2048 donnent un
/// pas de raie de 21,5 Hz pour une résolution vraie de 25,0 Hz. C'est cette
/// dernière qu'il faut publier — sans quoi le client croit l'analyse plus fine
/// qu'elle n'est.
#[inline]
pub fn spectrum_resolution_hz(sample_rate: u32, frames: usize) -> f32 {
    if frames == 0 {
        return 0.0;
    }
    sample_rate as f32 / frames as f32
}

/// Pour chaque bande : l'analyse sait-elle vraiment la séparer de ses voisines ?
///
/// `true` quand la largeur ANNONCÉE de la bande atteint la résolution vraie de
/// l'analyse. Sinon la barre existe, mais elle ne montre rien qui lui soit
/// propre : elle recopie la ou les raies de sa voisine, et l'étiqueter d'une
/// fréquence distincte est une invention (#2866).
///
/// C'est le champ qui permet à un client d'arrêter d'annoncer ce que la
/// résolution ne permet pas, **sans changer le nombre de bandes** — lequel est
/// le contrat des clients déjà déployés.
///
/// Chemin temps réel : mémorisée par `(bins, sample_rate, frames)` et rendue
/// par `Arc`, comme la table de fréquences.
pub fn band_resolved(bins: usize, sample_rate: u32, frames: usize) -> Arc<[bool]> {
    let key = (bins, sample_rate, frames);
    if let Ok(tables) = resolved_tables().read() {
        if let Some(table) = tables.get(&key) {
            return Arc::clone(table);
        }
    }
    let table = compute_band_resolved(bins, sample_rate, frames);
    if let Ok(mut tables) = resolved_tables().write() {
        if tables.len() < 64 {
            tables.insert(key, Arc::clone(&table));
        }
    }
    table
}

fn compute_band_resolved(bins: usize, sample_rate: u32, frames: usize) -> Arc<[bool]> {
    if bins == 0 {
        return Arc::from(Vec::new());
    }
    let nyquist = sample_rate as f64 / 2.0;
    let freq_max = nyquist.min(SPECTRUM_FREQ_MAX_HZ);
    let resolution = spectrum_resolution_hz(sample_rate, frames) as f64;
    if resolution <= 0.0 || freq_max <= SPECTRUM_FREQ_MIN_HZ {
        return Arc::from(vec![false; bins]);
    }
    let log_ratio = freq_max / SPECTRUM_FREQ_MIN_HZ;
    let out: Vec<bool> = (0..bins)
        .map(|b| band_width_hz(b, bins, log_ratio) >= resolution)
        .collect();
    Arc::from(out)
}

type TwiddleTables = RwLock<HashMap<usize, Arc<[(f64, f64)]>>>;

/// Twiddles `exp(-2iπ j / n)` pour `j < n/2`, mémorisés par taille de FFT.
///
/// La boucle papillon recalculait `cos`/`sin` pour CHAQUE bloc de chaque
/// étage, soit `(n/2)·log2(n)` appels transcendants par trame — 11 264 à
/// n = 2048. La table en coûte `n/2` **une seule fois par format**. C'est ce
/// qui rend l'agrandissement de la FFT abordable : sans elle, passer à 8192
/// multipliait par 4,3 le coût de l'analyse (#2866).
fn twiddle_table(n: usize) -> Arc<[(f64, f64)]> {
    static TABLES: OnceLock<TwiddleTables> = OnceLock::new();
    let tables = TABLES.get_or_init(|| RwLock::new(HashMap::new()));
    if let Ok(t) = tables.read() {
        if let Some(tw) = t.get(&n) {
            return Arc::clone(tw);
        }
    }
    let built: Vec<(f64, f64)> = (0..n / 2)
        .map(|j| {
            let a = -2.0 * std::f64::consts::PI * j as f64 / n as f64;
            (a.cos(), a.sin())
        })
        .collect();
    let table: Arc<[(f64, f64)]> = Arc::from(built);
    if let Ok(mut t) = tables.write() {
        // Une poignée de tailles existent (2048, 4096, 8192) ; la borne
        // protège d'un flux malformé.
        if t.len() < 16 {
            t.insert(n, Arc::clone(&table));
        }
    }
    table
}

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
    let spectrum = analyze_spectrum(pcm, bit_depth, channels, SPECTRUM_BANDS, sample_rate);

    AudioLevels {
        rms_left: (sum_sq_l / frames as f64).sqrt(),
        rms_right: (sum_sq_r / frames as f64).sqrt(),
        peak_left: peak_l,
        peak_right: peak_r,
        spectrum_db: spectrum.db,
        spectrum_hz: spectrum.hz,
        spectrum_fft_size: spectrum.fft_size,
        spectrum_frames: spectrum.frames,
        spectrum_resolution_hz: spectrum.resolution_hz,
        spectrum_resolved: spectrum.resolved,
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
    /// Fréquence centrale réelle de chaque bande — voir
    /// [`band_center_frequencies`].
    pub hz: Arc<[f32]>,
    /// Taille de FFT réellement employée.
    pub fft_size: usize,
    /// Trames de signal RÉEL entrées dans la FFT (avant zéro-padding).
    pub frames: usize,
    /// Résolution vraie de l'analyse, en Hz.
    pub resolution_hz: f32,
    /// Bandes que l'analyse sait vraiment séparer — voir [`band_resolved`].
    pub resolved: Arc<[bool]>,
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
    // Même sans signal, l'axe reste calculable à partir du format : un client
    // qui reçoit une trame vide garde ses graduations au lieu de les voir
    // retomber à 0 Hz.
    let empty = || Spectrum {
        shape: vec![0.0; bins],
        db: vec![SPECTRUM_FLOOR_DB; bins],
        hz: band_center_frequencies(bins, sample_rate, SPECTRUM_FFT_SIZE),
        fft_size: SPECTRUM_FFT_SIZE,
        frames: 0,
        resolution_hz: 0.0,
        resolved: Arc::from(vec![false; bins]),
    };
    if pcm.is_empty() || channels == 0 || bins == 0 {
        return empty();
    }

    let bytes_per_sample = (bit_depth / 8) as usize;
    let frame_size = bytes_per_sample * channels as usize;
    if frame_size == 0 {
        return empty();
    }

    // Échantillons mono (mixage G+D), au plus SPECTRUM_FFT_MAX.
    //
    // Le plafond valait 2048 : à 96 kHz les 3840 trames de la fenêtre de 40 ms
    // étaient tronquées à 2048, à 192 kHz les 7680 à 2048. On jetait 47 % puis
    // 73 % du signal DÉJÀ DISPONIBLE, et la résolution tombait de 25 Hz à
    // 46,9 puis 93,75 Hz (#2866). Le plafond à 8192 les garde — sans allonger
    // la fenêtre, donc sans latence ni traînée supplémentaires.
    let mut samples: Vec<f64> = Vec::with_capacity(SPECTRUM_FFT_MAX);
    for frame in pcm.chunks_exact(frame_size).take(SPECTRUM_FFT_MAX) {
        let left = read_sample(frame, 0, bytes_per_sample, bit_depth);
        let right = if channels >= 2 {
            read_sample(frame, bytes_per_sample, bytes_per_sample, bit_depth)
        } else {
            left
        };
        samples.push((left + right) * 0.5);
    }

    // `m` = signal RÉEL, `n` = longueur de FFT après zéro-padding. La
    // résolution se lit sur `m`, jamais sur `n` : le padding interpole, il
    // n'informe pas.
    let m = samples.len();
    let n = m.next_power_of_two().min(SPECTRUM_FFT_MAX);
    samples.resize(n, 0.0);

    // Retrait de la composante continue AVANT fenêtrage.
    //
    // Le continu n'est pas du son : aucun haut-parleur ne le restitue. Il
    // sortait pourtant en raie 0, que le découpage donnait à la bande la plus
    // grave — et, la fenêtre de Hann étalant le continu sur ±1 raie à −6 dB,
    // il allumait aussi la raie 1. Un offset d'enregistrement (numérisations,
    // certains convertisseurs) fabriquait ainsi un « grave » permanent, que
    // `spectrum` amplifiait encore en normalisant par la bande la plus
    // forte (#2866).
    if m > 0 {
        let mean = samples.iter().take(m).sum::<f64>() / m as f64;
        for s in samples.iter_mut().take(m) {
            *s -= mean;
        }
    }

    // Fenêtre de Hann sur les `m` échantillons RÉELS.
    //
    // Elle portait sur `n`, zéros compris : à 44,1 kHz le signal s'arrêtait à
    // l'échantillon 1763 d'une fenêtre de 2048, là où la Hann vaut encore
    // 0,179. Le signal était donc coupé net à 17,9 % de son amplitude — une
    // MARCHE, dont la fuite large bande retombe surtout dans les premières
    // raies. C'est-à-dire exactement les barres graves de l'issue, et le
    // continu de la raie 0 qui écrasait ensuite la normalisation (#2866).
    for (i, s) in samples.iter_mut().take(m).enumerate() {
        *s *= 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / m as f64).cos());
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
        let mut bit = n >> 1;
        while bit >= 1 && j >= bit {
            j -= bit;
            bit >>= 1;
        }
        j += bit;
    }

    // FFT butterfly — twiddles pris dans la table mémorisée, plus recalculés
    // par bloc (voir `twiddle_table`).
    let tw = twiddle_table(n);
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let step = n / len;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let (wr, wi) = tw[k * step];
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
    // Une fenêtre trop courte ne donne aucune raie AUDIBLE au-dessus du
    // continu : le découpage en bandes n'a alors rien à agréger (et le
    // `clamp(FIRST_BIN, half - 1)` de `band_bin_range` déborderait).
    if half < FIRST_BIN + 1 {
        return empty();
    }
    let mut mags: Vec<f64> = Vec::with_capacity(half);
    let mut max_mag: f64 = 1e-10;
    for i in 0..half {
        let mag = (re[i] * re[i] + im[i] * im[i]).sqrt();
        // La raie 0 (le continu) est exclue de la référence de normalisation
        // comme elle l'est du découpage : un offset d'enregistrement écrasait
        // sinon toutes les barres de `spectrum`, qui est divisé par ce maximum.
        if i >= FIRST_BIN {
            max_mag = max_mag.max(mag);
        }
        mags.push(mag);
    }

    // Map FFT bins to output bins using true logarithmic frequency scale.
    // Each output bin covers one equal fraction of the audible range on a
    // log axis (20 Hz – 20 kHz), matching human pitch perception.
    let nyquist = sample_rate as f64 / 2.0;
    let freq_max = nyquist.min(SPECTRUM_FREQ_MAX_HZ);
    let log_ratio = freq_max / SPECTRUM_FREQ_MIN_HZ;
    let mut result = vec![0.0f32; bins];
    let mut result_db = vec![SPECTRUM_FLOOR_DB; bins];
    // Sinusoïde pleine échelle au centre d'une raie, fenêtre de Hann : m/4.
    //
    // `m` et non `n` : la fenêtre porte sur le signal réel, le zéro-padding
    // n'ajoute pas d'énergie. La référence valait `n/4`, ce qui sous-estimait
    // les niveaux de 20·log10(m/n) — −1,3 dB à 44,1 kHz (#2866).
    let full_scale = (m as f64) / 4.0;
    for b in 0..bins {
        let (f_low, f_high) = band_bin_range(b, bins, log_ratio, nyquist, half);

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
        hz: band_center_frequencies(bins, sample_rate, n),
        fft_size: n,
        frames: m,
        resolution_hz: spectrum_resolution_hz(sample_rate, m),
        resolved: band_resolved(bins, sample_rate, m),
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

    // ------------------------------------------------------------------
    // #2081 — repères de fréquence : chaque bande dit à quelle fréquence
    // elle répond, au lieu de laisser le client la deviner.
    // ------------------------------------------------------------------

    /// Indice de la bande la plus forte, en niveau absolu.
    fn loudest_band(db: &[f32]) -> usize {
        db.iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .unwrap()
            .0
    }

    #[test]
    fn spectrum_hz_accompanies_every_band() {
        let lvl = compute_levels(&sine_pcm(1000.0, 0.5, 44100, 2048), 16, 2, 44100);
        assert_eq!(lvl.spectrum.len(), SPECTRUM_BANDS);
        assert_eq!(
            lvl.spectrum_hz.len(),
            lvl.spectrum.len(),
            "un repère par bande, sinon l'axe ne s'aligne pas sur les barres"
        );
        assert_eq!(lvl.spectrum_fft_size, SPECTRUM_FFT_SIZE);
        // L'axe monte : une bande ne peut pas répondre plus bas que sa voisine
        // de gauche.
        for w in lvl.spectrum_hz.windows(2) {
            assert!(
                w[1] >= w[0],
                "axe non monotone : {:?}",
                &lvl.spectrum_hz[..]
            );
        }
        // Et il reste dans le Nyquist du flux.
        assert!(
            *lvl.spectrum_hz.last().unwrap() <= 44100.0 / 2.0,
            "dernier repère à {} Hz, au-delà du Nyquist",
            lvl.spectrum_hz.last().unwrap()
        );
    }

    /// LE test de l'issue : le repère annoncé désigne la barre qui s'allume.
    ///
    /// Une sinusoïde pure à `freq` doit allumer une bande dont la fréquence
    /// centrale ANNONCÉE vaut `freq` à la largeur de bande près. C'est ce qui
    /// distingue un axe honnête d'un axe recalculé à côté : jusqu'ici le
    /// serveur n'en annonçait aucun, et l'axe logarithmique « annoncé » se
    /// décale d'une à deux barres dans le grave à cause de la troncature en
    /// raies FFT.
    #[test]
    fn spectrum_hz_designates_the_band_that_lights_up() {
        // Une bande fait 10^(3/32) ≈ 1,24× de large : ±25 % couvre le pire cas.
        for (sr, freq) in [
            (44100u32, 1000.0f64),
            (44100, 4000.0),
            (44100, 8000.0),
            (96000, 1000.0),
            (96000, 5000.0),
            (192000, 2000.0),
        ] {
            let s = analyze_spectrum(&sine_pcm(freq, 1.0, sr, 4096), 16, 2, SPECTRUM_BANDS, sr);
            let b = loudest_band(&s.db);
            let annonce = s.hz[b] as f64;
            let ratio = annonce / freq;
            assert!(
                (0.75..1.33).contains(&ratio),
                "{freq} Hz à {sr} Hz allume la bande {b}, annoncée à {annonce} Hz \
                 (rapport {ratio:.3}) — axe : {:?}",
                &s.hz[..]
            );
        }
    }

    /// L'axe SUIT la fréquence d'échantillonnage.
    ///
    /// C'est la raison pour laquelle il ne peut pas être écrit en dur côté
    /// client : à 44,1 kHz l'axe monte jusqu'à 20 kHz, à 32 kHz il s'arrête au
    /// Nyquist, et les mêmes 32 bandes ne couvrent alors pas la même chose.
    #[test]
    fn spectrum_hz_follows_the_sample_rate() {
        let a = band_center_frequencies(SPECTRUM_BANDS, 44100, SPECTRUM_FFT_SIZE);
        let b = band_center_frequencies(SPECTRUM_BANDS, 32000, SPECTRUM_FFT_SIZE);
        assert_ne!(
            a[SPECTRUM_BANDS - 1],
            b[SPECTRUM_BANDS - 1],
            "le haut de l'axe doit être borné par le Nyquist"
        );
        assert!(b[SPECTRUM_BANDS - 1] <= 16000.0);
        assert!(a[SPECTRUM_BANDS - 1] > 16000.0);
    }

    /// Dans le grave, plusieurs bandes lisent les MÊMES raies FFT : la
    /// résolution (21,5 Hz à 44,1 kHz) est plus grossière que les bandes
    /// annoncées. On l'assume au lieu de le cacher — le repère répété DIT au
    /// client que ces bandes ne sont pas distinctes, au lieu de le laisser
    /// étiqueter « 31 Hz » une barre qui ne s'allume jamais.
    #[test]
    fn spectrum_hz_repeats_where_the_analysis_cannot_separate() {
        let hz = band_center_frequencies(SPECTRUM_BANDS, 44100, SPECTRUM_FFT_SIZE);
        let doublons = hz.windows(2).filter(|w| w[0] == w[1]).count();
        assert!(
            doublons > 0,
            "à 44,1 kHz les bandes graves se confondent — l'axe doit le montrer : {hz:?}"
        );
        // Au-dessus de 1 kHz, en revanche, chaque bande est bien séparée.
        let hauts: Vec<f32> = hz.iter().copied().filter(|f| *f > 1000.0).collect();
        for w in hauts.windows(2) {
            assert!(
                w[1] > w[0],
                "bandes confondues au-dessus de 1 kHz : {hauts:?}"
            );
        }
    }

    /// Chemin temps réel : la table ne se recalcule pas à chaque trame.
    #[test]
    fn spectrum_hz_table_is_computed_once() {
        let a = band_center_frequencies(SPECTRUM_BANDS, 48000, SPECTRUM_FFT_SIZE);
        let b = band_center_frequencies(SPECTRUM_BANDS, 48000, SPECTRUM_FFT_SIZE);
        assert!(
            Arc::ptr_eq(&a, &b),
            "la table de fréquences doit être partagée, pas réallouée par trame"
        );

        // Et deux trames consécutives partagent bien la même allocation.
        let pcm = sine_pcm(1000.0, 0.5, 48000, 2048);
        let l1 = compute_levels(&pcm, 16, 2, 48000);
        let l2 = compute_levels(&pcm, 16, 2, 48000);
        assert!(Arc::ptr_eq(&l1.spectrum_hz, &l2.spectrum_hz));
    }

    // ------------------------------------------------------------------
    // #2866 — la FFT faisait 2048 points quel que soit le débit, la fenêtre
    // de Hann portait sur le zéro-padding, et la raie 0 (le continu) était
    // servie à la bande la plus grave sous une fréquence inventée.
    // ------------------------------------------------------------------

    /// Une fenêtre de niveaux RÉELLE : `tap::WINDOW_MS` = 40 ms de signal à
    /// `sr`, exactement ce que le forwarder passe à `compute_levels`.
    fn fenetre_reelle(freq: f64, amp: f64, sr: u32) -> Vec<u8> {
        let frames = (sr as usize * crate::audio::tap::WINDOW_MS as usize / 1000).max(1);
        sine_pcm(freq, amp, sr, frames)
    }

    /// Aucune bande ne peut annoncer une fréquence que l'analyse ne mesure
    /// pas.
    ///
    /// La première raie exploitable est à `sample_rate / fft_size` — 21,5 Hz à
    /// 44,1 kHz. La bande 0 annonçait pourtant **10,8 Hz** : elle agrégeait la
    /// raie 0, qui est le CONTINU (0 Hz), et publiait le centre de
    /// l'intervalle `[0, 1)` comme si c'était une fréquence audio. Un nombre
    /// sous la borne d'axe de 20 Hz, et sous ce que la FFT peut voir.
    #[test]
    fn aucune_bande_n_annonce_une_frequence_sous_la_premiere_raie() {
        for sr in [44100u32, 48000, 96000, 192000] {
            let s = analyze_spectrum(&fenetre_reelle(1000.0, 0.5, sr), 16, 2, SPECTRUM_BANDS, sr);
            let premiere_raie = sr as f32 / s.fft_size as f32;
            for (b, &hz) in s.hz.iter().enumerate() {
                assert!(
                    hz >= premiere_raie,
                    "{sr} Hz : la bande {b} annonce {hz} Hz alors que la première \
                     raie mesurable est à {premiere_raie} Hz (FFT {}) — axe : {:?}",
                    s.fft_size,
                    &s.hz[..]
                );
            }
        }
    }

    /// Le continu n'est pas du grave.
    ///
    /// Un offset d'enregistrement partait en raie 0, que la bande la plus
    /// grave agrégeait telle quelle ; la fenêtre de Hann l'étalait en plus sur
    /// la raie 1 à −6 dB. Résultat : les barres du bas s'allumaient en
    /// permanence sur un signal qui ne contient AUCUN grave — et comme
    /// `spectrum` est normalisé par la bande la plus forte, tout le reste du
    /// spectre était écrasé.
    #[test]
    fn le_continu_n_allume_pas_les_barres_graves() {
        let sr = 44100u32;
        let frames = (sr as usize * crate::audio::tap::WINDOW_MS as usize / 1000).max(1);
        // Sinusoïde à 1 kHz, −20 dBFS, posée sur un offset continu de +0,5.
        let mut pcm = Vec::with_capacity(frames * 4);
        for i in 0..frames {
            let t = i as f64 / sr as f64;
            let s = 0.5 + 0.1 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin();
            let v = (s.clamp(-1.0, 1.0) * i16::MAX as f64) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let s = analyze_spectrum(&pcm, 16, 2, SPECTRUM_BANDS, sr);
        let b1k = loudest_band(&s.db);
        assert!(
            (s.hz[b1k] as f64 - 1000.0).abs() < 250.0,
            "la bande la plus forte devrait être celle du 1 kHz, obtenu {} Hz",
            s.hz[b1k]
        );
        // Les huit bandes du bas ne portent QUE du continu : elles doivent
        // rester très loin sous la fondamentale.
        for b in 0..8 {
            assert!(
                s.db[b] < s.db[b1k] - 40.0,
                "l'offset continu allume la bande {b} à {} dBFS, contre {} dBFS \
                 pour le 1 kHz — le continu passe pour du grave",
                s.db[b],
                s.db[b1k]
            );
        }
    }

    /// La résolution ne doit pas s'effondrer quand le débit monte.
    ///
    /// L'analyse était plafonnée à 2048 échantillons : à 96 kHz elle jetait
    /// 1792 des 3840 trames de la fenêtre, à 192 kHz 5632 des 7680 — soit 47 %
    /// puis 73 % d'un signal DÉJÀ DISPONIBLE. La résolution tombait de 25,0 Hz
    /// à 46,9 puis 93,75 Hz : sur un 24/192, quatorze bandes sur trente-deux
    /// devenaient indiscernables. C'est le format des auditeurs que le spectre
    /// intéresse.
    #[test]
    fn la_resolution_ne_depend_pas_du_debit() {
        for sr in [44100u32, 48000, 88200, 96000, 176400, 192000] {
            let pcm = fenetre_reelle(1000.0, 0.5, sr);
            let s = analyze_spectrum(&pcm, 16, 2, SPECTRUM_BANDS, sr);
            let frames = (sr as usize * crate::audio::tap::WINDOW_MS as usize / 1000).max(1);
            assert_eq!(
                s.frames, frames,
                "{sr} Hz : {} trames analysées sur les {frames} de la fenêtre",
                s.frames
            );
            assert!(
                s.resolution_hz <= 26.0,
                "{sr} Hz : résolution de {} Hz (FFT {}, {} trames)",
                s.resolution_hz,
                s.fft_size,
                s.frames
            );
        }
    }

    /// Ce que la résolution ne permet pas est DÉCLARÉ, pas maquillé.
    ///
    /// Les bandes du grave sont plus étroites que ce que l'analyse sait
    /// séparer : la première fait 20,0 à 24,8 Hz, soit **4,8 Hz de large**,
    /// contre 25,0 Hz de résolution. On ne peut pas les supprimer — le nombre
    /// de bandes est le contrat des clients déployés — alors on dit lesquelles
    /// ne mesurent rien qui leur soit propre.
    #[test]
    fn les_bandes_sous_la_resolution_sont_declarees() {
        let sr = 44100u32;
        let s = analyze_spectrum(&fenetre_reelle(1000.0, 0.5, sr), 16, 2, SPECTRUM_BANDS, sr);
        assert_eq!(s.resolved.len(), SPECTRUM_BANDS, "un drapeau par bande");
        // 25,0 Hz de résolution : les bandes 0 à 7 (4,8 à 21,8 Hz de large)
        // sont plus étroites, la bande 8 (27,1 Hz) est la première à tenir.
        for b in 0..8 {
            assert!(!s.resolved[b], "la bande {b} est annoncée comme mesurée");
        }
        for b in 8..SPECTRUM_BANDS {
            assert!(
                s.resolved[b],
                "la bande {b} est déclarée non résolue à tort"
            );
        }
        // Et le drapeau SUIT la résolution : à 192 kHz, la fenêtre est plus
        // longue en échantillons, donc la résolution en Hz est la même et le
        // découpage aussi.
        let s192 = analyze_spectrum(
            &fenetre_reelle(1000.0, 0.5, 192_000),
            16,
            2,
            SPECTRUM_BANDS,
            192_000,
        );
        assert_eq!(
            s.resolved.iter().filter(|r| **r).count(),
            s192.resolved.iter().filter(|r| **r).count(),
            "192 kHz doit résoudre autant de bandes que 44,1 kHz"
        );
    }

    /// TÉMOIN — une sinusoïde de fréquence connue tombe dans la bande
    /// annoncée, sur une fenêtre RÉELLE de 40 ms, à toutes les cadences.
    ///
    /// Vert avant comme après : c'est ce qui garantit que l'agrandissement de
    /// la FFT n'a pas décalé l'axe.
    #[test]
    fn une_sinusoide_connue_tombe_dans_la_bande_annoncee() {
        for (sr, freq) in [
            (44100u32, 200.0f64),
            (44100, 1000.0),
            (44100, 6300.0),
            (48000, 500.0),
            (96000, 1000.0),
            (96000, 8000.0),
            (192000, 250.0),
            (192000, 2000.0),
        ] {
            let s = analyze_spectrum(&fenetre_reelle(freq, 1.0, sr), 16, 2, SPECTRUM_BANDS, sr);
            let b = loudest_band(&s.db);
            let ratio = s.hz[b] as f64 / freq;
            assert!(
                (0.8..1.25).contains(&ratio),
                "{freq} Hz à {sr} Hz allume la bande {b}, annoncée à {} Hz \
                 (rapport {ratio:.3}) — FFT {}, résolution {} Hz",
                s.hz[b],
                s.fft_size,
                s.resolution_hz
            );
        }
    }

    /// Chemin temps réel : les tables neuves ne s'allouent pas par trame.
    #[test]
    fn les_tables_de_resolution_sont_partagees() {
        let pcm = fenetre_reelle(1000.0, 0.5, 96_000);
        let a = compute_levels(&pcm, 16, 2, 96_000);
        let b = compute_levels(&pcm, 16, 2, 96_000);
        assert!(Arc::ptr_eq(&a.spectrum_resolved, &b.spectrum_resolved));
        assert!(Arc::ptr_eq(&a.spectrum_hz, &b.spectrum_hz));
    }

    /// Une trame vide garde un axe exploitable : le client ne voit pas ses
    /// graduations retomber à 0 Hz sur un silence.
    #[test]
    fn spectrum_hz_survives_an_empty_frame() {
        let s = analyze_spectrum(&[], 16, 2, SPECTRUM_BANDS, 44100);
        assert_eq!(s.hz.len(), SPECTRUM_BANDS);
        assert!(
            s.hz[SPECTRUM_BANDS - 1] > 1000.0,
            "axe perdu : {:?}",
            &s.hz[..]
        );
    }
}
