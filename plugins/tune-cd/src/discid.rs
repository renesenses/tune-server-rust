//! L'identifiant de disque MusicBrainz, calculé depuis la TOC.
//!
//! Algorithme publié (<https://musicbrainz.org/doc/Disc_ID_Calculation>) :
//! SHA-1 de la chaîne ASCII « première piste (2 chiffres hexa majuscules),
//! dernière piste (2), puis 100 offsets (8 chacun) » — l'offset 0 est la fin
//! du programme audio, les 99 suivants les débuts de pistes, tous en secteurs
//! AVEC le pré-gap de 150, zéro pour une piste absente. Le condensat est
//! encodé en base64 où `+`, `/` et `=` deviennent `.`, `_` et `-`.

use base64::Engine;
use sha1::{Digest, Sha1};

use crate::toc::{PREGAP, Toc};

pub fn disc_id(toc: &Toc) -> String {
    let (derniere_audio, fin_audio) = toc.fin_audio();
    let mut texte = String::with_capacity(4 + 800);
    texte.push_str(&format!("{:02X}{:02X}", toc.premiere, derniere_audio));
    texte.push_str(&format!("{:08X}", fin_audio + PREGAP));
    for numero in 1..=99u8 {
        let offset = toc
            .piste(numero)
            .filter(|p| p.audio && numero >= toc.premiere && numero <= derniere_audio)
            .map(|p| p.debut + PREGAP)
            .unwrap_or(0);
        texte.push_str(&format!("{offset:08X}"));
    }
    let condensat = Sha1::digest(texte.as_bytes());
    base64::engine::general_purpose::STANDARD
        .encode(condensat)
        .chars()
        .map(|c| match c {
            '+' => '.',
            '/' => '_',
            '=' => '-',
            c => c,
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::toc::PisteToc;

    /// Le vecteur de libdiscid (`test/test_put.c`), publié par MusicBrainz :
    /// 10 pistes, fin à l'offset 206 535. Ce disque existe dans la base
    /// (`/ws/2/discid/Wn8eRBtfLDfM0qjYPdxrz.Zjs_U-`, consulté le 24/09/2026).
    pub(crate) const OFFSETS: [u32; 10] = [
        150, 18_901, 39_738, 59_557, 79_152, 100_126, 124_833, 147_278, 166_336, 182_560,
    ];
    pub(crate) const FIN: u32 = 206_535;
    pub(crate) const ATTENDU: &str = "Wn8eRBtfLDfM0qjYPdxrz.Zjs_U-";

    pub(crate) fn toc_du_vecteur() -> Toc {
        let pistes = OFFSETS
            .iter()
            .enumerate()
            .map(|(i, o)| PisteToc {
                numero: i as u8 + 1,
                debut: o - PREGAP,
                audio: true,
            })
            .collect();
        Toc::nouvelle(pistes, FIN - PREGAP).unwrap()
    }

    /// Témoin 2 — l'identifiant est égal au vecteur publié.
    #[test]
    fn l_identifiant_est_celui_du_vecteur_publie_par_musicbrainz() {
        assert_eq!(disc_id(&toc_du_vecteur()), ATTENDU);
    }

    #[test]
    fn une_piste_de_donnees_finale_est_exclue_du_calcul() {
        // Même programme audio, suivi d'une piste de données : la fin audio
        // est le début des données moins 11 400, l'identifiant ne change pas.
        let mut toc = toc_du_vecteur();
        let debut_donnees = FIN - PREGAP + crate::toc::ECART_SESSION_DONNEES;
        toc.pistes.push(PisteToc {
            numero: 11,
            debut: debut_donnees,
            audio: false,
        });
        toc.derniere = 11;
        toc.fin = debut_donnees + 30_000;
        assert_eq!(disc_id(&toc), ATTENDU);
    }
}
