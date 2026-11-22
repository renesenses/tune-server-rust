//! #8807 — l'audit complet du .18 comptait 4 692 enregistrements `.m4a`
//! (AAC) « corrompus » en `longueur_incorrecte` : « 5823488 trames
//! décodées ; attendues : Some(5823248) ». L'excédent est toujours plus
//! court qu'un paquet AAC (1 024 échantillons) : c'est le bourrage du
//! dernier paquet, que le MP4 de symphonia 0.6 ne rogne pas alors que sa
//! `num_frames` l'exclut. Un vrai manque ou un paquet de trop reste une
//! faute ; le FLAC garde son compte exact.
use super::*;

const M4A: &[u8] = include_bytes!("../../tests/fixtures/test.m4a");

fn auditer(octets: &[u8], nom: &str) -> Resultat {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join(nom);
    std::fs::write(&chemin, octets).unwrap();
    verifier(&chemin, || true).unwrap()
}

#[test]
fn m4a_aac_dont_la_longueur_n_est_pas_un_multiple_de_1024_est_sain() {
    // 44 100 échantillons annoncés : le dernier paquet AAC est bourré.
    let r = auditer(M4A, "temoin.m4a");
    assert_eq!(
        r.verdict,
        Verdict::Sain,
        "le bourrage du dernier paquet AAC n'est pas une corruption : {r:?}"
    );
}

#[test]
fn excedent_plus_court_que_le_dernier_paquet_est_du_bourrage() {
    // Les trois relevés du .18 cités dans l'issue.
    assert!(longueur_conforme(Some(5_823_248), 5_823_488, 1024, false));
    assert!(longueur_conforme(Some(11_118_496), 11_118_592, 1024, false));
    assert!(longueur_conforme(Some(6_300_704), 6_301_696, 1024, false));
    assert!(longueur_conforme(Some(4096), 4096, 1024, false));
    assert!(longueur_conforme(None, 7, 0, false));
}

#[test]
fn manque_ou_paquet_de_trop_reste_une_faute() {
    // Contre-épreuve : un échantillon de moins, ou un paquet entier de plus.
    assert!(!longueur_conforme(Some(5_823_248), 5_823_247, 1024, false));
    assert!(!longueur_conforme(
        Some(5_823_248),
        5_823_248 + 1024,
        1024,
        false
    ));
    assert!(!longueur_conforme(Some(4096), 4097, 0, false));
}

#[test]
fn flac_garde_un_compte_exact() {
    assert!(longueur_conforme(Some(12_000), 12_000, 3808, true));
    assert!(!longueur_conforme(Some(12_000), 12_001, 4096, true));
    assert!(!longueur_conforme(Some(12_000), 11_999, 4096, true));
}

#[test]
fn m4a_tronque_n_est_pas_sain() {
    // Le fichier coupé en son milieu ne doit jamais passer pour sain.
    let r = auditer(&M4A[..M4A.len() / 2], "coupe.m4a");
    assert_ne!(r.verdict, Verdict::Sain, "m4a coupé déclaré sain : {r:?}");
}
