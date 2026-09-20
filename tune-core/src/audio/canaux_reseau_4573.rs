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

/// Combien de canaux SERVIR à cette sortie, ou `None` pour « n'y touche pas ».
///
/// C'est la porte unique du branchement, et elle porte ses trois gardes :
///
/// - `is_network_output` — la sortie LOCALE replie déjà, et le dit. Le journal
///   du 20/09 le montre sur la même piste, la même machine et à la même
///   minute : `local_audio_stream_config … input_ch=6 output_ch=2` d'un côté,
///   `dlna_set_uri_ok … advertised_mime=audio/flac` sur le FLAC 5.1 intact de
///   l'autre. Le défaut est sur le SEUL chemin réseau.
/// - `dsd_passthrough` — replier un DSD exigerait de le décoder, donc de
///   casser un passthrough que le renderer a lui-même annoncé. Le DSD
///   multicanal reste servi tel quel : c'est un autre dossier.
/// - `canaux_renderer` — `None` ne réduit rien. Voir le module.
///
/// Et deux bornes que la décision de Bertrand pose autour de la cible :
///
/// - la source doit être MULTICANALE (`> 2`) — sans quoi une sonde SOAP
///   partirait sur chaque piste d'une bibliothèque stéréo, pour rien ;
/// - le plancher est la STÉRÉO. « Réduire en stéréo, jamais refuser » : un
///   renderer qui n'annoncerait que `channels=1` ne fait pas taire une voie
///   sur deux. C'est aussi ce que dit déjà
///   [`super::channels::fold_stereo_to_mono_in_place`] — « le serveur ne
///   demande jamais une cible mono à un DAC qui annonce deux canaux ».
///
/// Rendre `Some(n)` engage le décodage : l'appelant doit AUSSI désarmer le
/// passthrough, sans quoi la décision serait prise et jamais empruntée.
pub fn canaux_a_servir(
    is_network_output: bool,
    dsd_passthrough: bool,
    canaux_source: u16,
    canaux_renderer: Option<u16>,
) -> Option<u16> {
    if !is_network_output || dsd_passthrough || canaux_source <= 2 {
        return None;
    }
    if !reduction_de_canaux_requise(canaux_source, canaux_renderer) {
        return None;
    }
    // `max(2)` : le plancher stéréo. `min` avec la source au cas où la borne
    // dépasserait ce qu'il y a à replier — un 3 canaux vers un lecteur mono
    // resterait alors un 3 canaux, et la règle ne s'arme pas pour rien.
    let cible = canaux_renderer?.max(2);
    (cible < canaux_source).then_some(cible)
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

    /// Le cas de Xavier, vu depuis la porte du branchement.
    #[test]
    fn une_zone_reseau_qui_declare_deux_canaux_fait_servir_deux_canaux() {
        assert_eq!(canaux_a_servir(true, false, 6, Some(2)), Some(2));
    }

    /// 🔴 Les trois gardes, une par une. Chacune seule suffit à ne rien faire.
    #[test]
    fn chaque_garde_seule_laisse_la_piste_intacte() {
        // La sortie locale replie déjà, et le dit.
        assert_eq!(canaux_a_servir(false, false, 6, Some(2)), None);
        // Un passthrough DSD annoncé par le renderer ne se décode pas.
        assert_eq!(canaux_a_servir(true, true, 6, Some(2)), None);
        // Le renderer n'a rien déclaré : ignorance n'est pas déclaration.
        assert_eq!(canaux_a_servir(true, false, 6, None), None);
        // Le scan n'a pas lu les canaux de la piste : rien à comparer.
        assert_eq!(canaux_a_servir(true, false, 0, Some(2)), None);
    }

    /// 🔴 Le plancher stéréo : un lecteur qui n'annonce QU'UN canal ne fait
    /// pas taire une voie sur deux. « Réduire en stéréo, jamais refuser. »
    #[test]
    fn un_lecteur_qui_n_annonce_qu_un_canal_ne_descend_jamais_sous_la_stereo() {
        assert_eq!(canaux_a_servir(true, false, 2, Some(1)), None);
        assert_eq!(
            canaux_a_servir(true, false, 6, Some(1)),
            Some(2),
            "un 5.1 vers un lecteur mono part en STÉRÉO, pas en mono"
        );
    }

    /// La stéréo ordinaire — l'immense majorité des lectures — ne change pas
    /// de chemin : `None`, donc aucun transcodage forcé.
    #[test]
    fn une_stereo_vers_un_lecteur_stereo_ne_change_rien() {
        assert_eq!(canaux_a_servir(true, false, 2, Some(2)), None);
        assert_eq!(canaux_a_servir(true, false, 2, Some(6)), None);
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
