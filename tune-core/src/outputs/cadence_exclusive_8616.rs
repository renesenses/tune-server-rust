//! Cadences PCM mesurées avant conversion : jamais de flux ouvert ni de sonde partagée.
use super::masque_de_canaux_4357::negocier_format_et_masque;
use super::negociation_format_exclusif_3837::{CandidatFormat, ResultatSonde};

/// Une erreur autre qu'UNSUPPORTED_FORMAT n'est pas une preuve de cadence refusée.
/// Dans ce cas on laisse l'ouverture normale produire son diagnostic (occupé, débranché…).
pub(crate) fn choisir_cadence<F>(
    demandee: u32,
    voies: u16,
    mut sonde: F,
) -> Result<Option<u32>, String>
where
    F: FnMut(u32, CandidatFormat, u32) -> ResultatSonde,
{
    let famille = if demandee.is_multiple_of(44_100) {
        44_100
    } else if demandee.is_multiple_of(48_000) {
        48_000
    } else {
        0
    };
    let mut cadences = vec![demandee];
    if famille != 0 {
        cadences.extend(
            [16, 8, 4, 2, 1]
                .into_iter()
                .map(|n| famille * n)
                .filter(|&hz| hz < demandee),
        );
    }
    for cadence in cadences {
        let mut inconnu = false;
        let retenu = negocier_format_et_masque(32, u32::from(voies), cadence, |format, masque| {
            let resultat = sonde(cadence, format, masque);
            if let ResultatSonde::Refuse { hr, .. } = resultat {
                inconnu |= hr != 0x88890008u32 as i32;
            }
            resultat
        });
        if inconnu {
            return Ok(None);
        }
        if retenu.is_ok() {
            return Ok(Some(cadence));
        }
    }
    Err(format!(
        "La sortie WASAPI exclusive refuse {demandee} Hz et les cadences inférieures de la même famille. Réduisez « Fréquence max » ou choisissez le mode partagé."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn refus() -> ResultatSonde {
        ResultatSonde::Refuse {
            hr: 0x88890008u32 as i32,
            propose: None,
        }
    }
    #[test]
    fn cadence_8616_refus_176k_replie_sur_88k_mesures() {
        let mut vus = Vec::new();
        let hz = choisir_cadence(176_400, 2, |hz, format, _| {
            vus.push(hz);
            if hz == 88_200 && format.bits_valides == 24 {
                ResultatSonde::Accepte
            } else {
                refus()
            }
        })
        .unwrap();
        assert_eq!(
            hz,
            Some(88_200),
            "176,4 kHz refusé doit produire du PCM à 88,2 kHz accepté"
        );
        assert_eq!(&vus[..4], &[176_400; 4]);
        assert!(!vus.contains(&48_000));
    }
    #[test]
    fn cadence_8616_acceptee_ne_change_pas() {
        let mut appels = 0;
        assert_eq!(
            choisir_cadence(176_400, 2, |_, _, _| {
                appels += 1;
                ResultatSonde::Accepte
            })
            .unwrap(),
            Some(176_400)
        );
        assert_eq!(appels, 1);
    }
    #[test]
    fn cadence_8616_occupee_ne_declenche_pas_de_repli() {
        let mut appels = 0;
        assert_eq!(
            choisir_cadence(176_400, 2, |_, _, _| {
                appels += 1;
                ResultatSonde::Refuse {
                    hr: super::super::negociation_format_exclusif_3837::AUDCLNT_E_DEVICE_IN_USE,
                    propose: None,
                }
            })
            .unwrap(),
            None
        );
        assert_eq!(appels, 1);
    }
    #[test]
    fn cadence_8616_refus_total_est_explicite_et_borne() {
        let mut vus = Vec::new();
        let erreur = choisir_cadence(192_000, 2, |hz, _, _| {
            vus.push(hz);
            refus()
        })
        .unwrap_err();
        assert_eq!(
            vus,
            [192_000, 96_000, 48_000]
                .into_iter()
                .flat_map(|hz| [hz; 4])
                .collect::<Vec<_>>()
        );
        assert!(erreur.contains("Fréquence max") && erreur.contains("mode partagé"));
    }
}
