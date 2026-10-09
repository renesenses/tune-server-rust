//! Nombre de fils de travail du moteur tokio du serveur.
//!
//! `#[tokio::main]` prend un fil par processeur. Sur une machine à UN seul
//! processeur, le moteur n'a donc qu'un fil : le moindre travail bloquant gèle
//! tout le serveur. Or le lecteur local lit son flux en HTTP auprès du serveur
//! lui-même : un testeur a mesuré des gels de 12 à 19 s et des coupures ALSA
//! (forum, fil 2124, ticket 224 ; #5677).
//!
//! On impose donc un plancher de [`MINIMUM`] fils. La variable que tokio
//! honore déjà, `TOKIO_WORKER_THREADS`, reste une surcharge explicite.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Plancher de fils de travail, quel que soit le nombre de processeurs.
pub const MINIMUM: usize = 4;

/// Variable d'environnement que tokio lit déjà pour ce réglage.
pub const VARIABLE_DE_SURCHARGE: &str = "TOKIO_WORKER_THREADS";

static RETENU: AtomicUsize = AtomicUsize::new(0);

/// Calcul pur : une surcharge valide (entier > 0) l'emporte telle quelle ;
/// sinon `max(processeurs, MINIMUM)`.
pub fn nombre_de_fils(processeurs: usize, surcharge: Option<&str>) -> usize {
    if let Some(n) = surcharge
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
    {
        return n;
    }
    processeurs.max(MINIMUM)
}

/// Construit le moteur multi-fils du serveur avec le nombre de fils retenu.
pub fn construire_le_moteur() -> tokio::runtime::Runtime {
    let processeurs = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let surcharge = std::env::var(VARIABLE_DE_SURCHARGE).ok();
    let fils = nombre_de_fils(processeurs, surcharge.as_deref());
    RETENU.store(fils, Ordering::Relaxed);
    eprintln!("tune-server: {fils} fils de travail ({processeurs} processeur(s))");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(fils)
        // #5677 : chaque fil de travail date son dernier réveil, pour que le
        // relevé d'un gel dise lesquels étaient pris, et depuis quand.
        .on_thread_park(crate::gel_executeur::travailleurs::au_garage)
        .on_thread_unpark(crate::gel_executeur::travailleurs::au_reveil)
        .on_thread_stop(crate::gel_executeur::travailleurs::a_l_arret)
        .enable_all()
        .build()
        .expect("construction du moteur tokio")
}

/// Nombre de fils retenu au démarrage (0 si le moteur n'a pas été construit
/// par [`construire_le_moteur`], p. ex. un binaire composeur).
pub fn retenu() -> usize {
    RETENU.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_processeur_donne_le_plancher() {
        assert_eq!(nombre_de_fils(1, None), 4);
    }

    #[test]
    fn huit_processeurs_donnent_huit() {
        assert_eq!(nombre_de_fils(8, None), 8);
    }

    #[test]
    fn la_surcharge_est_respectee() {
        assert_eq!(nombre_de_fils(1, Some("2")), 2);
        assert_eq!(nombre_de_fils(8, Some(" 16 ")), 16);
    }

    #[test]
    fn une_surcharge_invalide_est_ignoree() {
        assert_eq!(nombre_de_fils(1, Some("0")), 4);
        assert_eq!(nombre_de_fils(2, Some("abc")), 4);
    }
}
