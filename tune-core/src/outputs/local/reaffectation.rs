//! #6044 — le créneau de la réaffectation des canaux sur la sortie locale.
//!
//! L'orchestrateur y pose, par piste, la [`Matrice`] que la zone (ou l'album)
//! demande ; l'étage de conversion la lit à chaque bloc, à l'endroit où le
//! nombre de canaux change (`EtageDeConversion::convertir`,
//! `conformer_la_piste_decodee`). Rien n'y est posé en PURE, et rien n'y est
//! appliqué à un porteur DoP.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::audio::reaffectation_canaux::Matrice;

/// La matrice posée pour la piste, et ce que l'étage en a fait au dernier bloc.
pub(crate) struct CreneauReaffectation {
    matrice: std::sync::Mutex<Option<Arc<Matrice>>>,
    /// Vrai quand le dernier bloc converti est passé par la matrice.
    appliquee: AtomicBool,
}

/// Le créneau vide des étages montés hors d'une `LocalOutput` (témoins).
#[cfg(test)]
pub(crate) static SANS_REAFFECTATION: CreneauReaffectation = CreneauReaffectation::vide();

impl CreneauReaffectation {
    pub(crate) const fn vide() -> Self {
        Self {
            matrice: std::sync::Mutex::new(None),
            appliquee: AtomicBool::new(false),
        }
    }

    pub(crate) fn poser(&self, matrice: Option<Arc<Matrice>>) {
        if let Ok(mut m) = self.matrice.lock() {
            *m = matrice;
        }
    }

    /// La matrice posée, si elle va de `source` à `sortie` canaux et change
    /// quelque chose.
    pub(crate) fn pour(&self, source: u16, sortie: u16) -> Option<Arc<Matrice>> {
        self.matrice.lock().ok().and_then(|m| {
            m.as_ref()
                .filter(|m| m.entrees() == source && m.sorties() == sortie && !m.est_identite())
                .cloned()
        })
    }

    /// Une matrice est-elle posée (quelle que soit sa forme) ?
    pub(crate) fn posee(&self) -> bool {
        self.matrice.lock().is_ok_and(|m| m.is_some())
    }

    pub(crate) fn noter(&self, appliquee: bool) {
        self.appliquee.store(appliquee, Ordering::Relaxed);
    }

    /// Le dernier bloc converti est-il passé par la matrice ?
    pub(crate) fn appliquee(&self) -> bool {
        self.appliquee.load(Ordering::Relaxed)
    }
}

/// Adapter `samples` de `source` vers `sortie` canaux : par la matrice posée
/// quand elle correspond, sinon par l'adaptation par défaut.
pub(super) fn adapter_les_canaux(
    creneau: &CreneauReaffectation,
    samples: Vec<f32>,
    source: u16,
    sortie: u16,
    porteur_intouchable: bool,
) -> Vec<f32> {
    let matrice = (!porteur_intouchable)
        .then(|| creneau.pour(source, sortie))
        .flatten();
    creneau.noter(matrice.is_some());
    match matrice {
        Some(m) => m.appliquer_f32(&samples),
        None if source != sortie => super::adapt_channels(&samples, source, sortie),
        None => samples,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::reaffectation_canaux::prereglage;

    #[test]
    fn la_sortie_locale_remet_un_4_0_sur_les_bonnes_voies_d_un_5_1() {
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Matrice::du_reglage_arme(&prereglage("quad_to_5_1").unwrap()).map(Arc::new));
        let quad: Vec<f32> = (0..8)
            .flat_map(|t| [1.0, 2.0, 3.0, 4.0].map(|v| v + t as f32 * 10.0))
            .collect();
        let six = adapter_les_canaux(&creneau, quad.clone(), 4, 6, false);
        assert!(creneau.appliquee());
        for (t, trame) in six.as_chunks::<6>().0.iter().enumerate() {
            let base = t as f32 * 10.0;
            assert_eq!(
                *trame,
                [1.0 + base, 2.0 + base, 0.0, 0.0, 3.0 + base, 4.0 + base]
            );
        }
        // DoP ou PURE : la matrice n'est pas appliquée, l'adaptation d'avant reste.
        let brut = adapter_les_canaux(&creneau, quad.clone(), 4, 6, true);
        assert!(!creneau.appliquee());
        assert_eq!(&brut[..6], &[1.0, 2.0, 3.0, 4.0, 0.0, 0.0]);
        // Rien de posé et mêmes canaux : le tampon ressort tel quel.
        let vide = CreneauReaffectation::vide();
        assert_eq!(adapter_les_canaux(&vide, quad.clone(), 4, 4, false), quad);
    }

    /// L'étage de conversion du chemin partagé (`convertir`) : la matrice
    /// remplace l'adaptation par défaut, compte comme un traitement, et se
    /// retire devant un porteur DoP.
    #[test]
    fn l_etage_de_conversion_applique_la_matrice_au_lieu_de_l_adaptation_par_defaut() {
        use crate::outputs::traits::{AudioSpec, FormatOuvert, ProfondeurPcm};
        use std::sync::Mutex;
        use std::sync::atomic::AtomicU32;
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Matrice::du_reglage_arme(&prereglage("quad_to_5_1").unwrap()).map(Arc::new));
        let (eq, convolver, crossfeed) = (Mutex::new(None), Mutex::new(None), Mutex::new(None));
        let (pure, mono, dop) = (
            AtomicBool::new(false),
            AtomicBool::new(false),
            AtomicBool::new(false),
        );
        let (volume, user_volume, rg) = (
            AtomicU32::new(100),
            AtomicU32::new(100),
            AtomicU32::new(100),
        );
        let mut etage = super::super::EtageDeConversion {
            pcm: super::super::LocalPcmProcessor {
                eq: &eq,
                convolver: &convolver,
                crossfeed: &crossfeed,
                pure_bypass: &pure,
                mono_downmix: &mono,
                reaffectation: &creneau,
                dop_active: &dop,
                volume: &volume,
                user_volume: &user_volume,
                rg_factor: &rg,
            },
            en_attente: Vec::new(),
            resampler: None,
            resample_leftover: Vec::new(),
            pcm_kind: super::super::LocalPcmKind::for_bit_depth(24),
            spec: AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier24, 4).expect("spec"),
            sortie: FormatOuvert::new(48_000, 6),
            needs_resample: false,
        };
        let quad = vec![0.125f32, 0.25, 0.375, 0.5];
        assert_eq!(
            etage.convertir(quad.clone()),
            [0.125, 0.25, 0.0, 0.0, 0.375, 0.5],
            "BL/BR doivent sortir sur BL/BR du 5.1, pas sur FC/LFE"
        );
        assert!(creneau.appliquee());
        assert!(etage.dsp_actif(), "la matrice est un traitement du signal");
        dop.store(true, Ordering::Relaxed);
        assert_eq!(
            etage.convertir(quad),
            [0.125, 0.25, 0.375, 0.5, 0.0, 0.0],
            "un porteur DoP n'est jamais réaffecté"
        );
        assert!(!creneau.appliquee());
    }

    #[test]
    fn echange_gauche_droite_sans_changer_le_nombre_de_canaux() {
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Matrice::du_reglage_arme(&prereglage("swap_lr").unwrap()).map(Arc::new));
        let rendu = adapter_les_canaux(&creneau, vec![0.25, -0.5, 0.125, 0.75], 2, 2, false);
        assert_eq!(rendu, [-0.5, 0.25, 0.75, 0.125]);
    }
}
