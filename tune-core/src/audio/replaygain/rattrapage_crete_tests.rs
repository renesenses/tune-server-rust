//! Témoins internes du rattrapage des crêtes vraies (#2713). Le parcours
//! complet, par la cascade, est gardé par `tests/crete_vraie_rattrapage_2713.rs`.

use super::*;
use crate::audio::replaygain::tests::base_avec_piste;

/// Une remesure (#5882) a effacé la mesure PENDANT le décodage : la crête
/// refaite ne s'écrit pas sur une piste qui n'a plus de mesure de Tune — elle
/// y mentirait sur sa provenance. Le report éventuel, lui, s'efface.
#[test]
fn la_crete_ne_s_ecrit_que_sur_une_mesure_de_tune() {
    let (_db, backend) = base_avec_piste("/nulle/part.wav");
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    repo.set(42, PATH_UNRESOLVED_KEY, "1").unwrap();
    ecrire(&backend, 42, &Ecriture::Crete(0.9));
    let m = repo.get_all(42).unwrap();
    assert!(!m.contains_key("rg_track_true_peak"), "{m:?}");
    assert!(!m.contains_key(TRUE_PEAK_ALGO_KEY), "{m:?}");
    assert!(!m.contains_key(PATH_UNRESOLVED_KEY), "{m:?}");

    repo.set(42, TRACK_SOURCE_KEY, SOURCE_ANALYSIS).unwrap();
    ecrire(&backend, 42, &Ecriture::Crete(0.9));
    let m = repo.get_all(42).unwrap();
    assert_eq!(
        m.get("rg_track_true_peak").map(String::as_str),
        Some("0.900000")
    );
    assert_eq!(
        m.get(TRUE_PEAK_ALGO_KEY).map(String::as_str),
        Some(TRUE_PEAK_ALGO)
    );
}

/// La version courante n'est pas celle qu'étiquette la migration : sans quoi
/// rien ne serait jamais rattrapé.
#[test]
fn la_version_courante_n_est_pas_l_ancienne() {
    assert_ne!(TRUE_PEAK_ALGO, ANCIEN_TRUE_PEAK_ALGO);
}
