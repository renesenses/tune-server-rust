//! L'en-tête RTP (RFC 3550), 12 octets, sans source contributive ni extension.
//!
//! L'étude (§ 3.1) écarte la crate `rtp` : elle est pensée pour WebRTC, et
//! l'en-tête fixe s'écrit en quelques lignes, comme la sortie AirPlay 1 le
//! fait déjà.

/// Taille de l'en-tête fixe, sans CSRC.
pub const TAILLE_ENTETE_RTP: usize = 12;

/// Les champs utiles de l'en-tête. Version 2, sans bourrage, sans extension,
/// sans source contributive : c'est tout ce qu'un émetteur AES67 envoie.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnteteRtp {
    /// Bit M : posé sur le premier paquet après un départ ou une reprise,
    /// pour signaler une discontinuité de l'horodatage.
    pub marqueur: bool,
    /// Type de charge dynamique (96 à 127), celui du `a=rtpmap` du SDP.
    pub type_charge: u8,
    pub sequence: u16,
    /// Horodatage média, en trames, modulo 2³².
    pub horodatage: u32,
    pub ssrc: u32,
}

impl EnteteRtp {
    /// Écrit l'en-tête dans les 12 premiers octets de `dst`.
    pub fn ecrire(&self, dst: &mut [u8; TAILLE_ENTETE_RTP]) {
        dst[0] = 0x80; // V=2, P=0, X=0, CC=0
        dst[1] = (u8::from(self.marqueur) << 7) | (self.type_charge & 0x7f);
        dst[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        dst[4..8].copy_from_slice(&self.horodatage.to_be_bytes());
        dst[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }

    /// Un paquet complet : l'en-tête suivi de la charge.
    pub fn paquet(&self, charge: &[u8]) -> Vec<u8> {
        let mut entete = [0u8; TAILLE_ENTETE_RTP];
        self.ecrire(&mut entete);
        let mut paquet = Vec::with_capacity(TAILLE_ENTETE_RTP + charge.len());
        paquet.extend_from_slice(&entete);
        paquet.extend_from_slice(charge);
        paquet
    }

    /// Relit un en-tête et rend la charge qui le suit. Refuse ce qu'un
    /// émetteur AES67 n'envoie pas (autre version, bourrage, extension,
    /// sources contributives) : le récepteur de test doit échouer bruyamment
    /// si l'émetteur dérive.
    pub fn lire(paquet: &[u8]) -> Result<(Self, &[u8]), String> {
        if paquet.len() < TAILLE_ENTETE_RTP {
            return Err(format!("paquet RTP trop court : {} octets", paquet.len()));
        }
        let version = paquet[0] >> 6;
        if version != 2 {
            return Err(format!("version RTP {version}, attendu 2"));
        }
        if paquet[0] & 0x20 != 0 {
            return Err("bourrage RTP inattendu".into());
        }
        if paquet[0] & 0x10 != 0 {
            return Err("extension RTP inattendue".into());
        }
        if paquet[0] & 0x0f != 0 {
            return Err("sources contributives RTP inattendues".into());
        }
        Ok((
            Self {
                marqueur: paquet[1] & 0x80 != 0,
                type_charge: paquet[1] & 0x7f,
                sequence: u16::from_be_bytes([paquet[2], paquet[3]]),
                horodatage: u32::from_be_bytes([paquet[4], paquet[5], paquet[6], paquet[7]]),
                ssrc: u32::from_be_bytes([paquet[8], paquet[9], paquet[10], paquet[11]]),
            },
            &paquet[TAILLE_ENTETE_RTP..],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_entete_s_ecrit_octet_par_octet_comme_la_rfc_3550() {
        let e = EnteteRtp {
            marqueur: true,
            type_charge: 98,
            sequence: 0x1234,
            horodatage: 0xdead_beef,
            ssrc: 0x0102_0304,
        };
        let mut o = [0u8; TAILLE_ENTETE_RTP];
        e.ecrire(&mut o);
        assert_eq!(
            o,
            [
                0x80,
                0x80 | 98,
                0x12,
                0x34,
                0xde,
                0xad,
                0xbe,
                0xef,
                0x01,
                0x02,
                0x03,
                0x04
            ]
        );
        let sans_marqueur = EnteteRtp {
            marqueur: false,
            ..e
        };
        sans_marqueur.ecrire(&mut o);
        assert_eq!(o[1], 98);
    }

    #[test]
    fn l_entete_se_relit_a_l_identique_et_rend_la_charge() {
        let e = EnteteRtp {
            marqueur: false,
            type_charge: 97,
            sequence: 65_535,
            horodatage: 48,
            ssrc: 7,
        };
        let p = e.paquet(&[1, 2, 3]);
        let (relu, charge) = EnteteRtp::lire(&p).unwrap();
        assert_eq!(relu, e);
        assert_eq!(charge, &[1, 2, 3]);
    }

    #[test]
    fn ce_qu_un_emetteur_aes67_n_envoie_pas_est_refuse() {
        assert!(EnteteRtp::lire(&[0x80; 11]).is_err());
        let mut p = [0u8; 12];
        p[0] = 0x40; // version 1
        assert!(EnteteRtp::lire(&p).unwrap_err().contains("version"));
        p[0] = 0xa0; // bourrage
        assert!(EnteteRtp::lire(&p).unwrap_err().contains("bourrage"));
        p[0] = 0x90; // extension
        assert!(EnteteRtp::lire(&p).unwrap_err().contains("extension"));
        p[0] = 0x81; // un CSRC
        assert!(EnteteRtp::lire(&p).unwrap_err().contains("contributives"));
    }
}
