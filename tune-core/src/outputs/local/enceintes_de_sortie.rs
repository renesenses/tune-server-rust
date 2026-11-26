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
//! **Entrée en PURE par un fondu** (rc14, sécurité auditive, relecture de
//! #9556) : la sortie l'arme AVANT son drapeau ([`entrer_en_pure`]). Le
//! réglage passe en fondu au signal sec TENU au niveau de chaque voie (la
//! réserve des gains n'est plus rendue d'un coup), puis le niveau remonte
//! vers celui d'avant le réglage, borné par le volume, jamais au-delà ; à
//! volume inchangé, PURE redevient bit-perfect au bout. Sans cet armement
//! (drapeau posé seul), PURE rend le flux intact dès le bloc suivant. La
//! sortie de PURE passe par un fondu.

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

/// rc14 (sécurité auditive) — l'ENTRÉE en PURE de la sortie, avant son
/// drapeau : un réglage qui agit fond vers le sec tenu au niveau de chaque
/// voie, puis remonte vers `borne` (rapport `volume d'avant / volume
/// courant`, #6741), jamais au-delà. Hors temps réel.
pub(crate) fn entrer_en_pure(cle: &AtomicBool, borne: f64) {
    if let Some(c) = creneau(cle)
        && let Ok(mut p) = c.lock()
    {
        p.entrer_en_pure(borne);
    }
}

/// rc14 — la SORTIE de PURE de la sortie : une entrée en cours repart vers
/// le réglage par un fondu ; une entrée armée qui n'a pas joué est annulée.
pub(crate) fn sortir_de_pure(cle: &AtomicBool) {
    if let Some(c) = creneau(cle)
        && let Ok(mut p) = c.lock()
    {
        p.sortir_de_pure();
    }
}

/// rc14 — un geste de volume sous PURE : la remontée suit la borne.
pub(crate) fn borner_en_pure(cle: &AtomicBool, borne: f64) {
    if let Some(c) = creneau(cle)
        && let Ok(mut p) = c.lock()
    {
        p.borner_en_pure(borne);
    }
}

/// rc14 — « Rendre le niveau » (#6524) sous PURE.
pub(crate) fn liberer_en_pure(cle: &AtomicBool) {
    if let Some(c) = creneau(cle)
        && let Ok(mut p) = c.lock()
    {
        p.liberer_en_pure();
    }
}

/// rc14 — le niveau tenu par l'entrée en PURE (dB, le plus bas des voies)
/// et s'il remonte : le voyant « niveau réduit ».
pub(crate) fn garde_en_pure(cle: &AtomicBool) -> Option<(f64, bool)> {
    creneau(cle).and_then(|c| c.lock().ok().and_then(|p| p.garde_en_pure()))
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
    if dop {
        return false;
    }
    let pure = cle.load(Ordering::Relaxed);
    // rc14 — sous PURE, seule l'entrée en cours touche encore le signal.
    creneau(cle).is_some_and(|c| {
        c.lock().is_ok_and(|p| {
            if pure {
                p.en_transition_pure()
            } else {
                !p.neutre_au_repos()
            }
        })
    })
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
            if pure && p.en_transition_pure() {
                // rc14 — entrée en PURE : fondu, puis remontée bornée.
                p.traiter_en_pure(mots, canaux, cadence)
            } else if pure {
                p.contourner(mots, canaux, cadence);
                false
            } else {
                // rc14 — PURE quitté pendant son entrée : retour au réglage
                // par le fondu habituel. Une entrée armée dont le drapeau
                // n'est pas encore levé n'est pas annulée : le bloc reste
                // réglé, le fondu partira au premier bloc sous PURE.
                if p.en_transition_pure() && p.transition_commencee() {
                    p.sortir_de_pure();
                }
                p.traiter(mots, canaux, cadence)
            }
        })
        .unwrap_or(false)
}

#[cfg(test)]
#[path = "enceintes_de_sortie_tests.rs"]
mod tests;
