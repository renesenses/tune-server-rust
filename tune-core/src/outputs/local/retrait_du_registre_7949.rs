//! #7949 — une sortie locale retirée du registre rend son fil de lecture et
//! son périphérique.
//!
//! Le fil est factice (aucune carte son, aucun son) mais il attend la pause et
//! alimente l'anneau par `feed_ring_abortable`, la fonction de production, avec
//! le canal d'arrêt, `paused` et `force_silent` de la vraie sortie. Sa
//! sentinelle est celle du vrai fil : elle retombe quand le fil a fini, c'est-
//! à-dire quand le flux cpal serait rendu.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{LocalOutput, RingBuf, feed_ring_abortable};
use crate::outputs::registry::OutputRegistry;

const ID: &str = "local:Factice 7949";

/// Le fil factice : comme la boucle d'alimentation du vrai fil, il s'arrête
/// sur l'arrêt reçu ou `force_silent`, et `feed_ring_abortable` attend la
/// pause. Chaque morceau poussé compte un envoi.
fn fil_qui_alimente(
    envois: Arc<AtomicU64>,
) -> impl FnOnce(std::sync::mpsc::Receiver<()>, Arc<AtomicBool>, Arc<AtomicBool>) + Send + 'static {
    move |stop_rx, paused, force_silent| {
        let anneau = RingBuf::new(4096);
        let morceau = [0.0_f32; 256];
        let mut vidange = [0.0_f32; 4096];
        loop {
            if stop_rx.try_recv().is_ok() || force_silent.load(Ordering::Relaxed) {
                return;
            }
            if !feed_ring_abortable(&anneau, &morceau, &stop_rx, &paused, Some(&force_silent)) {
                return;
            }
            if force_silent.load(Ordering::Relaxed) {
                return;
            }
            envois.fetch_add(1, Ordering::SeqCst);
            anneau.pop(&mut vidange);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn attendre(condition: impl Fn() -> bool, delai: Duration) -> bool {
    let fin = Instant::now() + delai;
    while Instant::now() < fin {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    condition()
}

/// Une sortie locale factice, son fil lancé, enregistrée dans un registre.
fn registre_avec_une_sortie_qui_joue(
    en_pause: bool,
) -> (OutputRegistry, Arc<AtomicBool>, Arc<AtomicU64>) {
    let sortie = LocalOutput::new("Factice 7949".to_string());
    let envois = Arc::new(AtomicU64::new(0));
    let sentinelle = sortie.lancer_un_fil_factice_for_test(fil_qui_alimente(envois.clone()));
    assert!(
        attendre(|| envois.load(Ordering::SeqCst) > 0, Duration::from_secs(2)),
        "le fil factice n'a jamais alimenté l'anneau"
    );
    if en_pause {
        sortie.paused.store(true, Ordering::SeqCst);
    }
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(sortie));
    (registre, sentinelle, envois)
}

#[test]
fn sortie_en_pause_retiree_du_registre_rend_son_fil_et_son_peripherique() {
    let (mut registre, sentinelle, _envois) = registre_avec_une_sortie_qui_joue(true);

    // Retrait sans `stop()`, la dernière référence tombe : c'est ce que font
    // le balayage à chaud et la bascule ASIO pour une sortie en pause.
    drop(registre.remove(ID));

    assert!(
        attendre(
            || !sentinelle.load(Ordering::SeqCst),
            Duration::from_secs(3)
        ),
        "#7949 : la sortie locale en pause a été retirée du registre, mais son fil de \
         lecture vit encore et garde le périphérique (EBUSY / DeviceBusy au rebranchement)"
    );
}

#[test]
fn sortie_en_lecture_retiree_du_registre_n_envoie_plus_rien() {
    let (mut registre, sentinelle, envois) = registre_avec_une_sortie_qui_joue(false);

    drop(registre.remove(ID));

    // Laisser au fil le temps de voir l'arrêt, puis constater qu'il n'envoie plus.
    std::thread::sleep(Duration::from_millis(100));
    let apres_retrait = envois.load(Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(200));
    let plus_tard = envois.load(Ordering::SeqCst);
    assert_eq!(
        apres_retrait, plus_tard,
        "#7949 : la sortie locale a été retirée du registre pendant la lecture, mais \
         son fil envoie encore du son ({apres_retrait} puis {plus_tard} morceaux)"
    );
    assert!(
        !sentinelle.load(Ordering::SeqCst),
        "#7949 : le fil de la sortie retirée n'a pas fini"
    );
}

#[test]
fn sortie_remplacee_sous_le_meme_identifiant_rend_son_fil() {
    let (mut registre, sentinelle, _envois) = registre_avec_une_sortie_qui_joue(true);

    // La bascule ASIO enregistre la nouvelle sortie sous le même identifiant.
    drop(registre.register(Box::new(LocalOutput::new("Factice 7949".to_string()))));

    assert!(
        attendre(
            || !sentinelle.load(Ordering::SeqCst),
            Duration::from_secs(3)
        ),
        "#7949 : la sortie remplacée sous le même identifiant garde son fil de lecture"
    );
}

#[tokio::test]
async fn arreter_les_sorties_retirees_appelle_stop_sur_chacune() {
    use crate::outputs::mock::MockOutput;
    use crate::outputs::traits::OutputTarget;
    let mut registre = OutputRegistry::new();
    registre.register(Box::new(MockOutput::new("a", "A")));
    registre.register(Box::new(MockOutput::new("b", "B")));

    let retirees: Vec<_> = ["a", "b", "absente"]
        .iter()
        .filter_map(|id| registre.remove(id))
        .collect();
    assert_eq!(retirees.len(), 2);
    assert!(registre.list().is_empty());
    crate::outputs::registry::arreter_les_sorties_retirees(retirees.clone()).await;

    for sortie in retirees {
        let sortie = sortie.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        assert_eq!(
            mock.stop_call_count(),
            1,
            "#7949 : la sortie retirée « {} » n'a pas été arrêtée",
            mock.device_id()
        );
    }
}
