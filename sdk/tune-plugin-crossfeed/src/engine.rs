//! Headphone crossfeed DSP effect (built-in).
//!
//! ⚠️ Ce module s'est longtemps intitulé « local output only », et c'était vrai :
//! les trois seuls sites d'installation étaient derrière
//! `device_id.starts_with("local:")`. Depuis LAT-F1, le relais du bras
//! progressif (`StreamingDsp`) en porte un quatrième, ce qui ouvre les zones
//! RÉSEAU — sous conditions, énumérées par `crossfeed_status`. Le chemin
//! FICHIER, lui, n'en porte toujours aucun, et un garde-fou de ce module le
//! vérifie.
//!
//! Crossfeed blends a small, delayed amount of each channel into the other to
//! relax the hard left/right separation of a stereo recording heard on
//! headphones. On loudspeakers each ear hears both channels (with a tiny
//! interaural delay); headphones deliver a channel to one ear only, which the
//! brain reads as an unnaturally wide, "in-the-head" image. Crossfeed emulates
//! the acoustic cross-path so the stereo image moves in front of the listener.
//!
//! # Algorithm (difference-based, Mid-preserving)
//!
//! For each stereo frame `n`, with `Ld`/`Rd` the left/right samples delayed by
//! `delay_samples`:
//!
//! ```text
//! L_out = L[n] + amount * (Rd - Ld)
//! R_out = R[n] + amount * (Ld - Rd)
//! ```
//!
//! The two correction terms are exact negatives, so `L_out + R_out == L + R`:
//! the **Mid** (mono sum) is preserved bit-for-bit and only the **Side**
//! (difference) is attenuated/reshaped. Perfectly mono material (`L == R`) is
//! therefore returned untouched. When `delay_samples == 0` the terms collapse
//! to the instantaneous `amount * (R - L)` / `amount * (L - R)`.
//!
//! # v1 scope — head-shadow filter, OFF by default (#5081)
//!
//! Until 0.9.165 this section read « NO filtering, by design »: Thierry wanted
//! "zéro coloration" — an image narrowed with no tonal change — and the
//! low-pass on the crossfed term was deferred until he signed it off.
//!
//! What changed (#5081, <https://github.com/renesenses/tune-server-rust/issues/5081>):
//! a listener asked for the cutoff frequency and the slope of that low-pass —
//! « un paramètre clé pour trader entre ampleur de l'espace et précision de la
//! scène » — and Bertrand decided (26/09/2026) to put them in THIS v1,
//! **disabled by default**, with Thierry's agreement (he cites Jan Meier
//! "extended", about 3 dB/oct near 1200 Hz, and Gold Note, up to 20 kHz).
//!
//! Real HRTF crossfeed (bs2b, Meier, Linkwitz…) low-passes the crossfed term so
//! only lower frequencies bleed across, mimicking head shadowing at high
//! frequencies. Here that filter, `f`, only ever touches the crossfed term:
//!
//! ```text
//! L_out = L[n] + amount * f(Rd - Ld)
//! R_out = R[n] + amount * f(Ld - Rd)
//! ```
//!
//! so the Mid is still preserved and mono content still passes untouched.
//! - `f` OFF (the default, `head_shadow_enabled: false`): the historical
//!   loop runs, bit-for-bit — "zéro coloration" stays the default promise;
//! - `f` ON: cutoff 200 Hz – 20 kHz (default 700 Hz), slope 3 – 6 dB/oct
//!   (default 6). The design, what is measured and the documented deviations
//!   live in `ombre.rs`.
//!
//! A settings change while playing, with the filter involved on either side,
//! cross-fades the crossfed term over [`DUREE_DU_FONDU_S`] from the old
//! processor to the new one, both fed the same (shared) history: no step.

use crate::ombre::{FiltreOmbre, OmbreDeTete};

/// Hard ceiling on the crossfeed delay, in milliseconds.
///
/// Physiological interaural delay tops out around 0.6–0.7 ms; a few ms is
/// already well past anything useful and only bloats the ring buffers. We cap
/// at 5 ms as a sane guard against a bogus config value.
const MAX_DELAY_MS: f32 = 5.0;

/// #5081 — durée du fondu d'un changement de réglage en cours de lecture,
/// quand le filtre d'ombre est en jeu d'un côté ou de l'autre.
pub const DUREE_DU_FONDU_S: f64 = 0.010;

/// Difference-based, Mid-preserving headphone crossfeed.
///
/// State (the two per-channel delay lines) persists across `process_interleaved`
/// calls because audio arrives in arbitrarily sized chunks. No allocation
/// happens in the processing hot loop.
#[derive(Clone)]
pub struct CrossfeedProcessor {
    /// Crossfeed strength. 0.0 = bypass (identity), higher = narrower image.
    amount: f32,
    /// Delay applied to the crossfed term, in samples (0 = instantaneous).
    delay_samples: usize,
    /// Left-channel delay line (ring buffer of the dry left signal).
    ring_l: Vec<f32>,
    /// Right-channel delay line (ring buffer of the dry right signal).
    ring_r: Vec<f32>,
    /// Shared read/write cursor into both ring buffers.
    pos: usize,
    /// #5081 — débit, pour la durée du fondu.
    sample_rate: u32,
    /// #5081 — le réglage d'ombre (borné) et son filtre ; `None` = éteint.
    ombre: Option<OmbreDeTete>,
    filtre: Option<FiltreOmbre>,
    /// #5081 — le processeur d'avant un changement de réglage, qui continue
    /// de tourner le temps du fondu.
    fondu: Option<Box<Fondu>>,
}

/// #5081 — un fondu en cours : l'ancien processeur, nourri des mêmes
/// échantillons, et le nombre de trames qui restent.
#[derive(Clone)]
struct Fondu {
    ancien: CrossfeedProcessor,
    restant: usize,
    total: usize,
}

impl CrossfeedProcessor {
    /// Build a processor for the given `sample_rate` (Hz), `amount` (strength)
    /// and `delay_ms` (crossfeed delay, capped at `MAX_DELAY_MS`).
    ///
    /// `delay_samples = round(delay_ms / 1000 * sample_rate)`, clamped so a
    /// pathological config can never allocate an unbounded buffer.
    ///
    /// No head-shadow filter: see [`Self::avec_ombre`].
    pub fn new(sample_rate: u32, amount: f32, delay_ms: f32) -> Self {
        Self::avec_ombre(sample_rate, amount, delay_ms, None)
    }

    /// #5081 — [`Self::new`], avec le filtre d'ombre de la tête sur le terme
    /// croisé quand `ombre` est `Some` (le réglage est borné ici).
    pub fn avec_ombre(
        sample_rate: u32,
        amount: f32,
        delay_ms: f32,
        ombre: Option<OmbreDeTete>,
    ) -> Self {
        let delay_samples = retard_en_echantillons(sample_rate, delay_ms);
        let ombre = ombre.map(OmbreDeTete::bornee);
        Self {
            amount,
            delay_samples,
            ring_l: vec![0.0; delay_samples],
            ring_r: vec![0.0; delay_samples],
            pos: 0,
            sample_rate,
            ombre,
            filtre: ombre.map(|o| FiltreOmbre::concevoir(sample_rate, o)),
            fondu: None,
        }
    }

    /// #5081 — le réglage d'ombre de ce processeur (borné), `None` s'il est
    /// éteint.
    pub fn ombre(&self) -> Option<OmbreDeTete> {
        self.ombre
    }

    /// Rien à faire : force nulle et aucun fondu en cours.
    pub fn est_neutre(&self) -> bool {
        self.amount == 0.0 && self.fondu.is_none()
    }

    /// Les échantillons retardés `(Ld, Rd)` de la trame `(l, r)`, ligne à
    /// retard avancée.
    #[inline]
    fn retarder(&mut self, l: f32, r: f32) -> (f32, f32) {
        if self.delay_samples == 0 {
            return (l, r);
        }
        let d = (self.ring_l[self.pos], self.ring_r[self.pos]);
        self.ring_l[self.pos] = l;
        self.ring_r[self.pos] = r;
        self.pos += 1;
        if self.pos >= self.delay_samples {
            self.pos = 0;
        }
        d
    }

    /// #5081 — le terme croisé `c` de la trame (`L_out = l + c`,
    /// `R_out = r − c`), filtre et fondu compris.
    #[inline]
    fn terme_croise(&mut self, l: f32, r: f32) -> f64 {
        let neuf = if self.amount == 0.0 {
            0.0
        } else {
            let (ld, rd) = self.retarder(l, r);
            let difference = f64::from(rd - ld);
            let filtree = match &mut self.filtre {
                Some(f) => f.traiter(difference),
                None => difference,
            };
            f64::from(self.amount) * filtree
        };
        let Some(fondu) = self.fondu.as_deref_mut() else {
            return neuf;
        };
        let ancien = fondu.ancien.terme_croise(l, r);
        // Cosinus surélevé : 0 → 1, à dérivée nulle aux deux bouts.
        let t = 1.0 - fondu.restant as f64 / fondu.total as f64;
        let poids = 0.5 - 0.5 * (std::f64::consts::PI * t).cos();
        fondu.restant -= 1;
        if fondu.restant == 0 {
            self.fondu = None;
        }
        ancien + poids * (neuf - ancien)
    }

    /// Process a **stereo interleaved** f32 buffer (`[L0, R0, L1, R1, …]`,
    /// normalised to -1..1) in place.
    ///
    /// Safe no-op when the buffer holds an odd number of samples (not a whole
    /// number of stereo frames). The caller (`local.rs`) additionally gates this
    /// on `channels == 2`, so non-stereo audio never reaches here.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        // Not a whole number of stereo frames → cannot interpret as L/R pairs.
        if !samples.len().is_multiple_of(2) {
            return;
        }
        if self.est_neutre() {
            return; // exact identity, and no delay-line state to advance
        }
        // #5081 — filtre d'ombre allumé, ou fondu en cours : le terme croisé
        // passe par `terme_croise`. Sinon, la boucle historique ci-dessous,
        // intacte : éteint, le filtre ne change pas un bit.
        if self.filtre.is_some() || self.fondu.is_some() {
            for trame in samples.as_chunks_mut::<2>().0 {
                let (l, r) = (trame[0], trame[1]);
                let c = self.terme_croise(l, r) as f32;
                trame[0] = (l + c).clamp(-1.0, 1.0);
                trame[1] = (r - c).clamp(-1.0, 1.0);
            }
            return;
        }

        let frames = samples.len() / 2;
        for f in 0..frames {
            let li = 2 * f;
            let ri = li + 1;
            let l = samples[li];
            let r = samples[ri];

            // Delayed dry samples. delay_samples == 0 → instantaneous term.
            let (ld, rd) = if self.delay_samples == 0 {
                (l, r)
            } else {
                let d = (self.ring_l[self.pos], self.ring_r[self.pos]);
                // Store the CURRENT dry sample AFTER reading the delayed one.
                self.ring_l[self.pos] = l;
                self.ring_r[self.pos] = r;
                self.pos += 1;
                if self.pos >= self.delay_samples {
                    self.pos = 0;
                }
                d
            };

            let l_out = l + self.amount * (rd - ld);
            let r_out = r + self.amount * (ld - rd);

            // Guard against any overshoot before it hits the DAC.
            samples[li] = l_out.clamp(-1.0, 1.0);
            samples[ri] = r_out.clamp(-1.0, 1.0);
        }
    }

    /// Appliquer le crossfeed à du PCM entier entrelacé, en place — la même
    /// disposition que celle passée à `EqProcessor::process_pcm` et à
    /// `Convolver::process_pcm` (petit-boutien, 16 / 24 / 32 bits).
    ///
    /// C'est la porte du bras PROGRESSIF. La sortie locale, elle, tient déjà
    /// des `f32` et appelle directement `process_interleaved` : les deux
    /// chemins partagent donc l'algorithme, et lui seul.
    ///
    /// `channels` est un paramètre parce que ce processeur ne le porte pas :
    /// le crossfeed n'a de sens qu'en stéréo — c'est un effet de séparation
    /// gauche/droite — et tout autre nombre de canaux est un **non-op**
    /// silencieux, comme la garde `channels == 2` que `local.rs` applique de
    /// son côté. Une conversion qui interpréterait du 5.1 en paires L/R
    /// mélangerait des canaux sans rapport.
    ///
    /// La ligne à retard vit entre les appels : le découpage en chunks du bras
    /// progressif est donc transparent, exactement comme pour les biquads de
    /// l'égaliseur et le recouvrement du convolveur.
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16, channels: u16) {
        if channels != 2 || pcm.is_empty() || self.est_neutre() {
            return;
        }
        let bps = (bit_depth / 8) as usize;
        if bps == 0 {
            return;
        }
        let total = pcm.len() / bps;
        // Une trame stéréo incomplète en fin de chunk n'est pas interprétable
        // en paire L/R : on s'arrête à la dernière trame ENTIÈRE et on laisse
        // les octets restants intacts plutôt que de les décaler d'un canal.
        let total = total - (total % 2);
        if total == 0 {
            return;
        }
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
        self.process_interleaved(&mut buf);
        for (i, sample) in buf.iter().enumerate().take(total) {
            let o = i * bps;
            let s = sample.clamp(-1.0, 1.0);
            match bit_depth {
                16 => {
                    pcm[o..o + 2].copy_from_slice(&quantifier_i16(s).to_le_bytes());
                }
                24 => {
                    pcm[o..o + 3].copy_from_slice(&quantifier_i24(s));
                }
                32 => {
                    pcm[o..o + 4].copy_from_slice(&quantifier_i32(s).to_le_bytes());
                }
                _ => {}
            }
        }
    }

    /// Reprendre la ligne à retard d'un processeur précédent, pour qu'un
    /// remplacement **en cours de lecture** ne claque pas.
    ///
    /// Miroir de `EqProcessor::inherit_state_from`. Le terme croisé est bâti
    /// sur les échantillons retardés : si la ligne repart à zéro, il chute
    /// brutalement au silence pendant `delay_samples` échantillons — une
    /// discontinuité, donc un clic. Et un curseur qu'on fait glisser en
    /// produirait un par cran.
    ///
    /// Trois cas, du plus fréquent au plus rare :
    ///
    /// - **même retard** (on a bougé `amount`) : l'historique est transféré tel
    ///   quel, le changement est inaudible hors du réglage voulu ;
    /// - **retard raccourci** : on garde les échantillons les plus RÉCENTS, ce
    ///   sont eux que la nouvelle ligne va relire en premier ;
    /// - **retard allongé** : on ne possède pas l'histoire manquante. Les
    ///   échantillons connus sont placés à la fin, le début reste à zéro. Le
    ///   creux est inévitable — on ne l'invente pas — mais il est borné à la
    ///   différence de longueur au lieu de valoir toute la ligne.
    ///
    /// #5081 — quand le filtre d'ombre est en jeu d'un côté ou de l'autre (ou
    /// qu'un fondu est déjà en cours), la ligne est reprise de même, puis le
    /// terme croisé passe en fondu de [`DUREE_DU_FONDU_S`] de l'ancien
    /// processeur — qui continue de tourner sur les mêmes échantillons — au
    /// nouveau. Un filtre de mêmes coefficients reprend en plus l'état de
    /// l'ancien. Filtre éteint des deux côtés : rien de plus qu'avant, au bit
    /// près.
    pub fn inherit_state_from(&mut self, previous: &CrossfeedProcessor) {
        self.heriter_la_ligne_a_retard(previous);
        if self.filtre.is_none() && previous.filtre.is_none() && previous.fondu.is_none() {
            return;
        }
        if self.sample_rate == previous.sample_rate
            && self.ombre == previous.ombre
            && let (Some(neuf), Some(ancien)) = (&mut self.filtre, &previous.filtre)
        {
            neuf.reprendre_etat(ancien);
        }
        let identique = previous.fondu.is_none()
            && self.sample_rate == previous.sample_rate
            && self.amount == previous.amount
            && self.delay_samples == previous.delay_samples
            && self.ombre == previous.ombre;
        if identique {
            return;
        }
        let total = ((f64::from(self.sample_rate) * DUREE_DU_FONDU_S).round() as usize).max(1);
        let mut ancien = previous.clone();
        // Un fondu dans le fondu, au plus : un curseur qu'on fait glisser
        // n'empile pas les processeurs.
        if let Some(f) = ancien.fondu.as_deref_mut() {
            f.ancien.fondu = None;
        }
        self.fondu = Some(Box::new(Fondu {
            ancien,
            restant: total,
            total,
        }));
    }

    /// La reprise de la ligne à retard seule, décrite ci-dessus.
    fn heriter_la_ligne_a_retard(&mut self, previous: &CrossfeedProcessor) {
        let (n_neuf, n_prec) = (self.delay_samples, previous.delay_samples);
        if n_neuf == 0 || n_prec == 0 {
            return; // Pas de ligne à retard d'un côté ou de l'autre.
        }
        if n_neuf == n_prec {
            self.ring_l.clone_from(&previous.ring_l);
            self.ring_r.clone_from(&previous.ring_r);
            self.pos = previous.pos;
            return;
        }
        // Rejouer l'historique du plus ancien au plus récent : dans l'anneau
        // précédent le plus ancien est en `pos`, et on avance en bouclant.
        let a_reprendre = n_neuf.min(n_prec);
        // Départ = le plus récent moins `a_reprendre`, modulo la taille.
        let debut = (previous.pos + n_prec - a_reprendre) % n_prec;
        let decalage = n_neuf - a_reprendre; // 0 si on raccourcit
        for i in 0..a_reprendre {
            let src = (debut + i) % n_prec;
            self.ring_l[decalage + i] = previous.ring_l[src];
            self.ring_r[decalage + i] = previous.ring_r[src];
        }
        self.pos = 0;
    }

    /// Clear stream history without allocation; parameters are unchanged.
    pub fn reset_history(&mut self) {
        self.ring_l.fill(0.0);
        self.ring_r.fill(0.0);
        self.pos = 0;
        if let Some(f) = &mut self.filtre {
            f.reinitialiser();
        }
        self.fondu = None;
    }

    /// Crossfeed strength this processor was built with.
    pub fn amount(&self) -> f32 {
        self.amount
    }

    /// Effective crossfeed delay, in samples.
    pub fn delay_samples(&self) -> usize {
        self.delay_samples
    }
}

// ---------------------------------------------------------------------------
// #4973 — retour à l'entier, à la MÊME échelle que le décodage
// ---------------------------------------------------------------------------
//
// Le décodage divise par 2^(N−1) (32 768, 2^23 via `<< 8` puis 2^31, 2^31) ;
// l'encodage multipliait par 2^(N−1) − 1. Le module entier s'en trouvait
// atténué de (2^(N−1) − 1)/2^(N−1) : −32 768 revenait −32 767, et tout
// échantillon au-delà de la demi-échelle perdait 1 LSB — y compris sur une
// source MONO, dont le Mid est pourtant rendu intact par l'algorithme.
//
// C'est la convention de toute la chaîne principale qui est reprise ici,
// telle quelle : échelle 2^(N−1) dans les deux sens, arrondi au plus proche,
// saturation au rail EN DERNIER dans [−2^(N−1), 2^(N−1) − 1], par la porte
// partagée `quantifier_avec` avec un bruit nul — celle du convolveur et de
// l'égaliseur, et l'échelle de `f32_to_native_i32` de la sortie locale. La
// saturation borne le +1,0 exact, qui vaut 2^(N−1) et n'a pas de mot.
//
// Les trois fonctions servent le chemin `process_pcm` (bras progressif,
// ré-encodage réseau) ET l'instance du greffon (`sdk.rs`) : un seul arrondi
// pour les deux portes.

/// Un échantillon normalisé ramené en mot 16 bits signé.
pub(crate) fn quantifier_i16(s: f32) -> i16 {
    tune_plugin_audio_support::dither::quantifier_avec(
        f64::from(s) * 32_768.0,
        0.0,
        -32_768.0,
        32_767.0,
    ) as i16
}

/// Un échantillon normalisé ramené en mot 24 bits signé, petit-boutien.
pub(crate) fn quantifier_i24(s: f32) -> [u8; 3] {
    let v = tune_plugin_audio_support::dither::quantifier_avec(
        f64::from(s) * 8_388_608.0,
        0.0,
        -8_388_608.0,
        8_388_607.0,
    ) as i32;
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

/// Un échantillon normalisé ramené en mot 32 bits signé.
///
/// Le traitement se fait en `f32` : un mot 32 bits n'y survit exactement que
/// s'il tient dans sa mantisse (24 bits significatifs) — c'est le cas des
/// rails, et de tout 24 bits aligné à gauche. L'échelle, elle, est exacte.
pub(crate) fn quantifier_i32(s: f32) -> i32 {
    tune_plugin_audio_support::dither::quantifier_avec(
        f64::from(s) * 2_147_483_648.0,
        0.0,
        f64::from(i32::MIN),
        f64::from(i32::MAX),
    ) as i32
}

/// Le retard RÉEL du terme croisé, en échantillons : `delay_ms` borné à
/// [`MAX_DELAY_MS`] puis arrondi au débit. Une seule formule pour le
/// processeur et pour [`gain_moyen_db`] : la compensation doit parler du
/// filtre construit, pas du réglage demandé.
fn retard_en_echantillons(sample_rate: u32, delay_ms: f32) -> usize {
    let clamped_ms = delay_ms.clamp(0.0, MAX_DELAY_MS);
    ((clamped_ms / 1000.0) * sample_rate as f32).round() as usize
}

/// Corrélation gauche/droite du signal de référence de [`gain_moyen_db`].
///
/// Le crossfeed conserve le Mid au bit près et ne touche qu'au Side : ce
/// qu'il fait perdre en niveau dépend donc de la part de Side dans la
/// musique, que le filtre seul ne connaît pas. Il faut une référence, et
/// elle est posée ICI, une fois : 0 serait deux canaux sans rapport (le pire
/// cas, qu'aucun mixage ne produit), 1 une source mono (le crossfeed ne
/// change alors rien). 0,5 — Side 4,8 dB sous le Mid — est une CONVENTION
/// de mixage stéréo ordinaire, pas une mesure : à reprendre à l'oreille si
/// la compensation se révèle trop forte ou trop faible.
pub const CORRELATION_DE_REFERENCE: f64 = 0.5;

/// #4685 — ce que ce crossfeed fait gagner (> 0) ou perdre (< 0) au niveau
/// MOYEN d'un canal, en dB, sur un bruit rose stéréo de corrélation
/// [`CORRELATION_DE_REFERENCE`].
///
/// Calculé depuis le filtre lui-même, donc identique pour toute la musique :
/// la compensation qu'on en tire est un gain FIXE, pas un automatisme.
///
/// Le calcul suit l'algorithme ligne à ligne. Avec `M = (L+R)/2` et
/// `S = (L−R)/2`, le module rend `M` intact et
/// `S_out = S · (1 − 2a·z^−D)` — la différence des deux termes croisés.
/// Pour L et R de même puissance et de corrélation ρ, `M` et `S` sont
/// décorrélés, de puissances `(1+ρ)/2` et `(1−ρ)/2`, et un canal de sortie
/// vaut donc, à la fréquence `f` :
///
/// ```text
/// P(f) = (1+ρ)/2 + (1−ρ)/2 · |1 − 2a·e^(−j2πfD/fs)|²
///      = (1+ρ)/2 + (1−ρ)/2 · (1 − 4a·cos(2πfD/fs) + 4a²)
/// ```
///
/// Sans retard (`D = 0`) le Side est simplement multiplié par `1 − 2a` : le
/// niveau baisse partout (−0,90 dB à 25 %, −1,19 dB à 40 %). Avec retard,
/// c'est un peigne : il creuse le grave (`cos ≈ 1`) — d'où « surtout dans le
/// grave » — mais POUSSE le Side par endroits dans l'aigu, et en moyenne rose
/// les deux se compensent presque : les trois réglages du client restent à
/// quelques dixièmes de dB, dans un sens ou dans l'autre (témoin
/// `avec_retard_le_peigne_rend_dans_l_aigu_ce_qu_il_prend_au_grave`). La
/// compensation qui en découle peut donc être une ATTÉNUATION.
///
/// Hors calcul : l'écrêtage du module (clamp à ±1), non linéaire, et un flux
/// non stéréo, que le module laisse intact (0 dB serait alors juste ; ce
/// calcul ne connaît pas les canaux et suppose la stéréo).
pub fn gain_moyen_db(sample_rate: u32, amount: f32, delay_ms: f32) -> f64 {
    gain_moyen_db_avec_ombre(sample_rate, amount, delay_ms, None)
}

/// #5081 — [`gain_moyen_db`], filtre d'ombre compris : avec `H(f)` la réponse
/// complexe du filtre construit, le Side devient `S · (1 − 2a·H(f)·z^−D)`, et
///
/// ```text
/// P(f) = (1+ρ)/2 + (1−ρ)/2 · |1 − 2a·H(f)·e^(−j2πfD/fs)|²
/// ```
///
/// `None` rend exactement le calcul d'avant (`H = 1`).
pub fn gain_moyen_db_avec_ombre(
    sample_rate: u32,
    amount: f32,
    delay_ms: f32,
    ombre: Option<OmbreDeTete>,
) -> f64 {
    if amount == 0.0 || !amount.is_finite() || sample_rate == 0 {
        return 0.0;
    }
    let a = f64::from(amount);
    let retard = retard_en_echantillons(sample_rate, delay_ms) as f64;
    let fs = f64::from(sample_rate);
    let rho = CORRELATION_DE_REFERENCE;
    let (p_mid, p_side) = ((1.0 + rho) / 2.0, (1.0 - rho) / 2.0);
    let filtre = ombre.map(|o| FiltreOmbre::concevoir(sample_rate, o.bornee()));
    tune_plugin_audio_support::niveau_moyen::gain_moyen_rose_db(fs, |f| {
        let phase = 2.0 * std::f64::consts::PI * f * retard / fs;
        let Some(filtre) = &filtre else {
            let cos = phase.cos();
            return p_mid + p_side * (1.0 - 4.0 * a * cos + 4.0 * a * a);
        };
        let (hr, hi) = filtre.reponse(f, sample_rate);
        // X = H · e^(−jφ)
        let (xr, xi) = (
            hr * phase.cos() + hi * phase.sin(),
            hi * phase.cos() - hr * phase.sin(),
        );
        p_mid + p_side * ((1.0 - 2.0 * a * xr).powi(2) + (2.0 * a * xi).powi(2))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mono content (L == R) must pass through untouched: crossfeed only acts on
    /// the channel difference, which is zero here, so the Mid is preserved.
    #[test]
    fn mono_content_is_unchanged() {
        let mut cf = CrossfeedProcessor::new(44100, 0.30, 0.30);
        // Interleaved with L == R on every frame.
        let orig: Vec<f32> = (0..64)
            .flat_map(|i| {
                let v = ((i as f32) * 0.05).sin() * 0.5;
                [v, v]
            })
            .collect();
        let mut samples = orig.clone();
        cf.process_interleaved(&mut samples);
        for (i, (&a, &b)) in samples.iter().zip(orig.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "frame idx {i}: {a} != {b}");
        }
    }

    /// Hard-panned signal (L == 0, R != 0): the left channel must pick up a
    /// delayed fraction of R (image pulled in), and the right channel must lose
    /// a matching fraction (narrower Side).
    #[test]
    fn hard_panned_signal_narrows_image() {
        let amount = 0.30f32;
        // delay_ms = 0 keeps the term instantaneous so we can assert exact math.
        let mut cf = CrossfeedProcessor::new(44100, amount, 0.0);
        // Two frames: L=0, R=0.8.
        let mut samples = vec![0.0, 0.8, 0.0, 0.8];
        cf.process_interleaved(&mut samples);
        // L_out = 0 + amount*(R - L) = amount*0.8
        assert!(
            (samples[0] - amount * 0.8).abs() < 1e-6,
            "L_out {}",
            samples[0]
        );
        // R_out = 0.8 + amount*(L - R) = 0.8 - amount*0.8
        assert!(
            (samples[1] - (0.8 - amount * 0.8)).abs() < 1e-6,
            "R_out {}",
            samples[1]
        );
        // Left now carries signal (image resserrée), right is attenuated.
        assert!(samples[0] > 0.0);
        assert!(samples[1] < 0.8);
    }

    /// amount == 0 is an exact identity (bypass).
    #[test]
    fn zero_amount_is_identity() {
        let mut cf = CrossfeedProcessor::new(96000, 0.0, 0.30);
        let orig = vec![0.1f32, -0.2, 0.3, -0.4, 0.5, -0.6];
        let mut samples = orig.clone();
        cf.process_interleaved(&mut samples);
        assert_eq!(samples, orig);
    }

    /// Mid (mono sum) is preserved frame-by-frame for arbitrary stereo content.
    #[test]
    fn mid_is_preserved() {
        let mut cf = CrossfeedProcessor::new(44100, 0.45, 0.30);
        let orig = vec![0.6f32, -0.3, -0.9, 0.2, 0.1, 0.7, 0.4, -0.8];
        let mut samples = orig.clone();
        cf.process_interleaved(&mut samples);
        for f in 0..orig.len() / 2 {
            let sum_in = orig[2 * f] + orig[2 * f + 1];
            let sum_out = samples[2 * f] + samples[2 * f + 1];
            // Preserved up to the output clamp (none of these frames clip).
            assert!(
                (sum_in - sum_out).abs() < 1e-6,
                "frame {f}: mid {sum_in} != {sum_out}"
            );
        }
    }

    /// No NaN, and every output stays within the clamp range.
    #[test]
    fn no_nan_and_within_range() {
        let mut cf = CrossfeedProcessor::new(44100, 0.5, 0.30);
        // Hot signal near full scale, both polarities.
        let mut samples: Vec<f32> = (0..256)
            .map(|i| if i % 2 == 0 { 0.99 } else { -0.99 })
            .collect();
        cf.process_interleaved(&mut samples);
        for &s in &samples {
            assert!(s.is_finite(), "non-finite sample: {s}");
            assert!((-1.0..=1.0).contains(&s), "out of range: {s}");
        }
    }

    /// Odd-length buffer (not whole stereo frames) is a safe no-op.
    #[test]
    fn odd_length_is_noop() {
        let mut cf = CrossfeedProcessor::new(44100, 0.30, 0.30);
        let orig = vec![0.1f32, 0.2, 0.3];
        let mut samples = orig.clone();
        cf.process_interleaved(&mut samples);
        assert_eq!(samples, orig);
    }

    /// Delay state persists across chunk boundaries (buffer arrives in pieces).
    #[test]
    fn delay_state_persists_across_chunks() {
        // 1-sample delay: the crossfed term of frame n uses frame n-1.
        let mut cf = CrossfeedProcessor::new(1000, 0.5, 1.0);
        assert_eq!(cf.delay_samples(), 1);
        // Chunk 1: single frame L=0, R=1.0 — delay line was zero, so no bleed yet.
        let mut c1 = vec![0.0, 1.0];
        cf.process_interleaved(&mut c1);
        assert!((c1[0] - 0.0).abs() < 1e-6, "frame0 L {}", c1[0]);
        // Chunk 2: L=0, R=0 — now the delayed R (=1.0) from chunk 1 bleeds in.
        let mut c2 = vec![0.0, 0.0];
        cf.process_interleaved(&mut c2);
        // L_out = 0 + 0.5*(Rd - Ld) = 0.5*(1.0 - 0.0) = 0.5
        assert!((c2[0] - 0.5).abs() < 1e-6, "carried delay L {}", c2[0]);
    }

    /// Meme retard : l'historique doit etre transfere a l'identique, sinon le
    /// terme croise chute au silence et un curseur qu'on glisse claque a chaque
    /// cran (#1786).
    #[test]
    fn heritage_meme_retard_transfere_lhistorique() {
        let mut prec = CrossfeedProcessor::new(48000, 0.3, 1.0);
        let mut tampon: Vec<f32> = (0..200).map(|i| (i as f32) / 200.0).collect();
        prec.process_interleaved(&mut tampon);

        let mut neuf = CrossfeedProcessor::new(48000, 0.5, 1.0);
        assert_eq!(neuf.delay_samples(), prec.delay_samples());
        neuf.inherit_state_from(&prec);

        assert_eq!(neuf.ring_l, prec.ring_l);
        assert_eq!(neuf.ring_r, prec.ring_r);
        assert_eq!(neuf.pos, prec.pos);
    }

    /// Retard raccourci : on garde les echantillons les plus RECENTS, ce sont
    /// eux que la nouvelle ligne relira en premier.
    #[test]
    fn heritage_retard_raccourci_garde_les_plus_recents() {
        let mut prec = CrossfeedProcessor::new(48000, 0.3, 1.0);
        let mut tampon: Vec<f32> = (0..400).map(|i| (i as f32) / 400.0).collect();
        prec.process_interleaved(&mut tampon);

        let mut neuf = CrossfeedProcessor::new(48000, 0.3, 0.5);
        assert!(neuf.delay_samples() < prec.delay_samples());
        neuf.inherit_state_from(&prec);

        // Aucun zero : la ligne courte est entierement remplie d'historique.
        assert!(
            neuf.ring_l.iter().all(|v| *v != 0.0),
            "ligne partiellement vide"
        );
        assert_eq!(neuf.pos, 0);

        // Et ce sont bien les plus recents. Le dernier echantillon ecrit par
        // `prec` est en `pos - 1`, il doit se retrouver en fin de nouvelle ligne.
        let dernier = prec.ring_l[(prec.pos + prec.delay_samples() - 1) % prec.delay_samples()];
        assert_eq!(*neuf.ring_l.last().unwrap(), dernier);
    }

    /// Retard allonge : on ne possede pas l'histoire manquante, on ne l'invente
    /// pas. Les echantillons connus vont a la FIN, le creux est borne a la
    /// difference de longueur au lieu de valoir toute la ligne.
    #[test]
    fn heritage_retard_allonge_place_le_connu_a_la_fin() {
        let mut prec = CrossfeedProcessor::new(48000, 0.3, 0.5);
        let mut tampon: Vec<f32> = (0..400).map(|i| 0.1 + (i as f32) / 400.0).collect();
        prec.process_interleaved(&mut tampon);

        let mut neuf = CrossfeedProcessor::new(48000, 0.3, 1.0);
        assert!(neuf.delay_samples() > prec.delay_samples());
        neuf.inherit_state_from(&prec);

        let connus = prec.delay_samples();
        let creux = neuf.delay_samples() - connus;
        assert!(
            neuf.ring_l[..creux].iter().all(|v| *v == 0.0),
            "le creux doit etre en tete"
        );
        assert!(
            neuf.ring_l[creux..].iter().all(|v| *v != 0.0),
            "le connu doit etre en fin"
        );
        assert_eq!(neuf.pos, 0);
    }

    /// Sans ligne a retard d'un cote ou de l'autre, il n'y a rien a heriter et
    /// rien ne doit paniquer (division par zero, indexation hors bornes).
    #[test]
    fn heritage_sans_ligne_a_retard_ne_panique_pas() {
        let prec = CrossfeedProcessor::new(48000, 0.3, 0.0);
        let mut neuf = CrossfeedProcessor::new(48000, 0.3, 1.0);
        neuf.inherit_state_from(&prec);
        assert!(neuf.ring_l.iter().all(|v| *v == 0.0));

        let prec2 = CrossfeedProcessor::new(48000, 0.3, 1.0);
        let mut neuf2 = CrossfeedProcessor::new(48000, 0.3, 0.0);
        neuf2.inherit_state_from(&prec2);
        assert!(neuf2.ring_l.is_empty());
    }

    // -------------------------------------------------------------------
    // #4685 — le niveau moyen, calculé puis MESURÉ
    // -------------------------------------------------------------------

    /// Passe un Mid et un Side multi-sinus (corrélation L/R = 0,5, celle de
    /// la référence) dans le VRAI processeur et rend (entrée, sortie) en dB
    /// RMS sur le canal gauche, ligne à retard amorcée.
    fn rms_avant_apres(sample_rate: u32, amount: f32, delay_ms: f32) -> (f64, f64) {
        use tune_plugin_audio_support::niveau_moyen::{multisinus_rose, rms_db};
        let frames = sample_rate as usize * 2;
        let mid = multisinus_rose(sample_rate, frames, 0x4685);
        let side = multisinus_rose(sample_rate, frames, 0x1234_5678);
        // Puissance du Side = 1/3 de celle du Mid ⇔ ρ = (1−1/3)/(1+1/3) = 0,5.
        let k = (1.0_f64 / 3.0).sqrt();
        let entree: Vec<f32> = mid
            .iter()
            .zip(side.iter())
            .flat_map(|(m, s)| [(m + k * s) as f32, (m - k * s) as f32])
            .collect();
        let mut sortie = entree.clone();
        let mut cf = CrossfeedProcessor::new(sample_rate, amount, delay_ms);
        cf.process_interleaved(&mut sortie);
        let gauche = |v: &[f32]| -> Vec<f64> {
            v.as_chunks::<2>()
                .0
                .iter()
                .skip(frames / 4)
                .map(|p| f64::from(p[0]))
                .collect()
        };
        (rms_db(gauche(&entree)), rms_db(gauche(&sortie)))
    }

    /// Le témoin chiffré : sur les trois réglages tout faits du client
    /// (Léger 0,25/0,3 ms, Standard 0,30/0,5 ms, Fort 0,40/0,7 ms) et sans
    /// retard, l'écart de niveau MESURÉ au RMS doit retrouver
    /// [`gain_moyen_db`] à 0,25 dB près — et la compensation qu'on en tire
    /// doit rendre le niveau d'entrée.
    #[test]
    fn le_gain_moyen_calcule_retrouve_le_rms_mesure_4685() {
        for (sr, amount, delay) in [
            (44_100, 0.25_f32, 0.3_f32),
            (44_100, 0.30, 0.5),
            (48_000, 0.40, 0.7),
            (96_000, 0.30, 0.0),
            (44_100, 0.50, 0.0),
        ] {
            let calcule = gain_moyen_db(sr, amount, delay);
            let (avant, apres) = rms_avant_apres(sr, amount, delay);
            let mesure = apres - avant;
            eprintln!(
                "crossfeed {sr} Hz a={amount} d={delay} ms : calculé {calcule:+.3} dB, \
                 mesuré {mesure:+.3} dB (entrée {avant:.2} dB RMS, sortie {apres:.2}, \
                 compensée {:.2})",
                apres - calcule
            );
            assert!(
                (calcule - mesure).abs() < 0.25,
                "calculé {calcule:.3} dB ≠ mesuré {mesure:.3} dB ({sr} Hz, a={amount}, d={delay})"
            );
            assert!(
                ((apres - calcule) - avant).abs() < 0.25,
                "la compensation doit rendre le niveau d'entrée"
            );
        }
    }

    #[test]
    fn sans_crossfeed_il_n_y_a_rien_a_compenser() {
        assert_eq!(gain_moyen_db(44_100, 0.0, 0.3), 0.0);
        assert_eq!(gain_moyen_db(0, 0.3, 0.3), 0.0);
        assert_eq!(gain_moyen_db(44_100, f32::NAN, 0.3), 0.0);
    }

    /// SANS retard, le Side est simplement multiplié par `1 − 2a` : plus fort
    /// ⇒ plus de Side retiré ⇒ plus de niveau perdu, à toutes les fréquences.
    #[test]
    fn sans_retard_un_crossfeed_plus_fort_perd_plus() {
        let faible = gain_moyen_db(44_100, 0.10, 0.0);
        let moyen = gain_moyen_db(44_100, 0.25, 0.0);
        let fort = gain_moyen_db(44_100, 0.40, 0.0);
        assert!(
            0.0 > faible && faible > moyen && moyen > fort,
            "{faible} {moyen} {fort}"
        );
        // ρ = 0,5, a = 0,25 : Side × 0,5, soit 0,75 + 0,25 × 0,25 = 0,8125.
        assert!((moyen - 10.0 * 0.8125_f64.log10()).abs() < 1e-9, "{moyen}");
    }

    /// AVEC retard, le terme croisé devient un peigne : il creuse le Side
    /// dans le grave (sous `1/(4·retard)`) et le POUSSE par endroits dans
    /// l'aigu. En moyenne rose les deux se compensent presque : les trois
    /// réglages du client restent à quelques dixièmes de dB, dans un sens ou
    /// dans l'autre. La compensation qui en découle est donc petite — et elle
    /// peut ATTÉNUER. C'est le filtre qui le dit, pas une préférence.
    #[test]
    fn avec_retard_le_peigne_rend_dans_l_aigu_ce_qu_il_prend_au_grave() {
        for (amount, delay) in [(0.25_f32, 0.3_f32), (0.30, 0.5), (0.40, 0.7)] {
            let g = gain_moyen_db(44_100, amount, delay);
            assert!(g.abs() < 0.5, "a={amount} d={delay} : {g:+.3} dB");
            assert!(
                g > gain_moyen_db(44_100, amount, 0.0),
                "le retard doit rendre du niveau par rapport au même dosage instantané"
            );
        }
    }

    // -------------------------------------------------------------------
    // #4973 — l'échelle de retour à l'entier est celle du décodage
    // -------------------------------------------------------------------

    /// Chaque mot 16 bits, décodé puis requantifié, redonne LE MÊME mot —
    /// les deux rails compris. Avant : −32 768 → −32 767, et tout mot au-delà
    /// de la demi-échelle perdait 1 LSB.
    #[test]
    fn chaque_mot_16_bits_fait_l_aller_retour_4973() {
        for mot in i16::MIN..=i16::MAX {
            let s = f32::from(mot) / 32_768.0;
            assert_eq!(quantifier_i16(s), mot, "mot {mot}");
        }
    }

    /// Même chose sur les 2^24 mots 24 bits, par le décodage de `process_pcm`
    /// (octets placés en haut d'un `i32`, divisés par 2^31).
    #[test]
    fn chaque_mot_24_bits_fait_l_aller_retour_4973() {
        for mot in -8_388_608_i32..=8_388_607 {
            let b = mot.to_le_bytes();
            let s = i32::from_le_bytes([0, b[0], b[1], b[2]]) as f32 / 2_147_483_648.0;
            assert_eq!(quantifier_i24(s), [b[0], b[1], b[2]], "mot {mot}");
        }
    }

    /// 32 bits : le traitement est en `f32`, donc exact pour ce que sa
    /// mantisse porte — les rails, un 24 bits aligné à gauche, les petites
    /// valeurs. `i32::MAX` s'arrondit à 2^31 en `f32` (soit +1,0) et doit
    /// revenir `i32::MAX` par la saturation, pas déborder.
    #[test]
    fn les_mots_32_bits_representables_font_l_aller_retour_4973() {
        for mot in [
            i32::MIN,
            i32::MIN + 256,
            -(1 << 30),
            -256,
            -1,
            0,
            1,
            256,
            1 << 30,
            i32::MAX - 255,
            i32::MAX,
        ] {
            let s = mot as f32 / 2_147_483_648.0;
            assert_eq!(quantifier_i32(s), mot, "mot {mot}");
        }
    }

    /// Le +1,0 exact n'a pas de mot : il sature au rail positif, sans
    /// déborder ni changer de signe. Et au-delà, jamais de repli.
    #[test]
    fn le_plus_un_sature_au_rail_4973() {
        assert_eq!(quantifier_i16(1.0), i16::MAX);
        assert_eq!(quantifier_i16(-1.0), i16::MIN);
        assert_eq!(quantifier_i16(1.5), i16::MAX);
        assert_eq!(quantifier_i24(1.0), [0xff, 0xff, 0x7f]);
        assert_eq!(quantifier_i24(-1.0), [0x00, 0x00, 0x80]);
        assert_eq!(quantifier_i32(1.0), i32::MAX);
        assert_eq!(quantifier_i32(-1.0), i32::MIN);
    }

    /// Le PCM stéréo d'un mot par canal, L = R.
    fn pcm_mono(mots: &[i32], bps: usize) -> Vec<u8> {
        mots.iter()
            .flat_map(|m| {
                let b = m.to_le_bytes();
                let mot = if bps == 3 {
                    b[..3].to_vec()
                } else {
                    b[..bps].to_vec()
                };
                [mot.clone(), mot].concat()
            })
            .collect()
    }

    /// Le contrat du module — « Mid préservé au bit près, une source mono
    /// rendue intacte » — tenu sur du PCM ENTIER, crossfeed actif, rails
    /// compris, par la porte du bras progressif et du ré-encodage réseau.
    #[test]
    fn une_source_mono_traverse_process_pcm_au_bit_pres_4973() {
        let seize = [
            -32_768, -32_767, -16_385, -1, 0, 1, 16_384, 16_385, 32_766, 32_767,
        ];
        let vingt_quatre = [
            -8_388_608, -8_388_607, -4_194_305, -1, 0, 1, 4_194_304, 8_388_606, 8_388_607,
        ];
        let trente_deux = [i32::MIN, -1 << 30, -1, 0, 1, 1 << 30, i32::MAX];
        for (bits, mots) in [
            (16_u16, &seize[..]),
            (24, &vingt_quatre[..]),
            (32, &trente_deux[..]),
        ] {
            for (amount, delay_ms) in [(0.3_f32, 0.3_f32), (0.5, 0.0), (0.25, 1.0)] {
                let entree = pcm_mono(mots, usize::from(bits / 8));
                let mut sortie = entree.clone();
                CrossfeedProcessor::new(44_100, amount, delay_ms).process_pcm(&mut sortie, bits, 2);
                assert_eq!(sortie, entree, "{bits} bits, a={amount}, d={delay_ms} ms");
            }
        }
    }

    /// Crossfeed à 0 : bit-perfect, quelle que soit la profondeur — le module
    /// ne touche pas un octet, pas même par l'aller-retour.
    #[test]
    fn a_zero_process_pcm_ne_touche_aucun_octet_4973() {
        let entree: Vec<u8> = (0..=255_u8).cycle().take(4 * 3 * 64).collect();
        for bits in [16_u16, 24, 32] {
            let mut sortie = entree.clone();
            CrossfeedProcessor::new(48_000, 0.0, 0.3).process_pcm(&mut sortie, bits, 2);
            assert_eq!(sortie, entree, "{bits} bits");
        }
    }
}

#[cfg(test)]
#[path = "engine_ombre_5081_tests.rs"]
mod ombre_5081_tests;
