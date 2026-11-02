//! La source d'horloge des horodatages RTP.
//!
//! AES67 lie l'horodatage RTP à une horloge de référence commune (PTPv2,
//! RFC 7273) : `horodatage = temps × cadence + décalage mod 2³²`. Deux
//! sources : [`HorlogeSysteme`] (étape E1, sans PTP) et le suiveur PTP
//! [`crate::ptp::SuiveurPtp`] (étape E2, `statime`, décision de Bertrand du
//! 30/10/2026). Le cadenceur ne lit le temps que par
//! [`SourceHorloge::maintenant_ns`] et [`SourceHorloge::generation`], et le
//! SDP ne décrit l'horloge que par [`SourceHorloge::reference`].

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Ce que le SDP dit de l'horloge (`a=ts-refclk`, RFC 7273).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceHorloge {
    /// Horloge locale de l'émetteur, sans référence partagée :
    /// `a=ts-refclk:local`. Un récepteur sans PTP joue le flux ; un récepteur
    /// qui exige PTP le refusera, et c'est attendu à l'étape E1.
    Locale,
    /// Maître PTP IEEE 1588-2008 : identité (EUI-64) et domaine.
    Ptp {
        identite_maitre: [u8; 8],
        domaine: u8,
    },
}

impl ReferenceHorloge {
    /// La valeur de l'attribut `a=ts-refclk:`.
    pub fn attribut_sdp(&self) -> String {
        match self {
            Self::Locale => "local".to_string(),
            Self::Ptp {
                identite_maitre,
                domaine,
            } => {
                let id: Vec<String> = identite_maitre.iter().map(|o| format!("{o:02X}")).collect();
                format!("ptp=IEEE1588-2008:{}:{domaine}", id.join("-"))
            }
        }
    }
}

/// Une horloge de référence pour l'émetteur.
pub trait SourceHorloge: Send + Sync {
    /// Le temps de référence, en nanosecondes depuis l'époque de l'horloge.
    /// Doit être monotone À GÉNÉRATION ÉGALE : l'émetteur en déduit à la fois
    /// l'horodatage et l'instant d'envoi de chaque paquet. Un saut (calage sur
    /// un maître PTP, changement de maître) change la génération.
    fn maintenant_ns(&self) -> u64;

    /// Change à chaque discontinuité de l'échelle de temps ou de la
    /// référence annoncée (saut d'horloge, calage, repli, autre maître).
    /// Le cadenceur repart alors d'une nouvelle origine, bit M posé, et le
    /// SDP est réannoncé. Toujours 0 pour l'horloge système.
    fn generation(&self) -> u64 {
        0
    }

    /// La référence à annoncer dans le SDP.
    fn reference(&self) -> ReferenceHorloge;

    /// L'horloge est-elle calée ? Toujours vrai pour l'horloge système ; un
    /// suiveur PTP rendra `false` tant qu'il n'a pas de maître.
    fn calee(&self) -> bool {
        true
    }
}

/// L'horloge du système, ancrée une fois sur le temps UNIX puis avancée par
/// l'horloge MONOTONE : un réglage NTP en cours de lecture ne fait pas sauter
/// les horodatages.
pub struct HorlogeSysteme {
    origine: Instant,
    origine_ns: u64,
}

impl HorlogeSysteme {
    pub fn new() -> Self {
        let origine_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Self {
            origine: Instant::now(),
            origine_ns,
        }
    }
}

impl Default for HorlogeSysteme {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceHorloge for HorlogeSysteme {
    fn maintenant_ns(&self) -> u64 {
        let ecoule = u64::try_from(self.origine.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.origine_ns.saturating_add(ecoule)
    }
    fn reference(&self) -> ReferenceHorloge {
        ReferenceHorloge::Locale
    }
}

/// L'horodatage RTP d'un instant : `temps × cadence`, en trames, modulo 2³²
/// (RFC 7273, `a=mediaclk:direct=0`).
pub fn horodatage_rtp(temps_ns: u64, cadence: u32) -> u32 {
    let trames = u128::from(temps_ns) * u128::from(cadence) / 1_000_000_000;
    trames as u32
}

/// L'horodatage RTP avec le décalage annoncé par `a=mediaclk:direct=<décalage>`
/// (RFC 7273 § 5.2) : `temps × cadence + décalage`, modulo 2³².
pub fn horodatage_rtp_decale(temps_ns: u64, cadence: u32, decalage: u32) -> u32 {
    horodatage_rtp(temps_ns, cadence).wrapping_add(decalage)
}

/// L'instant (en ns de la même horloge) où commence la trame numéro
/// `trames` comptée depuis `origine_ns`.
pub fn instant_de_la_trame(origine_ns: u64, trames: u64, cadence: u32) -> u64 {
    let ecart = u128::from(trames) * 1_000_000_000 / u128::from(cadence.max(1));
    origine_ns.saturating_add(u64::try_from(ecart).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_horodatage_rtp_compte_les_trames_modulo_2_puissance_32() {
        assert_eq!(horodatage_rtp(0, 48_000), 0);
        assert_eq!(horodatage_rtp(1_000_000, 48_000), 48, "1 ms = 48 trames");
        assert_eq!(horodatage_rtp(1_000_000_000, 48_000), 48_000);
        // 2³² trames à 48 kHz ≈ 89 478 s : l'horodatage revient à zéro.
        let tour_ns = (1u128 << 32) * 1_000_000_000 / 48_000;
        let tour_ns = tour_ns as u64;
        assert!(horodatage_rtp(tour_ns + 1_000_000, 48_000) <= 48);
        // Un temps PTP réaliste (2026) ne déborde pas le calcul.
        let ns_2026 = 1_793_000_000u64 * 1_000_000_000;
        let _ = horodatage_rtp(ns_2026, 96_000);
    }

    #[test]
    fn le_decalage_media_s_ajoute_modulo_2_puissance_32() {
        assert_eq!(horodatage_rtp_decale(1_000_000, 48_000, 0), 48);
        assert_eq!(horodatage_rtp_decale(1_000_000, 48_000, 2), 50);
        assert_eq!(horodatage_rtp_decale(0, 48_000, u32::MAX), u32::MAX);
        assert_eq!(horodatage_rtp_decale(1_000_000, 48_000, u32::MAX), 47);
    }

    #[test]
    fn l_instant_d_une_trame_est_exact_a_la_nanoseconde() {
        assert_eq!(instant_de_la_trame(10, 48, 48_000), 10 + 1_000_000);
        assert_eq!(instant_de_la_trame(0, 48_000, 48_000), 1_000_000_000);
    }

    #[test]
    fn l_horloge_systeme_est_monotone_et_se_dit_locale() {
        let h = HorlogeSysteme::new();
        let a = h.maintenant_ns();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = h.maintenant_ns();
        assert!(b >= a + 2_000_000);
        assert_eq!(h.reference(), ReferenceHorloge::Locale);
        assert!(h.calee());
        assert_eq!(h.generation(), 0);
    }

    #[test]
    fn la_reference_s_ecrit_comme_la_rfc_7273() {
        assert_eq!(ReferenceHorloge::Locale.attribut_sdp(), "local");
        let ptp = ReferenceHorloge::Ptp {
            identite_maitre: [0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x12, 0x34, 0x56],
            domaine: 0,
        };
        assert_eq!(
            ptp.attribut_sdp(),
            "ptp=IEEE1588-2008:00-1D-C1-FF-FE-12-34-56:0"
        );
    }
}
