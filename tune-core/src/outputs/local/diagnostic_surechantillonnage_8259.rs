//! Le diagnostic de la piste ouverte, distinct du réglage de la piste suivante.
//! Consulté par le producteur et le sondeur, jamais par le rappel audio.
use super::{AudioSpec, DOP_DETECT_FRAMES, TransformationsReelles, is_dop_pcm};
use std::sync::Mutex;

#[derive(Default)]
struct Etat {
    demande: Option<&'static str>,
    piste: Option<&'static str>,
    /// #9165 — un choix était armé à l'ouverture de la piste.
    choix_present: bool,
}

#[derive(Default)]
pub(super) struct Diagnostic {
    etat: Mutex<Etat>,
}

impl Diagnostic {
    pub(super) fn demander(&self, raison: Option<&'static str>) {
        if let Ok(mut e) = self.etat.lock() {
            e.demande = raison;
        }
    }

    /// Appelé sous le verrou du choix : la raison et le choix sont du même instant.
    pub(super) fn ouvrir(&self, choix_present: bool) {
        if let Ok(mut e) = self.etat.lock() {
            e.choix_present = choix_present;
            e.piste = if choix_present {
                // Les chemins qui n'ouvrent pas d'étage ne doivent pas promettre
                // un traitement à la piste suivante. CPAL affine cette raison.
                Some("output_path_not_supported")
            } else {
                e.demande
            };
        }
    }

    /// #9165 — un chemin qui n'ouvre pas d'étage nomme SA cause (DSD natif,
    /// flux compressé décodé en entier) au lieu du « chemin de sortie » posé
    /// d'office, réservé aux bras exclusifs macOS/Windows. Sans choix armé,
    /// la raison de la demande (licence, test de la machine…) reste.
    pub(super) fn chemin_sans_etage(&self, raison: &'static str) {
        if let Ok(mut e) = self.etat.lock()
            && e.choix_present
        {
            e.piste = Some(raison);
        }
    }

    pub(super) fn noter(&self, raison: Option<&'static str>) {
        if let Ok(mut e) = self.etat.lock() {
            e.piste = raison;
        }
    }

    pub(super) fn declarer(&self, t: TransformationsReelles) -> TransformationsReelles {
        t.avec_refus_surechantillonnage(self.etat.lock().ok().and_then(|e| e.piste))
    }
}

/// #9165 — ce que l'ouverture partagée ferait du greffon, jugé AVANT le
/// chemin natif S32 de Linux, par les mêmes règles : les gardes de
/// l'ouverture ([`refus_a_l_ouverture`]), puis la part de la négociation de
/// cadence qui se juge sans périphérique (bit-perfect strict, cadences
/// candidates, verdict de la machine, plafonds).
#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GreffonAvantLeNatif {
    /// Aucun choix armé pour la zone.
    Absent,
    /// Le chemin partagé porterait l'étage : le natif doit céder.
    Voulu,
    /// Armé, mais l'ouverture partagée l'écarterait : la raison, pour le
    /// chemin du signal quand le natif est pris.
    Ecarte(&'static str),
}

#[cfg(any(target_os = "linux", test))]
pub(super) fn greffon_avant_le_natif(
    choix: Option<&crate::audio::surechantillonnage::ChoixSurechantillonnage>,
    spec: AudioSpec,
    amorce: &[u8],
    pure: bool,
    dop: bool,
    sortie_double: bool,
    strict_bitperfect: bool,
) -> GreffonAvantLeNatif {
    let Some(choix) = choix else {
        return GreffonAvantLeNatif::Absent;
    };
    if let Some(raison) = refus_a_l_ouverture(spec, amorce, pure, dop, sortie_double) {
        return GreffonAvantLeNatif::Ecarte(raison);
    }
    match super::backend::surechantillonneur_possible_sans_peripherique(
        choix,
        strict_bitperfect,
        spec.cadence(),
    ) {
        Ok(()) => GreffonAvantLeNatif::Voulu,
        Err(refus) => GreffonAvantLeNatif::Ecarte(refus.code()),
    }
}

/// Un choix que le test de la machine admet vers toutes les cadences de
/// référence, filtre et plafonds par défaut.
#[cfg(test)]
pub(super) fn choix_admis_pour_test() -> crate::audio::surechantillonnage::ChoixSurechantillonnage {
    use crate::audio::surechantillonnage::{
        ChoixSurechantillonnage, MesureMachine, UpsamplerSettings, VerdictMachine,
    };
    let reglages = UpsamplerSettings {
        enabled: true,
        ..Default::default()
    };
    let results = [
        (44_100, 352_800),
        (44_100, 705_600),
        (48_000, 384_000),
        (48_000, 768_000),
    ]
    .into_iter()
    .map(|(from_hz, to_hz)| MesureMachine {
        filter: reglages.filter.clone(),
        from_hz,
        to_hz,
        realtime_factor: 10.0,
        admitted: true,
    })
    .collect();
    ChoixSurechantillonnage {
        reglages,
        verdict: VerdictMachine {
            simd_path: "scalaire".into(),
            margin: 1.0,
            results,
            measured_at: 0,
        },
    }
}

/// Les gardes existantes de l'ouverture, avec leur raison au lieu d'un booléen.
pub(super) fn refus_a_l_ouverture(
    spec: AudioSpec,
    amorce: &[u8],
    pure: bool,
    dop: bool,
    sortie_double: bool,
) -> Option<&'static str> {
    if sortie_double {
        return Some("multiple_outputs");
    }
    if pure {
        return Some("pure");
    }
    if dop {
        return Some("dop");
    }
    let bits = spec.profondeur().bits_declares();
    if bits == 24 {
        let sonde = DOP_DETECT_FRAMES * usize::from(spec.canaux()) * 3;
        if amorce.len() < sonde {
            return Some("source_probe_incomplete");
        }
        if is_dop_pcm(amorce, bits, spec.canaux()) {
            return Some("dop");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::traits::FormatOuvert;

    fn mesure() -> TransformationsReelles {
        TransformationsReelles::nouvelles(
            AudioSpec::depuis_entete(44_100, 16, 2).unwrap(),
            FormatOuvert::new(44_100, 2),
            false,
        )
    }

    #[test]
    fn diagnostic_8259_la_sortie_publie_le_refus_du_choix() {
        use crate::audio::surechantillonnage::Ecarte;
        use crate::outputs::traits::OutputTarget;
        let sortie = super::super::LocalOutput::new("diagnostic sans périphérique".into());
        sortie.set_decision_surechantillonnage(Err(Ecarte::PremiumRequis));
        sortie.diagnostic_surechantillonnage.ouvrir(false);
        *sortie.transformations_reelles.lock().unwrap() = Some(mesure());
        assert_eq!(
            sortie
                .transformations_reelles()
                .unwrap()
                .refus_surechantillonnage(),
            Some("premium_required")
        );
        sortie.set_decision_surechantillonnage(Err(Ecarte::TestDeLaMachineAbsent));
        assert_eq!(
            sortie
                .transformations_reelles()
                .unwrap()
                .refus_surechantillonnage(),
            Some("premium_required")
        );
        sortie.diagnostic_surechantillonnage.ouvrir(false);
        assert_eq!(
            sortie
                .transformations_reelles()
                .unwrap()
                .refus_surechantillonnage(),
            Some("machine_test_missing")
        );
    }

    #[test]
    fn diagnostic_8259_le_reglage_suivant_ne_reecrit_pas_la_piste_ouverte() {
        let d = Diagnostic::default();
        d.demander(Some("premium_required"));
        d.ouvrir(false);
        d.demander(None);
        assert_eq!(
            d.declarer(mesure()).refus_surechantillonnage(),
            Some("premium_required")
        );
        d.ouvrir(true);
        d.noter(Some("rates_not_measured"));
        assert_eq!(
            d.declarer(mesure()).refus_surechantillonnage(),
            Some("rates_not_measured")
        );
        assert!(
            d.declarer(mesure().avec_surechantillonnage("polyvalent", -3.0))
                .refus_surechantillonnage()
                .is_none()
        );
        d.noter(None);
        assert!(d.declarer(mesure()).refus_surechantillonnage().is_none());
    }

    #[test]
    fn diagnostic_9165_les_chemins_sans_etage_nomment_leur_cause() {
        let d = Diagnostic::default();
        // Choix armé : la valeur d'office cède à la cause du chemin.
        d.ouvrir(true);
        assert_eq!(
            d.declarer(mesure()).refus_surechantillonnage(),
            Some("output_path_not_supported")
        );
        d.chemin_sans_etage("dsd");
        assert_eq!(d.declarer(mesure()).refus_surechantillonnage(), Some("dsd"));
        d.ouvrir(true);
        d.chemin_sans_etage("compressed_stream");
        assert_eq!(
            d.declarer(mesure()).refus_surechantillonnage(),
            Some("compressed_stream")
        );
        // Sans choix armé, la raison de la demande n'est pas réécrite.
        d.demander(Some("premium_required"));
        d.ouvrir(false);
        d.chemin_sans_etage("dsd");
        assert_eq!(
            d.declarer(mesure()).refus_surechantillonnage(),
            Some("premium_required")
        );
    }

    #[test]
    fn greffon_avant_le_natif_9165_suit_les_regles_de_l_ouverture() {
        use GreffonAvantLeNatif::{Absent, Ecarte, Voulu};
        let choix = choix_admis_pour_test();
        let s32 = AudioSpec::depuis_entete(96_000, 32, 2).unwrap();
        let juger = |spec, pure, dop, double, strict| {
            greffon_avant_le_natif(Some(&choix), spec, &[], pure, dop, double, strict)
        };
        assert_eq!(
            greffon_avant_le_natif(None, s32, &[], false, false, false, false),
            Absent
        );
        assert_eq!(juger(s32, false, false, false, false), Voulu);
        assert_eq!(juger(s32, true, false, false, false), Ecarte("pure"));
        assert_eq!(juger(s32, false, true, false, false), Ecarte("dop"));
        assert_eq!(
            juger(s32, false, false, true, false),
            Ecarte("multiple_outputs")
        );
        // Bit-perfect strict : l'ouverture partagée refuserait le greffon.
        assert_eq!(
            juger(s32, false, false, false, true),
            Ecarte("strict_bitperfect")
        );
        // Source déjà au plus haut des cadences candidates.
        let s768 = AudioSpec::depuis_entete(768_000, 32, 2).unwrap();
        assert_eq!(
            juger(s768, false, false, false, false),
            Ecarte("no_candidate_rate_accepted_by_device")
        );
        // Machine qui n'admet rien.
        let mut refuse = choix.clone();
        refuse.verdict.results.clear();
        assert_eq!(
            greffon_avant_le_natif(Some(&refuse), s32, &[], false, false, false, false),
            Ecarte("machine_test_rejects_accepted_rates")
        );
    }

    #[test]
    fn diagnostic_8259_les_gardes_nomment_le_motif_sans_modifier_le_pcm() {
        let pcm = AudioSpec::depuis_entete(44_100, 16, 2).unwrap();
        let pcm24 = AudioSpec::depuis_entete(44_100, 24, 2).unwrap();
        assert_eq!(refus_a_l_ouverture(pcm, &[], false, false, false), None);
        for (pure, dop, double, attendu) in [
            (true, false, false, "pure"),
            (false, true, false, "dop"),
            (false, false, true, "multiple_outputs"),
        ] {
            assert_eq!(
                refus_a_l_ouverture(pcm, &[], pure, dop, double),
                Some(attendu)
            );
        }
        assert_eq!(
            refus_a_l_ouverture(pcm24, &[], false, false, false),
            Some("source_probe_incomplete")
        );
        let mut dop = Vec::new();
        for i in 0..DOP_DETECT_FRAMES {
            let m = if i % 2 == 0 { 0x05 } else { 0xfa };
            dop.extend_from_slice(&[0x69, 0x69, m, 0x69, 0x69, m]);
        }
        assert_eq!(
            refus_a_l_ouverture(pcm24, &dop, false, false, false),
            Some("dop")
        );
        assert_eq!(
            refus_a_l_ouverture(pcm24, &vec![0; dop.len()], false, false, false),
            None
        );
    }
}
