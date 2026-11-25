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
//! **Créneau seulement si le greffon est dans la chaîne de la zone** (même
//! neutre) : une sortie dont la zone n'a pas le greffon n'a pas de créneau ;
//! son chemin temps réel ne prend alors que la lecture d'un compteur atomique
//! (aucun créneau nulle part) ou d'une table sous `RwLock` (créneaux
//! ailleurs), sans `Mutex` ni copie. Avec le greffon, la ligne à retard est
//! alimentée en permanence ; un réglage neutre au repos ne touche aucun
//! échantillon.
//!
//! **Historique remis à zéro à chaque nouvelle lecture** (démarrage, reprise
//! après arrêt, seek, piste lancée hors enchaînement :
//! [`nouvelle_lecture`]) : un son d'une autre écoute n'est jamais rejoué. Seul
//! l'enchaînement sans blanc (même flux de sortie) garde l'historique.
//!
//! **Entrée en PURE sans fondu**, comme les autres étages : PURE rend le flux
//! intact dès le bloc suivant. La sortie de PURE, elle, passe par un fondu.

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

/// Change à chaud le réglage de la sortie dont `cle` est le drapeau PURE
/// (réglage modifié, PURE ou « Bypass all » basculés). Hors temps réel.
///
/// - Greffon dans la chaîne (`dans_la_chaine`) : le créneau est créé s'il
///   manque (il part du neutre et rejoint le réglage par un fondu) ; sinon le
///   processeur passe au nouveau réglage par son fondu (`None` = neutre).
/// - Greffon hors de la chaîne : un créneau existant fond vers le neutre ; il
///   disparaîtra à la prochaine lecture. Aucun créneau n'est créé.
pub(crate) fn poser(cle: &AtomicBool, dans_la_chaine: bool, reglages: Option<ReglagesEnceintes>) {
    let r = reglages.unwrap_or_default();
    if let Some(c) = creneau(cle) {
        if let Ok(mut p) = c.lock() {
            p.definir(r);
        }
        return;
    }
    if !dans_la_chaine {
        return;
    }
    if let Ok(mut t) = table().write() {
        t.entry(adresse(cle))
            .or_insert_with(|| Arc::new(Mutex::new(ProcesseurEnceintes::au_neutre(r))));
        POSES.store(t.len(), Ordering::Release);
    }
}

/// Une NOUVELLE lecture démarre sur la sortie (lancement, reprise après
/// arrêt, seek, piste hors enchaînement). Hors temps réel, avant le premier
/// bloc. Greffon hors de la chaîne : le créneau est retiré (coût nul).
/// Sinon l'historique est vidé (rien d'une écoute précédente n'est rejoué)
/// et le réglage s'applique dès le premier bloc, comme à tout début de flux :
/// les voies retardées commencent par leur délai de silence.
pub(crate) fn nouvelle_lecture(
    cle: &AtomicBool,
    dans_la_chaine: bool,
    reglages: Option<ReglagesEnceintes>,
) {
    if !dans_la_chaine {
        oublier(cle);
        return;
    }
    let r = reglages.unwrap_or_default();
    if let Some(c) = creneau(cle) {
        if let Ok(mut p) = c.lock() {
            p.nouveau_flux(r);
        }
        return;
    }
    if let Ok(mut t) = table().write() {
        t.entry(adresse(cle))
            .or_insert_with(|| Arc::new(Mutex::new(ProcesseurEnceintes::new(r))));
        POSES.store(t.len(), Ordering::Release);
    }
}

/// Arrêt de la sortie : l'historique est vidé, le réglage gardé.
pub(crate) fn vider(cle: &AtomicBool) {
    if let Some(c) = creneau(cle)
        && let Ok(mut p) = c.lock()
    {
        let r = p.reglages().clone();
        p.nouveau_flux(r);
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
