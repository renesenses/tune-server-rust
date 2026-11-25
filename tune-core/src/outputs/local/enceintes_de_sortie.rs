//! rc14 — le réglage des enceintes (`speaker-setup`) de chaque sortie
//! locale.
//!
//! Le processeur ([`ProcesseurEnceintes`]) est rangé par l'ADRESSE du drapeau
//! PURE de la sortie (`LocalOutput::pure_bypass`, un `Arc<AtomicBool>` propre
//! à la sortie et stable tant qu'elle vit), comme l'ordre TESS de la sortie
//! (#9520, `OrdreLocal::poser_pour_la_sortie`) : tous les bras de lecture
//! (partagé, CoreAudio, ASIO, WASAPI, ALSA natif) tiennent déjà ce drapeau, et
//! le greffon les atteint sans changer la signature de leurs étages.
//!
//! **Place** : après l'adaptation des voies (réaffectation de la zone ou de
//! l'album, disposition déclarée), avant le suréchantillonneur et le
//! rééchantillonnage (`EtageDeConversion::convertir`) ; sur les bras natifs,
//! après l'adaptation et avant le volume (`prepare_windows_native_pcm`).
//!
//! **Coût nul quand rien n'est posé** : un compteur atomique dit si une
//! sortie au moins a un réglage ; sinon le chemin temps réel ne prend aucun
//! verrou. Un réglage neutre au repos ne touche aucun échantillon.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::audio::reglage_enceintes::{Echantillon, ProcesseurEnceintes, ReglagesEnceintes};

type Creneau = Arc<Mutex<ProcesseurEnceintes>>;

static POSES: AtomicUsize = AtomicUsize::new(0);

fn table() -> &'static RwLock<HashMap<usize, Creneau>> {
    static T: OnceLock<RwLock<HashMap<usize, Creneau>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn adresse(cle: &AtomicBool) -> usize {
    std::ptr::from_ref(cle) as usize
}

fn creneau(cle: &AtomicBool) -> Option<Creneau> {
    if POSES.load(Ordering::Acquire) == 0 {
        return None;
    }
    table().read().ok()?.get(&adresse(cle)).cloned()
}

/// Pose (ou retire, `None`) le réglage de la sortie dont `cle` est le drapeau
/// PURE. Hors temps réel.
///
/// Le créneau est créé dès le PREMIER appel, réglage neutre compris (la
/// lecture en pose un à chaque démarrage) : sa ligne à retard est alors
/// alimentée en permanence, et un réglage posé plus tard trouve un historique
/// plein. Un créneau neuf part du neutre ; tout changement passe par le
/// fondu du processeur, retrait compris.
pub(crate) fn poser(cle: &AtomicBool, reglages: Option<ReglagesEnceintes>) {
    let r = reglages.unwrap_or_default();
    if let Some(c) = creneau(cle) {
        if let Ok(mut p) = c.lock() {
            p.definir(r);
        }
        return;
    }
    if let Ok(mut t) = table().write() {
        t.entry(adresse(cle))
            .or_insert_with(|| Arc::new(Mutex::new(ProcesseurEnceintes::au_neutre(r))));
        POSES.store(t.len(), Ordering::Release);
    }
}

/// Le nombre de voies qu'a vues le réglage des enceintes de cette sortie
/// (après adaptation des voies), au dernier bloc ; `None` si rien n'a joué.
pub(crate) fn voies_vues(cle: &AtomicBool) -> Option<u16> {
    creneau(cle).and_then(|c| c.lock().ok().and_then(|p| p.format_vu().map(|(n, _)| n)))
}

/// Temps réel, branche BRUTE (bit-perfect) d'un bras natif : alimente la
/// ligne à retard sans toucher les mots. Sous PURE, c'est un contournement
/// (historique remis à zéro à l'entrée). Rien sur un porteur DoP.
pub(crate) fn alimenter<T: Echantillon>(
    cle: &AtomicBool,
    mots: &[T],
    canaux: u16,
    cadence: u32,
    dop: bool,
) {
    if dop {
        return;
    }
    let Some(c) = creneau(cle) else {
        return;
    };
    if let Ok(mut p) = c.lock() {
        if cle.load(Ordering::Relaxed) {
            p.contourner(mots, canaux, cadence);
        } else {
            p.alimenter(mots, canaux, cadence);
        }
    }
}

/// La sortie disparaît : son réglage aussi.
pub(crate) fn oublier(cle: &AtomicBool) {
    if POSES.load(Ordering::Acquire) == 0 {
        return;
    }
    if let Ok(mut t) = table().write() {
        t.remove(&adresse(cle));
        POSES.store(t.len(), Ordering::Release);
    }
}

/// Le réglage de cette sortie touche-t-il (ou va-t-il toucher) le signal ?
/// Faux sous PURE, sous DoP (`dop`), sans réglage, ou au repos sur un réglage
/// neutre.
pub(crate) fn agit(cle: &AtomicBool, dop: bool) -> bool {
    if dop || cle.load(Ordering::Relaxed) {
        return false;
    }
    creneau(cle).is_some_and(|c| c.lock().is_ok_and(|p| !p.neutre_au_repos()))
}

/// Les réglages posés sur cette sortie (`None` : aucun).
#[cfg(test)]
pub(crate) fn reglages(cle: &AtomicBool) -> Option<ReglagesEnceintes> {
    creneau(cle).and_then(|c| c.lock().ok().map(|p| p.reglages().clone()))
}

/// Temps réel : traite `mots` (entrelacés, `canaux` voies à `cadence` Hz) en
/// place. Sous PURE (`cle` levé), rien n'est touché mais la ligne à retard
/// est alimentée (remise à zéro à l'entrée en PURE). Rien sur un porteur DoP.
/// Rend `true` si un échantillon a pu changer.
pub(crate) fn traiter<T: Echantillon>(
    cle: &AtomicBool,
    mots: &mut [T],
    canaux: u16,
    cadence: u32,
    dop: bool,
) -> bool {
    if dop {
        return false;
    }
    let Some(c) = creneau(cle) else {
        return false;
    };
    let pure = cle.load(Ordering::Relaxed);
    c.lock()
        .map(|mut p| {
            if pure {
                p.contourner(mots, canaux, cadence);
                false
            } else {
                p.traiter(mots, canaux, cadence)
            }
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "enceintes_de_sortie_tests.rs"]
mod tests;
