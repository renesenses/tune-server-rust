//! Épreuves du socle « source distante » (#8189), sur le faux système en
//! mémoire. Le registre est celui du processus : chaque épreuve prend son
//! propre identifiant de source.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use super::memoire::SystemeMemoire;
use super::*;

fn source(id: &str) -> Arc<SystemeMemoire> {
    let m = Arc::new(SystemeMemoire::nouveau());
    enregistrer(id, m.clone());
    m
}

/// Le FLAC de référence, suivi de `remplissage` octets nuls : un fichier
/// « gros » dont les étiquettes tiennent dans le premier bloc.
fn flac_alourdi(remplissage: usize) -> Vec<u8> {
    let mut v =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac"))
            .unwrap();
    v.extend(std::iter::repeat_n(0u8, remplissage));
    v
}

#[test]
fn chemins_virtuels() {
    assert_eq!(
        chemin_distant("nas1", "/A/b.flac"),
        "tune-src://nas1/A/b.flac"
    );
    assert_eq!(chemin_distant("nas1", ""), "tune-src://nas1");
    assert_eq!(
        decouper("tune-src://nas1/A/b.flac"),
        Some(("nas1".into(), "A/b.flac".into()))
    );
    assert_eq!(
        decouper("tune-src://nas1"),
        Some(("nas1".into(), String::new()))
    );
    assert_eq!(
        decouper("tune-src:\\\\nas1\\A\\b.flac"),
        Some(("nas1".into(), "A/b.flac".into())),
        "graphie renormalisée par Windows"
    );
    assert_eq!(
        decouper("tune-src://nas1/A/../../etc/passwd"),
        None,
        "jamais hors de la racine"
    );
    assert_eq!(decouper("tune-src://n a s/x"), None);
    assert_eq!(decouper("/Musique/A/b.flac"), None);
    assert!(!est_chemin_distant("/x/tune-src://y"));
    // Les chemins virtuels de source passent par les points d'entrée des images.
    assert!(crate::audio::iso9660::est_chemin_virtuel(
        "tune-src://nas1/A/b.flac"
    ));
}

/// La lecture par plages : les étiquettes d'un fichier de 8 Mio se lisent en
/// un ou deux blocs, jamais en copiant le fichier.
#[test]
fn etiquettes_lues_par_plages_sans_copie_complete() {
    let m = source("epreuve-plages");
    let octets = flac_alourdi(8 * 1024 * 1024);
    let taille = octets.len() as u64;
    m.ajouter("Album/01.flac", octets, 1_700_000_000);
    let chemin = "tune-src://epreuve-plages/Album/01.flac";

    let (t, mtime) = crate::audio::iso9660::taille_et_mtime(Path::new(chemin)).unwrap();
    assert_eq!(t, taille);
    assert_eq!(mtime as i64, 1_700_000_000);

    let meta = crate::metadata::read_metadata(Path::new(chemin))
        .expect("les étiquettes d'un fichier distant doivent se lire");
    assert!(meta.duration_ms.unwrap_or(0) > 0, "durée lue : {meta:?}");
    assert!(meta.sample_rate.unwrap_or(0) > 0);
    let lus = m.octets_lus();
    assert!(lus > 0);
    assert!(
        lus <= 4 * lecteur::BLOC,
        "{lus} octets lus pour les étiquettes d'un fichier de {taille} : copie complète ?"
    );
}

#[test]
fn le_lecteur_distant_lit_et_se_deplace_comme_un_fichier() {
    let m = source("epreuve-lecteur");
    let octets: Vec<u8> = (0..(3 * lecteur::BLOC as usize + 123))
        .map(|i| (i % 251) as u8)
        .collect();
    m.ajouter("x.bin", octets.clone(), 1);
    let mut f = crate::audio::iso9660::ouvrir_fichier("tune-src://epreuve-lecteur/x.bin").unwrap();
    assert_eq!(f.longueur().unwrap(), octets.len() as u64);

    // Fin du fichier d'abord (les étiquettes d'un MP3 ou d'un MP4).
    f.seek(SeekFrom::End(-50)).unwrap();
    let mut fin = Vec::new();
    f.read_to_end(&mut fin).unwrap();
    assert_eq!(fin, octets[octets.len() - 50..]);
    assert!(m.octets_lus() < lecteur::BLOC, "un seul bloc, et partiel");

    // Puis tout, en une lecture séquentielle : chaque bloc une fois.
    f.seek(SeekFrom::Start(0)).unwrap();
    let mut tout = Vec::new();
    f.read_to_end(&mut tout).unwrap();
    assert_eq!(tout, octets);
    assert!(m.octets_lus() <= octets.len() as u64 + lecteur::BLOC);

    // Retour court en arrière : servi par le tampon, sans nouvelle demande.
    let avant = m.lectures();
    f.seek(SeekFrom::Current(-10)).unwrap();
    let mut dix = [0u8; 10];
    f.read_exact(&mut dix).unwrap();
    assert_eq!(dix, octets[octets.len() - 10..]);
    assert_eq!(m.lectures(), avant, "relu dans le tampon");
}

/// Symphonia ouvre un fichier distant par le même point d'entrée que les
/// images ISO, et en reconnaît le format.
#[test]
fn symphonia_ouvre_un_fichier_distant() {
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let m = source("epreuve-symphonia");
    m.ajouter("a.flac", flac_alourdi(0), 1);
    let src = crate::audio::iso9660::source_symphonia("tune-src://epreuve-symphonia/a.flac")
        .unwrap()
        .unwrap();
    let mss = MediaSourceStream::new(src, Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("format reconnu");
    assert!(format.default_track(TrackType::Audio).is_some());
    assert!(format.next_packet().is_ok(), "un paquet se lit");
}

#[test]
fn source_absente_ou_coupee_rend_la_piste_indisponible() {
    let chemin = "tune-src://epreuve-jamais-enregistree/a.flac";
    assert!(crate::audio::iso9660::taille_et_mtime(Path::new(chemin)).is_none());
    let e = crate::audio::iso9660::ouvrir_fichier(chemin).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);

    let m = source("epreuve-coupee");
    m.ajouter("a.flac", vec![1, 2, 3], 1);
    assert!(stat("tune-src://epreuve-coupee/a.flac").is_some());
    m.couper(true);
    assert!(stat("tune-src://epreuve-coupee/a.flac").is_none());
    retirer("epreuve-coupee");
    assert!(obtenir("epreuve-coupee").is_none());
}

fn arbre(m: &SystemeMemoire) {
    for d in ["A", "B", "C", "D"] {
        for n in 1..=3 {
            m.ajouter(&format!("Musique/{d}/0{n}.flac"), vec![0; 10], 1);
        }
        m.ajouter(&format!("Musique/{d}/notes.txt"), vec![0; 3], 1);
    }
    m.ajouter("Musique/@eaDir/x.flac", vec![0; 3], 1);
    m.ajouter("Musique/A/._01.flac", vec![0; 3], 1);
}

/// Coupé après deux dossiers, le parcours échoue SANS liste partielle et
/// retient ce qu'il a fait ; relancé, il reprend sans relister ces dossiers.
#[test]
fn le_parcours_reprend_apres_une_coupure() {
    let m = source("epreuve-reprise");
    arbre(&m);
    let garder = |n: &str| n.ends_with(".flac");
    let jamais = || false;
    let racine = "tune-src://epreuve-reprise/Musique";

    m.couper_apres_listes(2);
    let e = parcourir_la_racine(racine, &garder, &jamais, std::time::Duration::ZERO).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
    let point = reprise_de("epreuve-reprise").expect("point de reprise retenu");
    assert_eq!(point.dossiers_finis.len(), 2, "{point:?}");

    m.retablir();
    let listes_avant = m.listes();
    let trouves = parcourir_la_racine(racine, &garder, &jamais, std::time::Duration::ZERO).unwrap();
    assert_eq!(
        m.listes() - listes_avant,
        3,
        "5 dossiers (Musique, A à D), 2 repris : 3 listes seulement"
    );
    assert_eq!(trouves.len(), 12, "{trouves:?}");
    assert_eq!(trouves[0], "tune-src://epreuve-reprise/Musique/A/01.flac");
    assert!(
        trouves
            .iter()
            .all(|t| !t.contains("@eaDir") && !t.contains("/._"))
    );
    assert!(
        reprise_de("epreuve-reprise").is_none(),
        "reprise effacée après succès"
    );

    // Un parcours complet suivant relit tout (les fichiers ont pu changer).
    let avant = m.listes();
    parcourir_la_racine(racine, &garder, &jamais, std::time::Duration::ZERO).unwrap();
    assert_eq!(m.listes() - avant, 5);
}

/// Le parcours de bibliothèque prend une racine distante comme un dossier ;
/// injoignable, elle est nommée et PROTÉGÉE (`missing_dirs`), sans fichier.
#[test]
fn le_parcours_de_bibliotheque_lit_une_racine_distante() {
    let m = source("epreuve-walker");
    arbre(&m);
    let racine = "tune-src://epreuve-walker/Musique".to_string();
    let r = crate::scanner::walker::list_audio_files(std::slice::from_ref(&racine));
    assert_eq!(r.files.len(), 12, "{:?}", r.files);
    assert!(r.missing_dirs.is_empty());
    assert!(r.files.iter().all(|f| {
        f.to_string_lossy()
            .starts_with("tune-src://epreuve-walker/Musique/")
    }));

    m.couper(true);
    let r = crate::scanner::walker::list_audio_files(std::slice::from_ref(&racine));
    assert!(r.files.is_empty());
    assert_eq!(
        r.missing_dirs,
        vec![racine],
        "la racine coupée doit être protégée"
    );
}

#[test]
fn la_pochette_du_dossier_distant_est_trouvee() {
    let m = source("epreuve-pochette");
    m.ajouter("Musique/Coffret/CD1/01.flac", vec![0; 4], 1);
    m.ajouter("Musique/Coffret/Cover.JPG", vec![0xFF, 0xD8, 0xFF], 1);
    let p = crate::audio::iso9660::chemin_de_pochette(Path::new(
        "tune-src://epreuve-pochette/Musique/Coffret/CD1/01.flac",
    ))
    .unwrap();
    assert_eq!(
        p,
        Path::new("tune-src://epreuve-pochette/Musique/Coffret/Cover.JPG")
    );
    let octets = crate::audio::iso9660::lire_si_virtuel(&p).unwrap().unwrap();
    assert_eq!(octets, vec![0xFF, 0xD8, 0xFF]);
}

#[test]
fn le_changement_de_reseau_atteint_chaque_source() {
    let m = source("epreuve-reseau");
    let avant = m.reseau_changes();
    reseau_change();
    assert_eq!(m.reseau_changes(), avant + 1);
}

/// Les identifiants : scellés sous la clé de l'hôte, jamais en clair, et un
/// serveur mobile sans clé refuse plutôt que d'écrire une clé sur le disque.
#[test]
fn identifiants_scelles_sous_la_cle_de_l_hote() {
    use identifiants::*;
    let ids = Identifiants {
        utilisateur: "bertrand".into(),
        mot_de_passe: "s3cret-Tres-Long".into(),
        domaine: None,
    };
    assert!(
        !format!("{ids:?}").contains("s3cret"),
        "Debug masque le mot de passe"
    );

    oublier_la_cle_de_l_hote();
    assert_eq!(sceller(&ids, true), Err(ErreurCoffre::CleDeLHoteAbsente));

    installer_la_cle_de_l_hote([7u8; 32]);
    let scelle = sceller(&ids, true).unwrap();
    assert!(
        !scelle.contains("s3cret") && !scelle.contains("bertrand"),
        "{scelle}"
    );
    assert_eq!(ouvrir(&scelle, true).unwrap(), ids);

    // Une autre clé (autre appareil, trousseau effacé) n'ouvre pas.
    installer_la_cle_de_l_hote([8u8; 32]);
    assert!(matches!(
        ouvrir(&scelle, true),
        Err(ErreurCoffre::Illisible(_))
    ));
    oublier_la_cle_de_l_hote();
    assert_eq!(ouvrir(&scelle, true), Err(ErreurCoffre::CleDeLHoteAbsente));
}
