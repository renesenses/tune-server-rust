//! Forum (relevé rc14 du 21/11/2026) — deux zones pour un même appareil.
//!
//! Une zone pointe une sortie disparue ; la seule sortie vivante du même nom
//! est déjà celle d'une AUTRE zone. Le rebond de #1287 n'a pas lieu (#5464 :
//! deux zones ne partagent pas une sortie) et le serveur écrit une phrase qui
//! nomme l'autre zone.
//!
//! Cette phrase n'atteignait pas l'écran : le refus partait sous le code de
//! famille `zone_output_unavailable` SANS motif, et le client web remplace ce
//! code sans motif par son bandeau générique (« l'appareil de cette zone est
//! introuvable, il se reconnectera tout seul »), faux ici — l'appareil est
//! présent, et il ne se reconnectera jamais à cette zone tant que l'autre le
//! tient. Le journal disait `zone_rebind_target_held_by_other_zone`,
//! l'auditeur ne lisait rien d'utile.
//!
//! Le refus porte désormais son motif, [`MOTIF`], et l'identifiant de la zone
//! qui tient la sortie : un client qui voit un motif affiche la phrase du
//! serveur. Aucune fusion automatique des deux zones : le message dit quoi
//! faire, l'auditeur décide.
//!
//! Forme de la sentinelle :
//! `zone_output_unavailable:output_held_by_other_zone:<id de la zone>:<phrase>`.
//! Elle commence toujours par `zone_output_unavailable:` : ce qui reconnaît la
//! famille de refus la reconnaît encore.

/// Le motif stable (champ `reason` du 409 et de `zone.playback_error`).
pub const MOTIF: &str = "output_held_by_other_zone";

const PREFIXE: &str = "zone_output_unavailable:output_held_by_other_zone:";

/// La sentinelle du refus, pour la zone `zone_qui_tient` et la phrase donnée.
pub fn sentinelle(zone_qui_tient: i64, phrase: &str) -> String {
    format!("{PREFIXE}{zone_qui_tient}:{phrase}")
}

/// `Some((zone qui tient la sortie, phrase))` si `message` est ce refus.
pub fn depuis_sentinelle(message: &str) -> Option<(i64, &str)> {
    let reste = message.strip_prefix(PREFIXE)?;
    let (id, phrase) = reste.split_once(':')?;
    Some((id.parse().ok()?, phrase))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_sentinelle_se_relit_avec_une_phrase_qui_contient_des_deux_points() {
        let s = sentinelle(12, "La zone « Salon : NanoPlayer » l'utilise déjà.");
        assert!(s.starts_with("zone_output_unavailable:"));
        assert_eq!(
            depuis_sentinelle(&s),
            Some((12, "La zone « Salon : NanoPlayer » l'utilise déjà."))
        );
    }

    #[test]
    fn un_refus_ordinaire_n_est_pas_ce_refus() {
        assert_eq!(
            depuis_sentinelle("zone_output_unavailable:La sortie a disparu."),
            None
        );
    }
}
