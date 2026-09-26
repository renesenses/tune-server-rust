//! #5034 — la règle, isolée du scan. Les passes réelles (scan rapide,
//! « Répertoires », complet, démarrage, surveillant) sont éprouvées dans
//! `tune-server/src/pochettes_disque_tests_5034.rs`.
use super::*;

fn lue(condensat: &str, source: SourcePochette) -> PochetteLue {
    PochetteLue {
        condensat: condensat.into(),
        source,
        fichier: "/musique/Album/01.flac".into(),
        empreinte: Some("1:2".into()),
    }
}

fn etat(cover: Option<&str>, source: Option<SourcePochette>) -> EtatPochette {
    EtatPochette {
        cover_path: cover.map(Into::into),
        source,
        fichier: None,
        empreinte: None,
    }
}

/// Le tableau de `arbitrer`, ligne à ligne.
#[test]
fn la_regle_retire_seulement_ce_qui_vient_du_disque_5034() {
    use SourcePochette::*;
    let a = lue("aaa", Integree);
    // (pochette en place, source, lue, complet, du_disque, source partie) → geste
    let cas: Vec<(
        Option<&str>,
        Option<SourcePochette>,
        Option<&PochetteLue>,
        bool,
        bool,
        bool,
        Geste,
    )> = vec![
        (
            None,
            None,
            Some(&a),
            false,
            false,
            false,
            Geste::Poser(a.clone()),
        ),
        (None, None, None, false, false, false, Geste::Garder),
        // Téléversée : jamais touchée, même par un scan complet.
        (
            Some("up"),
            Some(Televersee),
            None,
            true,
            false,
            true,
            Geste::Garder,
        ),
        (
            Some("up"),
            Some(Televersee),
            Some(&a),
            true,
            false,
            true,
            Geste::Garder,
        ),
        // Fournisseur : jamais retirée ; remplacée seulement par un complet.
        (
            Some("f"),
            Some(Fournisseur),
            None,
            true,
            false,
            true,
            Geste::Garder,
        ),
        (
            Some("f"),
            Some(Fournisseur),
            Some(&a),
            false,
            false,
            false,
            Geste::Garder,
        ),
        (
            Some("f"),
            Some(Fournisseur),
            Some(&a),
            true,
            false,
            false,
            Geste::Poser(a.clone()),
        ),
        (
            Some("f"),
            Some(Importee),
            None,
            false,
            false,
            true,
            Geste::Garder,
        ),
        // Du disque, source partie : retirée, ou remplacée par ce qui reste.
        (
            Some("old"),
            Some(Integree),
            None,
            false,
            true,
            true,
            Geste::Retirer,
        ),
        (
            Some("old"),
            Some(Dossier),
            None,
            true,
            true,
            true,
            Geste::Retirer,
        ),
        (
            Some("old"),
            Some(Integree),
            Some(&a),
            false,
            true,
            true,
            Geste::Poser(a.clone()),
        ),
        // Du disque, source là mais image CHANGÉE : suivie, même en passe
        // automatique (décision du 25/09/2026, #5034 point 1).
        (
            Some("old"),
            Some(Dossier),
            Some(&a),
            false,
            true,
            false,
            Geste::Poser(a.clone()),
        ),
        (
            Some("old"),
            None,
            Some(&a),
            false,
            true,
            false,
            Geste::Poser(a.clone()),
        ),
        // Inconnue, non prouvée, autre image : gardée hors scan complet.
        (
            Some("old"),
            None,
            Some(&a),
            false,
            false,
            false,
            Geste::Garder,
        ),
        (
            Some("old"),
            None,
            Some(&a),
            true,
            false,
            false,
            Geste::Poser(a.clone()),
        ),
        // Même image : confirmée.
        (
            Some("aaa"),
            Some(Dossier),
            Some(&a),
            false,
            true,
            false,
            Geste::Poser(a.clone()),
        ),
        // Inconnue, non prouvée : gardée.
        (Some("old"), None, None, false, false, true, Geste::Garder),
        (Some("old"), None, None, true, false, true, Geste::Garder),
        // Inconnue mais prouvée (adresse héritée) : retirée.
        (Some("old"), None, None, false, true, true, Geste::Retirer),
    ];
    for (i, (cover, source, l, complet, dd, partie, attendu)) in cas.into_iter().enumerate() {
        assert_eq!(
            arbitrer(&etat(cover, source), l, complet, dd),
            attendu,
            "ligne {i} : {cover:?} {source:?} lue={:?} complet={complet} du_disque={dd} partie={partie}",
            l.map(|x| &x.condensat)
        );
    }
}

/// Une ligne d'avant la migration n'est « du disque » que PROUVÉE.
#[test]
fn une_source_inconnue_n_est_du_disque_que_prouvee_5034() {
    let pistes = [PathBuf::from("/musique/Album/01.flac")];
    let heritee = artwork_hash("/musique/Album/01.flac");
    let heritee_dossier = artwork_hash("/musique/Album/Cover.jpg");
    let a = lue("aaa", SourcePochette::Dossier);
    assert!(vient_du_disque(&etat(Some(&heritee), None), None, &pistes));
    assert!(vient_du_disque(
        &etat(Some(&heritee_dossier), None),
        None,
        &pistes
    ));
    assert!(vient_du_disque(&etat(Some("aaa"), None), Some(&a), &pistes));
    assert!(
        !vient_du_disque(&etat(Some("televersee"), None), None, &pistes),
        "un condensat de contenu inconnu ne prouve rien"
    );
    assert!(!vient_du_disque(
        &etat(Some(&artwork_hash("album-upload-7")), None),
        None,
        &pistes
    ));
    assert!(!vient_du_disque(
        &etat(Some(&heritee), Some(SourcePochette::Televersee)),
        None,
        &pistes
    ));
}

#[test]
fn les_noms_d_images_de_pochette_5034() {
    for n in [
        "cover.jpg",
        "Cover.JPG",
        "folder.png",
        "FRONT.jpeg",
        "album.jpg",
    ] {
        assert!(
            est_une_image_de_pochette(Path::new(&format!("/m/A/{n}"))),
            "{n}"
        );
    }
    for n in [
        "artist.jpg",
        "back.jpg",
        "01.flac",
        "cover.jpg.part",
        "scan.png",
    ] {
        assert!(
            !est_une_image_de_pochette(Path::new(&format!("/m/A/{n}"))),
            "{n}"
        );
    }
}

#[test]
fn sous_le_dossier_coupe_aux_composants_5034() {
    assert!(sous_le_dossier("/m/Album/cover.jpg", "/m/Album"));
    assert!(sous_le_dossier("/m/Album/cover.jpg", "/m/Album/"));
    assert!(sous_le_dossier(r"D:\m\Album\cover.jpg", r"D:\m\Album"));
    assert!(!sous_le_dossier("/m/Album 2/cover.jpg", "/m/Album"));
    assert!(!sous_le_dossier("/m/Album", "/m/Album"));
}

/// La SOURCE voyage avec la pochette quand un doublon est absorbé — et
/// seulement quand la pochette voyage.
#[test]
fn l_absorption_emporte_la_source_avec_la_pochette_5034() {
    use crate::db::models::Album;
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: std::sync::Arc<dyn DbBackend> = std::sync::Arc::new(db);
    let repo = AlbumRepo::with_backend(db.clone());
    let cible = repo.create(&Album::new("Cible".into())).unwrap();
    let doublon = repo.create(&Album::new("Doublon".into())).unwrap();
    repo.poser_pochette_du_disque(
        doublon,
        "cd",
        SourcePochette::Dossier,
        "/m/A/cover.jpg",
        Some("1:2"),
    )
    .unwrap();
    repo.absorber(cible, doublon).unwrap();
    let e = repo.etat_pochette(cible).unwrap().unwrap();
    assert_eq!(e.cover_path.as_deref(), Some("cd"));
    assert_eq!(e.source, Some(SourcePochette::Dossier));
    assert_eq!(e.fichier.as_deref(), Some("/m/A/cover.jpg"));

    // La cible a DÉJÀ une pochette (source inconnue) : rien ne passe.
    let cible2 = repo.create(&Album::new("Cible 2".into())).unwrap();
    db.execute(
        "UPDATE albums SET cover_path = 'x' WHERE id = ?",
        &[&cible2],
    )
    .unwrap();
    let doublon2 = repo.create(&Album::new("Doublon 2".into())).unwrap();
    repo.poser_pochette_du_disque(
        doublon2,
        "cd2",
        SourcePochette::Integree,
        "/m/B/01.flac",
        None,
    )
    .unwrap();
    repo.absorber(cible2, doublon2).unwrap();
    let e = repo.etat_pochette(cible2).unwrap().unwrap();
    assert_eq!(e.cover_path.as_deref(), Some("x"));
    assert_eq!(
        e.source, None,
        "la source d'une autre image ne s'invite pas"
    );
}

/// Décision 3 — l'empreinte bon marché d'une jaquette FLAC reconnaît la MÊME
/// image, distingue une autre, et se tait (`None`) quand elle ne sait pas :
/// pas de jaquette, pas un FLAC.
#[test]
fn l_empreinte_d_une_jaquette_flac_reconnait_la_meme_image_5034() {
    use crate::library::artwork::empreinte_jaquette_flac;
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::flac::FlacFile;
    use lofty::ogg::OggPictureStorage;
    use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};

    let dir = tempfile::tempdir().unwrap();
    let gabarit = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
    let avec = |nom: &str, image: Option<&[u8]>| -> PathBuf {
        let p = dir.path().join(nom);
        std::fs::copy(&gabarit, &p).unwrap();
        let mut f = std::fs::File::open(&p).unwrap();
        let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
        drop(f);
        while !flac.pictures().is_empty() {
            flac.remove_picture(0);
        }
        if let Some(o) = image {
            let pic = Picture::unchecked(o.to_vec())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build();
            flac.insert_picture(pic, Some(PictureInformation::default()))
                .unwrap();
        }
        flac.save_to_path(&p, WriteOptions::default()).unwrap();
        p
    };
    let grande_a: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let mut grande_b = grande_a.clone();
    grande_b[150_010] ^= 0xFF; // même longueur, un octet au milieu
    let petite: &[u8] = b"\xFF\xD8\xFF\xE0PETITE";

    let a1 = empreinte_jaquette_flac(&avec("a1.flac", Some(&grande_a)));
    let a2 = empreinte_jaquette_flac(&avec("a2.flac", Some(&grande_a)));
    let b = empreinte_jaquette_flac(&avec("b.flac", Some(&grande_b)));
    let p = empreinte_jaquette_flac(&avec("p.flac", Some(petite)));
    assert!(
        a1.is_some() && p.is_some(),
        "une jaquette doit donner une empreinte"
    );
    assert_eq!(a1, a2, "la même image, deux pistes : même empreinte");
    assert_ne!(a1, p, "une autre longueur : autre empreinte");
    assert_ne!(a1, b, "même longueur, milieu différent : autre empreinte");
    assert_eq!(
        empreinte_jaquette_flac(&avec("sans.flac", None)),
        None,
        "sans jaquette : l'empreinte ne sait pas, l'appelant relit"
    );
    let pas_flac = dir.path().join("x.mp3");
    std::fs::write(&pas_flac, b"ID3\x04\x00\x00\x00\x00\x00\x00").unwrap();
    assert_eq!(empreinte_jaquette_flac(&pas_flac), None);
}
