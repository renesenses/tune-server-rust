//! #4357 — le **masque de canaux** (`dwChannelMask`) de l'ouverture WASAPI
//! exclusive, et sa négociation.
//!
//! # Ce que ce module répare
//!
//! Un FLAC multicanal vers un ampli AV en HDMI ne jouait pas en exclusif :
//! `aucun format PCM accepté pour 6ch 96000Hz — 4 essais refusés` (puis
//! `8ch 48000Hz` pour un 7.1), chaque profondeur rendant
//! `0x88890008` (`AUDCLNT_E_UNSUPPORTED_FORMAT`). La négociation de #3837 ne
//! fait varier que la profondeur ; le `WAVEFORMATEXTENSIBLE` portait, lui, un
//! masque **calculé** : `(1 << canaux) - 1`. Ce calcul n'est le bon masque que
//! par accident :
//!
//! | canaux | `(1 << n) - 1`                 | ce que porte la source (FLAC, WAV)       |
//! |--------|--------------------------------|------------------------------------------|
//! | 4      | `0x0F` FL FR FC LFE (3.1)      | `0x33` FL FR BL BR (quadriphonie)        |
//! | 5      | `0x1F` FL FR FC LFE BL         | `0x37` FL FR FC BL BR                    |
//! | 6      | `0x3F` 5.1                     | `0x3F` 5.1 — juste, mais un seul essai   |
//! | 7      | `0x7F` … BL BR FLC             | `0x70F` FL FR FC LFE BC SL SR (6.1)      |
//! | 8      | `0xFF` « 7.1 large » (FLC FRC) | `0x63F` FL FR FC LFE BL BR SL SR (7.1)   |
//!
//! `0xFF` est l'ancien `KSAUDIO_SPEAKER_7POINT1`, que Windows déclare obsolète
//! et que les pilotes HDMI n'exposent pas : un 7.1 était refusé **par
//! construction**, quelle que soit la profondeur. Et un pilote qui l'aurait
//! accepté aurait envoyé les voies latérales aux haut-parleurs avant-centre.
//!
//! # Ce que ce module fait
//!
//! Pour chaque nombre de canaux, une **courte liste de masques** : d'abord
//! celui de l'ordre des voies que le décodeur produit (l'ordre FLAC, qui est
//! celui des bits du masque Windows), puis, quand il existe, son équivalent
//! **à positions identiques** — le 5.1 « arrière » (`0x3F`) et le 5.1
//! « latéral » (`0x60F`) rangent les six voies dans le même ordre ; c'est
//! cette seconde forme que beaucoup de pilotes HDMI annoncent. Aucun masque
//! qui changerait la place d'une voie n'est proposé : on ne remixe pas, on ne
//! déplace pas une voie, on ne rééchantillonne pas.
//!
//! La stéréo n'a qu'un masque (`0x3`) : son ouverture est strictement celle
//! d'avant, une sonde par profondeur.
//!
//! # Compilation, et ce que la CI juge vraiment
//!
//! Comme `negociation_format_exclusif_3837` : sans FFI ni `cfg` de plateforme.
//! La table et l'ordre des essais sont jugés par `cargo test` sur Linux ; ce
//! que répond un vrai pilote HDMI ne l'est que sur Windows.

use super::negociation_format_exclusif_3837::{
    CandidatFormat, FormatNegocie, ResultatSonde, message_peripherique_occupe,
    negocier_format_exclusif,
};

// Les positions de haut-parleur de `ksmedia.h` (`SPEAKER_*`).
const FL: u32 = 0x1;
const FR: u32 = 0x2;
const FC: u32 = 0x4;
const LFE: u32 = 0x8;
const BL: u32 = 0x10;
const BR: u32 = 0x20;
const BC: u32 = 0x100;
const SL: u32 = 0x200;
const SR: u32 = 0x400;

/// `KSAUDIO_SPEAKER_STEREO`.
pub(crate) const MASQUE_STEREO: u32 = FL | FR;
/// `KSAUDIO_SPEAKER_5POINT1` : FL FR FC LFE BL BR.
pub(crate) const MASQUE_5_1: u32 = FL | FR | FC | LFE | BL | BR;
/// `KSAUDIO_SPEAKER_5POINT1_SURROUND` : FL FR FC LFE SL SR.
pub(crate) const MASQUE_5_1_LATERAL: u32 = FL | FR | FC | LFE | SL | SR;
/// `KSAUDIO_SPEAKER_7POINT1_SURROUND` : FL FR FC LFE BL BR SL SR.
pub(crate) const MASQUE_7_1: u32 = FL | FR | FC | LFE | BL | BR | SL | SR;

/// Les masques à présenter au pilote pour `canaux` voies, dans l'ordre.
///
/// Le premier est celui que porte la source décodée ; les suivants ne
/// diffèrent que par le **nom** des positions, jamais par leur ordre. La
/// liste n'est jamais vide.
pub(crate) fn masques_de_canaux(canaux: u32) -> Vec<u32> {
    match canaux {
        // `KSAUDIO_SPEAKER_MONO`, puis FL seul : ce que l'ancien calcul
        // présentait, gardé pour ne rien retirer à un pilote qui l'acceptait.
        1 => vec![FC, FL],
        2 => vec![MASQUE_STEREO],
        3 => vec![FL | FR | FC],
        // Quadriphonie arrière, puis latérale.
        4 => vec![FL | FR | BL | BR, FL | FR | SL | SR],
        5 => vec![FL | FR | FC | BL | BR, FL | FR | FC | SL | SR],
        6 => vec![MASQUE_5_1, MASQUE_5_1_LATERAL],
        // 6.1 : FL FR FC LFE BC SL SR, l'ordre FLAC à sept voies.
        7 => vec![FL | FR | FC | LFE | BC | SL | SR],
        8 => vec![MASQUE_7_1],
        // Au-delà, aucune disposition normalisée : les N premières positions,
        // comme avant, bornées aux 18 que `ksmedia.h` définit.
        n if n <= 18 => vec![(1u32 << n) - 1],
        // `KSAUDIO_SPEAKER_DIRECTOUT` : aucune position, voies brutes.
        _ => vec![0],
    }
}

/// Le format retenu, avec le masque qui l'a fait accepter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FormatEtMasque {
    pub(crate) negocie: FormatNegocie,
    pub(crate) masque: u32,
    /// Nombre de masques entièrement refusés avant celui-ci. `0` = le masque
    /// de la source est passé.
    pub(crate) masques_refuses: usize,
}

/// Déroule les masques de [`masques_de_canaux`] ; pour chacun, la
/// négociation de profondeur de #3837. Retient le premier couple accepté.
///
/// `sonde` est **un** `IsFormatSupported` pour un couple (profondeur, masque).
/// Un périphérique occupé (#3067) arrête tout, au premier masque. Avec un seul
/// masque (la stéréo), l'erreur est mot pour mot celle de #3837 ; avec
/// plusieurs, elle nomme chaque masque. Pour plus de deux voies, elle dit en
/// plus ce que l'utilisateur peut vérifier côté Windows.
pub(crate) fn negocier_format_et_masque<F>(
    bits_demandes: u32,
    canaux: u32,
    sample_rate: u32,
    mut sonde: F,
) -> Result<FormatEtMasque, String>
where
    F: FnMut(CandidatFormat, u32) -> ResultatSonde,
{
    let masques = masques_de_canaux(canaux);
    let mut refus: Vec<(u32, String)> = Vec::with_capacity(masques.len());
    for (index, masque) in masques.iter().copied().enumerate() {
        let mut occupe = false;
        let resultat = negocier_format_exclusif(bits_demandes, canaux, sample_rate, |candidat| {
            let reponse = sonde(candidat, masque);
            if let ResultatSonde::Refuse { hr, .. } = reponse {
                occupe |= message_peripherique_occupe(hr).is_some();
            }
            reponse
        });
        match resultat {
            Ok(negocie) => {
                return Ok(FormatEtMasque {
                    negocie,
                    masque,
                    masques_refuses: index,
                });
            }
            // Le refus « occupé » ne dépend pas du format : un autre masque
            // n'y changerait rien.
            Err(erreur) if occupe => return Err(erreur),
            Err(erreur) => refus.push((masque, erreur)),
        }
    }

    let liste = refus
        .iter()
        .map(|(masque, _)| format!("0x{masque:X}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut message = if refus.len() == 1 {
        refus.remove(0).1
    } else {
        format!(
            "{} masques de canaux refusés — {}",
            refus.len(),
            refus
                .iter()
                .map(|(masque, erreur)| format!("masque 0x{masque:X} : {erreur}"))
                .collect::<Vec<_>>()
                .join(" ; ")
        )
    };
    if canaux > 2 {
        message.push_str(&format!(
            ". Le pilote refuse {canaux} voies à {sample_rate} Hz en mode exclusif \
             (masque {liste}) : vérifiez que cette sortie est configurée en {canaux} \
             canaux dans Windows (Panneau de configuration Son > Lecture > Configurer), \
             ou décochez le mode exclusif pour passer par le mixeur de Windows"
        ));
    }
    Err(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::negociation_format_exclusif_3837::AUDCLNT_E_DEVICE_IN_USE;

    const REFUS: i32 = 0x88890008u32 as i32;

    /// Un pilote HDMI simulé : il n'accepte que les couples de sa liste, et
    /// note ce qu'on lui a présenté.
    struct PiloteHdmi {
        accepte: Vec<(CandidatFormat, u32)>,
        vus: Vec<(CandidatFormat, u32)>,
    }

    impl PiloteHdmi {
        fn qui_accepte(accepte: &[(CandidatFormat, u32)]) -> Self {
            Self {
                accepte: accepte.to_vec(),
                vus: Vec::new(),
            }
        }

        fn sonder(&mut self, candidat: CandidatFormat, masque: u32) -> ResultatSonde {
            self.vus.push((candidat, masque));
            if self.accepte.contains(&(candidat, masque)) {
                ResultatSonde::Accepte
            } else {
                ResultatSonde::Refuse {
                    hr: REFUS,
                    propose: None,
                }
            }
        }
    }

    /// Le nombre de bits à 1 d'un masque doit toujours égaler le nombre de
    /// voies : sinon `nChannels` et `dwChannelMask` se contredisent, et
    /// Windows refuse le format avant même le pilote.
    #[test]
    fn chaque_masque_porte_exactement_le_nombre_de_voies() {
        for canaux in 1..=18u32 {
            for masque in masques_de_canaux(canaux) {
                assert_eq!(
                    masque.count_ones(),
                    canaux,
                    "{canaux} voies, masque 0x{masque:X}"
                );
            }
        }
        assert_eq!(masques_de_canaux(32), vec![0], "au-delà de 18 : DIRECTOUT");
    }

    /// ⭐ Le témoin du 7.1 de #4357 : le masque présenté est le 7.1 de
    /// Windows (`0x63F`, voies latérales), et non plus l'ancien `0xFF` (« 7.1
    /// large ») que les pilotes HDMI n'exposent pas. Sabotage attendu —
    /// remettre `(1 << n) - 1` : ce test rougit sur `0xFF`.
    #[test]
    fn un_7_1_presente_le_masque_7_1_de_windows_et_non_plus_le_7_1_large() {
        assert_eq!(masques_de_canaux(8), vec![0x63F]);
        assert!(!masques_de_canaux(8).contains(&0xFF));
    }

    #[test]
    fn la_stereo_garde_son_unique_masque() {
        assert_eq!(masques_de_canaux(2), vec![0x3]);
    }

    /// Les masques suivent l'ordre des voies que le décodeur produit (l'ordre
    /// FLAC, qui est celui des bits du masque Windows).
    #[test]
    fn le_premier_masque_suit_l_ordre_des_voies_flac() {
        assert_eq!(masques_de_canaux(3)[0], 0x7);
        assert_eq!(masques_de_canaux(4)[0], 0x33);
        assert_eq!(masques_de_canaux(5)[0], 0x37);
        assert_eq!(masques_de_canaux(6)[0], 0x3F);
        assert_eq!(masques_de_canaux(7)[0], 0x70F);
        assert_eq!(masques_de_canaux(8)[0], 0x63F);
    }

    /// Une forme de repli ne change que le NOM des positions arrière ↔
    /// latérales : l'avant, le centre et le LFE restent à leur place.
    #[test]
    fn un_masque_de_repli_ne_deplace_ni_l_avant_ni_le_centre_ni_le_lfe() {
        let fixes = FL | FR | FC | LFE;
        for canaux in 2..=8u32 {
            let masques = masques_de_canaux(canaux);
            for autre in &masques[1..] {
                assert_eq!(
                    masques[0] & fixes,
                    autre & fixes,
                    "{canaux} voies : 0x{:X} → 0x{autre:X}",
                    masques[0]
                );
            }
        }
    }

    /// ⭐ Le 5.1 de #4357 : un pilote HDMI qui n'expose que le 5.1 latéral
    /// (`0x60F`) en conteneur 32/24. Avant ce module, les quatre profondeurs
    /// étaient refusées sous `0x3F` et la zone s'arrêtait. Sabotage attendu —
    /// ne garder qu'un masque par nombre de voies : la négociation rougit.
    #[test]
    fn un_pilote_qui_n_expose_que_le_5_1_lateral_joue_le_5_1() {
        let mut pilote =
            PiloteHdmi::qui_accepte(&[(CandidatFormat::nouveau(32, 24), MASQUE_5_1_LATERAL)]);
        let retenu = negocier_format_et_masque(32, 6, 96_000, |c, m| pilote.sonder(c, m))
            .expect("le 5.1 latéral doit être présenté");

        assert_eq!(retenu.masque, 0x60F);
        assert_eq!(retenu.masques_refuses, 1);
        assert_eq!(retenu.negocie.format, CandidatFormat::nouveau(32, 24));
        // Le masque de la source est essayé d'abord, à toutes les profondeurs.
        assert_eq!(
            &pilote.vus[..4],
            &[
                (CandidatFormat::plein(32), 0x3F),
                (CandidatFormat::nouveau(32, 24), 0x3F),
                (CandidatFormat::plein(24), 0x3F),
                (CandidatFormat::plein(16), 0x3F),
            ]
        );
    }

    #[test]
    fn un_pilote_qui_accepte_le_7_1_en_32_24_le_joue() {
        let mut pilote = PiloteHdmi::qui_accepte(&[(CandidatFormat::nouveau(32, 24), MASQUE_7_1)]);
        let retenu = negocier_format_et_masque(32, 8, 48_000, |c, m| pilote.sonder(c, m))
            .expect("7.1 accepté");
        assert_eq!(retenu.masque, 0x63F);
        assert_eq!(retenu.masques_refuses, 0);
        assert_eq!(retenu.negocie.refus_avant, 1);
    }

    /// La stéréo : exactement les sondes d'avant ce module.
    #[test]
    fn la_stereo_ne_coute_aucune_sonde_de_plus() {
        let mut pilote =
            PiloteHdmi::qui_accepte(&[(CandidatFormat::nouveau(32, 24), MASQUE_STEREO)]);
        let retenu = negocier_format_et_masque(32, 2, 44_100, |c, m| pilote.sonder(c, m))
            .expect("stéréo acceptée");
        assert_eq!(retenu.masque, 0x3);
        assert_eq!(
            pilote.vus,
            vec![
                (CandidatFormat::plein(32), 0x3),
                (CandidatFormat::nouveau(32, 24), 0x3)
            ]
        );
    }

    /// Un périphérique occupé arrête tout dès le premier masque (#3067).
    #[test]
    fn un_peripherique_occupe_n_essaie_pas_les_autres_masques() {
        let mut sondes = 0usize;
        let erreur = negocier_format_et_masque(32, 6, 96_000, |_, _| {
            sondes += 1;
            ResultatSonde::Refuse {
                hr: AUDCLNT_E_DEVICE_IN_USE,
                propose: None,
            }
        })
        .expect_err("occupé");
        assert_eq!(sondes, 1);
        assert!(erreur.contains("déjà tenu en mode exclusif"), "{erreur}");
    }

    /// Tout refusé en multicanal : chaque masque est nommé, et le message dit
    /// quoi vérifier côté Windows. En stéréo, le message reste mot pour mot
    /// celui de #3837.
    #[test]
    fn tout_refuser_en_multicanal_nomme_les_masques_et_la_piste_windows() {
        let mut pilote = PiloteHdmi::qui_accepte(&[]);
        let erreur = negocier_format_et_masque(32, 6, 96_000, |c, m| pilote.sonder(c, m))
            .expect_err("rien n'est accepté");
        assert_eq!(pilote.vus.len(), 8, "4 profondeurs × 2 masques");
        assert!(erreur.contains("2 masques de canaux refusés"), "{erreur}");
        assert!(
            erreur.contains("0x3F") && erreur.contains("0x60F"),
            "{erreur}"
        );
        assert!(erreur.contains("configurée en 6 canaux"), "{erreur}");
        assert!(erreur.contains("mode exclusif"), "{erreur}");

        let mut stereo = PiloteHdmi::qui_accepte(&[]);
        let erreur = negocier_format_et_masque(32, 2, 96_000, |c, m| stereo.sonder(c, m))
            .expect_err("rien n'est accepté");
        let mut seul = PiloteHdmi::qui_accepte(&[]);
        let attendu = negocier_format_exclusif(32, 2, 96_000, |c| seul.sonder(c, MASQUE_STEREO))
            .expect_err("rien n'est accepté");
        assert_eq!(erreur, attendu);
    }
}
