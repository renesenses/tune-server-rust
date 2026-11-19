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

/// #9165 — le greffon porterait-il ce flux sur le chemin partagé ? Un choix
/// armé (non écarté) qu'aucune garde d'ouverture n'écarte : le chemin natif
/// S32 de Linux doit alors céder, comme pour un DSP actif.
#[cfg(any(target_os = "linux", test))]
pub(super) fn surechantillonneur_voulu(
    choix_present: bool,
    spec: AudioSpec,
    amorce: &[u8],
    pure: bool,
    dop: bool,
    sortie_double: bool,
) -> bool {
    choix_present && refus_a_l_ouverture(spec, amorce, pure, dop, sortie_double).is_none()
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
    fn surechantillonneur_voulu_9165_suit_les_gardes_de_l_ouverture() {
        let s32 = AudioSpec::depuis_entete(192_000, 32, 2).unwrap();
        assert!(surechantillonneur_voulu(
            true,
            s32,
            &[],
            false,
            false,
            false
        ));
        assert!(!surechantillonneur_voulu(
            false,
            s32,
            &[],
            false,
            false,
            false
        ));
        for (pure, dop, double) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            assert!(!surechantillonneur_voulu(true, s32, &[], pure, dop, double));
        }
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
