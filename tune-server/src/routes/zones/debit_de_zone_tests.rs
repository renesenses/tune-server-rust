use super::{FENETRE_MINIMALE, debit_observe_kbps, mesure_de_session};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;
use tune_core::http::streamer::{StreamInfo, StreamSession};

fn session_de_test() -> StreamSession {
    StreamSession::new(
        "session-de-test".to_string(),
        StreamInfo {
            format: "flac".to_string(),
            mime_type: "audio/flac".to_string(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            seek_ms: None,
        },
        true,
        8,
    )
}

/// La fenêtre de mesure appartient au FLUX, pas au serveur.
///
/// Une session qui vient de naître n'a rien à annoncer, quel que soit le
/// nombre d'octets déjà comptés : on n'a pas encore observé assez
/// longtemps. L'horloge du serveur, elle, aurait rendu un chiffre — c'est
/// tout le défaut : elle avance depuis le démarrage du processus et ne
/// sait rien de ce flux-ci.
#[test]
fn la_fenetre_de_mesure_est_celle_de_la_session() {
    let session = session_de_test();
    session.bytes_sent.store(1_000_000, Relaxed);

    let (octets, fenetre) = mesure_de_session(&session);

    assert_eq!(octets, 1_000_000, "le compteur de la session doit être lu");
    assert!(
        fenetre < FENETRE_MINIMALE,
        "session tout juste créée : sa fenêtre vaut {fenetre:?}, \
         elle ne peut pas déjà dépasser {FENETRE_MINIMALE:?}"
    );
    assert_eq!(
        debit_observe_kbps(octets, fenetre),
        None,
        "trop tôt pour mesurer ce flux — une horloge de serveur, elle, \
         aurait fourni une fenêtre et donc un chiffre"
    );
}

/// Un débit qu'on n'a pas mesuré ne s'annonce pas.
#[test]
fn aucun_octet_ne_permet_aucune_annonce() {
    assert_eq!(
        debit_observe_kbps(0, Duration::from_secs(30)),
        None,
        "pas un octet envoyé : il n'y a rien à mesurer, donc rien à annoncer"
    );
}

/// Trop tôt pour mesurer : la rafale d'amorçage n'est pas un débit.
#[test]
fn une_fenetre_trop_courte_ne_permet_aucune_annonce() {
    assert_eq!(
        debit_observe_kbps(200_000, Duration::from_millis(120)),
        None,
        "120 ms de session : le remplissage du tampon n'est pas un débit"
    );
}

/// Le débit annoncé est celui qu'on a compté, pas un entier arrondi en
/// chemin. `octets * 8 / 1000` en arithmétique entière jette les décimales
/// AVANT la division par le temps.
#[test]
fn le_debit_annonce_est_la_mesure_pas_une_troncature() {
    assert_eq!(
        debit_observe_kbps(12_345, Duration::from_secs(1)),
        Some(98.8),
        "12 345 octets en 1 s = 98,76 kbit/s, arrondi à 98,8 — pas 98,0"
    );
}

/// Le cas nominal : un FLAC stéréo 16/44,1 tourne autour de 1 000 kbit/s.
#[test]
fn un_flac_s_annonce_a_son_vrai_debit() {
    assert_eq!(
        debit_observe_kbps(1_000_000, Duration::from_secs(8)),
        Some(1000.0),
        "1 Mo en 8 s = 1 000 kbit/s"
    );
}
