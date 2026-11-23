//! Le dépôt public transporte les déclarations opaques du greffon.
use std::collections::BTreeMap;
use std::process::Command;

// État du lot avant le raccordement. Les homonymes publics (API, protocoles,
// documentation) y existent déjà. Aucune nouvelle occurrence n'est acquise
// en déplaçant ou en dupliquant une de ces lignes.
const LOT_PUBLIC: &str = "43372d764c3f667fa43e55ac2ac873c95296534e";
const GARDE: &str = ":(top,exclude)sdk/tune-plugin-surechantillonnage/tests/catalogue_prive.rs";
const PROFILS: &str = r"(^|[^[:alnum:]_])(natural|impact|texture)([^[:alnum:]_]|$)";
const AXES: &str = r#"["'](brand|family|device|interface)["']|[.](brand|family|device|interface)|(^|[[:space:]])(brand|family|device|interface)[[:space:]]*[:,]"#;

type Occurrences = BTreeMap<(Vec<u8>, Vec<u8>), usize>;

fn occurrences(revision: Option<&str>) -> Occurrences {
    let mut git = Command::new("git");
    git.env("LC_ALL", "C").args([
        "grep",
        "--full-name",
        "--no-color",
        "--no-heading",
        "--no-break",
        "--no-line-number",
        "-a",
        "-z",
        "-i",
        "-E",
        "-e",
        PROFILS,
        "-e",
        AXES,
    ]);
    git.args(revision).args(["--", ":(top,glob)**", GARDE]);
    let sortie = git.output().expect("lancer git grep");
    // git grep sort avec 1 quand il n'y a aucune correspondance.
    assert!(
        matches!(sortie.status.code(), Some(0 | 1)),
        "lecture du dépôt et de son commit de référence : {}",
        String::from_utf8_lossy(&sortie.stderr)
    );

    let mut lignes = Occurrences::new();
    let mut donnees = sortie.stdout.as_slice();
    let prefixe = revision.map(|sha| format!("{sha}:")).unwrap_or_default();
    while !donnees.is_empty() {
        let separateur = donnees.iter().position(|b| *b == 0).expect("chemin NUL");
        let chemin = donnees[..separateur]
            .strip_prefix(prefixe.as_bytes())
            .expect("préfixe Git");
        donnees = &donnees[separateur + 1..];
        let fin = donnees
            .iter()
            .position(|b| *b == b'\n')
            .expect("fin de ligne");
        let ligne = donnees[..fin]
            .strip_suffix(b"\r")
            .unwrap_or(&donnees[..fin]);
        *lignes.entry((chemin.to_vec(), ligne.to_vec())).or_default() += 1;
        donnees = &donnees[fin + 1..];
    }
    lignes
}

#[test]
fn aucun_identifiant_du_catalogue_dans_le_contrat_public() {
    let public = occurrences(Some(LOT_PUBLIC));
    for (entree, nombre) in occurrences(None) {
        assert!(
            nombre <= public.get(&entree).copied().unwrap_or_default(),
            "nouvelle occurrence à vérifier dans {}",
            String::from_utf8_lossy(&entree.0)
        );
    }
}
