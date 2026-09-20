//! #4573 — ce qu'on fait d'une piste MULTICANALE vers un lecteur réseau.
//!
//! # La mesure qui ouvre le dossier
//!
//! Xavier Joly, 20/09/2026, sur un **Denon AVR-X1600H** : le Sink DLNA annonce
//! du LPCM en `channels=1` et `channels=2` seulement, et `audio/flac:*` sans
//! aucun `channels=`. Le manuel confirme : FLAC, WAV, ALAC et DSD y sont lus
//! **en deux canaux**, jusqu'à 192 kHz. Un FLAC 5.1 n'ira donc jamais tel quel
//! sur cet ampli ; son vrai surround passe par HDMI.
//!
//! # 🔴 Une déclaration, jamais une supposition
//!
//! `canaux_renderer` vaut `None` quand le Sink ne porte aucun `channels=`.
//! C'est « on ne sait pas », pas « deux » : la règle rend alors `false` et on
//! ne touche à rien. Réduire une piste sur une ignorance, ce serait faire
//! taire quatre voies sur six chez quelqu'un qui n'a rien demandé — et la
//! plainte arriverait des semaines plus tard.
//!
//! C'est le même principe que le socle multicanal
//! ([`super::canaux_declares`]) : on DÉCLARE, on ne FORCE pas.

/// La piste doit-elle être réduite pour ce lecteur ?
///
/// `true` seulement quand les deux nombres sont connus ET que la source en a
/// plus que le lecteur. Le mélange lui-même reste celui qui existe déjà —
/// [`super::mixer::downmix`], coefficients ITU-R BS.775.
pub fn reduction_de_canaux_requise(canaux_source: u16, canaux_renderer: Option<u16>) -> bool {
    match canaux_renderer {
        Some(cible) if cible > 0 && canaux_source > cible => true,
        _ => false,
    }
}

/// Ce que le chemin du signal doit dire, ou `None` quand il n'y a rien à dire.
///
/// Le réglage qui ment est le défaut d'origine de ce dossier (cf. #3254) : une
/// réduction silencieuse serait un mensonge de plus. La sortie locale affiche
/// déjà « 6 → 2 canaux (mesuré) » ; le réseau doit le dire de la même façon.
pub fn etiquette_de_reduction(canaux_source: u16, canaux_renderer: Option<u16>) -> Option<String> {
    let cible = canaux_renderer?;
    reduction_de_canaux_requise(canaux_source, canaux_renderer)
        .then(|| format!("{canaux_source} → {cible} canaux (annoncés par le lecteur)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas de Xavier : 5.1 vers un Denon qui n'annonce que deux canaux.
    #[test]
    fn un_flac_51_vers_un_lecteur_stereo_doit_etre_reduit() {
        assert!(reduction_de_canaux_requise(6, Some(2)));
        assert_eq!(
            etiquette_de_reduction(6, Some(2)).as_deref(),
            Some("6 → 2 canaux (annoncés par le lecteur)")
        );
    }

    /// 🔴 La contre-épreuve qui compte : un lecteur qui n'annonce RIEN ne
    /// déclenche aucune réduction.
    #[test]
    fn un_lecteur_muet_sur_ses_canaux_ne_fait_rien_reduire() {
        assert!(!reduction_de_canaux_requise(6, None));
        assert_eq!(etiquette_de_reduction(6, None), None);
    }

    #[test]
    fn une_piste_qui_tient_dans_le_lecteur_passe_intacte() {
        assert!(!reduction_de_canaux_requise(2, Some(2)));
        assert!(!reduction_de_canaux_requise(2, Some(6)));
        assert!(!reduction_de_canaux_requise(6, Some(8)));
        assert_eq!(etiquette_de_reduction(2, Some(2)), None);
    }

    /// Un lecteur qui annonce zéro canal est une déclaration absurde : on la
    /// traite comme une ignorance, pas comme « réduis tout à rien ».
    #[test]
    fn zero_canal_annonce_vaut_une_ignorance() {
        assert!(!reduction_de_canaux_requise(6, Some(0)));
        assert_eq!(etiquette_de_reduction(6, Some(0)), None);
    }

    #[test]
    fn un_lecteur_71_reduit_bien_une_source_plus_large() {
        assert!(reduction_de_canaux_requise(8, Some(6)));
        assert_eq!(
            etiquette_de_reduction(8, Some(6)).as_deref(),
            Some("8 → 6 canaux (annoncés par le lecteur)")
        );
    }
}
