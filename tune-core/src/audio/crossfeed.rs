//! Headphone crossfeed DSP effect (built-in, local output only).
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
//! # v1 scope — NO filtering, by design
//!
//! Real HRTF crossfeed (bs2b, Meier, Linkwitz…) low-passes the crossfed term so
//! only lower frequencies bleed across, mimicking head shadowing at high
//! frequencies. We deliberately ship **zero** frequency shaping in v1: Thierry
//! wants "zéro coloration" — a pure, phase-linear image narrower with no tonal
//! change. The low-pass on the crossfed term is explicitly deferred to a v2
//! discussion. Do not add an EQ/low-pass here without that sign-off.

/// Hard ceiling on the crossfeed delay, in milliseconds.
///
/// Physiological interaural delay tops out around 0.6–0.7 ms; a few ms is
/// already well past anything useful and only bloats the ring buffers. We cap
/// at 5 ms as a sane guard against a bogus config value.
const MAX_DELAY_MS: f32 = 5.0;

/// Difference-based, Mid-preserving headphone crossfeed.
///
/// State (the two per-channel delay lines) persists across `process_interleaved`
/// calls because audio arrives in arbitrarily sized chunks. No allocation
/// happens in the processing hot loop.
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
}

impl CrossfeedProcessor {
    /// Build a processor for the given `sample_rate` (Hz), `amount` (strength)
    /// and `delay_ms` (crossfeed delay, capped at [`MAX_DELAY_MS`]).
    ///
    /// `delay_samples = round(delay_ms / 1000 * sample_rate)`, clamped so a
    /// pathological config can never allocate an unbounded buffer.
    pub fn new(sample_rate: u32, amount: f32, delay_ms: f32) -> Self {
        let clamped_ms = delay_ms.clamp(0.0, MAX_DELAY_MS);
        let delay_samples = ((clamped_ms / 1000.0) * sample_rate as f32).round() as usize;
        Self {
            amount,
            delay_samples,
            ring_l: vec![0.0; delay_samples],
            ring_r: vec![0.0; delay_samples],
            pos: 0,
        }
    }

    /// Process a **stereo interleaved** f32 buffer (`[L0, R0, L1, R1, …]`,
    /// normalised to -1..1) in place.
    ///
    /// Safe no-op when the buffer holds an odd number of samples (not a whole
    /// number of stereo frames). The caller (`local.rs`) additionally gates this
    /// on `channels == 2`, so non-stereo audio never reaches here.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        // Not a whole number of stereo frames → cannot interpret as L/R pairs.
        if samples.len() % 2 != 0 {
            return;
        }
        if self.amount == 0.0 {
            return; // exact identity, and no delay-line state to advance
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
    pub fn inherit_state_from(&mut self, previous: &CrossfeedProcessor) {
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

    /// Crossfeed strength this processor was built with.
    pub fn amount(&self) -> f32 {
        self.amount
    }

    /// Effective crossfeed delay, in samples.
    pub fn delay_samples(&self) -> usize {
        self.delay_samples
    }
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
}
