//! #4295 (fil 2006) — un arrêt pendant le décodage d'un flux compressé chargé
//! en entier rend la main tout de suite.
//!
//! La branche compressée de `play_url` (M4A, Ogg, AAC… et, jusqu'à la
//! v0.9.168, FLAC et MP3) lit tout le corps HTTP, puis décode la piste
//! entière. Le décodage ne regardait pas `force_silent` : un `stop()` tombé
//! pendant ce temps attendait 2 000 ms, détachait le fil
//! (`local_audio_stop_thread_detached`), et la lecture suivante attendait
//! encore 1 500 ms le PCM `hw:` exclusif que ce fil allait ouvrir une fois la
//! piste décodée (`local_audio_ouverture_forcee_le_fil_precedent_tient_encore`).
//!
//! Contre-épreuve : retirer la relecture de `arret` dans la boucle de
//! `decode_compressed_stream` fait rougir
//! `un_arret_deja_leve_ne_decode_pas_la_piste` ; retirer la porte
//! `ouverture_encore_voulue` qui suit le décodage fait rougir
//! `la_branche_compressee_n_ouvre_pas_le_peripherique_apres_un_arret`.

use std::sync::atomic::AtomicBool;

use super::decode_compressed_stream;

const FLAC_44K: &[u8] = include_bytes!("../../../tests/fixtures/flac/ref_16_44100_stereo.flac");

/// TÉMOIN VERT : sans arrêt, la piste se décode comme avant.
#[test]
fn sans_arret_la_piste_se_decode_en_entier() {
    let (canaux, cadence, echantillons) =
        decode_compressed_stream(FLAC_44K, &AtomicBool::new(false))
            .expect("le FLAC de référence se décode")
            .expect("aucun arrêt n'a été demandé : la piste doit être rendue");
    assert_eq!((canaux, cadence), (2, 44_100));
    assert!(!echantillons.is_empty());
}

/// LA propriété : l'arrêt est relu par le décodeur. Sans lui, la piste entière
/// est décodée quoi qu'il arrive et le fil tient la sortie le temps qu'il faut.
#[test]
fn un_arret_deja_leve_ne_decode_pas_la_piste() {
    let resultat = decode_compressed_stream(FLAC_44K, &AtomicBool::new(true));
    assert!(
        matches!(resultat, Ok(None)),
        "un arrêt levé doit interrompre le décodage (Ok(None)) ; le décodeur a \
         rendu {:?} : il a décodé la piste sans regarder `force_silent`",
        resultat.map(|o| o.map(|(c, s, e)| (c, s, e.len())))
    );
}

/// Un arrêt n'est pas un échec : il ne doit rien écrire dans `open_failure`.
/// Le bras `Ok(None)` de `play_url` ne nomme donc aucun motif.
#[test]
fn un_arret_n_est_pas_un_echec_de_decodage() {
    assert!(decode_compressed_stream(FLAC_44K, &AtomicBool::new(true)).is_ok());
}

/// Garde de site : entre le décodage et la recherche du périphérique, la
/// branche compressée passe la même porte que le chemin PCM.
#[test]
fn la_branche_compressee_n_ouvre_pas_le_peripherique_apres_un_arret() {
    const LOCAL: &str = include_str!("../local.rs");
    let appel = LOCAL
        .find("decode_compressed_stream(&all_data, &force_silent)")
        .expect("le site d'appel du décodeur compressé a disparu de local.rs");
    let reste = &LOCAL[appel..];
    let ouverture = reste
        .find("find_device_with_fallback(")
        .expect("la branche compressée ne cherche plus son périphérique");
    let entre = &reste[..ouverture];
    assert!(
        entre.contains("Ok(None) =>"),
        "le bras d'arrêt du décodage a disparu :\n{entre}"
    );
    assert!(
        entre.contains("ouverture_encore_voulue("),
        "la branche compressée ouvre le périphérique sans vérifier qu'un arrêt \
         n'est pas tombé pendant le décodage :\n{entre}"
    );
}
