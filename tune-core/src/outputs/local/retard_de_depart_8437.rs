//! #8437 — l'attente du retard de départ d'une sortie locale groupée avec des
//! enceintes réseau (voir [`crate::zones::retard_groupe_mixte_8437`]).
//!
//! L'attente est découpée en tranches courtes pour qu'un Stop (le drapeau
//! `force_silent` du flux) l'interrompe sans attendre la fin du retard. Le
//! sommeil est injecté : les tests comptent le temps sur une horloge simulée.

use std::sync::atomic::{AtomicBool, Ordering};

/// La plus longue tranche de sommeil entre deux regards sur le Stop.
pub(super) const TRANCHE_MS: u64 = 20;

/// Attend `retard_ms` par tranches de [`TRANCHE_MS`] au plus, avec `dormir`.
/// Rend `false` si `arret` est levé avant la fin (le flux ne doit pas partir),
/// `true` sinon.
pub(super) fn attendre(retard_ms: u64, arret: &AtomicBool, mut dormir: impl FnMut(u64)) -> bool {
    let mut reste = retard_ms;
    while reste > 0 {
        if arret.load(Ordering::SeqCst) {
            return false;
        }
        let tranche = reste.min(TRANCHE_MS);
        dormir(tranche);
        reste -= tranche;
    }
    !arret.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// Le membre local démarre `sync_delay_ms` plus tard : sur l'horloge
    /// simulée, l'attente dure exactement le retard demandé.
    #[test]
    fn le_depart_attend_exactement_le_retard() {
        let horloge = Cell::new(0u64);
        let arret = AtomicBool::new(false);
        assert!(attendre(2000, &arret, |ms| horloge.set(horloge.get() + ms)));
        assert_eq!(horloge.get(), 2000);
    }

    /// Contre-épreuve : sans réglage, aucun sommeil, départ immédiat.
    #[test]
    fn sans_retard_aucune_attente() {
        let horloge = Cell::new(0u64);
        let arret = AtomicBool::new(false);
        assert!(attendre(0, &arret, |ms| horloge.set(horloge.get() + ms)));
        assert_eq!(horloge.get(), 0);
    }

    /// Un Stop pendant l'attente l'interrompt dans la tranche : le flux ne
    /// part pas, et l'attente ne court pas jusqu'au bout.
    #[test]
    fn un_stop_interrompt_l_attente() {
        let horloge = Cell::new(0u64);
        let arret = AtomicBool::new(false);
        let parti = attendre(2000, &arret, |ms| {
            horloge.set(horloge.get() + ms);
            if horloge.get() >= 100 {
                arret.store(true, Ordering::SeqCst);
            }
        });
        assert!(!parti);
        assert!(horloge.get() <= 100 + TRANCHE_MS, "{}", horloge.get());
    }
}
