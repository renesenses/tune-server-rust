//! La réserve de niveau du suréchantillonneur, COMPENSÉE dans le gain de
//! rendu (décision de Bertrand du 19/11/2026, étude de la spec de Rémi § B.1).
//!
//! L'étage du greffon atténue de sa réserve (−3 dB en pratique, davantage si
//! la borne L1 du noyau l'exige) pour que son limiteur n'ait presque jamais à
//! agir. Sans compensation, une écoute comparative mettait toujours le
//! suréchantillonnage 3 dB plus bas que la lecture sans lui, et le plus fort
//! gagne une comparaison.
//!
//! La compensation est rendue ICI, dans le facteur que les rappels de rendu
//! multiplient APRÈS l'étage, sur le modèle de la compensation de
//! l'égaliseur (#4685, #5227) : le facteur de rendu reçoit `1 / gain`, puis
//! [`super::effective_volume_units`] rabote le produit à l'unité. D'où les
//! trois régimes, que le chemin du signal dit :
//!
//! - volume logiciel à −3 dB ou moins : niveaux EXACTEMENT égaux (au
//!   millième de facteur près, < 0,005 dB) ;
//! - entre −3 dB et 0 dB : compensé en partie, le produit plafonne à 1 ;
//! - volume à 100 % (volume réglé au DAC ou au préampli) : rien n'est
//!   compensé, il reste la réserve entière.
//!
//! Aucun de ces régimes ne peut écrêter : la sortie de l'étage est déjà sous
//! la pleine échelle (limiteur), et le facteur rendu ne dépasse jamais 1.
//!
//! Le gain de l'étage est posé par le fil de lecture à l'ouverture (il n'est
//! connu qu'une fois la cadence négociée) et remis à l'unité au début de
//! chaque `play_url` : un chemin qui ne porte pas l'étage (natif S32, DoP,
//! bras exclusifs) ne doit jamais hériter de la compensation d'une autre
//! piste, sous peine de jouer 3 dB trop fort.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Le gain linéaire de l'étage est rangé en millionièmes.
const UNITE: u32 = 1_000_000;

/// Les entrées et les sorties du facteur de rendu, partagées entre la sortie
/// (qui recompose à chaque ReplayGain, égaliseur ou crossfeed) et le fil de
/// lecture (qui pose le gain de l'étage à l'ouverture).
#[derive(Clone)]
pub(super) struct ReserveDuSurechantillonneur {
    /// Le facteur de rendu tel que [`super::LocalOutput`] le compose, SANS la
    /// réserve : même empaquetage que `rg_factor` (#5227).
    facteur_sans_reserve: Arc<AtomicU32>,
    /// Le gain moyen du DSP SANS la réserve, en millièmes.
    dsp_sans_reserve: Arc<AtomicU32>,
    /// Le gain de l'étage ouvert, en millionièmes. [`UNITE`] : aucun étage.
    gain_de_l_etage: Arc<AtomicU32>,
    /// Ce que lisent les rappels de rendu et le crête-mètre.
    rg_factor: Arc<AtomicU32>,
    gain_moyen_dsp: Arc<AtomicU32>,
    user_volume: Arc<AtomicU32>,
    dop_active: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    device_name: Arc<str>,
    /// Deux écrivains (la sortie, le fil de lecture) : chacun pose son entrée
    /// PUIS recompose sous ce verrou, si bien que le dernier à recomposer lit
    /// les deux entrées les plus récentes. Il garde aussi le flux auquel
    /// l'étage appartient (voir [`FluxOuvert`]).
    verrou: Arc<Mutex<FluxOuvert>>,
}

/// Le flux que la sortie a ouvert en dernier : sa génération (`play_url`) et
/// les réglages du suréchantillonneur lus à son ouverture.
#[derive(Default)]
struct FluxOuvert {
    generation: u64,
    reglages: Option<crate::audio::surechantillonnage::UpsamplerSettings>,
}

impl ReserveDuSurechantillonneur {
    pub(super) fn nouvelle(
        rg_factor: Arc<AtomicU32>,
        gain_moyen_dsp: Arc<AtomicU32>,
        user_volume: Arc<AtomicU32>,
        dop_active: Arc<AtomicBool>,
        volume: Arc<AtomicU32>,
        device_name: &str,
    ) -> Self {
        Self {
            facteur_sans_reserve: Arc::new(AtomicU32::new(rg_factor.load(Ordering::SeqCst))),
            dsp_sans_reserve: Arc::new(AtomicU32::new(gain_moyen_dsp.load(Ordering::SeqCst))),
            gain_de_l_etage: Arc::new(AtomicU32::new(UNITE)),
            rg_factor,
            gain_moyen_dsp,
            user_volume,
            dop_active,
            volume,
            device_name: Arc::from(device_name),
            verrou: Arc::new(Mutex::new(FluxOuvert::default())),
        }
    }

    /// La sortie a recomposé ReplayGain, compensation de l'égaliseur et gain
    /// moyen du DSP : les ranger, puis republier.
    pub(super) fn poser_le_rendu(&self, facteur: u32, gain_moyen_dsp: u32) {
        let _tenu = self.tenir();
        self.facteur_sans_reserve.store(facteur, Ordering::SeqCst);
        self.dsp_sans_reserve
            .store(gain_moyen_dsp, Ordering::SeqCst);
        self.publier_sous_le_verrou();
    }

    fn tenir(&self) -> std::sync::MutexGuard<'_, FluxOuvert> {
        self.verrou.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `play_url` ouvre le flux `generation` : rien n'y porte (encore)
    /// l'étage. La compensation retombe à l'unité, et seul le fil de CE flux
    /// pourra la réarmer.
    pub(super) fn ouvrir_le_flux(&self, generation: u64) {
        let mut flux = self.tenir();
        *flux = FluxOuvert {
            generation,
            reglages: None,
        };
        self.gain_de_l_etage.store(UNITE, Ordering::SeqCst);
        self.publier_sous_le_verrou();
    }

    /// Le fil de lecture du flux `generation` a lu `reglages` à l'ouverture
    /// et ouvert (`Some(reserve_db)`, réserve ≤ 0) ou non (`None`) l'étage.
    ///
    /// Refusé (`false`, rien ne change) quand ce flux n'est plus le flux en
    /// cours : un fil détaché par `stop()` ou remplacé par un `play_url` plus
    /// récent qui ouvre son étage en retard ne doit pas réarmer la
    /// compensation du flux suivant (il jouerait jusqu'à 3 dB trop fort).
    pub(super) fn poser_l_ouverture(
        &self,
        generation: u64,
        generation_courante: &AtomicU64,
        reglages: Option<crate::audio::surechantillonnage::UpsamplerSettings>,
        reserve_db: Option<f64>,
    ) -> bool {
        let mut flux = self.tenir();
        if flux.generation != generation || generation_courante.load(Ordering::SeqCst) != generation
        {
            return false;
        }
        flux.reglages = reglages;
        self.gain_de_l_etage
            .store(gain_en_millioniemes(reserve_db), Ordering::SeqCst);
        self.publier_sous_le_verrou();
        true
    }

    /// Les réglages du suréchantillonneur lus à l'ouverture du flux qui joue
    /// (`None` : désactivé à l'ouverture, chemin sans greffon, rien ouvert).
    pub(super) fn reglages_du_flux(
        &self,
    ) -> Option<crate::audio::surechantillonnage::UpsamplerSettings> {
        self.tenir().reglages.clone()
    }

    /// Pose le gain de l'étage sans regarder le flux : témoins seulement.
    #[cfg(test)]
    pub(super) fn poser_l_etage(&self, reserve_db: Option<f64>) {
        let _tenu = self.tenir();
        self.gain_de_l_etage
            .store(gain_en_millioniemes(reserve_db), Ordering::SeqCst);
        self.publier_sous_le_verrou();
    }

    /// Le gain linéaire de l'étage ouvert, `1.0` sans étage.
    pub(super) fn gain(&self) -> f64 {
        f64::from(self.gain_de_l_etage.load(Ordering::SeqCst)) / f64::from(UNITE)
    }

    /// À appeler en tenant [`Self::tenir`].
    fn publier_sous_le_verrou(&self) {
        let gain = self.gain();
        let facteur = facteur_compense(self.facteur_sans_reserve.load(Ordering::SeqCst), gain);
        // Le crête-mètre (RMS, spectre) mesure AVANT l'étage : la réserve est
        // un gain de plus entre son point de mesure et le DAC.
        let dsp = (f64::from(self.dsp_sans_reserve.load(Ordering::SeqCst)) * gain)
            .round()
            .clamp(0.0, 64_000.0) as u32;
        self.gain_moyen_dsp.store(dsp, Ordering::SeqCst);
        if self.rg_factor.swap(facteur, Ordering::SeqCst) != facteur {
            let user = self.user_volume.load(Ordering::SeqCst);
            let dop = self.dop_active.load(Ordering::Relaxed);
            let v = super::effective_volume_units(user, facteur, dop);
            super::dire_le_clamp_a_l_unite(&self.device_name, user, facteur, dop, v);
            self.volume.store(v, Ordering::SeqCst);
        }
    }

    /// Ce qui reste d'écart de niveau, en dB (≤ 0), entre la lecture avec
    /// l'étage et la même lecture sans lui, au volume en cours. `None` sans
    /// étage. `0` : niveaux égaux ; la réserve entière : rien de compensé.
    pub(super) fn ecart_de_niveau_db(&self) -> Option<f64> {
        let gain = self.gain();
        if gain >= 1.0 {
            return None;
        }
        let user = self.user_volume.load(Ordering::SeqCst);
        let facteur = self.facteur_sans_reserve.load(Ordering::SeqCst);
        Some(ecart_de_niveau_db(user, facteur, gain))
    }
}

/// `reserve_db` (≤ 0) en gain linéaire, en millionièmes ; l'unité sans étage
/// ou pour une valeur absurde (positive, non finie).
fn gain_en_millioniemes(reserve_db: Option<f64>) -> u32 {
    match reserve_db {
        Some(db) if db.is_finite() && db < 0.0 => {
            ((10.0_f64.powf(db / 20.0) * f64::from(UNITE)).round() as u32).clamp(1, UNITE)
        }
        _ => UNITE,
    }
}

/// Le facteur de rendu empaqueté (#5227), sa part ReplayGain × compensation
/// multipliée par `1 / gain`. Le rabot à l'unité reste celui
/// d'[`super::effective_volume_units`].
// `!(x > 0.0)` n'est pas `x <= 0.0` : seul le premier écarte `NaN`.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub(super) fn facteur_compense(facteur: u32, gain: f64) -> u32 {
    if !(gain > 0.0 && gain < 1.0) {
        return facteur;
    }
    let (rg, porte) = super::decomposer_le_facteur_de_rendu(facteur);
    let rg = (f64::from(rg) / gain).round().clamp(0.0, 64_000.0) as u32;
    super::composer_le_facteur_de_rendu(rg, porte)
}

/// Voir [`ReserveDuSurechantillonneur::ecart_de_niveau_db`] : niveau rendu
/// avec l'étage (`gain × min(demande / gain, 1)`) contre le niveau sans lui
/// (`min(demande, 1)`), la part portée par l'égaliseur s'annulant des deux
/// côtés.
#[allow(clippy::neg_cmp_op_on_partial_ord)]
pub(super) fn ecart_de_niveau_db(user_units: u32, facteur: u32, gain: f64) -> f64 {
    let (rg, _) = super::decomposer_le_facteur_de_rendu(facteur);
    let demande =
        f64::from(user_units) / f64::from(super::UNITE_DU_GAIN_DE_RENDU) * (f64::from(rg) / 1000.0);
    if !(demande > 0.0) || !(gain > 0.0) {
        return 0.0;
    }
    let sans = demande.min(1.0);
    let avec = gain * (demande / gain).min(1.0);
    (20.0 * (avec / sans).log10()).min(0.0)
}

#[cfg(test)]
#[path = "reserve_du_surechantillonneur_tests.rs"]
mod tests;
