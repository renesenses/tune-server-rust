//! #4894 — ce qu'on sait du LPCM (WAV) d'une sortie réseau qui n'est PAS un
//! `DlnaOutput`, donc sans Sink `GetProtocolInfo` à sonder.
//!
//! Avant ce module, `dlna_accepte_lpcm` répondait « non » pour toute sortie
//! connue qui n'était pas un `DlnaOutput` : c'était le repli d'un `downcast`
//! raté, pas une mesure. Un Chromecast ou un lecteur BluOS, qui lisent le WAV,
//! étaient déclarés `network_renderer_no_lpcm` et privés du WAV progressif.
//!
//! La réponse est désormais donnée PAR TYPE de sortie, et chaque « accepte »
//! ou « refuse » cite sa source. Ce qui n'est pas établi reste
//! [`CapaciteLpcm::Inconnu`], qui vaut NON pour changer le format servi —
//! exactement comme une sonde DLNA inconcluante.
//!
//! | type | réponse | source |
//! |---|---|---|
//! | `chromecast` | accepte en 16 bits, inconnu au-delà | Google Cast, « Supported Media » : « WAV (LPCM) », sans profondeur ni fréquence (le FLAC, lui, est borné à 96 kHz / 24 bits) |
//! | `bluos` | accepte en 16 bits, inconnu au-delà | Bluesound, « What Formats are supported by Bluesound? » : WAV listé, aucune profondeur ni fréquence |
//! | `slimproto` | refuse | `slimproto::build_strm_start` annonce TOUJOURS `f` (FLAC) au lecteur : un WAV y serait décodé comme du FLAC |
//! | `squeezebox` | inconnu | LMS tire l'URL (`playlist play`) et décide seul ; rien dans Tune ne l'établit |
//! | `openhome` | inconnu | `OpenHomeOutput` ne sonde aucun Sink ; le protocole n'impose aucun format |
//! | `dlna` sans `DlnaOutput` | inconnu | pas de Sink à lire (cas d'une sortie de substitution) |
//! | tout autre type | inconnu | hors de `is_network_output_type` : ni le WAV progressif ni le statut du crossfeed ne les interrogent |

/// Ce qu'on sait du LPCM d'un type de sortie, à une profondeur donnée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapaciteLpcm {
    /// Établi par le code ou par la documentation du protocole.
    Accepte,
    /// Établi par le code : servir du LPCM casserait la lecture.
    Refuse,
    /// Rien ne l'établit. Prudent : vaut non pour changer le format servi.
    Inconnu,
}

impl CapaciteLpcm {
    /// Seul « accepte » autorise à servir du WAV à la place du format habituel.
    pub fn autorise_le_wav(self) -> bool {
        self == CapaciteLpcm::Accepte
    }
}

/// La capacité LPCM d'un type de sortie sans Sink à sonder. `hi_res` = plus
/// de 16 bits, comme pour `dlna_accepte_lpcm`.
pub fn capacite_lpcm(output_type: &str, hi_res: bool) -> CapaciteLpcm {
    match output_type {
        // Documentation officielle : WAV (LPCM) lu, profondeur non dite.
        "chromecast" | "bluos" if !hi_res => CapaciteLpcm::Accepte,
        "chromecast" | "bluos" => CapaciteLpcm::Inconnu,
        // `build_strm_start` écrit `b'f'` en dur : le lecteur attend du FLAC.
        "slimproto" => CapaciteLpcm::Refuse,
        _ => CapaciteLpcm::Inconnu,
    }
}

/// Le repli de `dlna_accepte_lpcm` quand aucune sonde n'est possible.
///
/// - `None` : la sortie est ABSENTE du registre. La réponse reste `true`,
///   inchangée : c'est la convention de `dlna_supports_mime` (une sortie
///   absente n'est pas là pour dire le contraire), et les témoins de décision
///   (`temoin_decision_locale.rs`, LAT-F1 de `tests.rs`) la fixent. Sans type
///   connu, ce module n'a rien de mieux à dire.
/// - `Some(type)` : la sortie est connue mais n'est pas un `DlnaOutput` — la
///   réponse vient de [`capacite_lpcm`], plus d'un `false` par défaut.
pub fn repli_sans_sonde(output_type: Option<&str>, hi_res: bool) -> bool {
    match output_type {
        None => true,
        Some(t) => capacite_lpcm(t, hi_res).autorise_le_wav(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chromecast_et_bluos_acceptent_le_16_bits_et_restent_inconnus_au_dela() {
        for t in ["chromecast", "bluos"] {
            assert_eq!(capacite_lpcm(t, false), CapaciteLpcm::Accepte, "{t}");
            assert_eq!(capacite_lpcm(t, true), CapaciteLpcm::Inconnu, "{t}");
        }
    }

    #[test]
    fn slimproto_refuse_et_les_autres_restent_inconnus() {
        assert_eq!(capacite_lpcm("slimproto", false), CapaciteLpcm::Refuse);
        for t in [
            "squeezebox",
            "openhome",
            "dlna",
            "airplay",
            "hqplayer",
            "oaat",
        ] {
            assert_eq!(capacite_lpcm(t, false), CapaciteLpcm::Inconnu, "{t}");
        }
    }

    #[test]
    fn inconnu_et_refuse_valent_non_la_sortie_absente_reste_oui() {
        assert!(!CapaciteLpcm::Inconnu.autorise_le_wav());
        assert!(!CapaciteLpcm::Refuse.autorise_le_wav());
        assert!(repli_sans_sonde(None, true));
        assert!(!repli_sans_sonde(Some("openhome"), false));
    }
}
