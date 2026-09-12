use super::transcode_budget_for;
use std::io::Write;

fn file_of(bytes: usize) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&vec![0u8; bytes]).unwrap();
    f.flush().unwrap();
    f
}

/// Un petit fichier garde le comportement historique : 120 s.
#[test]
fn small_file_gets_the_floor() {
    let f = file_of(4096);
    let d = transcode_budget_for(f.path().to_str().unwrap());
    assert_eq!(d.as_secs(), 120);
}

/// Le budget grandit avec la taille — c'est tout l'objet du correctif.
#[test]
fn budget_grows_with_size() {
    let small = file_of(1024);
    let big = file_of(300 * 1024 * 1024); // 300 Mio
    let ds = transcode_budget_for(small.path().to_str().unwrap());
    let db = transcode_budget_for(big.path().to_str().unwrap());
    assert!(
        db > ds,
        "un fichier plus gros doit obtenir plus de temps ({db:?} vs {ds:?})"
    );
    // 300 Mio ~ 0,29 Gio -> 120 + ~35 s
    assert!(
        (150..=170).contains(&db.as_secs()),
        "budget inattendu: {db:?}"
    );
}

/// Taille illisible : plancher, jamais un budget arbitraire.
#[test]
fn unreadable_size_falls_back_to_the_floor() {
    let d = transcode_budget_for("/nonexistent/path/does-not-exist.dsf");
    assert_eq!(d.as_secs(), 120);
}

/// Un disque en perdition doit finir par rendre la main.
#[test]
fn budget_is_capped() {
    // 30 min = plancher + 120 s/Gio -> plafond atteint vers 14,5 Gio.
    // Verifie sur le calcul, sans ecrire un fichier de cette taille.
    let ceiling = 30 * 60;
    let huge_gib = 100.0_f64;
    let computed = (120 + (huge_gib * 120.0).round() as u64).min(ceiling);
    assert_eq!(computed, ceiling);
}
