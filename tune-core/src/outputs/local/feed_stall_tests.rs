use super::{
    FEED_STALL_TIMEOUT, OpenFailure, RingBuf, drain_deadline_for,
    feed_ring_abortable_with_stall_timeout, record_feed_stall_failure,
};
use std::sync::atomic::AtomicBool;

/// La boucle de production, avec son seuil ramené à zéro : un anneau plein
/// que personne ne vide rend le verdict « consommateur mort » — tout de
/// suite, sans dormir cinq secondes.
#[test]
fn un_anneau_que_personne_ne_vide_rend_le_verdict_de_blocage() {
    let ring = RingBuf::new(4);
    ring.push(&[0.0; 4]); // plein, et personne ne tirera jamais
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let debut = std::time::Instant::now();
    assert!(
        !feed_ring_abortable_with_stall_timeout(
            &ring,
            &[0.5f32; 8],
            &rx,
            &paused,
            None,
            std::time::Duration::ZERO,
        ),
        "un anneau plein et jamais vidé doit être déclaré bloqué"
    );
    assert!(
        debut.elapsed() < std::time::Duration::from_secs(1),
        "le seuil injecté doit rendre le verdict sans attendre"
    );
}

/// TÉMOIN VERT : le même appel, sur un anneau qui a de la place, ne
/// déclare rien. Le détecteur ne doit pas devenir un couperet.
#[test]
fn un_anneau_qui_accepte_tout_ne_declare_aucun_blocage() {
    let ring = RingBuf::new(16);
    let (_tx, rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    assert!(feed_ring_abortable_with_stall_timeout(
        &ring,
        &[0.5f32; 8],
        &rx,
        &paused,
        None,
        std::time::Duration::ZERO,
    ));
    assert_eq!(ring.available(), 8);
}

/// Le seuil réel n'est pas nul : un test qui l'injecterait à zéro partout
/// masquerait une production devenue instantanément couperet.
#[test]
fn le_seuil_de_production_laisse_le_temps_a_la_contre_pression() {
    assert_eq!(FEED_STALL_TIMEOUT, std::time::Duration::from_secs(5));
}

/// Le message doit nommer la sortie, la position où l'écran s'est figé, et
/// le geste à faire. « Une erreur est survenue » ne répare rien.
#[test]
fn le_blocage_dit_la_sortie_la_position_et_le_geste() {
    let slot = std::sync::Mutex::new(None);
    record_feed_stall_failure("CoreAudio", "DAC USB", 2000, &slot);
    let message = slot.lock().unwrap().clone().expect("le canal doit porter");
    assert!(message.contains("DAC USB"), "sortie absente : {message}");
    assert!(
        message.contains("CoreAudio"),
        "transport absent : {message}"
    );
    assert!(
        message.contains("2000 ms"),
        "la position figée est le chiffre qui relie l'écran au journal : {message}"
    );
    assert!(
        message.contains(OpenFailure::DeviceGone.user_message()),
        "le geste à faire est absent : {message}"
    );
}

/// Le canal est celui du poller : `take_output_failure()` le draine, une
/// fois, et le tick suivant ne re-stoppe pas la zone.
#[test]
fn le_blocage_passe_par_le_canal_que_le_poller_draine() {
    use super::super::traits::OutputTarget;
    let sortie = super::LocalOutput::new("DAC USB".into());
    assert!(
        sortie.take_output_failure().is_none(),
        "TÉMOIN VERT : une sortie saine ne remonte rien"
    );

    record_feed_stall_failure("CoreAudio", "DAC USB", 2000, &sortie.open_failure);
    let remonte = sortie
        .take_output_failure()
        .expect("le blocage doit remonter par le canal du poller");
    assert!(remonte.contains("2000 ms"), "got: {remonte}");
    assert!(
        sortie.take_output_failure().is_none(),
        "un échec ne doit jamais être remonté deux fois"
    );
}

/// Le vidage borné : durée de l'audio en attente + 5 s de marge.
#[test]
fn le_delai_de_vidage_couvre_l_audio_en_attente_plus_la_marge() {
    // Deux secondes de stéréo à 44,1 kHz = 176 400 échantillons entrelacés.
    assert_eq!(
        drain_deadline_for(44_100 * 2 * 2, 44_100, 2),
        std::time::Duration::from_millis(7000)
    );
    // Anneau vide : la marge seule.
    assert_eq!(
        drain_deadline_for(0, 44_100, 2),
        std::time::Duration::from_millis(5000)
    );
}

/// Une cadence ou un nombre de canaux nuls ne doivent pas diviser par zéro
/// — ce serait tuer le fil de lecture au lieu de borner son vidage.
#[test]
fn une_cadence_nulle_ne_divise_pas_par_zero() {
    assert_eq!(
        drain_deadline_for(0, 0, 0),
        std::time::Duration::from_millis(5000)
    );
}
