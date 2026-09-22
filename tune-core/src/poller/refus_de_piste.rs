//! Un refus qui ne vise QUE cette piste, ou une panne qui vise tout ?
//!
//! # Le fait, mesuré
//!
//! Alex Campbell (20/09/2026) puis Bertrand (21/09) : une playlist Qobuz de
//! **1454 pistes dont 186 (12 %) que le service annonce injouables** s'arrête
//! en cours de lecture. Le départ, lui, est déjà réparé — `premiere_piste_jouable`
//! (`routes/playback.rs`) enjambe les indisponibles pour choisir un meilleur
//! point de démarrage.
//!
//! Ce qui restait, c'est l'AVANCE. `avancer_avec_reprises` enjambe les pistes
//! qui échouent, mais s'arrête à `MAX_CONSECUTIVE_SKIPS = 25` — et le
//! commentaire de cette constante dit exactement ce qu'elle veut dire :
//!
//! > One or two dead tracks in an album is ordinary (a title pulled from the
//! > catalogue); a long run means something **systemic** — expired
//! > credentials, no network.
//!
//! L'intention est juste ; le code ne sait pas la tenir. Il compte de la même
//! façon une piste que Qobuz refuse individuellement — « no url », le service
//! a répondu, il a simplement dit non pour celle-ci — et une panne de jeton ou
//! de réseau. Sur une playlist dont l'indisponibilité est GROUPÉE (un label,
//! un album retiré d'un coup), vingt-six titres morts d'affilée épuisent un
//! budget prévu pour tout autre chose, et la lecture s'arrête.
//!
//! # La règle
//!
//! Un refus **propre à la piste** n'entame pas le budget des pannes
//! systémiques. Il a son propre plafond, beaucoup plus large, qui n'existe que
//! pour empêcher une boucle infinie en répétition intégrale.
//!
//! 🔴 La liste des motifs est **étroite et explicite**. Élargir au jugé
//! transformerait une panne réelle en série d'enjambées silencieuses — c'est
//! exactement ce que le plafond de 25 protège, et ce garde-fou doit survivre.

/// Les motifs par lesquels un service dit « pas celle-ci », piste par piste.
///
/// `no url` : Qobuz, `qobuz.rs` — `data["url"].as_str().ok_or("no url")`. Le
/// service a répondu, la piste n'a simplement pas d'adresse de flux pour ce
/// compte, cette région, cet abonnement.
const MOTIFS_PROPRES_A_LA_PISTE: [&str; 1] = ["no url"];

/// Ce refus ne vise-t-il QUE cette piste ?
///
/// Comparaison en minuscules et par inclusion : le motif remonte enveloppé
/// dans le message de l'orchestrateur, jamais nu.
pub fn refus_propre_a_la_piste(motif: &str) -> bool {
    let bas = motif.to_lowercase();
    MOTIFS_PROPRES_A_LA_PISTE.iter().any(|m| bas.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_refus_de_qobuz_pour_une_piste_est_reconnu() {
        // La forme exacte que remonte `qobuz.rs`, enveloppée par l'appelant.
        assert!(refus_propre_a_la_piste("no url"));
        assert!(refus_propre_a_la_piste(
            "streaming resolution failed: no url"
        ));
        assert!(refus_propre_a_la_piste("Qobuz: NO URL"));
    }

    #[test]
    fn une_panne_systemique_ne_l_est_PAS() {
        // 🔴 Le cœur du garde-fou : ces motifs-là doivent continuer d'épuiser
        // le budget de 25, sans quoi un jeton expiré ferait marteler le
        // service une fois par piste de la file.
        for panne in [
            "401 Unauthorized",
            "token expired",
            "error sending request for url (https://...): operation timed out",
            "tcp connect error",
            "503 Service Unavailable",
        ] {
            assert!(
                !refus_propre_a_la_piste(panne),
                "« {panne} » a été pris pour un refus de piste"
            );
        }
    }

    #[test]
    fn un_motif_vide_ou_inconnu_ne_conclut_rien() {
        assert!(!refus_propre_a_la_piste(""));
        assert!(!refus_propre_a_la_piste("quelque chose d'autre"));
    }
}
