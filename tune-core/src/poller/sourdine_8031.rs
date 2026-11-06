//! #8031 — un volume relu sur l'appareil pendant la sourdine n'est pas un
//! geste de l'auditeur.
//!
//! Eversolo DMP-A8 sur le .18, le 06/11 : la sourdine (`SetMute`, #6700)
//! coupe le son, la levée le rend, mais PARFOIS le curseur ne revient pas à
//! sa place. Les trois sites d'adoption de `tick.rs` ignoraient la sourdine :
//! un relevé pris pendant la coupure (0 sur le site radio, qui n'écarte pas
//! le zéro ; une valeur transitoire ailleurs) était plus BAS que la consigne,
//! donc adopté (#6739 adopte les baisses). À la levée, l'appareil retrouvait
//! son volume d'avant — une HAUSSE, que #6739 refuse à juste titre : la zone
//! restait à la valeur relue pendant la sourdine, et le curseur avec elle.
//! « Parfois » : selon qu'un tour du sondeur tombait ou non dans la coupure.
//!
//! Désormais, tant que la zone ou l'appareil est muet, et pendant la grâce
//! de volume qui suit un changement de sourdine (un évènement `LastChange`
//! en retard, une remontée progressive), le relevé n'est ni adopté ni pris
//! pour repère du front suivant. Rien n'est commandé : aucune hausse.
use super::VOLUME_GRACE_SECS;
use std::time::Instant;

/// La sourdine est en jeu : la zone ou l'appareil est muet, ou la sourdine
/// vient d'être posée ou levée.
pub(super) fn sourdine_en_jeu(
    zone_muette: bool,
    appareil_muet: bool,
    derniere_sourdine: Option<Instant>,
) -> bool {
    zone_muette
        || appareil_muet
        || derniere_sourdine.is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn la_regle() {
        assert!(sourdine_en_jeu(true, false, None));
        assert!(sourdine_en_jeu(false, true, None));
        assert!(sourdine_en_jeu(false, false, Some(Instant::now())));
        assert!(!sourdine_en_jeu(false, false, None));
        let ancienne = Instant::now()
            .checked_sub(Duration::from_secs(VOLUME_GRACE_SECS + 1))
            .unwrap();
        assert!(!sourdine_en_jeu(false, false, Some(ancienne)));
    }
}
