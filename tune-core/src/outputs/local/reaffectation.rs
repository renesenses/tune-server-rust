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
use crate::outputs::traits::AudioSpec;

/// La matrice posée pour la piste, et ce que l'étage en a fait au dernier bloc.
pub(crate) struct CreneauReaffectation {
    matrice: std::sync::Mutex<Option<Arc<Matrice>>>,
    /// Vrai quand le dernier bloc converti est passé par la matrice.
    appliquee: AtomicBool,
    /// #6230 — ce que la dernière ouverture a demandé et obtenu pour la
    /// matrice. `None` : le bras ne pilote pas ses voies (transports
    /// exclusifs), ou rien n'est ouvert.
    pilotage: std::sync::Mutex<Option<PilotageDesVoies>>,
}

/// #6230 — ce qu'une ouverture du chemin partagé a fait des voies que la
/// matrice demandait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct PilotageDesVoies {
    /// Les voies demandées au périphérique pour la matrice (`None` : aucune,
    /// l'ouverture a suivi la règle de la source, #3632).
    pub(crate) demandees: Option<u16>,
    /// Le périphérique les a-t-il ouvertes ?
    pub(crate) obtenues: bool,
}

impl PilotageDesVoies {
    /// Ce que l'ouverture a fait de `demandees`, vu le nombre de voies
    /// réellement ouvertes.
    pub(crate) fn constater(demandees: Option<u16>, ouvertes: u16) -> Self {
        Self {
            demandees,
            obtenues: demandees == Some(ouvertes),
        }
    }
}

/// #6230 — faut-il rouvrir la sortie pour que le nombre de voies suive la
/// matrice ? Fonction pure.
///
/// - la matrice veut `n` voies et la sortie en a ouvert un autre nombre : oui,
///   sauf si `n` vient déjà d'être demandé et refusé (rouvrir redonnerait le
///   même refus) ;
/// - aucune matrice ne pilote plus les voies, mais l'ouverture en cours avait
///   été pilotée : oui si les voies ouvertes ne sont pas celles de la source,
///   pour retrouver l'ouverture ordinaire ;
/// - sinon, non. Sans règle armée, rien n'est jamais rouvert.
pub(crate) fn rouvrir_pour_les_voies(
    voulues: Option<u16>,
    pilotage: PilotageDesVoies,
    ouvertes: u16,
    canaux_source: u16,
) -> bool {
    match voulues {
        Some(n) => n != ouvertes && pilotage.demandees != Some(n),
        None => pilotage.obtenues && canaux_source != ouvertes,
    }
}

/// #6230 (étape 2) — la règle, pure : des voies PILOTÉES par une grille ne
/// s'ouvrent en exclusif que si la sortie en déclare au moins autant (pour
/// WASAPI : le format du mixeur de l'endpoint ; pour ASIO : les sorties du
/// pilote ; pour CoreAudio : le format physique en place). Un nombre déclaré
/// illisible ou nul refuse : dans le doute, on rouvre au format de la source.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(crate) fn voies_pilotees_permises(voies: u32, declarees: Option<u32>) -> bool {
    voies > 0 && declarees.is_some_and(|d| d > 0 && voies <= d)
}

/// #6230 (étape 2) — l'ouverture d'un transport exclusif ASIO ou CoreAudio
/// quand une grille pilote les voies (6 → 2), sur le modèle du WASAPI
/// exclusif (#6898) :
///
/// - les voies de la grille d'abord, PLAFONNÉES à celles que la sortie
///   déclare : `ouvrir(voies, true)` ;
/// - sur refus, `refusees` le dit, puis UNE réouverture au format de la
///   source, l'ouverture d'avant : `ouvrir(canaux_source, false)` ;
/// - sans grille qui pilote (ou une grille qui garde le nombre de voies), un
///   seul appel, celui d'avant.
///
/// Rend la sortie et les voies pilotées obtenues (`None` : ouverte au format
/// de la source). Le refus de la réouverture est celui d'avant.
#[cfg_attr(
    not(any(all(target_os = "windows", feature = "asio"), target_os = "macos")),
    allow(dead_code)
)]
pub(crate) fn ouvrir_sur_les_voies_pilotees<T, E>(
    voies_de_la_grille: Option<u16>,
    canaux_source: u16,
    mut ouvrir: impl FnMut(u16, bool) -> Result<T, E>,
    refusees: impl FnOnce(u16, &E),
) -> Result<(T, Option<u16>), E> {
    if let Some(voies) = voies_de_la_grille.filter(|&voies| voies != canaux_source) {
        match ouvrir(voies, true) {
            Ok(sortie) => return Ok((sortie, Some(voies))),
            Err(erreur) => refusees(voies, &erreur),
        }
    }
    ouvrir(canaux_source, false).map(|sortie| (sortie, None))
}

/// Le créneau vide des étages montés hors d'une `LocalOutput` (témoins).
#[cfg(test)]
pub(crate) static SANS_REAFFECTATION: CreneauReaffectation = CreneauReaffectation::vide();

impl CreneauReaffectation {
    pub(crate) const fn vide() -> Self {
        Self {
            matrice: std::sync::Mutex::new(None),
            appliquee: AtomicBool::new(false),
            pilotage: std::sync::Mutex::new(None),
        }
    }

    /// #6230 — le nombre de voies que la matrice posée demande pour une source
    /// de `source` canaux : ses sorties, si elle part de ces canaux, change
    /// quelque chose et change le nombre de voies. `None` sinon — l'ouverture
    /// suit alors la règle ordinaire (#3632), exactement comme avant.
    pub(crate) fn voies_de_la_matrice(&self, source: u16) -> Option<u16> {
        self.matrice.lock().ok().and_then(|m| {
            m.as_ref()
                .filter(|m| m.entrees() == source && !m.est_identite() && m.sorties() != source)
                .map(|m| m.sorties())
        })
    }

    /// #6230 — les voies à demander au périphérique pour cette piste, d'après
    /// la matrice posée. Jamais en PURE, jamais pour un porteur DoP : la
    /// matrice ne s'y applique pas, et un DoP ouvert sur moins de voies que
    /// la source serait refusé (#3233). Un flux 24 bits dont l'amorce ne
    /// suffit pas à écarter le DoP n'est pas piloté non plus.
    pub(crate) fn voies_a_demander(
        &self,
        spec: AudioSpec,
        amorce: &[u8],
        pure: bool,
        dop: bool,
    ) -> Option<u16> {
        if pure || dop {
            return None;
        }
        let canaux = spec.canaux();
        let bits = spec.profondeur().bits_declares();
        if bits == 24 {
            let sonde = super::DOP_DETECT_FRAMES * usize::from(canaux) * 3;
            if amorce.len() < sonde || super::is_dop_pcm(amorce, bits, canaux) {
                return None;
            }
        }
        self.voies_de_la_matrice(canaux)
    }

    /// #6230 — les voies qu'un transport EXCLUSIF (WASAPI, ASIO, CoreAudio
    /// « hog ») doit ouvrir pour cette piste : celles de la matrice quand elle
    /// pilote ([`Self::voies_a_demander`], mêmes exclusions : PURE, DoP,
    /// amorce 24 bits insuffisante), sinon les canaux de la source — la règle
    /// « format ouvert = format source » d'avant. Sert à l'ouverture et à la
    /// frontière gapless, pour qu'elles décident pareil.
    pub(crate) fn voies_a_ouvrir_en_exclusif(
        &self,
        spec: AudioSpec,
        amorce: &[u8],
        pure: bool,
        dop: bool,
    ) -> u16 {
        self.voies_a_demander(spec, amorce, pure, dop)
            .unwrap_or(spec.canaux())
    }

    /// #6230 — noter ce que l'ouverture a fait des voies (`None` : bras qui
    /// ne les pilote pas, ou plus rien d'ouvert).
    pub(crate) fn noter_l_ouverture(&self, pilotage: Option<PilotageDesVoies>) {
        if let Ok(mut p) = self.pilotage.lock() {
            *p = pilotage;
        }
    }

    /// #6230 — ce qu'une ouverture du chemin partagé a fait des voies que la
    /// matrice demandait : noté pour les décisions de réouverture, et dit au
    /// journal quand une demande a été faite.
    pub(crate) fn constater_l_ouverture(
        &self,
        demandees: Option<u16>,
        ouvertes: u16,
        device_name: &str,
    ) {
        let pilotage = PilotageDesVoies::constater(demandees, ouvertes);
        if let Some(demandees) = demandees {
            if pilotage.obtenues {
                tracing::info!(
                    device = %device_name,
                    voies = demandees,
                    "local_reaffectation_voies_ouvertes"
                );
            } else {
                tracing::warn!(
                    device = %device_name,
                    demandees,
                    ouvertes,
                    "local_reaffectation_voies_refusees"
                );
            }
        }
        self.noter_l_ouverture(Some(pilotage));
    }

    /// #6230 — ce que la dernière ouverture a fait des voies.
    pub(crate) fn pilotage(&self) -> Option<PilotageDesVoies> {
        self.pilotage.lock().ok().and_then(|p| *p)
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
        // DoP ou PURE : la matrice n'est pas appliquée, l'adaptation par défaut
        // reste (celle de #6057, qui route par position).
        let brut = adapter_les_canaux(&creneau, quad.clone(), 4, 6, true);
        assert!(!creneau.appliquee());
        assert_eq!(brut, super::super::adapt_channels(&quad, 4, 6));
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
                disposition: &crate::outputs::local::SANS_DISPOSITION,
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
            etage.convertir(quad.clone()),
            super::super::adapt_channels(&quad, 4, 6),
            "un porteur DoP n'est jamais réaffecté : l'adaptation par défaut reste"
        );
        assert!(!creneau.appliquee());
    }

    /// #4969 — la sonde que lisent les niveaux décrit ce que l'étage fait :
    /// la matrice posée, sauf en PURE et en DoP ; rien hors lecture.
    #[test]
    fn la_sonde_des_canaux_suit_la_matrice_posee_sur_la_sortie() {
        use crate::outputs::traits::{
            AudioSpec, FormatOuvert, ProfondeurPcm, TransformationsReelles,
        };
        let sortie = super::super::LocalOutput::new_with_exclusive("témoin".into(), false);
        let sonde = sortie.sonde_des_canaux();
        assert!(sonde().is_none(), "hors lecture, aucune carte");
        if let Ok(mut t) = sortie.transformations_reelles.lock() {
            *t = Some(TransformationsReelles::nouvelles(
                AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier24, 2).expect("spec"),
                FormatOuvert::new(48_000, 2),
                false,
            ));
        }
        let carte = sonde().expect("carte en lecture");
        assert!(carte.est_identite());
        sortie.set_reaffectation(
            Matrice::du_reglage_arme(&prereglage("swap_lr").unwrap()).map(Arc::new),
        );
        let carte = sonde().expect("carte");
        assert_eq!(carte.coefficient(0, 1), 1.0, "FL reçoit FR");
        assert_eq!(carte.coefficient(1, 0), 1.0, "FR reçoit FL");
        sortie.pure_bypass.store(true, Ordering::Relaxed);
        assert!(
            sonde().expect("carte").est_identite(),
            "PURE : pas de matrice"
        );
        sortie.pure_bypass.store(false, Ordering::Relaxed);
        sortie.dop_active.store(true, Ordering::Relaxed);
        assert!(
            sonde().expect("carte").est_identite(),
            "DoP : pas de matrice"
        );
    }

    /// La grille de Didier (fil 2202) : 5.1 → 2, FL et FR à 0 dB, le reste
    /// muet.
    fn grille_6_vers_2() -> Arc<Matrice> {
        use crate::audio::reaffectation_canaux::ChannelRemapSettings;
        let mut gains_db = vec![vec![None; 6]; 2];
        gains_db[0][0] = Some(0.0);
        gains_db[1][1] = Some(0.0);
        Arc::new(
            Matrice::du_reglage_arme(&ChannelRemapSettings {
                enabled: true,
                inputs: 6,
                outputs: 2,
                gains_db,
                normalize: false,
                preset: None,
            })
            .expect("grille 6 → 2 armée"),
        )
    }

    fn spec(canaux: u16, profondeur: crate::outputs::traits::ProfondeurPcm) -> AudioSpec {
        AudioSpec::nouvelle(48_000, profondeur, canaux).expect("spec")
    }

    fn cfg(canaux: u16) -> cpal::StreamConfig {
        cpal::StreamConfig {
            channels: canaux,
            sample_rate: 48_000,
            buffer_size: cpal::BufferSize::Default,
        }
    }

    /// #6230 — LE témoin de la décision du 20/10 : une grille 6 → 2 armée sur
    /// une source 6 canaux fait demander 2 voies au périphérique (avant la
    /// cascade ordinaire), et l'étage, ouvert sur ces 2 voies, applique la
    /// matrice. Avant, la sortie ouvrait les 6 voies de la source et la
    /// règle était ignorée.
    #[test]
    fn grille_6_vers_2_sur_un_5_1_ouvre_2_voies_et_applique_la_matrice_6230() {
        use crate::outputs::traits::{FormatOuvert, ProfondeurPcm};
        use std::sync::Mutex;
        use std::sync::atomic::AtomicU32;
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Some(grille_6_vers_2()));
        let source = spec(6, ProfondeurPcm::Entier16);
        let amorce = vec![0u8; 4096];
        let voies = creneau.voies_a_demander(source, &amorce, false, false);
        assert_eq!(voies, Some(2), "la grille 6 → 2 demande 2 voies");

        // L'ouverture : un périphérique FACTICE qui accepte tout et note ce
        // qu'on lui demande. La première demande est celle de la matrice.
        let matrice_cfg = voies.map(cfg);
        let tentatives = super::super::cascade_de_formats_avec_la_matrice(
            matrice_cfg.as_ref(),
            &cfg(6),
            &cfg(6),
        );
        let mut demandes = Vec::new();
        let (_, ouvert, _) = super::super::ouvrir_premier_format_accepte(&tentatives, |c, _| {
            demandes.push(c.channels);
            Ok::<(), ()>(())
        })
        .expect("le périphérique factice accepte");
        assert_eq!(demandes, [2], "la sortie demande 2 voies, pas 6");
        assert_eq!(ouvert.channels, 2);
        creneau.constater_l_ouverture(voies, ouvert.channels, "témoin");
        assert_eq!(
            creneau.pilotage(),
            Some(PilotageDesVoies {
                demandees: Some(2),
                obtenues: true
            })
        );

        // L'étage, au format réellement ouvert : la matrice s'applique.
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
                disposition: &crate::outputs::local::SANS_DISPOSITION,
                dop_active: &dop,
                volume: &volume,
                user_volume: &user_volume,
                rg_factor: &rg,
            },
            en_attente: Vec::new(),
            resampler: None,
            resample_leftover: Vec::new(),
            pcm_kind: super::super::LocalPcmKind::for_bit_depth(16),
            spec: source,
            sortie: FormatOuvert::new(48_000, ouvert.channels),
            needs_resample: false,
        };
        let cinq_un = vec![0.125f32, 0.25, 0.375, 0.5, 0.625, 0.75];
        assert_eq!(
            etage.convertir(cinq_un),
            [0.125, 0.25],
            "FL et FR seuls, sans le repli ITU du centre ni des surrounds"
        );
        assert!(creneau.appliquee(), "la matrice est appliquée");
        assert!(etage.dsp_actif(), "et annoncée comme traitement");
    }

    /// #6230 — sans règle armée, ni une matrice qui garde le nombre de voies,
    /// rien ne change : aucune voie n'est demandée, la cascade est celle
    /// d'avant, et rien n'est jamais rouvert (le gapless stéréo ordinaire
    /// reste intact).
    #[test]
    fn sans_regle_le_nombre_de_voies_est_inchange_6230() {
        use crate::outputs::traits::ProfondeurPcm;
        let amorce = vec![0u8; 4096];
        let vide = CreneauReaffectation::vide();
        for canaux in [2, 6] {
            let source = spec(canaux, ProfondeurPcm::Entier24);
            assert_eq!(vide.voies_a_demander(source, &amorce, false, false), None);
        }
        let echange = CreneauReaffectation::vide();
        echange.poser(Matrice::du_reglage_arme(&prereglage("swap_lr").unwrap()).map(Arc::new));
        assert_eq!(
            echange.voies_a_demander(spec(2, ProfondeurPcm::Entier16), &amorce, false, false),
            None,
            "2 → 2 : la matrice ne change pas le nombre de voies"
        );
        let avant = super::super::cascade_de_formats(&cfg(8), &cfg(6));
        let apres = super::super::cascade_de_formats_avec_la_matrice(None, &cfg(8), &cfg(6));
        let canaux = |t: &[(cpal::StreamConfig, super::super::FormatDeSortie)]| {
            t.iter()
                .map(|(c, f)| (c.channels, f.nom()))
                .collect::<Vec<_>>()
        };
        assert_eq!(canaux(&avant), canaux(&apres));
        assert_eq!(
            apres[0].0.channels, 8,
            "le format principal d'abord, comme avant"
        );
        vide.constater_l_ouverture(None, 2, "témoin");
        let pilotage = vide.pilotage().expect("ouverture notée");
        for (ouvertes, source) in [(2, 2), (8, 2), (6, 6), (2, 6)] {
            assert!(!rouvrir_pour_les_voies(None, pilotage, ouvertes, source));
        }
    }

    /// #6230 — ce qui ne pilote jamais les voies : PURE, un porteur DoP (ou
    /// un 24 bits trop court pour l'écarter), une grille qui ne part pas des
    /// canaux de la source.
    #[test]
    fn la_matrice_ne_pilote_ni_pure_ni_dop_ni_une_autre_source_6230() {
        use crate::outputs::traits::ProfondeurPcm;
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Some(grille_6_vers_2()));
        let pcm = vec![0u8; 4096];
        let s24 = spec(6, ProfondeurPcm::Entier24);
        assert_eq!(creneau.voies_a_demander(s24, &pcm, false, false), Some(2));
        assert_eq!(
            creneau.voies_a_demander(s24, &pcm, true, false),
            None,
            "PURE"
        );
        assert_eq!(
            creneau.voies_a_demander(s24, &pcm, false, true),
            None,
            "DoP verrouillé"
        );
        assert_eq!(
            creneau.voies_a_demander(s24, &pcm[..10], false, false),
            None,
            "24 bits sans assez d'amorce pour écarter le DoP"
        );
        let dop: Vec<u8> = (0..64)
            .flat_map(|t| {
                let marqueur = if t % 2 == 0 { 0x05 } else { 0xFA };
                (0..6).flat_map(move |_| [0x69, 0x69, marqueur])
            })
            .collect();
        assert!(super::super::is_dop_pcm(&dop, 24, 6));
        assert_eq!(
            creneau.voies_a_demander(s24, &dop, false, false),
            None,
            "porteur DoP"
        );
        assert_eq!(
            creneau.voies_a_demander(spec(8, ProfondeurPcm::Entier16), &pcm, false, false),
            None,
            "grille 6 → 2 sur un 7.1"
        );
    }

    /// #6230 — quand rouvrir : la règle change le nombre de voies voulu, ou
    /// une ouverture pilotée n'est plus voulue ; jamais pour redemander ce que
    /// le périphérique vient de refuser.
    #[test]
    fn la_reouverture_suit_le_nombre_de_voies_voulu_6230() {
        let ordinaire = PilotageDesVoies::constater(None, 6);
        let pilotee = PilotageDesVoies::constater(Some(2), 2);
        let refusee = PilotageDesVoies::constater(Some(2), 6);
        assert!(!refusee.obtenues);
        // Règle 6 → 2 posée sur une lecture ouverte en 6 voies : rouvrir.
        assert!(rouvrir_pour_les_voies(Some(2), ordinaire, 6, 6));
        // Déjà sur 2 voies : rien.
        assert!(!rouvrir_pour_les_voies(Some(2), pilotee, 2, 6));
        // 2 voies déjà refusées : ne pas boucler.
        assert!(!rouvrir_pour_les_voies(Some(2), refusee, 6, 6));
        // Règle retirée (ou PURE) après une ouverture pilotée : retrouver les
        // voies de la source…
        assert!(rouvrir_pour_les_voies(None, pilotee, 2, 6));
        // … sauf si la source tient déjà sur les voies ouvertes.
        assert!(!rouvrir_pour_les_voies(None, pilotee, 2, 2));
    }

    /// #6230 — la sortie elle-même : une règle 6 → 2 posée en pleine lecture
    /// d'un 5.1 ouvert en 6 voies impose de rouvrir ; sur un bras qui ne
    /// pilote pas ses voies (exclusif), jamais.
    #[test]
    fn la_sortie_dit_quand_la_regle_impose_de_rouvrir_6230() {
        use crate::outputs::traits::{
            AudioSpec, FormatOuvert, ProfondeurPcm, TransformationsReelles,
        };
        let sortie = super::super::LocalOutput::new_with_exclusive("témoin".into(), false);
        let ouvrir = |canaux: u16| {
            if let Ok(mut t) = sortie.transformations_reelles.lock() {
                *t = Some(TransformationsReelles::nouvelles(
                    AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier24, 6).expect("spec"),
                    FormatOuvert::new(48_000, canaux),
                    false,
                ));
            }
        };
        ouvrir(6);
        sortie.set_reaffectation(Some(grille_6_vers_2()));
        assert!(
            !sortie.les_voies_imposent_de_rouvrir(),
            "bras sans pilotage (rien de noté) : jamais"
        );
        sortie
            .reaffectation
            .constater_l_ouverture(None, 6, "témoin");
        assert!(
            sortie.les_voies_imposent_de_rouvrir(),
            "6 voies ouvertes, la grille en veut 2"
        );
        sortie.pure_bypass.store(true, Ordering::Relaxed);
        assert!(
            !sortie.les_voies_imposent_de_rouvrir(),
            "PURE : la grille ne compte pas"
        );
        sortie.pure_bypass.store(false, Ordering::Relaxed);
        ouvrir(2);
        sortie
            .reaffectation
            .constater_l_ouverture(Some(2), 2, "témoin");
        assert!(!sortie.les_voies_imposent_de_rouvrir(), "déjà sur 2 voies");
        sortie.set_reaffectation(None);
        assert!(
            sortie.les_voies_imposent_de_rouvrir(),
            "règle retirée : retrouver les 6 voies"
        );
    }

    #[test]
    fn echange_gauche_droite_sans_changer_le_nombre_de_canaux() {
        let creneau = CreneauReaffectation::vide();
        creneau.poser(Matrice::du_reglage_arme(&prereglage("swap_lr").unwrap()).map(Arc::new));
        let rendu = adapter_les_canaux(&creneau, vec![0.25, -0.5, 0.125, 0.75], 2, 2, false);
        assert_eq!(rendu, [-0.5, 0.25, 0.75, 0.125]);
    }
}

/// #6230 (étape 2) — jamais plus de voies que la sortie n'en déclare.
#[cfg(test)]
mod voies_pilotees_6230 {
    use super::voies_pilotees_permises;

    #[test]
    fn des_voies_pilotees_ne_depassent_jamais_celles_que_la_sortie_declare_6230() {
        // Grille 6 → 2 sur une sortie stéréo, ou sur un 7.1 : permis.
        assert!(voies_pilotees_permises(2, Some(2)));
        assert!(voies_pilotees_permises(2, Some(8)));
        // Grille 2 → 6 sur une sortie stéréo : refusé, quoi qu'accepte le
        // pilote en exclusif.
        assert!(
            !voies_pilotees_permises(6, Some(2)),
            "#6230 : 6 voies pilotées sur une sortie qui en déclare 2"
        );
        // Illisible ou absurde : refusé, on rouvre au format source.
        assert!(!voies_pilotees_permises(2, None));
        assert!(!voies_pilotees_permises(2, Some(0)));
        assert!(!voies_pilotees_permises(0, Some(2)));
    }

    /// Ce que rend [`ouvertures`] : les appels `(voies, plafonnée)` dans
    /// l'ordre, les refus annoncés, et le résultat.
    type Ouvertures = (
        Vec<(u16, bool)>,
        Vec<(u16, String)>,
        Result<(u16, Option<u16>), String>,
    );

    /// #6230 (étape 2, ASIO et CoreAudio) — les ouvertures que fait
    /// `ouvrir_sur_les_voies_pilotees` pour une sortie qui accepte
    /// `acceptees` voies.
    fn ouvertures(grille: Option<u16>, source: u16, acceptees: &[u16]) -> Ouvertures {
        let mut appels = Vec::new();
        let mut refus = Vec::new();
        let rendu = super::ouvrir_sur_les_voies_pilotees(
            grille,
            source,
            |voies, plafonnee| {
                appels.push((voies, plafonnee));
                if acceptees.contains(&voies) {
                    Ok(voies)
                } else {
                    Err(format!("{voies} voie(s) refusée(s)"))
                }
            },
            |voies, erreur: &String| refus.push((voies, erreur.clone())),
        );
        (appels, refus, rendu)
    }

    #[test]
    fn asio_et_coreaudio_ouvrent_d_abord_les_voies_de_la_grille_plafonnees_6230() {
        // Grille 6 → 2 sur un appareil qui accepte 2 voies : une seule
        // ouverture, sur 2 voies, plafonnée aux voies déclarées.
        let (appels, refus, rendu) = ouvertures(Some(2), 6, &[2, 6]);
        assert_eq!(
            appels,
            [(2, true)],
            "#6230 : la grille 6 → 2 armée doit ouvrir l'exclusif sur 2 voies, \
             pas au format de la source"
        );
        assert!(refus.is_empty());
        assert_eq!(rendu, Ok((2, Some(2))));
    }

    #[test]
    fn un_refus_des_voies_de_la_grille_se_dit_puis_rouvre_au_format_source_6230() {
        // L'appareil refuse 2 voies (ou en déclare moins) : le refus est
        // annoncé, puis UNE réouverture au format de la source, non plafonnée.
        let (appels, refus, rendu) = ouvertures(Some(2), 6, &[6]);
        assert_eq!(appels, [(2, true), (6, false)]);
        assert_eq!(refus, [(2, "2 voie(s) refusée(s)".to_string())]);
        assert_eq!(rendu, Ok((6, None)));
        // Les deux refusées : c'est le refus de l'ouverture d'avant qui part.
        let (appels, _, rendu) = ouvertures(Some(2), 6, &[]);
        assert_eq!(appels, [(2, true), (6, false)]);
        assert_eq!(rendu, Err("6 voie(s) refusée(s)".to_string()));
    }

    #[test]
    fn sans_grille_qui_pilote_l_ouverture_est_celle_d_avant_6230() {
        for grille in [None, Some(6)] {
            let (appels, refus, rendu) = ouvertures(grille, 6, &[2, 6]);
            assert_eq!(appels, [(6, false)], "grille {grille:?}");
            assert!(refus.is_empty());
            assert_eq!(rendu, Ok((6, None)));
        }
    }
}
