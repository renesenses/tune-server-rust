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

/// Contrainte qui prive le réglage « crossfeed » de son effet.
///
/// #2742 — Tades : « Crossfeed n'a aucune action ». Le serveur avait raison sur
/// le fond — le crossfeed est un effet de CASQUE, il n'est appliqué que par la
/// sortie locale — mais il l'imposait EN SILENCE. `GET /zones/{id}/dsp` rend le
/// réglage pour n'importe quelle zone, `PUT` le persiste pour n'importe quelle
/// zone, et les TROIS sites qui installent réellement un `CrossfeedProcessor`
/// (`orchestrator.rs` : le chemin de lecture, `refresh_zone_crossfeed`,
/// `refresh_zone_pure_dsp`) sont tous derrière la même double garde
/// `device_id.starts_with("local:")` + `downcast_ref::<LocalOutput>()`. Sur une
/// zone réseau le crossfeed n'a littéralement aucun chemin de code : la case
/// s'allume, la valeur part en base, et rien ne se passe.
///
/// Même défaut que #3192 (« mode exclusif » décoché sans effet sous ASIO) :
/// le défaut n'est pas la règle, c'est que le réglage MENT. D'où le même
/// vocabulaire — un `code()` stable pour la machine, un `detail()` en clair
/// pour un écran sans table de traduction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CrossfeedConstraint {
    /// La zone ne joue pas par une sortie LOCALE. Le crossfeed n'est appliqué
    /// que par `LocalOutput` (`apply_local_dsp` / `flush_local_dsp`) ; le
    /// chemin réseau passe par `transcode_source_to_file`, dont la signature
    /// ne porte que l'égaliseur, le convolveur et le ReplayGain — jamais de
    /// crossfeed. Couvre aussi la zone dont aucun périphérique n'est résolu :
    /// elle ne joue nulle part, donc pas davantage par une sortie locale.
    NonLocalOutput,
    /// Le mode PURE (audiophile) désarme volontairement le crossfeed pour
    /// garder le chemin bit-perfect (`load_crossfeed_processor` rend `None`).
    /// C'est un choix assumé, pas une panne — mais tant qu'il dure, le réglage
    /// est sans effet et l'écran doit le dire.
    PureMode,
}

impl CrossfeedConstraint {
    /// Code stable, celui que porte la charge utile JSON.
    pub fn code(self) -> &'static str {
        match self {
            Self::NonLocalOutput => "non_local_output",
            Self::PureMode => "pure_mode",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal — le serveur y écrit
    /// déjà ses `detail` en français.
    pub fn detail(self) -> &'static str {
        match self {
            Self::NonLocalOutput => {
                "Le crossfeed est un effet de casque : il n'est appliqué que par \
                 une sortie LOCALE (DAC USB ou carte son de la machine). Cette \
                 zone ne joue pas par une sortie locale, le réglage est \
                 enregistré mais n'atteint pas le son. Pour l'entendre, écoutez \
                 sur une zone à sortie locale."
            }
            Self::PureMode => {
                "Le mode PURE garde le chemin bit-perfect : aucun traitement ne \
                 touche le signal, crossfeed compris. Le réglage est conservé et \
                 reprendra effet dès que le mode PURE sera désactivé."
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : une contrainte
    /// ajoutée sans code ni libellé fait tomber le test qui parcourt cette liste.
    pub const ALL: [Self; 2] = [Self::NonLocalOutput, Self::PureMode];
}

/// Ce que le crossfeed VAUT réellement pour une zone, à côté de ce que le
/// réglage demande — et pourquoi, quand les deux diffèrent.
///
/// Additif : aucun champ ne remplace l'objet `crossfeed` de
/// `GET/PUT /zones/{id}/dsp`, qui reste publié tel quel. Un client qui ne lit
/// pas cette structure voit le même écran qu'avant.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CrossfeedStatus {
    /// Ce que l'utilisateur a demandé (la case `enabled` du réglage persisté).
    pub requested: bool,
    /// Ce qui sera réellement appliqué au son de cette zone.
    pub effective: bool,
    /// `true` dès que la contrainte s'applique — **y compris quand la case
    /// était déjà décochée**. C'est ce champ qui doit VERROUILLER le contrôle :
    /// la question n'est pas « le réglage a-t-il été changé ? » mais « ce
    /// réglage a-t-il encore un sens sur cette zone ? ».
    pub unavailable: bool,
    /// Pourquoi. `None` = le réglage est honoré tel quel.
    pub reason: Option<CrossfeedConstraint>,
    /// La même chose en clair, pour un écran qui n'a pas de table de traduction.
    pub detail: Option<&'static str>,
}

/// Le prédicat des trois sites d'installation, écrit UNE fois.
///
/// `orchestrator.rs` garde chacun d'eux par `device_id.starts_with("local:")`,
/// et `create_zone` le dit noir sur blanc : « une sortie locale s'identifie par
/// `local:{nom}` — c'est ce préfixe, et lui seul, qui dit à l'orchestrateur
/// "carte son" plutôt que "renderer réseau" ». Le statut publié doit donc
/// interroger EXACTEMENT ce préfixe, sinon l'écran et le son se répondraient
/// sur deux règles différentes.
///
/// `None` (zone sans périphérique résolu) rend `false` : elle ne joue nulle
/// part, donc pas davantage par une sortie locale.
pub fn crossfeed_runs_on_output(output_device_id: Option<&str>) -> bool {
    output_device_id.is_some_and(|d| d.starts_with("local:"))
}

/// La règle, isolée de toute base de données pour être vérifiable partout.
///
/// `output_is_local` et `audiophile` sont des PARAMÈTRES, pas des lectures : la
/// règle doit être éprouvable sans monter une zone ni une sortie, et sur une
/// cible compilée sans `local-audio` — où les trois sites d'installation
/// n'existent même pas, ce qui ne rend le réglage que plus muet. Même intention
/// que le `on_windows` d'`exclusive_mode_status` (#3192).
///
/// Ordre des motifs : une zone réseau ne verra JAMAIS de crossfeed, PURE ou
/// non ; c'est donc `NonLocalOutput` qui prime, parce que c'est celui qui ne
/// se lève pas en décochant une case.
pub fn crossfeed_status(
    requested: bool,
    output_is_local: bool,
    audiophile: bool,
) -> CrossfeedStatus {
    let reason = if !output_is_local {
        Some(CrossfeedConstraint::NonLocalOutput)
    } else if audiophile {
        Some(CrossfeedConstraint::PureMode)
    } else {
        None
    };
    let unavailable = reason.is_some();
    CrossfeedStatus {
        requested,
        effective: requested && !unavailable,
        unavailable,
        reason,
        detail: reason.map(CrossfeedConstraint::detail),
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

    // -----------------------------------------------------------------
    // #2742 — le réglage « crossfeed » ne doit plus MENTIR.
    //
    // Ces essais portent sur `crossfeed_status`, qui prend le type de sortie
    // et le mode PURE en PARAMÈTRES. C'est délibéré : les trois sites qui
    // installent un `CrossfeedProcessor` sont derrière
    // `#[cfg(feature = "local-audio")]`, et un essai entouré du même `cfg` ne
    // serait exécuté par aucune des cibles qui compilent sans — vert contre
    // rien, alors que c'est justement là que le réglage est le plus muet.
    // -----------------------------------------------------------------

    /// Le TÉMOIN de la règle : sortie locale, hors PURE, le réglage est honoré
    /// tel quel et **rien** ne vient s'ajouter à l'écran. Si ce test rougit,
    /// c'est qu'on a désarmé le cas nominal en corrigeant le cas réseau.
    #[test]
    fn une_sortie_locale_honore_le_crossfeed_sans_rien_annoncer() {
        let s = crossfeed_status(true, true, false);
        assert!(
            s.effective,
            "sortie locale hors PURE : le crossfeed s'applique"
        );
        assert!(
            !s.unavailable,
            "le contrôle doit rester ACTIF : c'est le cas nominal"
        );
        assert_eq!(s.reason, None);
        assert_eq!(s.detail, None);
        assert!(s.requested);

        // Décoché sur une sortie locale : rien à annoncer non plus.
        let eteint = crossfeed_status(false, true, false);
        assert!(!eteint.effective);
        assert!(!eteint.unavailable);
        assert_eq!(eteint.reason, None);
    }

    /// 1. Zone réseau + case COCHÉE : le réglage est sans effet, **et la raison
    ///    est donnée**. C'est tout le ticket : avant, le premier point était
    ///    vrai et le second manquait.
    #[test]
    fn une_sortie_reseau_dit_que_le_crossfeed_n_agit_pas() {
        let s = crossfeed_status(true, false, false);
        assert!(
            !s.effective,
            "aucun des trois sites d'installation n'est atteignable hors sortie locale"
        );
        assert!(
            s.unavailable,
            "et le contrôle doit être annoncé comme INDISPONIBLE, pas honoré"
        );
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalOutput));
        let detail = s
            .detail
            .expect("une contrainte sans explication, c'est le défaut de #2742");
        assert!(
            detail.contains("locale"),
            "l'explication doit dire à l'utilisateur ce qu'il PEUT faire \
             (écouter sur une zone locale), pas seulement ce qu'il subit : {detail}"
        );
        assert!(
            s.requested,
            "`requested` doit rester ce que l'utilisateur a demandé, sinon \
             l'écran ne peut pas dire que son choix est resté lettre morte"
        );
    }

    /// 2. Zone réseau + case DÉCOCHÉE : `unavailable` se lève quand même. La
    ///    question n'est pas « le réglage a-t-il été changé ? » mais « ce
    ///    réglage a-t-il encore un sens ici ? » — sinon le client ne verrouille
    ///    le contrôle qu'APRÈS que l'utilisateur a cliqué pour rien.
    #[test]
    fn une_sortie_reseau_verrouille_meme_case_decochee() {
        let s = crossfeed_status(false, false, false);
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalOutput));
    }

    /// 3. Le mode PURE désarme le crossfeed sur une sortie locale, et le dit.
    ///    `load_crossfeed_processor` rend déjà `None` en PURE ; ce qui manquait
    ///    était de l'annoncer.
    #[test]
    fn le_mode_pure_desarme_le_crossfeed_et_le_dit() {
        let s = crossfeed_status(true, true, true);
        assert!(!s.effective);
        assert!(s.unavailable);
        assert_eq!(s.reason, Some(CrossfeedConstraint::PureMode));
        assert!(
            s.detail.is_some_and(|d| d.contains("PURE")),
            "l'explication doit nommer le mode qui désarme"
        );
    }

    /// 4. Zone réseau ET en PURE : c'est `NonLocalOutput` qui prime. PURE se
    ///    lève en décochant une case ; la sortie réseau, non — annoncer PURE
    ///    laisserait croire qu'il suffit de le désactiver pour entendre le
    ///    crossfeed.
    #[test]
    fn la_sortie_reseau_prime_sur_le_mode_pure() {
        let s = crossfeed_status(true, false, true);
        assert_eq!(s.reason, Some(CrossfeedConstraint::NonLocalOutput));
    }

    /// Le prédicat publié doit être EXACTEMENT celui des trois sites
    /// d'installation : `starts_with("local:")`, et rien d'autre. Un `dlna:` ou
    /// un nom nu ne doit jamais passer pour une sortie locale — c'est
    /// précisément la confusion que `create_zone` documente.
    #[test]
    fn seul_le_prefixe_local_fait_courir_le_crossfeed() {
        assert!(crossfeed_runs_on_output(Some("local:Realtek")));
        assert!(crossfeed_runs_on_output(Some("local:")));
        assert!(!crossfeed_runs_on_output(Some("dlna:uuid-1234")));
        assert!(!crossfeed_runs_on_output(Some("chromecast:salon")));
        assert!(!crossfeed_runs_on_output(Some("Realtek")));
        assert!(!crossfeed_runs_on_output(Some("browser:1")));
        assert!(
            !crossfeed_runs_on_output(None),
            "une zone sans périphérique ne joue nulle part"
        );
    }

    /// Contre-épreuve permanente : toute contrainte doit porter un code STABLE,
    /// unique, et une explication non vide. Une variante ajoutée sans être
    /// décrite fait tomber ce test.
    #[test]
    fn chaque_contrainte_a_un_code_unique_et_une_explication() {
        let mut codes: Vec<&str> = Vec::new();
        for c in CrossfeedConstraint::ALL {
            assert!(!c.code().is_empty(), "code vide");
            assert!(
                !c.detail().trim().is_empty(),
                "contrainte sans explication : {}",
                c.code()
            );
            assert!(!codes.contains(&c.code()), "code dupliqué : {}", c.code());
            codes.push(c.code());
        }
        assert_eq!(codes.len(), CrossfeedConstraint::ALL.len());
    }

    /// Le code stable doit être celui que porte le JSON — pas une chaîne
    /// recopiée à côté. C'est ce que le client lira pour choisir sa traduction.
    #[test]
    fn le_code_serialise_est_le_code_stable() {
        let s = crossfeed_status(true, false, false);
        let v = serde_json::to_value(&s).expect("le statut doit être sérialisable");
        assert_eq!(
            v["reason"].as_str(),
            Some(CrossfeedConstraint::NonLocalOutput.code()),
            "le client lit ce code, il ne doit pas dériver du nom Rust"
        );
        assert_eq!(v["unavailable"].as_bool(), Some(true));
        assert_eq!(v["requested"].as_bool(), Some(true));
        assert_eq!(v["effective"].as_bool(), Some(false));
        assert!(v["detail"].as_str().is_some_and(|d| !d.is_empty()));

        // Le cas nominal ne publie AUCUN motif : `null`, pas une chaîne vide.
        let nominal = serde_json::to_value(crossfeed_status(true, true, false)).unwrap();
        assert!(nominal["reason"].is_null());
        assert!(nominal["detail"].is_null());
    }

    /// ⭐ GARDE DE SITE — la prémisse de toute la règle, relue dans le code de
    /// PRODUCTION.
    ///
    /// `crossfeed_status` affirme « hors sortie locale, le crossfeed n'a aucun
    /// chemin de code ». Cette affirmation n'est vraie que tant que le chemin
    /// réseau n'installe pas de crossfeed. Si quelqu'un porte un jour le
    /// crossfeed sur `transcode_source_to_file` sans revenir ici, le serveur se
    /// mettrait à mentir dans l'AUTRE sens : un écran qui annonce « sans effet »
    /// pendant que le DAC reçoit un signal traité.
    ///
    /// On relit donc `orchestrator.rs` — le fichier réel, par `include_str!`,
    /// l'idiome du dépôt — et on vérifie que tout appel qui INSTALLE un
    /// crossfeed est bien précédé, de peu, de la garde `local:`.
    #[test]
    fn aucun_site_d_installation_du_crossfeed_hors_de_la_garde_locale() {
        const ORCHESTRATEUR: &str = include_str!("../orchestrator.rs");
        // Témoin : si `include_str!` pointait sur un fichier vide ou faux, tout
        // le reste passerait pour vert sans rien avoir lu.
        assert!(
            ORCHESTRATEUR.contains("fn load_crossfeed_processor"),
            "include_str! ne lit pas l'orchestrateur attendu"
        );
        let lignes: Vec<&str> = ORCHESTRATEUR.lines().collect();

        // Début d'une fonction — la BORNE de la recherche en amont. Une garde
        // posée dans une AUTRE fonction ne garde rien ; sans cette borne, une
        // simple fenêtre de N lignes attraperait le `local:` du voisin et
        // resterait verte contre une garde supprimée.
        fn debut_de_fonction(l: &str) -> bool {
            let t = l.trim_start();
            [
                "fn ",
                "pub fn ",
                "async fn ",
                "pub async fn ",
                "pub(crate) fn ",
                "pub(crate) async fn ",
            ]
            .iter()
            .any(|p| t.starts_with(p))
        }

        // Remonter depuis `depuis` (exclu) jusqu'à `motif`, sans franchir le
        // début de la fonction courante. Rend la ligne trouvée, ou `None`.
        fn remonter(lignes: &[&str], depuis: usize, motif: &str) -> Option<usize> {
            for i in (0..depuis).rev() {
                if lignes[i].contains(motif) {
                    return Some(i);
                }
                if debut_de_fonction(lignes[i]) {
                    return None;
                }
            }
            None
        }

        let mut sites = 0usize;
        for (i, ligne) in lignes.iter().enumerate() {
            // Les APPELS, pas les définitions ni les commentaires.
            let nu = ligne.trim_start();
            let appel = (ligne.contains(".set_crossfeed(")
                || ligne.contains(".replace_crossfeed_live("))
                && !nu.starts_with("//");
            if !appel {
                continue;
            }
            sites += 1;
            let downcast = remonter(
                &lignes,
                i,
                "downcast_ref::<crate::outputs::local::LocalOutput>()",
            )
            .unwrap_or_else(|| {
                panic!(
                    "site d'installation du crossfeed sans `LocalOutput` dans sa \
                     propre fonction (orchestrator.rs, ligne {}) : {}",
                    i + 1,
                    ligne.trim()
                )
            });
            assert!(
                remonter(&lignes, downcast, "starts_with(\"local:\")").is_some(),
                "site d'installation du crossfeed sans garde `local:` dans sa \
                 propre fonction (orchestrator.rs, ligne {}) : {}\n\
                 Si le crossfeed atteint désormais une sortie NON locale, \
                 `crossfeed_status` ment et doit être corrigé AVEC ce site.",
                i + 1,
                ligne.trim()
            );
        }
        // Le compte est un plancher VÉRIFIÉ, pas une supposition : au 03/09/2026
        // il y a exactement trois sites — le chemin de lecture,
        // `refresh_zone_crossfeed` et `refresh_zone_pure_dsp`. Un quatrième est
        // le bienvenu ; zéro signifierait que ce test ne mesure plus rien.
        assert!(
            sites >= 3,
            "seulement {sites} site(s) d'installation trouvé(s) : le test ne \
             garde plus le chemin qu'il prétend garder"
        );

        // La prémisse INVERSE, et c'est la plus importante : le chemin réseau
        // ne doit pas se mettre à appliquer un crossfeed dans notre dos.
        // `transcode_source_to_file` est la seule porte du chemin transcodé
        // (DLNA, OpenHome, Chromecast, BluOS…) ; sa signature ne porte que
        // l'égaliseur, le convolveur et le ReplayGain. Le jour où l'on y ajoute
        // le crossfeed, `crossfeed_status` mentira dans l'AUTRE sens — un écran
        // qui annonce « sans effet » pendant que le DAC reçoit un signal
        // traité — et ce test doit l'exiger AVANT que ça n'arrive.
        let (_, apres) = ORCHESTRATEUR
            .split_once("async fn transcode_source_to_file(")
            .expect("la porte du chemin transcodé doit exister");
        let (signature, _) = apres
            .split_once(") -> ")
            .expect("signature de transcode_source_to_file illisible");
        assert!(
            !signature.contains("crossfeed"),
            "le chemin transcodé accepte désormais un crossfeed : \
             `CrossfeedConstraint::NonLocalOutput` n'est plus vrai et doit \
             être revu ici AVANT d'être publié à l'écran.\nsignature : {signature}"
        );
    }
}
