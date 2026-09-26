//! Des échantillons captés aux octets PCM servis — pur, sans périphérique.
//!
//! Le PCM servi est de l'entier signé petit-boutiste entrelacé (le contrat de
//! `tune_core::source_pcm::FormatPcm`), en 16 ou 24 bits.
//!
//! ## Pourquoi la conversion depuis le flottant est EXACTE
//!
//! CoreAudio rend toute entrée en `f32`, quel que soit le format physique du
//! périphérique : un mot entier de N ≤ 24 bits y devient `n / 2^(N-1)`. Un
//! `f32` a 24 bits de mantisse : ce quotient est représenté SANS perte, et
//! `round(x · 2^(B-1))` avec B = 24 (ou 16 pour une source 16 bits) rend
//! exactement l'entier d'origine. C'est ce qui permet la comparaison octet
//! pour octet à travers Loopback.

/// Format des échantillons tels que le pilote les rend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Echantillons {
    F32,
    I16,
    /// Entier 24 bits porté dans un `i32` (valeur dans ±2^23).
    I24,
    /// Entier 32 bits : les 24 bits de poids fort sont servis.
    I32,
}

/// Profondeur servie pour un format capté, et la profondeur PHYSIQUE quand le
/// système la connaît (CoreAudio) : une source 16 bits est servie en 16 bits,
/// tout le reste en 24.
pub fn bits_servis(echantillons: Echantillons, bits_physiques: Option<u16>) -> u16 {
    match (echantillons, bits_physiques) {
        (Echantillons::I16, _) => 16,
        (Echantillons::F32, Some(16)) => 16,
        _ => 24,
    }
}

/// Ce qu'un bloc capté a donné, en plus de ses octets.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Mesure {
    /// Crête absolue du bloc, en pleine échelle (0.0 ..= 1.0).
    pub crete: f32,
    /// Vrai si CHAQUE échantillon du bloc vaut exactement zéro.
    pub nul: bool,
    /// Un préambule IEC 61937 (Dolby, DTS… non décodés) a été vu.
    pub iec61937: bool,
}

fn empiler(sortie: &mut Vec<u8>, v: i32, bits: u16) {
    let o = v.to_le_bytes();
    if bits == 16 {
        sortie.extend_from_slice(&o[..2]);
    } else {
        sortie.extend_from_slice(&o[..3]);
    }
}

fn depuis_flottant(x: f32, bits: u16) -> i32 {
    let echelle = if bits == 16 { 32_768.0 } else { 8_388_608.0 };
    let max = echelle as i64 - 1;
    ((x as f64 * echelle).round() as i64).clamp(-(echelle as i64), max) as i32
}

/// Convertit un bloc de `f32` en PCM `bits`, et le mesure.
pub fn convertir_f32(entree: &[f32], bits: u16, sortie: &mut Vec<u8>) -> Mesure {
    let mut crete = 0f32;
    let mut nul = true;
    for &x in entree {
        let a = x.abs();
        if a > crete {
            crete = a;
        }
        nul &= x == 0.0;
        empiler(sortie, depuis_flottant(x, bits), bits);
    }
    Mesure {
        crete,
        nul,
        iec61937: false,
    }
}

/// Convertit un bloc d'entiers : `decalage` bits de poids faible sont retirés
/// (8 pour un 32 bits servi en 24), `pleine_echelle` sert à la crête.
pub fn convertir_entiers(
    entree: impl Iterator<Item = i32>,
    decalage: u32,
    pleine_echelle: f32,
    bits: u16,
    sortie: &mut Vec<u8>,
) -> Mesure {
    let mut crete = 0i64;
    let mut nul = true;
    for v in entree {
        let a = (v as i64).abs();
        if a > crete {
            crete = a;
        }
        nul &= v == 0;
        empiler(sortie, v >> decalage, bits);
    }
    Mesure {
        crete: (crete as f32 / pleine_echelle).min(1.0),
        nul,
        iec61937: false,
    }
}

/// Le bloc porte-t-il un flux COMPRESSÉ encapsulé en IEC 61937 (Dolby
/// Digital, DTS… tels qu'une TV ou un lecteur les envoient en S/PDIF ou en
/// HDMI « bitstream ») ?
///
/// Un tel flux arrive comme du PCM 16 bits stéréo, mais ce n'est pas du son :
/// servi tel quel, c'est du BRUIT à pleine échelle. Chaque rafale commence par
/// le préambule `Pa = 0xF872`, `Pb = 0x4E1F` sur deux mots consécutifs (voies
/// gauche puis droite d'une trame). En 24 bits, le mot de 16 bits occupe les
/// poids forts et l'octet faible est nul.
pub fn contient_iec61937(octets: &[u8], canaux: u16, bits: u16) -> bool {
    if canaux < 2 || !(bits == 16 || bits == 24) {
        return false;
    }
    let o = bits as usize / 8;
    let mot = |s: &[u8]| -> Option<u16> {
        match o {
            2 => Some(u16::from_le_bytes([s[0], s[1]])),
            _ if s[0] == 0 => Some(u16::from_le_bytes([s[1], s[2]])),
            _ => None,
        }
    };
    octets
        .chunks_exact(o * canaux as usize)
        .any(|t| mot(&t[..o]) == Some(0xF872) && mot(&t[o..2 * o]) == Some(0x4E1F))
}

/// Une crête en dBFS (`None` pour le silence numérique).
pub fn dbfs(crete: f32) -> Option<f64> {
    (crete > 0.0).then(|| 20.0 * (crete as f64).log10())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tout entier 24 bits passé par le `f32` de CoreAudio revient à
    /// l'identique : c'est la condition de la comparaison octet pour octet.
    #[test]
    fn un_entier_24_bits_traverse_le_flottant_sans_perte() {
        let valeurs: Vec<i32> = (-8_388_608..=8_388_607)
            .step_by(997)
            .chain([-8_388_608, 8_388_607, 0, 1, -1])
            .collect();
        let flottants: Vec<f32> = valeurs.iter().map(|&v| v as f32 / 8_388_608.0).collect();
        let mut octets = Vec::new();
        convertir_f32(&flottants, 24, &mut octets);
        let rendus: Vec<i32> = octets
            .chunks(3)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], 0]) << 8 >> 8)
            .collect();
        assert_eq!(rendus, valeurs);
    }

    #[test]
    fn un_entier_16_bits_traverse_le_flottant_sans_perte() {
        let valeurs: Vec<i32> = (-32_768..=32_767).collect();
        let flottants: Vec<f32> = valeurs.iter().map(|&v| v as f32 / 32_768.0).collect();
        let mut octets = Vec::new();
        convertir_f32(&flottants, 16, &mut octets);
        let rendus: Vec<i32> = octets
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
            .collect();
        assert_eq!(rendus, valeurs);
    }

    #[test]
    fn la_pleine_echelle_positive_est_ecretee_et_la_crete_mesuree() {
        let mut o = Vec::new();
        let m = convertir_f32(&[1.0, -1.0, 0.5], 24, &mut o);
        assert_eq!(m.crete, 1.0);
        assert!(!m.nul);
        assert_eq!(&o[..3], &8_388_607i32.to_le_bytes()[..3]);
        assert_eq!(&o[3..6], &(-8_388_608i32).to_le_bytes()[..3]);
    }

    #[test]
    fn un_bloc_de_zeros_est_dit_nul() {
        let mut o = Vec::new();
        assert!(convertir_f32(&[0.0; 64], 24, &mut o).nul);
        assert_eq!(dbfs(0.0), None);
        let m = convertir_entiers([0, 0, 3].into_iter(), 0, 32_768.0, 16, &mut o);
        assert!(!m.nul);
    }

    #[test]
    fn un_32_bits_est_servi_par_ses_24_bits_de_poids_fort() {
        let mut o = Vec::new();
        convertir_entiers([0x1234_5600].into_iter(), 8, 2_147_483_648.0, 24, &mut o);
        assert_eq!(o, vec![0x56, 0x34, 0x12]);
    }

    #[test]
    fn la_profondeur_servie_suit_le_format_physique() {
        assert_eq!(bits_servis(Echantillons::I16, None), 16);
        assert_eq!(bits_servis(Echantillons::F32, Some(16)), 16);
        assert_eq!(bits_servis(Echantillons::F32, Some(24)), 24);
        assert_eq!(bits_servis(Echantillons::F32, Some(32)), 24);
        assert_eq!(bits_servis(Echantillons::F32, None), 24);
        assert_eq!(bits_servis(Echantillons::I32, None), 24);
    }

    /// Une rafale IEC 61937 (préambule Pa/Pb) est reconnue en 16 comme en
    /// 24 bits ; de la musique ordinaire ne l'est pas.
    #[test]
    fn un_flux_dolby_ou_dts_encapsule_est_reconnu() {
        let mut v16 = Vec::new();
        for (g, d) in [(0x1234u16, 0x0042u16), (0xF872, 0x4E1F), (0x0001, 0x0B77)] {
            v16.extend_from_slice(&g.to_le_bytes());
            v16.extend_from_slice(&d.to_le_bytes());
        }
        assert!(contient_iec61937(&v16, 2, 16));
        let mut v24 = Vec::new();
        for (g, d) in [(0xF872u16, 0x4E1Fu16)] {
            v24.push(0);
            v24.extend_from_slice(&g.to_le_bytes());
            v24.push(0);
            v24.extend_from_slice(&d.to_le_bytes());
        }
        assert!(contient_iec61937(&v24, 2, 24));
        // Le même motif avec un octet faible non nul : de l'audio 24 bits.
        v24[0] = 1;
        assert!(!contient_iec61937(&v24, 2, 24));
        // Un sinus plein ne contient pas le préambule.
        let sinus: Vec<f32> = (0..48_000)
            .map(|n| (n as f32 * 0.0654).sin() * 0.99)
            .collect();
        let mut o = Vec::new();
        convertir_f32(&sinus, 16, &mut o);
        assert!(!contient_iec61937(&o, 2, 16));
    }
}
