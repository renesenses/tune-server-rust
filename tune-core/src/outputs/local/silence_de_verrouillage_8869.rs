//! #8869 — le silence de verrouillage, inséré devant le flux d'une sortie
//! locale à chaque ouverture du périphérique (règle et réglage :
//! [`crate::audio::silence_de_verrouillage`]).
//!
//! **Où.** À la frontière producteur → puits, que tous les bras PCM et DoP
//! traversent ([`super::backend::Puits`]) : le puits du backend est enveloppé
//! une fois, et c'est au PREMIER bloc qu'il reçoit que le format réellement
//! envoyé au DAC est connu — cadence et canaux ouverts, et le mode, lu dans
//! le bloc lui-même (marqueurs DoP), à la même règle que le DAC. Les deux
//! bras DSD natif (ALSA `DSD_U32`, ASIO natif) n'ont pas de puits : ils
//! appellent [`pousser_dans_l_anneau_dsd`] avant leur premier octet.
//!
//! **Quoi.** Un silence que le DAC reconnaît dans le mode du flux qui suit :
//!
//! - PCM : des zéros ;
//! - DoP : des trames de repos (charge utile 0x69, marqueurs 0x05/0xFA qui
//!   alternent) dont la dernière précède la première trame réelle sans
//!   répéter son marqueur — le DAC entre en DSD dès la première trame et n'en
//!   sort plus ;
//! - DSD natif : le motif de repos 0x69, jamais des zéros (pleine échelle).
//!
//! **Position.** Les bras publient « alimenté moins ce qui attend dans
//! l'anneau » : le silence attend dans l'anneau sans compter comme alimenté,
//! la position reste à zéro (ou au point de reprise) pendant qu'il joue.

use super::*;
use crate::audio::dop_continu::{haut_f32, repos_f32};
use crate::audio::dsd_brut::{AnneauDsd, SILENCE_DSD_MSB_FIRST};
use crate::audio::dsd_to_dop::{MARQUEUR_IMPAIR, MARQUEUR_PAIR, marqueur_suivant, mot_de_repos};
use crate::audio::silence_de_verrouillage::{
    ModeDuFlux, SignatureDuFormat, SilenceDeVerrouillage, trames_pour_ms,
};
use crate::outputs::traits::PuitsNatif;

/// Trames écrites par appel au puits : le silence ne s'alloue jamais en
/// entier (2 s à 384 kHz font 1,5 million de mots).
const TRAMES_PAR_TRANCHE: usize = 4_096;

/// Le marqueur de la `k`-ième trame d'un silence DoP de `total` trames, pour
/// que la dernière ne répète pas `premier_reel`, le marqueur de la première
/// trame réelle.
fn marqueur_de_repos(k: usize, total: usize, premier_reel: u8) -> u8 {
    if (total - 1 - k) % 2 == 0 {
        marqueur_suivant(premier_reel)
    } else {
        premier_reel
    }
}

/// Le marqueur DoP de la première trame si les trames de tête de `mots`
/// (`f32` entrelacés) portent un DoP : marqueur identique sur tous les
/// canaux, alternance stricte sur `DOP_DETECT_FRAMES` trames, comme
/// [`is_dop_pcm`].
fn premier_marqueur_dop_f32(mots: &[f32], canaux: usize) -> Option<u8> {
    let canaux = canaux.max(1);
    if mots.len() < canaux * DOP_DETECT_FRAMES {
        return None;
    }
    let mut precedent: Option<u8> = None;
    for trame in mots.chunks_exact(canaux).take(DOP_DETECT_FRAMES) {
        let m = haut_f32(trame[0]);
        if (m != MARQUEUR_PAIR && m != MARQUEUR_IMPAIR)
            || trame.iter().any(|&s| haut_f32(s) != m)
            || precedent == Some(m)
        {
            return None;
        }
        precedent = Some(m);
    }
    Some(haut_f32(mots[0]))
}

/// [`premier_marqueur_dop_f32`] pour un bloc d'octets natifs.
fn premier_marqueur_dop_natif(bloc: &BlocPcm<'_>) -> Option<u8> {
    let spec = bloc.spec();
    let octets = bloc.octets_alignes();
    is_dop_pcm(octets, spec.profondeur().bits_declares(), spec.canaux()).then(|| octets[2])
}

fn journaliser(signature: SignatureDuFormat, ms: u32, trames: usize) {
    info!(
        cadence = signature.cadence,
        canaux = signature.canaux,
        mode = signature.mode.code(),
        silence_ms = ms,
        trames,
        "local_audio_silence_de_verrouillage"
    );
}

/// Le puits flottant précédé du silence de verrouillage.
pub(super) struct PuitsFlottantVerrouille<'a> {
    reel: Box<dyn PuitsDEchantillons + 'a>,
    sortie: FormatOuvert,
    /// `Some` jusqu'au premier bloc.
    verrouillage: Option<SilenceDeVerrouillage>,
    /// Plafond (trames) du silence quand le rendu ne démarre qu'après le
    /// pré-remplissage (CPAL partagé) : au-delà, l'anneau plein bloquerait
    /// le producteur avant que le rendu ne le vide.
    plafond_trames: Option<usize>,
}

impl<'a> PuitsFlottantVerrouille<'a> {
    pub(super) fn nouveau(
        reel: Box<dyn PuitsDEchantillons + 'a>,
        sortie: FormatOuvert,
        verrouillage: SilenceDeVerrouillage,
        plafond_trames: Option<usize>,
    ) -> Self {
        Self {
            reel,
            sortie,
            verrouillage: Some(verrouillage),
            plafond_trames,
        }
    }

    /// Écrit le silence que demande le premier bloc `mots`. `false` : le
    /// puits réel est mort.
    fn preceder(&mut self, verrouillage: &SilenceDeVerrouillage, mots: &[f32]) -> bool {
        let canaux = usize::from(self.sortie.canaux.max(1));
        let dop = premier_marqueur_dop_f32(mots, canaux);
        let signature = SignatureDuFormat {
            cadence: self.sortie.cadence,
            canaux: self.sortie.canaux,
            mode: if dop.is_some() {
                ModeDuFlux::DoP
            } else {
                ModeDuFlux::Pcm
            },
        };
        let ms = verrouillage.constater(signature);
        let mut total = trames_pour_ms(self.sortie.cadence, ms);
        if let Some(plafond) = self.plafond_trames
            && total > plafond
        {
            warn!(
                demande_trames = total,
                plafond_trames = plafond,
                "local_audio_silence_de_verrouillage_plafonne"
            );
            total = plafond;
        }
        if total == 0 {
            return true;
        }
        journaliser(signature, ms, total);
        let mut tampon = Vec::with_capacity(TRAMES_PAR_TRANCHE.min(total) * canaux);
        let mut k = 0;
        while k < total {
            let n = (total - k).min(TRAMES_PAR_TRANCHE);
            tampon.clear();
            for i in k..k + n {
                let v = dop.map_or(0.0, |m0| repos_f32(marqueur_de_repos(i, total, m0)));
                tampon.extend(std::iter::repeat_n(v, canaux));
            }
            if !self.reel.ecrire(&tampon) {
                return false;
            }
            k += n;
        }
        true
    }
}

impl PuitsDEchantillons for PuitsFlottantVerrouille<'_> {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        if mots.is_empty() {
            return self.reel.ecrire(mots);
        }
        if let Some(verrouillage) = self.verrouillage.take()
            && !self.preceder(&verrouillage, mots)
        {
            return false;
        }
        self.reel.ecrire(mots)
    }
}

/// Le puits natif (octets au format du périphérique) précédé du silence de
/// verrouillage.
pub(super) struct PuitsNatifVerrouille<'a> {
    reel: Box<dyn PuitsNatif + 'a>,
    verrouillage: Option<SilenceDeVerrouillage>,
}

impl<'a> PuitsNatifVerrouille<'a> {
    pub(super) fn nouveau(
        reel: Box<dyn PuitsNatif + 'a>,
        verrouillage: SilenceDeVerrouillage,
    ) -> Self {
        Self {
            reel,
            verrouillage: Some(verrouillage),
        }
    }

    fn preceder(&mut self, verrouillage: &SilenceDeVerrouillage, bloc: &BlocPcm<'_>) -> bool {
        let spec = bloc.spec();
        let dop = premier_marqueur_dop_natif(bloc);
        let signature = SignatureDuFormat {
            cadence: spec.cadence(),
            canaux: spec.canaux(),
            mode: if dop.is_some() {
                ModeDuFlux::DoP
            } else {
                ModeDuFlux::Pcm
            },
        };
        let ms = verrouillage.constater(signature);
        let total = trames_pour_ms(spec.cadence(), ms);
        if total == 0 {
            return true;
        }
        journaliser(signature, ms, total);
        let canaux = usize::from(spec.canaux());
        let octets_par_mot = spec.profondeur().octets();
        let mut tampon =
            Vec::with_capacity(TRAMES_PAR_TRANCHE.min(total) * spec.octets_par_trame());
        let mut k = 0;
        while k < total {
            let n = (total - k).min(TRAMES_PAR_TRANCHE);
            tampon.clear();
            for i in k..k + n {
                match dop {
                    // DoP : seulement en 24 bits (`is_dop_pcm`), petit-boutien.
                    Some(m0) => {
                        let mot = mot_de_repos(marqueur_de_repos(i, total, m0)).to_le_bytes();
                        for _ in 0..canaux {
                            tampon.extend_from_slice(&mot[..3]);
                        }
                    }
                    // PCM entier ou flottant : le zéro est fait d'octets nuls.
                    None => tampon.extend(std::iter::repeat_n(0u8, canaux * octets_par_mot)),
                }
            }
            if !self.reel.ecrire(spec.bloc(&tampon)) {
                return false;
            }
            k += n;
        }
        true
    }
}

impl PuitsNatif for PuitsNatifVerrouille<'_> {
    fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
        if bloc.trames() == 0 {
            return self.reel.ecrire(bloc);
        }
        if let Some(verrouillage) = self.verrouillage.take()
            && !self.preceder(&verrouillage, &bloc)
        {
            return false;
        }
        self.reel.ecrire(bloc)
    }
}

impl<'a> super::backend::Puits<'a> {
    /// #8869 — le puits du backend, précédé du silence de verrouillage.
    /// `sortie` : le format ouvert (seul le puits flottant s'en sert, le bloc
    /// natif porte le sien).
    pub(super) fn precede_du_silence(
        self,
        verrouillage: &SilenceDeVerrouillage,
        sortie: FormatOuvert,
    ) -> Self {
        match self {
            Self::Flottant(reel) => Self::Flottant(Box::new(PuitsFlottantVerrouille::nouveau(
                reel,
                sortie,
                verrouillage.clone(),
                None,
            ))),
            Self::Natif(reel) => Self::Natif(Box::new(PuitsNatifVerrouille::nouveau(
                reel,
                verrouillage.clone(),
            ))),
        }
    }
}

/// Le silence de verrouillage d'un bras DSD natif, poussé dans son anneau
/// AVANT le premier octet du flux : motif 0x69 (identique quels que soient
/// l'ordre des bits et l'empaquetage en mots), en trames entières de
/// `octets_par_trame`. `arret` est relu entre deux poussées. Rend le nombre
/// d'octets poussés.
pub(super) fn pousser_dans_l_anneau_dsd(
    verrouillage: &SilenceDeVerrouillage,
    anneau: &AnneauDsd,
    cadence_dsd: u32,
    canaux: u16,
    octets_par_trame: usize,
    arret: impl Fn() -> bool,
) -> usize {
    let signature = SignatureDuFormat {
        cadence: cadence_dsd,
        canaux,
        mode: ModeDuFlux::DsdNatif,
    };
    let ms = verrouillage.constater(signature);
    let octets_par_trame = octets_par_trame.max(1);
    let total = crate::audio::dsd_brut::octets_par_canal_pour_ms(cadence_dsd, u64::from(ms))
        as usize
        * usize::from(canaux);
    let total = total - total % octets_par_trame;
    if total == 0 {
        return 0;
    }
    journaliser(signature, ms, total / octets_par_trame);
    let tranche = vec![SILENCE_DSD_MSB_FIRST; (TRAMES_PAR_TRANCHE * octets_par_trame).min(total)];
    let mut pousse = 0;
    while pousse < total {
        if arret() {
            break;
        }
        let n = (total - pousse).min(tranche.len());
        let ecrit = anneau.pousser_trames(&tranche[..n], octets_par_trame);
        if ecrit == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
            continue;
        }
        pousse += ecrit;
    }
    pousse
}

#[cfg(test)]
#[path = "silence_de_verrouillage_8869_tests.rs"]
mod tests;
