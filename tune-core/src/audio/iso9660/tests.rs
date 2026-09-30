//! Épreuves du lecteur d'images (#5299), sur des images fabriquées ici.

use super::fabrique::{self, Contenu, Noms};
use super::*;
use std::io::{Read, Seek, SeekFrom};

/// Octets reconnaissables : chaque position porte une valeur différente.
fn motif(n: usize, graine: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u32 * 7 + graine as u32) as u8 ^ (i >> 11) as u8)
        .collect()
}

fn contenu_de_test() -> Contenu {
    vec![
        (
            "Album A/01 - Une piste au nom bien trop long pour ISO.flac".into(),
            motif(5000, 1),
        ),
        ("Album A/cover.jpg".into(), b"\xFF\xD8\xFF\xE0JPEG".to_vec()),
        ("Album B/Disque 2/02 - Autre.mp3".into(), motif(3000, 2)),
        ("LISEZMOI.TXT".into(), b"bonjour".to_vec()),
    ]
}

fn ecrire(dossier: &Path, nom: &str, octets: &[u8]) -> PathBuf {
    let p = dossier.join(nom);
    std::fs::write(&p, octets).unwrap();
    p
}

#[test]
fn joliet_rend_les_noms_longs_et_les_sous_dossiers() {
    let d = tempfile::tempdir().unwrap();
    let image = ecrire(
        d.path(),
        "donnees.iso",
        &fabrique::iso(&contenu_de_test(), Noms::Joliet),
    );
    let index = lire_index(&image).unwrap();
    assert_eq!(index.systeme, Systeme::Joliet);
    let chemins: Vec<&str> = index.fichiers.iter().map(|f| f.chemin.as_str()).collect();
    assert_eq!(
        chemins,
        vec![
            "Album A/01 - Une piste au nom bien trop long pour ISO.flac",
            "Album A/cover.jpg",
            "Album B/Disque 2/02 - Autre.mp3",
            "LISEZMOI.TXT",
        ],
        "Joliet doit rendre les noms longs, la casse et l'arborescence"
    );
}

#[test]
fn sans_joliet_les_noms_iso_nus_sont_rendus() {
    let d = tempfile::tempdir().unwrap();
    let image = ecrire(
        d.path(),
        "nus.iso",
        &fabrique::iso(&contenu_de_test(), Noms::Nus),
    );
    let index = lire_index(&image).unwrap();
    assert_eq!(index.systeme, Systeme::Iso9660);
    assert!(
        index
            .fichiers
            .iter()
            .any(|f| f.chemin == "ALBUM_A/01___UNE.FLA"),
        "noms nus : {:?}",
        index.fichiers.iter().map(|f| &f.chemin).collect::<Vec<_>>()
    );
}

#[test]
fn rock_ridge_rend_les_noms_posix() {
    let d = tempfile::tempdir().unwrap();
    let image = ecrire(
        d.path(),
        "rr.iso",
        &fabrique::iso(&contenu_de_test(), Noms::RockRidge),
    );
    let index = lire_index(&image).unwrap();
    assert_eq!(index.systeme, Systeme::RockRidge);
    assert!(
        index.trouver("Album B/Disque 2/02 - Autre.mp3").is_some(),
        "Rock Ridge : {:?}",
        index.fichiers.iter().map(|f| &f.chemin).collect::<Vec<_>>()
    );
}

#[test]
fn udf_seul_est_lu() {
    let d = tempfile::tempdir().unwrap();
    let contenu = contenu_de_test();
    let image = ecrire(d.path(), "udf.iso", &fabrique::udf(&contenu));
    let index = lire_index(&image).unwrap();
    assert_eq!(index.systeme, Systeme::Udf);
    for (chemin, octets) in &contenu {
        let f = index
            .trouver(chemin)
            .unwrap_or_else(|| panic!("{chemin} absent de l'index UDF"));
        assert_eq!(f.taille(), octets.len() as u64);
        let mut lu = Vec::new();
        ouvrir_dans(&image, chemin)
            .unwrap()
            .read_to_end(&mut lu)
            .unwrap();
        assert_eq!(&lu, octets, "{chemin} : octets UDF");
    }
}

/// La lecture est CIBLÉE : chaque octet lu vient de l'étendue du fichier, au
/// bon décalage, y compris après un déplacement arbitraire.
#[test]
fn la_lecture_et_le_deplacement_suivent_l_etendue() {
    let d = tempfile::tempdir().unwrap();
    let contenu = contenu_de_test();
    let image = ecrire(
        d.path(),
        "donnees.iso",
        &fabrique::iso(&contenu, Noms::Joliet),
    );
    let (chemin, attendu) = &contenu[0];
    let mut l = ouvrir(&chemin_virtuel(&image, chemin)).unwrap();
    assert_eq!(l.taille(), attendu.len() as u64);
    let mut tout = Vec::new();
    l.read_to_end(&mut tout).unwrap();
    assert_eq!(&tout, attendu, "lecture entière");

    for &(pos, n) in &[
        (0usize, 16usize),
        (2047, 3),
        (2048, 100),
        (4990, 50),
        (1234, 1),
    ] {
        l.seek(SeekFrom::Start(pos as u64)).unwrap();
        let mut b = vec![0u8; n];
        let lu = l.read(&mut b).unwrap();
        let fin = (pos + n).min(attendu.len());
        assert_eq!(&b[..lu], &attendu[pos..fin], "à {pos}");
    }
    l.seek(SeekFrom::End(-10)).unwrap();
    let mut b = [0u8; 64];
    let n = l.read(&mut b).unwrap();
    assert_eq!(&b[..n], &attendu[attendu.len() - 10..], "depuis la fin");
    assert_eq!(
        l.read(&mut b).unwrap(),
        0,
        "rien au-delà de la fin du fichier"
    );
    l.seek(SeekFrom::Current(-3)).unwrap();
    let n = l.read(&mut b).unwrap();
    assert_eq!(n, 3);
}

/// Un fichier en plusieurs étendues se lit d'un seul tenant.
#[test]
fn plusieurs_etendues_se_lisent_d_un_tenant() {
    let d = tempfile::tempdir().unwrap();
    let octets = motif(10_000, 9);
    let image = ecrire(d.path(), "brut.bin", &octets);
    let f = FichierInterne {
        chemin: "x".into(),
        etendues: vec![
            Etendue {
                debut: 6000,
                longueur: 1000,
            },
            Etendue {
                debut: 100,
                longueur: 500,
            },
            Etendue {
                debut: 9000,
                longueur: 1000,
            },
        ],
    };
    let mut l = LecteurInterne::nouveau(File::open(&image).unwrap(), &f);
    let mut lu = Vec::new();
    l.read_to_end(&mut lu).unwrap();
    let mut attendu = octets[6000..7000].to_vec();
    attendu.extend_from_slice(&octets[100..600]);
    attendu.extend_from_slice(&octets[9000..10000]);
    assert_eq!(lu, attendu);
    l.seek(SeekFrom::Start(995)).unwrap();
    let mut b = [0u8; 10];
    let mut tout = Vec::new();
    while tout.len() < 10 {
        let n = l.read(&mut b[..10 - tout.len()]).unwrap();
        tout.extend_from_slice(&b[..n]);
    }
    assert_eq!(&tout[..5], &octets[6995..7000]);
    assert_eq!(&tout[5..], &octets[100..105]);
}

#[test]
fn le_chemin_virtuel_se_decoupe() {
    assert_eq!(
        decouper("/m/Disque.ISO!/A/b.flac"),
        Some((PathBuf::from("/m/Disque.ISO"), "A/b.flac".to_string()))
    );
    assert_eq!(
        decouper(r"C:\m\d.iso!\A\b.flac"),
        Some((PathBuf::from(r"C:\m\d.iso"), "A/b.flac".to_string()))
    );
    assert_eq!(decouper("/m/Wow!/b.flac"), None);
    assert_eq!(decouper("/m/d.iso"), None);
    assert_eq!(decouper("/m/d.iso!/"), None);
    let v = chemin_virtuel(Path::new("/m/d.iso"), "A/b.flac");
    assert_eq!(v, "/m/d.iso!/A/b.flac");
    assert_eq!(Path::new(&v).extension().unwrap(), "flac");
}

#[test]
fn contenu_audio_trie_les_extensions_et_la_pochette() {
    let d = tempfile::tempdir().unwrap();
    let mut contenu = contenu_de_test();
    contenu.push(("Album C/piste.dsf".into(), motif(100, 3)));
    let image = ecrire(
        d.path(),
        "donnees.iso",
        &fabrique::iso(&contenu, Noms::Joliet),
    );
    let c = contenu_audio(&image).unwrap();
    let pistes: Vec<String> = c
        .pistes
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        pistes,
        vec![
            chemin_virtuel(
                &image,
                "Album A/01 - Une piste au nom bien trop long pour ISO.flac"
            ),
            chemin_virtuel(&image, "Album B/Disque 2/02 - Autre.mp3"),
            // #5299 — le DSF se lit désormais dans l'image : il devient piste.
            chemin_virtuel(&image, "Album C/piste.dsf"),
        ]
    );
    assert!(c.ecartes.is_empty(), "{:?}", c.ecartes);
    let pochette = chemin_de_pochette(Path::new(&pistes[0])).expect("cover.jpg d'Album A");
    assert_eq!(
        pochette.to_string_lossy(),
        chemin_virtuel(&image, "Album A/cover.jpg")
    );
    assert_eq!(
        lire_si_virtuel(&pochette).unwrap().unwrap(),
        b"\xFF\xD8\xFF\xE0JPEG".to_vec()
    );
    assert_eq!(
        chemin_de_pochette(Path::new(&pistes[1])),
        None,
        "pas de pochette dans Album B ni à la racine"
    );
    // Celle d'un coffret, posée au-dessus des disques, vaut pour chacun.
    let mut coffret = contenu_de_test();
    coffret.push(("Album B/folder.jpg".into(), b"coffret".to_vec()));
    let image_coffret = ecrire(
        d.path(),
        "coffret.iso",
        &fabrique::iso(&coffret, Noms::Joliet),
    );
    assert_eq!(
        chemin_de_pochette(Path::new(&chemin_virtuel(
            &image_coffret,
            "Album B/Disque 2/02 - Autre.mp3"
        ))),
        Some(PathBuf::from(chemin_virtuel(
            &image_coffret,
            "Album B/folder.jpg"
        )))
    );
    assert!(
        lire_si_virtuel(&image).is_none(),
        "l'image elle-même n'est pas un chemin virtuel"
    );
    let (taille, _) = taille_et_mtime(Path::new(&pistes[0])).unwrap();
    assert_eq!(taille, 5000, "taille propre du fichier interne");
}

#[test]
fn une_image_sans_systeme_de_fichiers_est_une_erreur() {
    let d = tempfile::tempdir().unwrap();
    let image = ecrire(d.path(), "vide.iso", &vec![0u8; 40 * 2048]);
    assert!(lire_index(&image).is_err());
    let courte = ecrire(d.path(), "courte.iso", b"abc");
    assert!(lire_index(&courte).is_err());
}

/// Une image forgée — répertoire qui se contient lui-même, étendue hors de
/// l'image — rend une erreur ou un index borné, jamais une boucle.
#[test]
fn une_image_forgee_ne_boucle_pas() {
    let d = tempfile::tempdir().unwrap();
    let mut octets = fabrique::iso(&contenu_de_test(), Noms::Nus);
    // Taille de fichier démesurée pour le premier fichier trouvé.
    let taille = octets.len();
    octets.truncate(taille - 2048);
    let image = ecrire(d.path(), "abimee.iso", &octets);
    let _ = lire_index(&image);
}
