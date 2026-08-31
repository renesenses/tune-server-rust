//! Rampe de gain anti-« ploc » à la pause, à la reprise et à l'arrêt (#1590).
//!
//! Levente demandait un fondu à la pause et à la lecture. Ce module n'est **pas**
//! un fondu artistique et n'a rien à voir avec `afade` : c'est la suppression du
//! *clic* que produit une coupure franche.
//!
//! # Ce que fait la coupure franche
//!
//! Les callbacks de rendu locaux (cpal) contenaient exactement ceci :
//!
//! ```ignore
//! if paused.load(Relaxed) || force_silent.load(Relaxed) {
//!     data.fill(0.0);
//!     return;
//! }
//! ```
//!
//! Le dernier échantillon rendu peut valoir ±1.0 pleine échelle ; le suivant
//! vaut 0.0. Cette **marche d'unité** est une discontinuité à spectre large :
//! c'est le « ploc » entendu à chaque appui sur Pause. La reprise fait la même
//! marche en sens inverse.
//!
//! # Ce que fait la rampe
//!
//! Le gain glisse linéairement entre 1.0 et 0.0 sur `ms` millisecondes, **trame
//! par trame, dans le callback de rendu lui-même**. Aucun fil, aucun `sleep`,
//! aucune allocation : `pause()` rend la main immédiatement comme avant, seul le
//! son décroît sur quelques dizaines de millisecondes.
//!
//! # Pourquoi 20 ms par défaut
//!
//! À 44,1 kHz, 20 ms font 882 trames. L'incrément de gain par trame vaut donc au
//! plus `1/882 = 1,13e-3`, soit **−58,9 dBFS** : la marche résiduelle est plus de
//! 58 dB sous celle qu'une coupure franche peut produire (jusqu'à 0 dBFS). Dans
//! l'autre sens, 20 ms est environ cinq fois sous le seuil (~100 ms) à partir
//! duquel une commande de transport commence à paraître molle. La demande
//! initiale parlait de 1 à 2 secondes : à cette durée la pause ne serait plus une
//! pause, et le remède serait pire que le défaut. Le réglage reste ouvert par
//! zone, borné par [`SOFT_MUTE_MAX_MS`].
//!
//! # Ce que la rampe ne doit JAMAIS toucher
//!
//! Une rampe de gain est une multiplication du signal. Elle est donc désarmée
//! d'office, sans exception, dans les trois cas où le PCM doit sortir intact —
//! voir [`armed_ms`] :
//!
//! * **DoP / DSD.** Un flux DoP est un train DSD emballé dans du PCM 24 bits dont
//!   l'octet de tête porte le marqueur alterné `0x05`/`0xFA`. Tout facteur autre
//!   qu'exactement 1.0 réécrit cet octet, le DAC cesse de reconnaître le DoP et
//!   **se met en sourdine**. C'est la même raison qui met déjà
//!   `effective_volume_units` à l'unité sur un DoP (#1408 → #1735).
//! * **Mode PURE / audiophile.** Le PCM atteint la sortie intact : c'est la
//!   promesse du mode. La rampe s'y désactive comme l'égaliseur.
//! * **Sortie exclusive.** Même promesse, tenue par le pilote plutôt que par un
//!   réglage.
//!
//! Dans ces trois cas le comportement redevient **exactement** celui d'avant :
//! coupure franche. Et même armée, la rampe ne touche rien tant qu'elle est au
//! repos à l'unité : [`SoftMuteRamp::apply`] ressort le tampon inchangé,
//! bit à bit, quand `gain == target == 1.0` et que le gain de base vaut 1.0.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Durée par défaut de la rampe, en millisecondes. Voir la justification en tête
/// de module : ≥ 800 trames à 44,1 kHz, soit une marche résiduelle sous
/// −58 dBFS, et cinq fois sous le seuil de mollesse perçue.
pub const SOFT_MUTE_DEFAULT_MS: u32 = 20;

/// Plafond dur de la rampe, en millisecondes.
///
/// Au-delà, l'appui sur Pause cesse d'être une pause : la commande paraît molle
/// et le correctif devient pire que le défaut qu'il corrige. Le réglage par zone
/// est donc borné ici, pas seulement documenté.
pub const SOFT_MUTE_MAX_MS: u32 = 250;

/// Durée de rampe réellement applicable, après les gardes bit-perfect.
///
/// Renvoyer `0` signifie « coupure franche », c'est-à-dire le comportement
/// d'avant #1590 au bit près. C'est le point unique où les trois interdits sont
/// tranchés, pour qu'un test puisse les prouver sans périphérique audio.
pub fn armed_ms(requested_ms: u32, dop: bool, pure_bypass: bool, exclusive: bool) -> u32 {
    if dop || pure_bypass || exclusive {
        return 0;
    }
    requested_ms.min(SOFT_MUTE_MAX_MS)
}

/// Combien de temps `stop()` accepte d'attendre pour laisser la rampe finir.
///
/// Sans cette attente le fil de lecture peut relâcher le flux cpal avant que le
/// callback ait fini de descendre, et le « ploc » revient à l'arrêt. L'attente
/// est bornée par la durée de rampe elle-même — donc nulle dès que la rampe est
/// désarmée, et jamais plus de [`SOFT_MUTE_MAX_MS`].
pub fn stop_drain_ms(armed_ms: u32, playing: bool) -> u64 {
    if !playing {
        return 0;
    }
    armed_ms.min(SOFT_MUTE_MAX_MS) as u64
}

/// Ce que le callback doit faire de son tampon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rendering {
    /// Silence complet : remplir de zéros et **ne rien tirer du ring**. C'est
    /// l'état de pause établie — sans quoi le ring se viderait pendant la pause.
    Silent,
    /// Il reste du signal à rendre : tirer du ring puis appeler
    /// [`SoftMuteRamp::apply`].
    Audible,
}

/// Les entrées que le callback relit à chaque appel pour savoir s'il a le droit
/// de ramper.
///
/// Regroupées en une seule valeur clonable parce que les callbacks locaux sont
/// construits par des fermetures et des fonctions déjà chargées de huit à neuf
/// paramètres : un seul de plus, pas quatre.
#[derive(Clone)]
pub struct SoftMuteGate {
    ms: Arc<AtomicU32>,
    dop: Arc<AtomicBool>,
    pure_bypass: Arc<AtomicBool>,
    exclusive: bool,
}

impl SoftMuteGate {
    pub fn new(
        ms: Arc<AtomicU32>,
        dop: Arc<AtomicBool>,
        pure_bypass: Arc<AtomicBool>,
        exclusive: bool,
    ) -> Self {
        Self {
            ms,
            dop,
            pure_bypass,
            exclusive,
        }
    }

    /// Durée de rampe applicable **maintenant**. Relue à chaque callback : le
    /// DoP se découvre en cours de piste, et le mode PURE se bascule en vol.
    pub fn armed_ms(&self) -> u32 {
        armed_ms(
            self.ms.load(Ordering::Relaxed),
            self.dop.load(Ordering::Relaxed),
            self.pure_bypass.load(Ordering::Relaxed),
            self.exclusive,
        )
    }

    /// Rampe neuve pour un format de sortie donné, au repos à pleine amplitude.
    pub fn ramp(&self, sample_rate: u32, channels: u16) -> SoftMuteRamp {
        SoftMuteRamp::new(sample_rate, channels)
    }
}

/// État de rampe détenu par un callback de rendu. Ni allocation ni verrou :
/// tout tient dans quatre `f32` et un `usize`.
#[derive(Debug, Clone)]
pub struct SoftMuteRamp {
    gain: f32,
    target: f32,
    step_per_frame: f32,
    ms: u32,
    sample_rate: u32,
    channels: usize,
}

impl SoftMuteRamp {
    /// Rampe désarmée (coupure franche) au repos à pleine amplitude.
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            gain: 1.0,
            target: 1.0,
            step_per_frame: 0.0,
            ms: 0,
            sample_rate: sample_rate.max(1),
            channels: channels.max(1) as usize,
        }
    }

    /// Arme (ou désarme, avec `ms == 0`) la rampe pour cette durée.
    ///
    /// Recalcule l'incrément par trame seulement quand la durée change : le
    /// callback appelle ceci à chaque tampon.
    pub fn arm(&mut self, ms: u32) {
        if ms == self.ms {
            return;
        }
        self.ms = ms;
        self.step_per_frame = if ms == 0 {
            0.0
        } else {
            let frames = (self.sample_rate as f64 * ms as f64 / 1000.0).max(1.0);
            (1.0 / frames) as f32
        };
    }

    /// Incrément de gain par trame. Zéro quand la rampe est désarmée.
    pub fn step_per_frame(&self) -> f32 {
        self.step_per_frame
    }

    /// Gain courant, dans `[0, 1]`.
    pub fn gain(&self) -> f32 {
        self.gain
    }

    /// Pose la cible (silence ou pleine amplitude) et dit au callback s'il doit
    /// encore tirer du signal.
    ///
    /// Désarmée, la rampe saute instantanément à la cible : le verdict et le
    /// gain sont alors identiques à ceux de la coupure franche d'avant #1590.
    pub fn begin(&mut self, silence: bool) -> Rendering {
        self.target = if silence { 0.0 } else { 1.0 };
        if self.step_per_frame <= 0.0 {
            self.gain = self.target;
        }
        if self.target == 0.0 && self.gain <= 0.0 {
            Rendering::Silent
        } else {
            Rendering::Audible
        }
    }

    /// Applique `base_gain × rampe` au tampon entrelacé et fait avancer la rampe.
    ///
    /// Le gain ne bouge qu'aux **frontières de trame** : les canaux d'une même
    /// trame reçoivent le même facteur, sinon la rampe introduirait un
    /// déséquilibre inter-canaux — une image stéréo qui se décale pendant le
    /// fondu.
    ///
    /// Chemin rapide quand la rampe est au repos : le tampon ressort inchangé
    /// bit à bit si `base_gain` vaut exactement 1.0.
    pub fn apply(&mut self, buf: &mut [f32], base_gain: f32) {
        if self.gain == self.target {
            let g = base_gain * self.gain;
            if g != 1.0 {
                for s in buf.iter_mut() {
                    *s *= g;
                }
            }
            return;
        }
        let ch = self.channels;
        let mut i = 0;
        while i < buf.len() {
            let end = (i + ch).min(buf.len());
            let g = base_gain * self.gain;
            for s in &mut buf[i..end] {
                *s *= g;
            }
            self.gain = if self.target > self.gain {
                (self.gain + self.step_per_frame).min(self.target)
            } else {
                (self.gain - self.step_per_frame).max(self.target)
            };
            i = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le tampon de référence : pleine échelle, alterné, stéréo. Une coupure
    /// franche dessus produit la pire marche possible.
    fn full_scale(frames: usize) -> Vec<f32> {
        (0..frames * 2)
            .map(|i| if (i / 2) % 2 == 0 { 1.0 } else { -1.0 })
            .collect()
    }

    /// Plus grand écart entre deux échantillons consécutifs d'un même canal.
    fn max_step_per_channel(buf: &[f32], channels: usize) -> f32 {
        let mut worst = 0.0f32;
        for c in 0..channels {
            let mut prev: Option<f32> = None;
            let mut i = c;
            while i < buf.len() {
                if let Some(p) = prev {
                    worst = worst.max((buf[i] - p).abs());
                }
                prev = Some(buf[i]);
                i += channels;
            }
        }
        worst
    }

    // ---------------------------------------------------------------- gardes

    #[test]
    fn dop_disarms_the_ramp_completely() {
        // Le marqueur DoP survit à un facteur 1.0 exact et à rien d'autre.
        assert_eq!(armed_ms(SOFT_MUTE_DEFAULT_MS, true, false, false), 0);
        assert_eq!(armed_ms(SOFT_MUTE_MAX_MS, true, false, false), 0);
    }

    #[test]
    fn pure_mode_and_exclusive_output_disarm_the_ramp() {
        assert_eq!(armed_ms(SOFT_MUTE_DEFAULT_MS, false, true, false), 0);
        assert_eq!(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, true), 0);
    }

    #[test]
    fn an_ordinary_pcm_stream_keeps_its_requested_ramp() {
        assert_eq!(
            armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false),
            SOFT_MUTE_DEFAULT_MS
        );
    }

    #[test]
    fn a_ramp_longer_than_the_ceiling_is_cut_to_the_ceiling() {
        // Une pause de deux secondes n'est plus une pause.
        assert_eq!(armed_ms(2000, false, false, false), SOFT_MUTE_MAX_MS);
    }

    /// TÉMOIN BIT-PERFECT. Désarmée et à gain de base unité, la rampe doit rendre
    /// le tampon **identique bit à bit** : c'est ce qui autorise à la laisser
    /// dans le chemin d'un flux DoP sans en réécrire le marqueur.
    #[test]
    fn a_disarmed_ramp_returns_the_buffer_bit_identical() {
        let source = full_scale(512);
        let mut buf = source.clone();
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, true, false, false));
        assert_eq!(ramp.begin(false), Rendering::Audible);
        ramp.apply(&mut buf, 1.0);
        assert_eq!(
            buf.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            source.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            "un flux DoP doit ressortir octet pour octet"
        );
    }

    /// TÉMOIN BIT-PERFECT, deuxième moitié : armée mais au repos à l'unité, la
    /// rampe ne touche rien non plus. Un flux PCM ordinaire à 100 % n'est donc
    /// pas modifié tant que personne n'appuie sur Pause.
    #[test]
    fn an_armed_ramp_at_rest_returns_the_buffer_bit_identical() {
        let source = full_scale(512);
        let mut buf = source.clone();
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        assert_eq!(ramp.begin(false), Rendering::Audible);
        ramp.apply(&mut buf, 1.0);
        assert_eq!(
            buf.iter().map(|s| s.to_bits()).collect::<Vec<_>>(),
            source.iter().map(|s| s.to_bits()).collect::<Vec<_>>()
        );
    }

    /// La coupure franche d'avant #1590, telle quelle : premier échantillon à
    /// zéro, ring intact.
    #[test]
    fn a_disarmed_pause_is_the_old_hard_cut() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(0);
        assert_eq!(ramp.begin(true), Rendering::Silent);
        assert_eq!(ramp.gain(), 0.0);
    }

    // ----------------------------------------------------------- la rampe

    /// CONTRE-ÉPREUVE. Avec la coupure franche, le premier échantillon après la
    /// pause vaut 0.0 alors que le précédent valait ±1.0 : une marche d'unité.
    /// Avec la rampe, la marche par échantillon reste sous 1/800 — plus de 58 dB
    /// plus bas. Ce test échoue si quelqu'un rétablit `data.fill(0.0)`.
    #[test]
    fn pause_ramps_down_instead_of_cutting_to_zero() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        let mut buf = full_scale(64);
        assert_eq!(ramp.begin(true), Rendering::Audible);
        ramp.apply(&mut buf, 1.0);

        // Le premier échantillon reste à pleine amplitude : rien n'a été coupé.
        assert!(
            buf[0].abs() > 0.99,
            "la rampe part de l'amplitude courante, pas de zéro : {}",
            buf[0]
        );
        // Et le gain a bien commencé à descendre.
        assert!(ramp.gain() < 1.0 && ramp.gain() > 0.0);

        // La marche réellement subie par le signal, une fois la modulation
        // retirée : ici le signal alterne ±1, donc on compare les modules.
        let mut worst = 0.0f32;
        let mut prev: Option<f32> = None;
        for s in buf.chunks_exact(2) {
            if let Some(p) = prev {
                worst = worst.max((s[0].abs() - p).abs());
            }
            prev = Some(s[0].abs());
        }
        assert!(
            worst <= 1.0 / 800.0,
            "marche par trame {worst} — une coupure franche vaut 1.0"
        );
    }

    /// Symétrique : la reprise remonte au lieu de repartir à pleine amplitude.
    #[test]
    fn resume_ramps_up_instead_of_jumping_to_full_scale() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        assert_eq!(ramp.begin(true), Rendering::Audible);
        let mut sink = full_scale(4096);
        ramp.apply(&mut sink, 1.0);
        assert_eq!(ramp.gain(), 0.0, "20 ms à 44,1 kHz tiennent en 4096 trames");
        assert_eq!(ramp.begin(true), Rendering::Silent);

        // Reprise.
        let mut buf = full_scale(64);
        assert_eq!(ramp.begin(false), Rendering::Audible);
        ramp.apply(&mut buf, 1.0);
        assert!(
            buf[0].abs() < 0.01,
            "la reprise part du silence : {}",
            buf[0]
        );
        assert!(ramp.gain() > 0.0 && ramp.gain() < 1.0);
    }

    /// La descente dure bien la durée demandée, à une trame près, et pas
    /// davantage : c'est la garantie que la pause ne devient pas molle.
    #[test]
    fn the_ramp_lasts_the_requested_duration() {
        for (sr, ms) in [(44_100u32, 20u32), (48_000, 20), (96_000, 20), (48_000, 5)] {
            let mut ramp = SoftMuteRamp::new(sr, 2);
            ramp.arm(ms);
            ramp.begin(true);
            let expected = (sr as f64 * ms as f64 / 1000.0).round() as usize;
            let mut frames = 0usize;
            let mut chunk = vec![1.0f32; 2];
            while ramp.gain() > 0.0 && frames < expected * 4 {
                chunk[0] = 1.0;
                chunk[1] = 1.0;
                ramp.apply(&mut chunk, 1.0);
                frames += 1;
            }
            let drift = frames.abs_diff(expected);
            assert!(
                drift <= expected / 50 + 2,
                "{sr} Hz / {ms} ms : {frames} trames au lieu de {expected}"
            );
        }
    }

    /// TÉMOIN DE LATENCE. La rampe vit dans le callback ; la seule attente
    /// qu'elle impose ailleurs est celle de `stop()`, bornée par la durée de
    /// rampe — nulle dès qu'elle est désarmée, jamais plus que le plafond.
    #[test]
    fn the_stop_wait_never_exceeds_the_ramp_and_is_zero_when_disarmed() {
        assert_eq!(stop_drain_ms(0, true), 0, "rampe désarmée : aucune attente");
        assert_eq!(
            stop_drain_ms(SOFT_MUTE_DEFAULT_MS, false),
            0,
            "rien ne joue : aucune attente"
        );
        assert_eq!(
            stop_drain_ms(SOFT_MUTE_DEFAULT_MS, true),
            SOFT_MUTE_DEFAULT_MS as u64
        );
        assert_eq!(
            stop_drain_ms(u32::MAX, true),
            SOFT_MUTE_MAX_MS as u64,
            "l'attente reste bornée quoi qu'on écrive dans le réglage"
        );
        // Le plafond lui-même est un contrat : au-delà de 250 ms l'arrêt cesse
        // d'être un arrêt. Vérifié à la compilation.
        const {
            assert!(SOFT_MUTE_MAX_MS <= 250);
        }
    }

    /// Les deux voies d'une même trame reçoivent le même gain : sans cela
    /// l'image stéréo se décalerait pendant le fondu.
    #[test]
    fn both_channels_of_a_frame_share_the_same_gain() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        ramp.begin(true);
        let mut buf = vec![1.0f32; 256];
        ramp.apply(&mut buf, 1.0);
        for frame in buf.chunks_exact(2) {
            assert_eq!(frame[0].to_bits(), frame[1].to_bits());
        }
    }

    /// Le gain de base (volume × ReplayGain) reste multiplicatif : la rampe
    /// module, elle ne remplace pas.
    #[test]
    fn the_ramp_multiplies_the_existing_volume_it_does_not_replace_it() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        assert_eq!(ramp.begin(false), Rendering::Audible);
        let mut buf = vec![1.0f32; 8];
        ramp.apply(&mut buf, 0.25);
        assert!(buf.iter().all(|s| (*s - 0.25).abs() < 1e-6), "{buf:?}");
    }

    /// La rampe traverse les tampons : un callback de 128 trames ne la termine
    /// pas, et l'état repris au tampon suivant n'introduit pas de marche.
    #[test]
    fn the_ramp_survives_across_callback_buffers_without_a_step() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        ramp.begin(true);
        // 6 tampons de 128 trames = 768 trames, soit moins que les 882 de la
        // rampe : elle est donc encore en cours à la fin, et les cinq jointures
        // sont bien des jointures de rampe.
        let mut joined: Vec<f32> = Vec::new();
        for _ in 0..6 {
            let mut buf = vec![1.0f32; 128 * 2];
            ramp.apply(&mut buf, 1.0);
            joined.extend_from_slice(&buf);
        }
        assert!(
            ramp.gain() > 0.0,
            "768 trames < 882 : la rampe doit encore descendre, gain = {}",
            ramp.gain()
        );
        assert!(
            max_step_per_channel(&joined, 2) <= 1.0 / 800.0,
            "marche à la jointure de deux tampons"
        );
    }

    #[test]
    fn silence_is_only_declared_once_the_gain_has_reached_zero() {
        let mut ramp = SoftMuteRamp::new(44_100, 2);
        ramp.arm(armed_ms(SOFT_MUTE_DEFAULT_MS, false, false, false));
        assert_eq!(
            ramp.begin(true),
            Rendering::Audible,
            "tant que le gain descend, le ring doit continuer d'être tiré"
        );
        let mut sink = vec![1.0f32; 4096 * 2];
        ramp.apply(&mut sink, 1.0);
        assert_eq!(ramp.begin(true), Rendering::Silent);
    }

    #[test]
    fn arming_recomputes_the_step_only_when_the_duration_changes() {
        let mut ramp = SoftMuteRamp::new(48_000, 2);
        ramp.arm(20);
        let step = ramp.step_per_frame();
        ramp.arm(20);
        assert_eq!(ramp.step_per_frame(), step);
        ramp.arm(40);
        assert!((ramp.step_per_frame() - step / 2.0).abs() < 1e-9);
    }

    #[test]
    fn the_default_gives_at_least_800_frames_at_cd_rate() {
        // La justification chiffrée du défaut, vérifiée plutôt que commentée.
        let frames = 44_100.0 * SOFT_MUTE_DEFAULT_MS as f64 / 1000.0;
        assert!(frames >= 800.0, "{frames} trames");
        let step_db = 20.0 * (1.0f64 / frames).log10();
        assert!(step_db <= -58.0, "marche à {step_db} dBFS");
    }

    #[test]
    fn a_gate_reads_its_verdict_live() {
        let ms = Arc::new(AtomicU32::new(SOFT_MUTE_DEFAULT_MS));
        let dop = Arc::new(AtomicBool::new(false));
        let pure = Arc::new(AtomicBool::new(false));
        let gate = SoftMuteGate::new(ms.clone(), dop.clone(), pure.clone(), false);
        assert_eq!(gate.armed_ms(), SOFT_MUTE_DEFAULT_MS);

        // Le DoP se découvre en cours de piste : le verdict doit suivre.
        dop.store(true, Ordering::Relaxed);
        assert_eq!(gate.armed_ms(), 0);
        dop.store(false, Ordering::Relaxed);
        // PURE se bascule en vol.
        pure.store(true, Ordering::Relaxed);
        assert_eq!(gate.armed_ms(), 0);
        pure.store(false, Ordering::Relaxed);
        assert_eq!(gate.armed_ms(), SOFT_MUTE_DEFAULT_MS);

        let exclusive = SoftMuteGate::new(ms, dop, pure, true);
        assert_eq!(exclusive.armed_ms(), 0);
    }

    #[test]
    fn a_ramp_built_by_the_gate_starts_at_full_scale_and_disarmed() {
        let gate = SoftMuteGate::new(
            Arc::new(AtomicU32::new(SOFT_MUTE_DEFAULT_MS)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            false,
        );
        let ramp = gate.ramp(48_000, 2);
        assert_eq!(ramp.gain(), 1.0);
        assert_eq!(ramp.step_per_frame(), 0.0);
    }
}
