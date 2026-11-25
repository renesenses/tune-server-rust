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
/// PURE. Hors temps réel. Une sortie qui joue passe au nouveau réglage par le
/// fondu du processeur ; retirer un réglage actif passe par un fondu vers le
/// neutre (le créneau reste, au repos il ne touche plus rien).
pub(crate) fn poser(cle: &AtomicBool, reglages: Option<ReglagesEnceintes>) {
    let existant = creneau(cle);
    match (existant, reglages) {
        (Some(c), r) => {
            if let Ok(mut p) = c.lock() {
                p.definir(r.unwrap_or_default());
            }
        }
        (None, Some(r)) if !r.est_neutre_partout() => {
            if let Ok(mut t) = table().write() {
                t.insert(adresse(cle), Arc::new(Mutex::new(ProcesseurEnceintes::new(r))));
                POSES.store(t.len(), Ordering::Release);
            }
        }
        (None, _) => {}
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
pub(crate) fn reglages(cle: &AtomicBool) -> Option<ReglagesEnceintes> {
    creneau(cle).and_then(|c| c.lock().ok().map(|p| p.reglages().clone()))
}

/// Temps réel : traite `mots` (entrelacés, `canaux` voies à `cadence` Hz) en
/// place. Rien sous PURE (`cle` levé) ni sur un porteur DoP. Rend `true` si un
/// échantillon a pu changer.
pub(crate) fn traiter<T: Echantillon>(
    cle: &AtomicBool,
    mots: &mut [T],
    canaux: u16,
    cadence: u32,
    dop: bool,
) -> bool {
    if dop || cle.load(Ordering::Relaxed) {
        return false;
    }
    let Some(c) = creneau(cle) else {
        return false;
    };
    c.lock()
        .map(|mut p| p.traiter(mots, canaux, cadence))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::reglage_enceintes::VoieEnceinte;

    fn deux_voies(distance_droite: f64, gain_droite: f64) -> ReglagesEnceintes {
        ReglagesEnceintes {
            channels: vec![
                VoieEnceinte::default(),
                VoieEnceinte {
                    distance_cm: distance_droite,
                    gain_db: gain_droite,
                    invert: false,
                },
            ],
            invert_all: false,
        }
    }

    #[test]
    fn une_sortie_sans_reglage_n_est_pas_touchee() {
        let pure = AtomicBool::new(false);
        let mut t = vec![0.5_f32; 64];
        assert!(!traiter(&pure, &mut t, 2, 44_100, false));
        assert!(!agit(&pure, false));
        // Un réglage neutre ne crée même pas de créneau.
        poser(&pure, Some(ReglagesEnceintes::default()));
        assert!(reglages(&pure).is_none());
        assert!(t.iter().all(|x| *x == 0.5));
    }

    #[test]
    fn le_reglage_est_propre_a_chaque_sortie_et_coupe_sous_pure() {
        let a = AtomicBool::new(false);
        let b = AtomicBool::new(false);
        poser(&a, Some(deux_voies(300.0, -6.0)));
        assert!(agit(&a, false));
        assert!(!agit(&b, false), "l'autre sortie n'a rien");
        assert!(!agit(&a, true), "jamais sur un porteur DoP");
        let mut t = vec![0.5_f32; 64];
        assert!(traiter(&a, &mut t, 2, 44_100, false));
        assert_eq!(t[0], 0.5);
        assert!(t[1] < 0.5);
        // PURE : coupé tout de suite, rien n'est touché.
        a.store(true, Ordering::Relaxed);
        let mut t = vec![0.5_f32; 64];
        assert!(!traiter(&a, &mut t, 2, 44_100, false));
        assert!(t.iter().all(|x| *x == 0.5));
        assert!(!agit(&a, false));
        a.store(false, Ordering::Relaxed);
        // Retiré : fondu vers le neutre, puis plus rien.
        poser(&a, None);
        let mut t = vec![0.5_f32; 2 * 4_410];
        traiter(&a, &mut t, 2, 44_100, false);
        assert_eq!(*t.last().unwrap(), 0.5);
        assert!(!agit(&a, false));
        oublier(&a);
        assert!(reglages(&a).is_none());
    }

    /// Le bras natif (ALSA natif, WASAPI, ASIO) : un réglage actif fait
    /// prendre la branche flottante et décale la voie au mot près ; sans
    /// réglage (ou neutre), les mots partent bruts, bit-perfect.
    #[test]
    fn le_bras_natif_applique_le_reglage_et_reste_bit_perfect_sans() {
        use crate::audio::volume_scale::UNITE_DU_GAIN_DE_RENDU;
        let eq = std::sync::Mutex::new(None);
        let conv = std::sync::Mutex::new(None);
        let cf = std::sync::Mutex::new(None);
        let mono = AtomicBool::new(false);
        let pure = AtomicBool::new(false);
        // 16 bits stéréo, 200 trames : une rampe, gauche ≠ droite.
        let mut octets = Vec::new();
        for n in 0..200i16 {
            octets.extend_from_slice(&(n * 100).to_le_bytes());
            octets.extend_from_slice(&(-n * 50).to_le_bytes());
        }
        let preparer = || {
            super::super::prepare_windows_native_pcm(
                &octets,
                16,
                2,
                2,
                false,
                false,
                UNITE_DU_GAIN_DE_RENDU,
                &eq,
                &conv,
                &cf,
                &pure,
                &mono,
                None,
                44_100,
            )
            .unwrap()
        };
        let brut = preparer();
        assert!(brut.bit_perfect, "sans réglage : mots bruts");
        // La voie droite 100 cm plus près : 129 trames de retard à 44,1 kHz
        // (128,57 arrondi) ; la gauche, plus lointaine, n'a rien.
        poser(&pure, Some(deux_voies(300.0 - 100.0, 0.0)));
        let traite = preparer();
        assert!(!traite.bit_perfect);
        for n in 0..200 {
            assert_eq!(traite.samples[n * 2], brut.samples[n * 2], "gauche {n}");
            let attendu = if n < 129 { 0 } else { brut.samples[(n - 129) * 2 + 1] };
            assert_eq!(traite.samples[n * 2 + 1], attendu, "droite {n}");
        }
        // PURE : de nouveau bit-perfect.
        pure.store(true, Ordering::Relaxed);
        assert!(preparer().bit_perfect);
        pure.store(false, Ordering::Relaxed);
        oublier(&pure);
        assert!(preparer().bit_perfect);
    }
}
