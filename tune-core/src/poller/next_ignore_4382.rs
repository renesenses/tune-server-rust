//! #4382 — un `Next` acquitté, que le transport déclare lui-même IGNORÉ.
//!
//! ## Ce que le journal de Villerio établit (Eversolo DMP-A6, logiciel
//! 1.6.01, Tune 1.0.0-rc2, 05/10)
//!
//! Les échantillons de surveillance de #5239 tranchent la question laissée
//! ouverte le 27/09 (« a-t-il changé de piste sans le dire, ou n'a-t-il rien
//! fait ? ») :
//!
//! - à l'armement, 30 s avant la fin : `SetNextAVTransportURI` acquitté,
//!   `GetMediaInfo` rend NOTRE URL en `NextURI` et `GetCurrentTransportActions`
//!   déclare `Next` (`dlna_suivante_tenue`) ; le renderer tire le flux armé ;
//! - à la fin, il reste `PLAYING`, position épinglée sur sa durée, `TrackURI`
//!   sur la piste finie : il n'enchaîne pas seul ;
//! - Tune lui envoie `Next`, qu'il acquitte sans erreur SOAP ;
//! - à 1 s, 2 s et 3 s : position toujours épinglée, et `GetMediaInfo` rend
//!   ENCORE la piste finie en `CurrentURI` et ENCORE notre suivante en
//!   `NextURI`. Le transport n'a rien fait : il le dit lui-même.
//!
//! La surveillance attendait pourtant ses trois secondes avant de relancer,
//! et recommençait à chaque piste : trois secondes de blanc payées pour
//! rien, à chaque transition.
//!
//! ## Les deux décisions
//!
//! 1. Au premier sondage de la fenêtre où le transport déclare la suivante
//!    toujours EN ATTENTE (pas devenue courante, toujours tenue en suivante)
//!    et où la position n'a pas bougé, le `Next` est ignoré : relance
//!    immédiate, sans attendre le délai.
//! 2. L'appareil qui l'a fait une fois n'en reçoit plus : à la fin suivante,
//!    Tune relance aussitôt par `SetAVTransportURI` + `Play`, comme pour un
//!    renderer qui ne tient pas de suivante. La mémoire vit le temps du
//!    processus : un logiciel d'appareil mis à jour retrouve sa chance au
//!    prochain démarrage.

use super::decisions::EnchainementArme;

/// Écart de position (ms) au-delà duquel le renderer a bougé : celui de
/// `decisions::suite_de_l_adoption`.
const MOUVEMENT_MINIMAL_MS: u64 = 1000;

/// Vrai quand, après un `Next` acquitté (`preuve == Bascule`), le transport
/// déclare lui-même ne pas l'avoir exécuté.
///
/// Trois constats, tous nécessaires :
/// - `NextURI` nomme ENCORE le flux adopté : la suivante est toujours en
///   attente — un `Next` exécuté l'aurait fait passer en `CurrentURI` ;
/// - `CurrentURI` ne le nomme pas ;
/// - la position n'a pas quitté sa valeur gelée.
///
/// Un transport muet (`None`), une autre preuve qu'une bascule commandée, un
/// flux adopté vide : faux, et la surveillance garde son délai.
pub(super) fn next_ignore_par_le_transport(
    preuve: EnchainementArme,
    media_courante: Option<&str>,
    media_suivante: Option<&str>,
    flux_adopte: &str,
    position_ms: u64,
    position_figee_ms: u64,
) -> bool {
    let nomme = |uri: Option<&str>| {
        !flux_adopte.is_empty() && uri.is_some_and(|u| u.trim().contains(flux_adopte))
    };
    preuve == EnchainementArme::Bascule
        && nomme(media_suivante)
        && !nomme(media_courante)
        && position_ms.abs_diff(position_figee_ms) < MOUVEMENT_MINIMAL_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLUX: &str = "e77cba48-0b81-4a13-9f66-3892732d0d7a";
    const FINIE: &str = "http://tune.local:8888/stream/4a810318-6db0-46a0-92c8-7b5887fca043.wav";
    const ARMEE: &str = "http://tune.local:8888/stream/e77cba48-0b81-4a13-9f66-3892732d0d7a.wav";

    #[test]
    fn la_signature_du_dmp_a6_le_05_10() {
        // 18:30:04.499 : 1 s après le `Next`, rien n'a bougé.
        assert!(next_ignore_par_le_transport(
            EnchainementArme::Bascule,
            Some(FINIE),
            Some(ARMEE),
            FLUX,
            237_000,
            237_000,
        ));
    }

    #[test]
    fn un_next_execute_n_est_pas_ignore() {
        // La suivante est devenue courante.
        assert!(!next_ignore_par_le_transport(
            EnchainementArme::Bascule,
            Some(ARMEE),
            None,
            FLUX,
            237_000,
            237_000,
        ));
        // Le transport a avancé mais garde un écho de NextURI.
        assert!(!next_ignore_par_le_transport(
            EnchainementArme::Bascule,
            Some(ARMEE),
            Some(ARMEE),
            FLUX,
            237_000,
            237_000,
        ));
        // La position repart : signe de vie, la surveillance décide.
        assert!(!next_ignore_par_le_transport(
            EnchainementArme::Bascule,
            Some(FINIE),
            Some(ARMEE),
            FLUX,
            1_000,
            237_000,
        ));
    }

    #[test]
    fn sans_preuve_du_transport_on_garde_le_delai() {
        for (courante, suivante) in [(None, None), (Some(FINIE), None), (None, Some(FINIE))] {
            assert!(!next_ignore_par_le_transport(
                EnchainementArme::Bascule,
                courante,
                suivante,
                FLUX,
                237_000,
                237_000,
            ));
        }
        assert!(!next_ignore_par_le_transport(
            EnchainementArme::Bascule,
            Some(FINIE),
            Some(ARMEE),
            "",
            237_000,
            237_000,
        ));
    }

    #[test]
    fn seule_une_bascule_commandee_est_concernee() {
        for preuve in [
            EnchainementArme::Certain,
            EnchainementArme::Probable,
            EnchainementArme::Aucun,
        ] {
            assert!(!next_ignore_par_le_transport(
                preuve,
                Some(FINIE),
                Some(ARMEE),
                FLUX,
                237_000,
                237_000,
            ));
        }
    }
}
