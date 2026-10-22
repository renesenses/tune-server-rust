//! #6168 — le seuil `mmap` fixé au démarrage rend au noyau les gros tampons
//! libérés, au lieu de les laisser résidents dans les arènes.
//!
//! Le scénario reproduit ce que fait l'analyse ReplayGain : des fils qui
//! allouent, remplissent et libèrent des tampons de quelques dizaines de Mio,
//! en même temps. Avec le seuil DYNAMIQUE d'origine, le premier bloc libéré
//! relève le seuil, et les suivants restent dans les tas des arènes, libres
//! mais résidents (`mallinfo2().fordblks`). Avec le seuil fixé, il ne reste
//! rien.
//!
//! Chaque scénario tourne dans un PROCESSUS fils (le test se relance
//! lui-même) : l'état de l'allocateur est global au processus, et le seuil ne
//! se fixe qu'une fois.
//!
//! Hors Linux + glibc, le test vérifie seulement que rien n'est fait.

const FILS: usize = 8;
const ROLE: &str = "SEUIL_MMAP_6168_ROLE";

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn scenario() -> usize {
    use std::sync::{Arc, Barrier};
    let issue = tune_core::memoire_rendue::fixer_le_seuil_mmap();
    eprintln!("seuil : {}", issue.libelle());
    // Le premier bloc, plus gros, relève le seuil dynamique à sa taille.
    let mut v = vec![1u8; 24 << 20];
    v[12345] = 7;
    drop(std::hint::black_box(v));
    let barriere = Arc::new(Barrier::new(FILS));
    let fils: Vec<_> = (0..FILS)
        .map(|i| {
            let b = barriere.clone();
            std::thread::spawn(move || {
                b.wait();
                for k in 0..3 {
                    let mut t = vec![0u8; (20 << 20) - k * 4096];
                    t[i] = 1;
                    std::hint::black_box(&t);
                }
                b.wait();
            })
        })
        .collect();
    for f in fils {
        f.join().unwrap();
    }
    // SAFETY : lecture seule des compteurs de l'allocateur.
    unsafe { libc::mallinfo2() }.fordblks
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn libres_dans_un_fils(seuil: Option<&str>) -> usize {
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args([
        "--exact",
        "le_seuil_fixe_ne_garde_pas_les_gros_tampons_6168",
        "--nocapture",
    ])
    .env(ROLE, "fils")
    .env_remove("MALLOC_MMAP_THRESHOLD_");
    match seuil {
        Some(v) => cmd.env(tune_core::memoire_rendue::VARIABLE_SEUIL_MMAP, v),
        None => cmd.env_remove(tune_core::memoire_rendue::VARIABLE_SEUIL_MMAP),
    };
    let sortie = cmd.output().expect("relancer le test");
    let texte = String::from_utf8_lossy(&sortie.stdout).into_owned()
        + &String::from_utf8_lossy(&sortie.stderr);
    texte
        .lines()
        .find_map(|l| l.strip_prefix("LIBRES="))
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or_else(|| panic!("le fils n'a rien rendu :\n{texte}"))
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn le_seuil_fixe_ne_garde_pas_les_gros_tampons_6168() {
    if std::env::var_os(ROLE).is_some() {
        println!("LIBRES={}", scenario());
        return;
    }
    let fixe = libres_dans_un_fils(None);
    let coupe = libres_dans_un_fils(Some("0"));
    eprintln!("octets libres gardés : seuil fixé {fixe}, seuil dynamique {coupe}");
    // Témoin : sans le geste, les tampons restent dans les arènes. S'il ne
    // rougit pas ici, le scénario ne mesure plus rien.
    assert!(
        coupe >= FILS * (10 << 20),
        "témoin muet : avec le seuil dynamique, seulement {coupe} octets gardés"
    );
    assert!(
        fixe < 8 << 20,
        "#6168 — seuil fixé, la glibc garde encore {fixe} octets libres dans ses arènes \
         (seuil dynamique : {coupe})"
    );
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
#[test]
fn hors_glibc_le_seuil_n_est_pas_touche_6168() {
    let _ = (FILS, ROLE);
    assert_eq!(
        tune_core::memoire_rendue::fixer_le_seuil_mmap(),
        tune_core::memoire_rendue::IssueSeuilMmap::HorsGlibc
    );
}
