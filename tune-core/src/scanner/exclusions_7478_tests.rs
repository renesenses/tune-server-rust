//! #7478 — motifs glob par emplacement, à côté des sous-chaînes globales.

use super::*;

fn regle(root: &str, motifs: &[&str], casse: bool) -> ExclusionsDEmplacement {
    ExclusionsDEmplacement {
        root: root.into(),
        patterns: motifs.iter().map(|m| m.to_string()).collect(),
        case_sensitive: casse,
    }
}

#[test]
fn un_motif_de_dossier_n_ecarte_que_sous_son_emplacement_7478() {
    let e = Exclusions::new(
        &[],
        &[regle("/mnt/nas/Musique", &["**/Incoming/**"], false)],
    );
    assert!(e.exclut("/mnt/nas/Musique/Incoming/a.flac"), "à la racine");
    assert!(
        e.exclut("/mnt/nas/Musique/Jazz/Incoming/b/c.flac"),
        "en profondeur"
    );
    assert!(
        e.exclut("/mnt/nas/Musique/Jazz/Incoming"),
        "le dossier lui-même"
    );
    assert!(!e.exclut("/mnt/nas/Musique/Jazz/IncomingX/c.flac"));
    assert!(
        !e.exclut("/mnt/disque/Incoming/a.flac"),
        "un autre emplacement n'est pas concerné"
    );
    assert!(
        !e.exclut("/mnt/nas/MusiqueBis/Incoming/a.flac"),
        "préfixe de nom ≠ racine"
    );
}

#[test]
fn un_motif_de_nom_vise_toutes_les_profondeurs_7478() {
    let e = Exclusions::new(&[], &[regle("/m", &["*.bak", "@eaDir"], false)]);
    assert!(e.exclut("/m/a.bak"));
    assert!(
        e.exclut("/m/x/y/piste.BAK"),
        "insensible à la casse par défaut"
    );
    assert!(e.exclut("/m/x/@eaDir/cover.jpg"));
    assert!(!e.exclut("/m/x/piste.flac"));
}

#[test]
fn la_casse_compte_si_on_le_demande_7478() {
    let e = Exclusions::new(&[], &[regle("/m", &["*.bak"], true)]);
    assert!(e.exclut("/m/a.bak"));
    assert!(!e.exclut("/m/a.BAK"));
}

#[test]
fn un_motif_avec_barre_vise_le_chemin_relatif_7478() {
    let e = Exclusions::new(&[], &[regle("/m/", &["Rips/*.wav"], false)]);
    assert!(e.exclut("/m/Rips/a.wav"));
    assert!(!e.exclut("/m/Rips/sous/a.wav"), "`*` ne franchit pas `/`");
    assert!(!e.exclut("/m/Autre/Rips/a.wav"), "ancré à la racine");
}

#[test]
fn les_exclusions_globales_restent_des_sous_chaines_7478() {
    let e = Exclusions::new(&["Incoming".into()], &[]);
    assert!(e.exclut("/n/importe/ou/INCOMING/a.flac"));
    assert!(
        e.exclut_globalement("/x/incomingstuff/a.flac"),
        "sous-chaîne, comme avant"
    );
    assert!(!e.exclut_par_emplacement("/x/incoming/a.flac"));
    assert_eq!(e.globales(), ["incoming"]);
}

#[test]
fn chemins_windows_7478() {
    let e = Exclusions::new(&[], &[regle(r"D:\Musique", &["**/Incoming/**"], false)]);
    assert!(e.exclut(r"D:\Musique\Jazz\Incoming\a.flac"));
}

#[test]
fn valider_nomme_le_motif_illisible_7478() {
    assert!(valider(&[regle("/m", &["**/ok/**", "*.bak"], false)]).is_ok());
    let e = valider(&[regle("/m", &["[abc"], false)]).unwrap_err();
    assert!(e.contains("[abc"), "{e}");
    assert!(valider(&[regle("  ", &["*.bak"], false)]).is_err());
    assert!(valider(&[regle("/m", &[" "], false)]).is_err());
}

#[test]
fn le_parcours_laisse_de_cote_les_motifs_de_l_emplacement_7478() {
    // PAS sous temp_dir() : `is_tune_temp_file` y écarte tout.
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "target/tune_exclusions_7478_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let (nas, disque) = (base.join("nas"), base.join("disque"));
    for d in [
        nas.join("Jazz/Incoming"),
        nas.join("Jazz/Album"),
        disque.join("Incoming"),
    ] {
        std::fs::create_dir_all(&d).unwrap();
    }
    for f in [
        nas.join("Jazz/Incoming/a.flac"),
        nas.join("Jazz/Album/b.flac"),
        nas.join("Jazz/Album/c.mp3.bak"),
        disque.join("Incoming/d.flac"),
    ] {
        std::fs::write(&f, b"x").unwrap();
    }
    let racines = vec![
        nas.to_string_lossy().into_owned(),
        disque.to_string_lossy().into_owned(),
    ];
    let e = Exclusions::new(
        &[],
        &[regle(&racines[0], &["**/Incoming/**", "*.bak"], false)],
    );
    let r = crate::scanner::walker::list_audio_files_avec_exclusions(
        &racines,
        &e,
        crate::scanner::walker::CADENCE_PROGRESSION_PARCOURS,
        &mut |_| {},
        &|| false,
    )
    .unwrap();
    let mut noms: Vec<String> = r
        .files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    noms.sort();
    assert_eq!(noms, ["b.flac", "d.flac"], "le disque garde son Incoming");
    let _ = std::fs::remove_dir_all(&base);
}
