//! #3837 — la **négociation du format** d'ouverture WASAPI exclusive.
//!
//! # Ce que ce module répare
//!
//! `resolve_local.rs` transcode **toujours** la sortie locale en WAV 32 bits
//! (« Symphonia décode en `AudioBuffer<i32>`, le chemin cpal reconvertit en
//! `f32`, zéro perte »). Ce raisonnement ne vaut que pour le chemin *partagé*.
//! Sur le chemin **exclusif**, ce format est présenté **tel quel** au pilote :
//! `wasapi_exclusive.rs` construisait un unique `WAVEFORMATEXTENSIBLE` en
//! `wBitsPerSample = wValidBitsPerSample = 32`, appelait **un** seul
//! `IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, …)` et abandonnait au
//! premier refus.
//!
//! Beaucoup d'interfaces d'enregistrement (TASCAM, Focusrite, RME…) n'exposent
//! en exclusif que **24 bits valides**, dans un conteneur 24 ou 32. Pour
//! elles, le mode exclusif de Tune était **impossible par construction**,
//! quelle que soit la source — même un FLAC 16/44 arrivait en WAV 32 bits.
//! C'est le `0x88890008` (`AUDCLNT_E_UNSUPPORTED_FORMAT`) de la TASCAM US-366
//! de Didier, là où le SMSL SU-8 accepte `32/32`.
//!
//! # Ce que ce module fait, et ce qu'il ne fait pas
//!
//! Il déroule une **courte liste de profondeurs PCM** à la **même cadence** et
//! sur les **mêmes canaux** — jamais de rééchantillonnage, jamais de remixage,
//! jamais de repli vers le mode partagé : rien de ce que l'exclusif interdit.
//! Chaque essai est un `IsFormatSupported`, pas un `Initialize` : c'est
//! quelques microsecondes.
//!
//! La liste descend en précision, jamais elle ne l'invente :
//! `32/32` → `32/24` → `24/24` → `16/16`, tronquée à ce que la demande peut
//! porter. Un conteneur 32 bits à 24 bits valides est **bit-identique** à la
//! source 24 bits que Symphonia justifie déjà à gauche.
//!
//! # Pourquoi le conteneur suffit à faire jouer le fil de rendu
//!
//! `NativePcmRing::pop_pcm_bytes(out, bits)` sérialise les **octets hauts** du
//! mot `i32` aligné à gauche, pour `bits ∈ {16, 24, 32}`. Le seul chiffre dont
//! le fil de rendu a besoin est donc [`CandidatFormat::bits_conteneur`] ;
//! `bits_valides` ne sort jamais du `WAVEFORMATEXTENSIBLE` remis au pilote.
//!
//! # Compilation, et ce que la CI juge vraiment
//!
//! ⚠️ **Aucun job de CI n'exécute WASAPI.** Tout ce qui est derrière
//! `cfg(target_os = "windows")` est compilé et lancé par personne. Ce module
//! est donc **sans FFI et sans `cfg` de plateforme** : la logique
//! d'énumération et de sélection est jugée par `cargo test` sur Linux, comme
//! `wasapi_aligned_duration_100ns` (#2208) l'est déjà. Ce qui reste non
//! couvert — le vrai fil `IsFormatSupported`, la lecture du format proposé par
//! le pilote, le rendu en 24 bits sur un vrai périphérique — n'est jugé que par
//! une écoute sur Windows.

use std::fmt;

/// Un format PCM entier candidat à `IsFormatSupported(EXCLUSIVE)`.
///
/// Les deux chiffres du `WAVEFORMATEXTENSIBLE` que les pilotes distinguent, et
/// que l'ancien code confondait :
///
/// | champ Windows          | ici                |
/// |------------------------|--------------------|
/// | `wBitsPerSample`       | [`Self::bits_conteneur`] |
/// | `wValidBitsPerSample`  | [`Self::bits_valides`]   |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CandidatFormat {
    /// `wBitsPerSample` : la taille du **conteneur**, donc le nombre d'octets
    /// par échantillon réellement écrits dans le tampon d'endpoint — et le
    /// seul chiffre que `pop_pcm_bytes` doit connaître.
    pub(crate) bits_conteneur: u16,
    /// `wValidBitsPerSample` : les bits de poids fort réellement porteurs. Les
    /// bits bas restants sont nuls, le mot étant aligné à gauche.
    pub(crate) bits_valides: u16,
}

impl CandidatFormat {
    /// Un conteneur et un nombre de bits valides quelconques.
    pub(crate) const fn nouveau(bits_conteneur: u16, bits_valides: u16) -> Self {
        Self {
            bits_conteneur,
            bits_valides,
        }
    }

    /// Le conteneur entièrement occupé : `wBitsPerSample == wValidBitsPerSample`.
    pub(crate) const fn plein(bits: u16) -> Self {
        Self::nouveau(bits, bits)
    }

    /// Vrai si ce format peut être **rendu** : un conteneur que
    /// `NativePcmRing::pop_pcm_bytes` sait sérialiser, et des bits valides qui
    /// y tiennent. Un format irrecevable n'est jamais présenté à un pilote :
    /// même accepté, il ne produirait que du silence.
    pub(crate) const fn est_recevable(&self) -> bool {
        matches!(self.bits_conteneur, 16 | 24 | 32)
            && self.bits_valides > 0
            && self.bits_valides <= self.bits_conteneur
    }

    /// Les octets par échantillon que le fil de rendu écrira.
    pub(crate) const fn octets_par_echantillon(&self) -> u16 {
        self.bits_conteneur / 8
    }
}

impl fmt::Display for CandidatFormat {
    /// `32/24` — conteneur sur bits valides, la notation du relevé de #3837.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.bits_conteneur, self.bits_valides)
    }
}

/// Les replis, du plus fidèle au moins fidèle, tels que #3837 les prescrit.
///
/// `32/24` d'abord : c'est exactement ce que Symphonia produit (un `i32`
/// justifié à gauche portant 24 bits utiles), et le format que les pilotes
/// d'interfaces d'enregistrement exposent le plus souvent. `24/24` ensuite,
/// pour les pilotes qui refusent le conteneur large. `16/16` en dernier — le
/// seul repli réellement destructeur, et le seul que tout pilote accepte.
pub(crate) const REPLIS_EXCLUSIFS: [CandidatFormat; 3] = [
    CandidatFormat::nouveau(32, 24),
    CandidatFormat::plein(24),
    CandidatFormat::plein(16),
];

/// `AUDCLNT_E_DEVICE_IN_USE` : l'endpoint est déjà tenu en exclusif (#3067).
pub(crate) const AUDCLNT_E_DEVICE_IN_USE: i32 = 0x8889000Au32 as i32;

/// #3067 — le refus « périphérique occupé », dit en clair.
///
/// Relevé sur la .42 : avec le réglage Windows par défaut (« donner la
/// priorité aux applications en mode exclusif »), un flux PARTAGÉ — un onglet
/// de navigateur, un son système — n'empêche pas l'exclusif, il perd le
/// périphérique. `AUDCLNT_E_DEVICE_IN_USE` veut donc dire qu'un autre
/// programme tient déjà l'endpoint EN EXCLUSIF, ou que ce réglage est décoché.
/// Le message d'avant — un `HRESULT` nu, ou « aucun format PCM accepté » quand
/// le refus tombait dès `IsFormatSupported` — envoyait chercher du côté du
/// format ou du pilote.
///
/// `None` pour tout autre code : ceux-là restent rapportés tels quels.
pub(crate) fn message_peripherique_occupe(hr: i32) -> Option<String> {
    (hr == AUDCLNT_E_DEVICE_IN_USE).then(|| {
        format!(
            "le périphérique est déjà tenu en mode exclusif par une autre application \
             (0x{hr:08X}, AUDCLNT_E_DEVICE_IN_USE) — un autre lecteur (Audirvana, foobar2000…) \
             ou une lecture précédente qui ne l'a pas rendu. Fermez-la, puis relancez la lecture"
        )
    })
}

/// Nombre maximal de sondes, y compris les formats proposés par le pilote.
/// Une borne dure : un pilote qui proposerait en boucle ne fait pas boucler
/// l'ouverture d'une zone.
const SONDES_MAX: usize = 8;

/// Ce qu'un `IsFormatSupported` a répondu pour un candidat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultatSonde {
    /// `S_OK` ou `S_FALSE` : le pilote prend ce format en exclusif.
    Accepte,
    /// Refus, avec le `HRESULT` tel quel (`0x88890008` pour la TASCAM) et,
    /// s'il y en a un d'exploitable, le format que le pilote a proposé en
    /// retour. En mode exclusif Windows documente `*ppClosestMatch = NULL` ;
    /// tous les pilotes ne s'y tiennent pas, et celui qui répond nous épargne
    /// la liste.
    Refuse {
        hr: i32,
        propose: Option<CandidatFormat>,
    },
}

/// Le format retenu, et ce qu'il a fallu essayer pour l'obtenir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FormatNegocie {
    pub(crate) format: CandidatFormat,
    /// Nombre de candidats refusés AVANT celui-ci. `0` = le format demandé est
    /// passé du premier coup, comme sur le SMSL SU-8.
    pub(crate) refus_avant: usize,
}

impl FormatNegocie {
    /// Vrai si le format retenu n'est pas celui que la source demandait : la
    /// zone joue, mais pas à la profondeur transcodée. Ce que le journal doit
    /// dire, et ce que l'ancien code ne pouvait pas dire puisqu'il abandonnait.
    pub(crate) fn est_un_repli(&self) -> bool {
        self.refus_avant > 0
    }
}

/// La liste ordonnée des formats à présenter au pilote pour une demande de
/// `bits_demandes` bits.
///
/// Le format demandé passe **toujours en premier** : un périphérique qui
/// l'accepte (SMSL SU-8) ne voit strictement aucun changement de
/// comportement — une seule sonde, comme avant. Les replis suivent, filtrés à
/// ceux dont les bits valides tiennent dans la demande : on ne propose jamais
/// à un pilote plus de précision que la source n'en porte.
pub(crate) fn candidats_exclusifs(bits_demandes: u32) -> Vec<CandidatFormat> {
    let demande = u16::try_from(bits_demandes).unwrap_or(u16::MAX);
    let mut liste: Vec<CandidatFormat> = Vec::with_capacity(1 + REPLIS_EXCLUSIFS.len());
    if demande > 0 {
        // Le format demandé tel quel, recevable ou non : ne rien changer à ce
        // que l'ancien code présentait en premier.
        liste.push(CandidatFormat::plein(demande));
    }
    for repli in REPLIS_EXCLUSIFS {
        if repli.bits_valides <= demande && !liste.contains(&repli) {
            liste.push(repli);
        }
    }
    liste
}

/// Range le format proposé par le pilote **juste après** le candidat qui vient
/// d'être refusé (index `apres`), donc avant les replis génériques encore à
/// essayer. Rend `true` si la liste a bougé.
///
/// Trois cas, et un seul déplace quelque chose :
/// - format irrecevable → ignoré, le pilote ne fait pas écrire du silence ;
/// - format **déjà essayé** (position ≤ `apres`) → ignoré, un pilote qui se
///   répète ne fait pas boucler l'ouverture d'une zone ;
/// - format encore à essayer, prévu plus loin ou pas prévu du tout → **remonté
///   en tête du reste**, c'est le pilote qui sait ce qu'il accepte.
pub(crate) fn ranger_le_format_propose(
    candidats: &mut Vec<CandidatFormat>,
    apres: usize,
    propose: CandidatFormat,
) -> bool {
    if !propose.est_recevable() {
        return false;
    }
    let position = (apres + 1).min(candidats.len());
    match candidats.iter().position(|candidat| *candidat == propose) {
        Some(deja) if deja <= apres || deja == position => false,
        Some(deja) => {
            let candidat = candidats.remove(deja);
            candidats.insert(position, candidat);
            true
        }
        None => {
            candidats.insert(position, propose);
            true
        }
    }
}

/// Déroule la liste et retient le premier format accepté.
///
/// `sonde` est **un** `IsFormatSupported` : sur Windows la vraie FFI, dans les
/// témoins un pilote simulé. C'est toute la raison d'être de ce découpage —
/// aucun job de CI n'exécute WASAPI.
///
/// L'erreur nomme **chaque** essai et son `HRESULT`, pour que le testeur qui
/// lit le message sache que Tune a bien tout tenté.
pub(crate) fn negocier_format_exclusif<F>(
    bits_demandes: u32,
    channels: u32,
    sample_rate: u32,
    mut sonde: F,
) -> Result<FormatNegocie, String>
where
    F: FnMut(CandidatFormat) -> ResultatSonde,
{
    let mut candidats = candidats_exclusifs(bits_demandes);
    let mut refus: Vec<String> = Vec::with_capacity(candidats.len());
    let mut index = 0usize;

    while index < candidats.len() && index < SONDES_MAX {
        let candidat = candidats[index];
        match sonde(candidat) {
            ResultatSonde::Accepte => {
                return Ok(FormatNegocie {
                    format: candidat,
                    refus_avant: index,
                });
            }
            ResultatSonde::Refuse { hr, propose } => {
                // Un endpoint occupé refuse tous les formats pour la même
                // raison : inutile de descendre la liste, et surtout ne pas
                // conclure « aucun format accepté » (#3067).
                if let Some(occupe) = message_peripherique_occupe(hr) {
                    return Err(occupe);
                }
                refus.push(format!("{candidat} → 0x{hr:08X}"));
                if let Some(propose) = propose {
                    ranger_le_format_propose(&mut candidats, index, propose);
                }
            }
        }
        index += 1;
    }

    Err(format!(
        "WASAPI Exclusive: aucun format PCM accepté pour {channels}ch {sample_rate}Hz \
         — {} essais refusés ({})",
        refus.len(),
        refus.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `AUDCLNT_E_UNSUPPORTED_FORMAT` — le code que la TASCAM US-366 rend.
    const REFUS_TASCAM: i32 = 0x88890008u32 as i32;

    /// Un pilote simulé : il n'accepte QUE les formats de sa liste blanche, et
    /// note ce qu'on lui a présenté, dans l'ordre.
    struct PiloteSimule {
        accepte: Vec<CandidatFormat>,
        propose: Option<CandidatFormat>,
        vus: Vec<CandidatFormat>,
    }

    impl PiloteSimule {
        fn qui_accepte(accepte: &[CandidatFormat]) -> Self {
            Self {
                accepte: accepte.to_vec(),
                propose: None,
                vus: Vec::new(),
            }
        }

        fn et_qui_propose(mut self, propose: CandidatFormat) -> Self {
            self.propose = Some(propose);
            self
        }

        fn sonder(&mut self, candidat: CandidatFormat) -> ResultatSonde {
            self.vus.push(candidat);
            if self.accepte.contains(&candidat) {
                ResultatSonde::Accepte
            } else {
                ResultatSonde::Refuse {
                    hr: REFUS_TASCAM,
                    propose: self.propose,
                }
            }
        }
    }

    /// #3067 — `AUDCLNT_E_DEVICE_IN_USE` : l'endpoint est déjà tenu en
    /// exclusif. Ce n'est pas le format qui est refusé : descendre la liste des profondeurs n'y change rien, et le
    /// message « aucun format PCM accepté » envoyait chercher au mauvais
    /// endroit.
    #[test]
    fn un_peripherique_occupe_arrete_la_negociation_et_se_nomme() {
        let mut sondes = 0usize;
        let erreur = negocier_format_exclusif(32, 2, 44_100, |_| {
            sondes += 1;
            ResultatSonde::Refuse {
                hr: AUDCLNT_E_DEVICE_IN_USE,
                propose: None,
            }
        })
        .expect_err("un périphérique occupé ne s'ouvre pas");
        assert_eq!(
            sondes, 1,
            "le premier refus « occupé » suffit : les replis de format ne libèrent pas le périphérique"
        );
        assert!(
            erreur.contains("déjà tenu en mode exclusif"),
            "le message doit nommer la cause : {erreur}"
        );
        assert!(erreur.contains("0x8889000A"), "{erreur}");
    }

    #[test]
    fn la_liste_pour_une_demande_32_bits_descend_de_32_32_a_16_16() {
        assert_eq!(
            candidats_exclusifs(32),
            vec![
                CandidatFormat::plein(32),
                CandidatFormat::nouveau(32, 24),
                CandidatFormat::plein(24),
                CandidatFormat::plein(16),
            ]
        );
    }

    #[test]
    fn la_liste_ne_propose_jamais_plus_de_precision_que_la_demande() {
        // 16 bits demandés : aucun repli 24 valides, sinon on inventerait de
        // la précision que la source n'a pas.
        assert_eq!(candidats_exclusifs(16), vec![CandidatFormat::plein(16)]);
        // 24 bits demandés : le conteneur large est bit-identique, il reste.
        assert_eq!(
            candidats_exclusifs(24),
            vec![
                CandidatFormat::plein(24),
                CandidatFormat::nouveau(32, 24),
                CandidatFormat::plein(16),
            ]
        );
    }

    #[test]
    fn tout_candidat_produit_est_serialisable_par_pop_pcm_bytes() {
        for bits in [16u32, 24, 32] {
            for candidat in candidats_exclusifs(bits) {
                assert!(
                    candidat.est_recevable(),
                    "{candidat} ne serait pas sérialisé par pop_pcm_bytes"
                );
                assert!(matches!(candidat.octets_par_echantillon(), 2 | 3 | 4));
            }
        }
    }

    /// Le SMSL SU-8 de #3801 : il accepte `32/32`. Aucune sonde
    /// supplémentaire, aucun repli — le correctif ne change rien pour lui.
    #[test]
    fn un_pilote_qui_accepte_32_32_n_est_sonde_qu_une_fois() {
        let mut pilote = PiloteSimule::qui_accepte(&[CandidatFormat::plein(32)]);
        let negocie =
            negocier_format_exclusif(32, 2, 96_000, |c| pilote.sonder(c)).expect("32/32 accepté");

        assert_eq!(negocie.format, CandidatFormat::plein(32));
        assert_eq!(negocie.refus_avant, 0);
        assert!(!negocie.est_un_repli());
        assert_eq!(pilote.vus, vec![CandidatFormat::plein(32)]);
    }

    /// ⭐ Le témoin de #3837. La TASCAM US-366 : `32/32` refusé
    /// (`0x88890008`), `32/24` accepté. Sabotage attendu — vider
    /// [`REPLIS_EXCLUSIFS`] : la négociation rougit sur `0x88890008` sans
    /// jamais avoir présenté `32/24`.
    #[test]
    fn un_pilote_qui_refuse_32_32_et_accepte_32_24_retient_32_24() {
        let mut pilote = PiloteSimule::qui_accepte(&[CandidatFormat::nouveau(32, 24)]);
        let negocie = negocier_format_exclusif(32, 2, 96_000, |c| pilote.sonder(c))
            .expect("le repli 32/24 doit être présenté et retenu");

        assert_eq!(negocie.format, CandidatFormat::nouveau(32, 24));
        assert_eq!(negocie.refus_avant, 1);
        assert!(negocie.est_un_repli());
        // Le format demandé reste présenté EN PREMIER, le repli juste après.
        assert_eq!(
            pilote.vus,
            vec![CandidatFormat::plein(32), CandidatFormat::nouveau(32, 24)]
        );
        // Le fil de rendu écrira toujours 4 octets par échantillon : c'est le
        // conteneur, pas les bits valides, qui commande `pop_pcm_bytes`.
        assert_eq!(negocie.format.octets_par_echantillon(), 4);
    }

    /// L'autre profil d'interface d'enregistrement : conteneur 24 seulement.
    #[test]
    fn un_pilote_qui_n_accepte_que_24_24_est_atteint_au_troisieme_essai() {
        let mut pilote = PiloteSimule::qui_accepte(&[CandidatFormat::plein(24)]);
        let negocie =
            negocier_format_exclusif(32, 2, 96_000, |c| pilote.sonder(c)).expect("24/24 accepté");

        assert_eq!(negocie.format, CandidatFormat::plein(24));
        assert_eq!(negocie.refus_avant, 2);
        assert_eq!(negocie.format.octets_par_echantillon(), 3);
    }

    #[test]
    fn un_pilote_qui_n_accepte_que_16_16_est_atteint_en_dernier() {
        let mut pilote = PiloteSimule::qui_accepte(&[CandidatFormat::plein(16)]);
        let negocie =
            negocier_format_exclusif(32, 2, 44_100, |c| pilote.sonder(c)).expect("16/16 accepté");

        assert_eq!(negocie.format, CandidatFormat::plein(16));
        assert_eq!(negocie.refus_avant, 3);
        assert_eq!(negocie.format.octets_par_echantillon(), 2);
    }

    /// Le format que le pilote propose lui-même passe avant les replis
    /// génériques encore à essayer — et n'est pas re-présenté deux fois.
    #[test]
    fn le_format_propose_par_le_pilote_passe_avant_les_replis_generiques() {
        let mut pilote = PiloteSimule::qui_accepte(&[CandidatFormat::plein(24)])
            .et_qui_propose(CandidatFormat::plein(24));
        let negocie = negocier_format_exclusif(32, 2, 192_000, |c| pilote.sonder(c))
            .expect("le format proposé par le pilote doit être essayé");

        assert_eq!(negocie.format, CandidatFormat::plein(24));
        // 32/32 refusé, puis 24/24 (proposé) au lieu de 32/24.
        assert_eq!(
            pilote.vus,
            vec![CandidatFormat::plein(32), CandidatFormat::plein(24)]
        );
    }

    #[test]
    fn un_format_propose_irrecevable_est_ignore() {
        let mut candidats = candidats_exclusifs(32);
        // 20 bits : aucun conteneur que pop_pcm_bytes sache écrire.
        assert!(!ranger_le_format_propose(
            &mut candidats,
            0,
            CandidatFormat::plein(20)
        ));
        assert_eq!(candidats, candidats_exclusifs(32));
    }

    #[test]
    fn un_format_deja_essaye_n_est_pas_represente() {
        let mut candidats = candidats_exclusifs(32);
        // Le pilote propose ce qu'il vient de refuser : on ne le rejoue pas.
        assert!(!ranger_le_format_propose(
            &mut candidats,
            0,
            CandidatFormat::plein(32)
        ));
        assert_eq!(candidats, candidats_exclusifs(32));
    }

    #[test]
    fn un_repli_deja_prevu_est_remonte_en_tete_du_reste() {
        let mut candidats = candidats_exclusifs(32);
        assert!(ranger_le_format_propose(
            &mut candidats,
            0,
            CandidatFormat::plein(24)
        ));
        assert_eq!(
            candidats,
            vec![
                CandidatFormat::plein(32),
                CandidatFormat::plein(24),
                CandidatFormat::nouveau(32, 24),
                CandidatFormat::plein(16),
            ]
        );
    }

    #[test]
    fn un_pilote_qui_propose_sans_fin_ne_fait_pas_boucler_l_ouverture() {
        let mut sondes = 0usize;
        let erreur = negocier_format_exclusif(32, 2, 96_000, |_| {
            sondes += 1;
            // Un format neuf à chaque tour : 32/8, 32/9, 32/10…
            ResultatSonde::Refuse {
                hr: REFUS_TASCAM,
                propose: Some(CandidatFormat::nouveau(32, 8 + sondes as u16)),
            }
        })
        .expect_err("aucun format n'est accepté");

        assert!(sondes <= SONDES_MAX, "{sondes} sondes, borne {SONDES_MAX}");
        assert!(erreur.contains("WASAPI Exclusive"), "{erreur}");
    }

    /// Le conteneur négocié commande RÉELLEMENT la sérialisation : le mot
    /// `i32` aligné à gauche traverse `NativePcmRing::pop_pcm_bytes` au
    /// nombre d'octets du conteneur retenu, octets HAUTS d'abord.
    ///
    /// C'est le maillon que le fil WASAPI utilise (`self.bit_depth / 8` puis
    /// `pop_pcm_bytes(out, self.bit_depth)`) ; il est jugé ici, sur Linux,
    /// parce qu'aucun job de CI n'exécute WASAPI.
    #[test]
    #[cfg(feature = "local-audio")]
    fn le_conteneur_negocie_commande_la_serialisation_de_l_anneau() {
        use crate::outputs::local::NativePcmRing;

        // Un mot 24 bits justifié à gauche : 0xAABBCC dans les bits 31..8.
        const MOT: i32 = 0xAABBCC00u32 as i32;

        for (conteneur, attendu) in [
            (16u16, vec![0xBBu8, 0xAA]),
            (24, vec![0xCC, 0xBB, 0xAA]),
            (32, vec![0x00, 0xCC, 0xBB, 0xAA]),
        ] {
            let candidat = CandidatFormat::nouveau(conteneur, conteneur.min(24));
            assert!(candidat.est_recevable(), "{candidat}");
            let anneau = NativePcmRing::new(8);
            assert_eq!(anneau.push(&[MOT]), 1);
            let mut octets = vec![0u8; usize::from(candidat.octets_par_echantillon())];
            let ecrits = anneau.pop_pcm_bytes(&mut octets, candidat.bits_conteneur);

            assert_eq!(ecrits, usize::from(candidat.octets_par_echantillon()));
            assert_eq!(octets, attendu, "conteneur {conteneur}");
        }
    }

    /// Le message que le testeur lit quand rien ne passe : les quatre essais
    /// nommés, et le `HRESULT` de chacun.
    #[test]
    fn tout_refuser_rend_une_erreur_qui_nomme_les_quatre_essais() {
        let mut pilote = PiloteSimule::qui_accepte(&[]);
        let erreur = negocier_format_exclusif(32, 2, 96_000, |c| pilote.sonder(c))
            .expect_err("aucun format n'est accepté");

        assert!(erreur.contains("0x88890008"), "{erreur}");
        assert!(erreur.contains("4 essais refusés"), "{erreur}");
        for attendu in ["32/32", "32/24", "24/24", "16/16"] {
            assert!(erreur.contains(attendu), "{attendu} absent de : {erreur}");
        }
        assert_eq!(pilote.vus.len(), 4);
    }
}
