//! PCM linéaire 24 bits gros-boutiste (L24, RFC 3190), le codage d'AES67.
//!
//! Le décodeur du cœur rend des échantillons 32 bits pleine échelle. Le
//! passage à 24 bits arrondit au plus proche et sature (un `+0x80` sur
//! `i32::MAX` ne doit pas basculer en négatif). Pas de dither à cette étape :
//! la question se posera avec le volume logiciel (§ 5 de l'étude), qui est
//! désactivé par défaut.

/// Octets par échantillon L24.
pub const OCTETS_PAR_ECHANTILLON: usize = 3;

/// Un échantillon 32 bits pleine échelle vers sa valeur 24 bits signée.
#[inline]
pub fn i32_vers_24_bits(echantillon: i32) -> i32 {
    echantillon.saturating_add(0x80) >> 8
}

/// Ajoute à `dst` les échantillons en L24 gros-boutiste.
pub fn ecrire_l24(echantillons: &[i32], dst: &mut Vec<u8>) {
    dst.reserve(echantillons.len() * OCTETS_PAR_ECHANTILLON);
    for &e in echantillons {
        let v = i32_vers_24_bits(e);
        dst.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8]);
    }
}

/// Relit du L24 gros-boutiste en valeurs 24 bits signées (étendues sur 32).
/// Un reste de moins de 3 octets est ignoré.
pub fn lire_l24(src: &[u8]) -> Vec<i32> {
    src.as_chunks::<OCTETS_PAR_ECHANTILLON>()
        .0
        .iter()
        .map(|o| (i32::from_be_bytes([o[0], o[1], o[2], 0])) >> 8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_l24_est_gros_boutiste_et_signe() {
        let mut o = Vec::new();
        ecrire_l24(&[0x0123_4500, -256, 0], &mut o);
        assert_eq!(o, [0x01, 0x23, 0x45, 0xff, 0xff, 0xff, 0, 0, 0]);
        assert_eq!(lire_l24(&o), vec![0x01_2345, -1, 0]);
    }

    #[test]
    fn l_arrondi_sature_aux_deux_bouts_de_l_echelle() {
        assert_eq!(i32_vers_24_bits(i32::MAX), 0x7f_ffff);
        assert_eq!(i32_vers_24_bits(i32::MIN), -0x80_0000);
        // Au plus proche : 0x7f arrondit vers le bas, 0x80 vers le haut.
        assert_eq!(i32_vers_24_bits(0x17f), 1);
        assert_eq!(i32_vers_24_bits(0x180), 2);
        let mut o = Vec::new();
        ecrire_l24(&[i32::MAX, i32::MIN], &mut o);
        assert_eq!(o, [0x7f, 0xff, 0xff, 0x80, 0x00, 0x00]);
    }

    #[test]
    fn un_echantillon_24_bits_fait_l_aller_retour_sans_perte() {
        for v in [-0x80_0000, -1, 0, 1, 0x12_3456, 0x7f_ffff] {
            let mut o = Vec::new();
            ecrire_l24(&[v << 8], &mut o);
            assert_eq!(lire_l24(&o), vec![v]);
        }
    }
}
