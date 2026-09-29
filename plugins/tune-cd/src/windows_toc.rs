//! Décodage pur de la TOC rendue par `IOCTL_CDROM_READ_TOC_EX`.
//!
//! `Msf = 0` demande des adresses LBA. Chaque `TRACK_DATA` mesure 8 octets ;
//! les adresses de quatre octets sont en ordre réseau.

use crate::toc::{PisteToc, Toc};

const TAILLE_ENTETE: usize = 4;
const TAILLE_PISTE: usize = 8;
const LEAD_OUT: u8 = 0xaa;
const PISTE_DONNEES: u8 = 0x04;

pub(crate) fn decoder_toc(octets: &[u8]) -> Result<Toc, String> {
    if octets.len() < TAILLE_ENTETE {
        return Err("TOC Windows tronquée (en-tête)".into());
    }
    let longueur = u16::from_be_bytes([octets[0], octets[1]]) as usize + 2;
    let premiere = octets[2];
    let derniere = octets[3];
    if premiere == 0 || derniere < premiere || derniere > 99 {
        return Err("TOC Windows : numéros de piste invalides".into());
    }
    let entrees = usize::from(derniere - premiere) + 2; // pistes + lead-out
    let necessaire = TAILLE_ENTETE + entrees * TAILLE_PISTE;
    if longueur < necessaire || octets.len() < longueur {
        return Err("TOC Windows tronquée (pistes ou lead-out)".into());
    }
    let mut pistes = Vec::with_capacity(entrees - 1);
    let mut fin = None;
    for i in 0..entrees {
        let p = &octets[TAILLE_ENTETE + i * TAILLE_PISTE..][..TAILLE_PISTE];
        let numero = if i == entrees - 1 {
            LEAD_OUT
        } else {
            premiere + i as u8
        };
        if p[2] != numero {
            return Err(format!("TOC Windows : piste {numero} absente"));
        }
        let lba = u32::from_be_bytes(p[4..8].try_into().unwrap());
        if numero == LEAD_OUT {
            fin = Some(lba);
        } else {
            pistes.push(PisteToc {
                numero,
                debut: lba,
                audio: p[1] & PISTE_DONNEES == 0,
            });
        }
    }
    Toc::nouvelle(pistes, fin.unwrap()).map_err(|e| format!("TOC Windows : {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entree(numero: u8, lba: u32, donnees: bool) -> [u8; 8] {
        let [a, b, c, d] = lba.to_be_bytes();
        [
            0,
            if donnees { PISTE_DONNEES } else { 0 },
            numero,
            0,
            a,
            b,
            c,
            d,
        ]
    }

    fn disque() -> Vec<u8> {
        let mut toc = vec![0, 34, 1, 3];
        toc.extend(entree(1, 0, false));
        toc.extend(entree(2, 18_751, false));
        toc.extend(entree(3, 39_588, true));
        toc.extend(entree(LEAD_OUT, 59_407, false));
        toc
    }

    #[test]
    fn pistes_audio_donnees_et_fin_en_lba() {
        let toc = decoder_toc(&disque()).unwrap();
        assert_eq!((toc.premiere, toc.derniere, toc.fin), (1, 3, 59_407));
        assert_eq!(toc.piste(2).unwrap().debut, 18_751);
        assert!(!toc.piste(3).unwrap().audio);
        assert_eq!(toc.fin_audio(), (2, 39_588 - 11_400));
    }

    #[test]
    fn refuse_une_toc_tronquee_ou_un_lead_out_manquant() {
        let mut toc = disque();
        assert!(decoder_toc(&toc[..toc.len() - 1]).is_err());
        toc[1] = 26;
        assert!(decoder_toc(&toc).is_err());
        let mut toc = disque();
        toc[4 + 3 * TAILLE_PISTE + 2] = 4;
        assert!(decoder_toc(&toc).is_err());
    }
}
