//! #4357 — la **période** d'ouverture WASAPI exclusive.
//!
//! Depuis a13a4c32, l'exclusif s'ouvrait à la période MINIMALE annoncée par
//! `IAudioClient::GetDevicePeriod` : 3 ms sur la plupart des pilotes. Rien dans
//! la lecture de musique ne réclame une latence de 3 ms — le volume, la pause
//! et le changement de piste passent par l'anneau, pas par la période.
//!
//! Le prix, lui, est payé par les pilotes qui tiennent mal ce rythme. Didier
//! (Marantz AV7706 en HDMI, Windows 11, 0.9.156) : son « déformé et nasillard »
//! en exclusif, propre en partagé — le partagé tourne à la période PAR DÉFAUT
//! du moteur audio (10 ms). Volume à 100 % dans son journal (`volume_units=1000`),
//! aucune famine côté Tune : le défaut naît dans le pilote HDMI, là où les
//! compteurs de Tune ne voient rien. Son SMSL SU-8 en USB tient les 3 ms, ce
//! qui explique que personne ne l'ait entendu avant.
//!
//! La période retenue est donc celle **par défaut** du pilote — celle que
//! Windows lui-même utilise. La minimale ne sert que si le pilote n'annonce pas
//! de période par défaut.
//!
//! Hors de la couche COM pour être testée sur toutes les plateformes de CI.

/// Période exclusive retenue, en unités de 100 ns, à partir des deux valeurs
/// de `GetDevicePeriod`. `Err` si aucune n'est exploitable.
pub(crate) fn periode_exclusive_100ns(par_defaut: i64, minimale: i64) -> Result<i64, String> {
    if par_defaut > 0 {
        Ok(par_defaut)
    } else if minimale > 0 {
        Ok(minimale)
    } else {
        Err(format!(
            "IAudioClient::GetDevicePeriod n'a annoncé aucune période exploitable \
             (par défaut {par_defaut}, minimale {minimale})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Le cas de Didier : pilote HDMI Intel, 10 ms par défaut, 3 ms minimum.
    /// L'exclusif doit s'ouvrir à 10 ms, comme le partagé qui sonne juste.
    #[test]
    fn un_pilote_hdmi_s_ouvre_a_sa_periode_par_defaut() {
        assert_eq!(periode_exclusive_100ns(100_000, 30_000), Ok(100_000));
    }

    /// Contre-épreuve : un pilote qui n'annonce pas de période par défaut
    /// garde la minimale — on ne refuse pas un périphérique qui s'ouvrait.
    #[test]
    fn sans_periode_par_defaut_la_minimale_reste_utilisee() {
        assert_eq!(periode_exclusive_100ns(0, 30_000), Ok(30_000));
        assert_eq!(periode_exclusive_100ns(-1, 30_000), Ok(30_000));
    }

    #[test]
    fn aucune_periode_exploitable_est_une_erreur() {
        assert!(periode_exclusive_100ns(0, 0).is_err());
        assert!(periode_exclusive_100ns(-5, -5).is_err());
    }

    /// Le branchement : `WasapiExclusiveOutput::new` passe par cette règle et
    /// ne retient plus la minimale en premier.
    #[test]
    fn l_ouverture_exclusive_passe_par_la_regle() {
        // Comparé sans blancs : rustfmt coupe librement un appel long.
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        assert!(
            source.contains("periode_exclusive_100ns(default_period,min_period)"),
            "wasapi_exclusive.rs doit choisir sa période par periode_exclusive_100ns"
        );
        assert!(
            !source.contains("letperiod=ifmin_period>0{"),
            "l'ancienne règle « minimale d'abord » est encore présente"
        );
    }
}
