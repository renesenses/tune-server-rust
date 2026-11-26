//! TESS T06 — témoin « aucune conversion `f32` entre deux étages » (Q5),
//! vérifié par l'exécution et non par une lecture de code.
//!
//! Les adaptateurs provisoires `f32` ↔ `f64` des chaînes locale et réseau
//! (`par_tranches_f32`) se signalent ici à chaque passage. Un banc arme le
//! compteur, fait passer un bloc par la chaîne, et lit combien de frontières
//! `f32` le signal a traversées : 0 pour une chaîne dont tous les moteurs
//! sont en `f64` natif (égaliseur, crossfeed, repli mono, réaffectation).
//! Seulement en test.
use std::cell::Cell;

thread_local! {
    static PASSAGES: Cell<Option<usize>> = const { Cell::new(None) };
}

/// Un adaptateur vient de convertir un bloc en `f32` (sans effet si aucun
/// banc n'écoute).
// T09b : plus aucun adaptateur `f32` dans les chaînes locale et réseau (le
// convolveur était le dernier). La sonde reste, prête pour tout adaptateur
// futur, et sa contre-épreuve l'appelle directement.
#[allow(dead_code)]
pub(crate) fn noter() {
    PASSAGES.with(|p| {
        if let Some(n) = p.get() {
            p.set(Some(n + 1));
        }
    });
}

/// Exécute `f` et rend le nombre de frontières `f32` traversées sur ce fil.
pub(crate) fn compter(f: impl FnOnce()) -> usize {
    PASSAGES.with(|p| p.set(Some(0)));
    f();
    PASSAGES.with(|p| p.take().unwrap_or(0))
}
