//! Temporisation après une erreur d'écoute réseau, pour les boucles qui ne
//! peuvent rien faire d'autre que réessayer.
//!
//! # Pourquoi (#2156)
//!
//! `loop { match listener.accept().await { Ok(..) => .., Err(e) => warn!(..) } }`
//! n'est pas un défaut visible tant que l'erreur est passagère. Il le devient
//! dès qu'elle est PERSISTANTE. `EMFILE` / `ENFILE` — la table de descripteurs
//! du processus est pleine — fait rendre la main à `accept()` **immédiatement**,
//! et à chaque tour, tant que la condition dure. Le tour de boucle ne coûte
//! alors qu'un appel système et le formatage d'une ligne : un cœur saturé, et
//! un journal qui grossit à la vitesse à laquelle le disque accepte d'écrire.
//!
//! C'est exactement la signature relevée par Levente Toth (#2156), et rien
//! dans ce relevé ne colle à un travail de fond :
//!
//! | observé | ce que ça dit |
//! |---|---|
//! | 21,7 Mio/s **écrits**, colonne « Disk Read » vide | on écrit sans lire : ni balayage de bibliothèque, ni copie de fichier |
//! | 25,9 % de CPU sur quatre cœurs | **un** cœur exactement : une boucle serrée mono-tâche, pas une passe multi-fils |
//! | un redémarrage suffit, et ça ne revient pas | l'état fautif est la table de descripteurs, que le redémarrage vide |
//!
//! Le plafond du journal posé par #2159 borne la TAILLE du fichier, pas le
//! DÉBIT : sous 21,7 Mio/s, il fait tourner le journal deux fois par seconde et
//! le disque encaisse exactement la même usure. Borner la source était le
//! travail restant.
//!
//! # Ce n'est pas un motif neuf dans ce dépôt
//!
//! Il a déjà été posé deux fois, à la main :
//!
//! * `slimproto::cli_server` (500 ms) — son commentaire nomme `EMFILE`/`ENFILE`
//!   et « busy-spin a core and flood the log » ;
//! * `discovery::ssdp` (200 ms) — « Transient errors shouldn't spin the loop hot ».
//!
//! Trois boucles ne l'avaient jamais reçu. Plutôt qu'une troisième et une
//! quatrième recopie, la temporisation vit ici, en un seul endroit, appelée par
//! toutes celles qui en relèvent.

use std::time::Duration;

/// Le délai laissé au système avant de retenter une écoute qui vient d'échouer.
///
/// 500 ms est la valeur déjà éprouvée par `slimproto::cli_server`. Elle ramène
/// une erreur persistante de ~10⁵ tours par seconde à deux, et une erreur
/// réellement passagère ne coûte qu'un demi-délai avant que le client suivant
/// soit accepté.
pub const TEMPO_APRES_ERREUR_RESEAU: Duration = Duration::from_millis(500);

/// Cède le processeur après une erreur d'écoute réseau, avant de réessayer.
///
/// À appeler dans le bras `Err` de toute boucle d'écoute (`accept`,
/// `recv_from`) qui ne sait rien faire d'autre que retenter : c'est la seule
/// chose qui distingue « on réessaie » de « on brûle un cœur ».
pub async fn temporiser_apres_erreur_reseau() {
    tokio::time::sleep(TEMPO_APRES_ERREUR_RESEAU).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La temporisation CÈDE réellement la main.
    ///
    /// Horloge en pause (`start_paused`) : le temps virtuel n'avance que
    /// lorsqu'une tâche dort pour de bon. Si le `sleep` disparaît du corps de
    /// la fonction, l'écart mesuré reste nul et ce test tombe — il ne relit pas
    /// le texte du code, il mesure son effet.
    #[tokio::test(start_paused = true)]
    async fn la_temporisation_cede_reellement_la_main() {
        let depart = tokio::time::Instant::now();
        temporiser_apres_erreur_reseau().await;
        assert!(
            depart.elapsed() >= TEMPO_APRES_ERREUR_RESEAU,
            "la temporisation n'a pas eu lieu : {:?} écoulées",
            depart.elapsed()
        );
    }

    /// Et le délai ne peut pas être vidé de sa substance sans rougir : une
    /// temporisation nulle est une boucle serrée qui porte un autre nom.
    #[test]
    fn le_delai_n_est_pas_nul() {
        assert!(
            TEMPO_APRES_ERREUR_RESEAU >= Duration::from_millis(100),
            "un délai de {TEMPO_APRES_ERREUR_RESEAU:?} ne borne plus rien (#2156)"
        );
    }

    /// Le marqueur de journal `marqueur` est-il suivi, dans les 900 caractères
    /// qui viennent, d'un appel à la temporisation ?
    ///
    /// Rend `false` AUSSI quand le marqueur a disparu : renommer le marqueur
    /// sans rien dire fait tomber la garde, ce qui est le comportement voulu —
    /// on veut être forcé de revenir ici.
    fn temporise_apres(source: &str, marqueur: &str) -> bool {
        let Some(debut) = source.find(marqueur) else {
            return false;
        };
        source[debut..]
            .chars()
            .take(900)
            .collect::<String>()
            .contains("temporiser_apres_erreur_reseau")
    }

    /// Garde de CÂBLAGE (#2156). Une temporisation qui existe et que personne
    /// n'appelle ne borne rien : c'est la forme exacte du piège « écrit mais
    /// pas branché ». Les quatre boucles d'écoute qui ne savent que réessayer
    /// doivent l'appeler dans leur bras d'erreur.
    ///
    /// `discovery::ssdp` n'est volontairement PAS dans cette liste : sa
    /// temporisation lui est propre (200 ms, posée pour une autre raison) et la
    /// rallonger sans mesure ralentirait la découverte des appareils.
    #[test]
    fn les_boucles_d_ecoute_temporisent_sur_erreur() {
        for (fichier, source, marqueur) in [
            (
                "tune-core/src/slimproto/mod.rs",
                include_str!("slimproto/mod.rs"),
                "\"slimproto_accept_error\"",
            ),
            (
                "tune-core/src/slimproto/cli_server.rs",
                include_str!("slimproto/cli_server.rs"),
                "\"lms_cli_accept_error\"",
            ),
            (
                "tune-core/src/slimproto/discovery.rs",
                include_str!("slimproto/discovery.rs"),
                "\"slimproto_discovery_recv_error\"",
            ),
            (
                "tune-core/src/outputs/oh_events.rs",
                include_str!("outputs/oh_events.rs"),
                "\"oh_event_accept_error\"",
            ),
        ] {
            assert!(
                temporise_apres(source, marqueur),
                "{fichier} : le bras d'erreur {marqueur} ne temporise plus — \
                 une erreur persistante (EMFILE) y brûlerait un cœur et \
                 remplirait le disque (#2156)"
            );
        }
    }
}
