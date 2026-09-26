//! « Écrire dans les fichiers » — tranche 4 du chantier « édition des albums,
//! compilations et coffrets » (GO de Bertrand du 25/09/2026).
//!
//! Sur un VRAI fichier par format courant, copié des fixtures du dépôt :
//! écrire les balises de l'édition, les RELIRE par le lecteur du scan
//! (`metadata::read_metadata`, celui qui remplit la base), et prouver que
//! l'audio n'a pas bougé de deux façons indépendantes — l'empreinte de la
//! partie audio (SHA-256 des trames / du `mdat` / du bloc `data` / des
//! paquets Ogg) ET le PCM décodé, échantillon pour échantillon.
use std::path::{Path, PathBuf};

use tune_core::metadata::empreinte_audio::empreinte_audio;
use tune_core::metadata::tag_writer::{
    BalisesEdition, DrapeauAEcrire, ecrire_balises_edition, format_balises_edition,
    plan_balises_edition,
};

fn fixture(nom: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom)
}

fn balises() -> BalisesEdition {
    BalisesEdition {
        album: "Köln Concert".into(),
        artiste_album: Some("Keith Jarrett".into()),
        disque: 2,
        disques: 3,
        nom_disque: Some("Part II".into()),
        piste: 4,
        pistes: 7,
        titre: "Part IIc".into(),
        artiste: Some("Keith Jarrett Trio".into()),
        compilation: Some(DrapeauAEcrire::Vrai),
    }
}

/// Le PCM décodé, pour la contre-épreuve indépendante de l'empreinte.
fn pcm(chemin: &Path) -> Vec<i32> {
    tune_core::audio::decode::decode_to_pcm(chemin.to_str().unwrap(), None, None, 0.0, 0.0)
        .unwrap_or_else(|e| panic!("{} : décodage impossible : {e}", chemin.display()))
        .samples_i32
}

/// Les formats courants : FLAC, MP3 (ID3v2.4), M4A AAC et ALAC, Ogg Vorbis,
/// Opus, WAV et AIFF (ID3 en bloc).
const FORMATS: [&str; 8] = [
    "test.flac",
    "test.mp3",
    "test.m4a",
    "alac/ref_16_44100_stereo.m4a",
    "test_vorbis.ogg",
    "test.opus",
    "test.wav",
    "test.aiff",
];

fn copier(dir: &Path, nom: &str) -> PathBuf {
    let feuille = Path::new(nom).file_name().unwrap();
    let cible = dir.join(feuille);
    std::fs::copy(fixture(nom), &cible).unwrap();
    cible
}

#[test]
fn chaque_format_ecrit_relu_par_le_scan_audio_intact() {
    for nom in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let cible = copier(dir.path(), nom);
        let chemin = cible.to_str().unwrap();
        assert!(format_balises_edition(chemin), "{nom}");

        let empreinte_avant = empreinte_audio(&cible).unwrap().expect(nom);
        let pcm_avant = pcm(&cible);
        assert!(
            !pcm_avant.is_empty(),
            "{nom} : PCM vide, la contre-épreuve ne prouverait rien"
        );

        let ecrits = ecrire_balises_edition(chemin, &balises())
            .unwrap_or_else(|e| panic!("{nom} : écriture refusée : {e}"));
        assert!(!ecrits.is_empty(), "{nom} : rien écrit");

        let m = tune_core::metadata::read_metadata(&cible).expect(nom);
        assert_eq!(m.album.as_deref(), Some("Köln Concert"), "{nom} ALBUM");
        assert_eq!(
            m.album_artist.as_deref(),
            Some("Keith Jarrett"),
            "{nom} ALBUMARTIST"
        );
        assert_eq!(m.title.as_deref(), Some("Part IIc"), "{nom} TITLE");
        assert_eq!(
            m.artist.as_deref(),
            Some("Keith Jarrett Trio"),
            "{nom} ARTIST"
        );
        assert_eq!(m.disc_number, Some(2), "{nom} DISCNUMBER");
        assert_eq!(m.total_discs, Some(3), "{nom} DISCTOTAL");
        assert_eq!(m.track_number, Some(4), "{nom} TRACKNUMBER");
        assert_eq!(m.total_tracks, Some(7), "{nom} TRACKTOTAL");
        assert_eq!(
            m.disc_subtitle.as_deref(),
            Some("Part II"),
            "{nom} DISCSUBTITLE"
        );
        assert_eq!(m.compilation, Some(true), "{nom} COMPILATION");

        assert_eq!(
            empreinte_audio(&cible).unwrap().unwrap(),
            empreinte_avant,
            "{nom} : l'empreinte audio a changé"
        );
        assert_eq!(pcm(&cible), pcm_avant, "{nom} : le PCM décodé a changé");

        // Rien ne traîne dans le dossier : ni copie de travail, ni autre.
        let restes: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .collect();
        assert_eq!(restes.len(), 1, "{nom} : {restes:?}");

        // Idempotence : une seconde écriture ne touche plus au fichier.
        let octets = std::fs::read(&cible).unwrap();
        let encore = ecrire_balises_edition(chemin, &balises()).unwrap();
        assert!(encore.is_empty(), "{nom} : {encore:?}");
        assert_eq!(std::fs::read(&cible).unwrap(), octets, "{nom}");
    }
}

#[test]
fn le_plan_decrit_sans_ecrire() {
    for nom in FORMATS {
        let dir = tempfile::tempdir().unwrap();
        let cible = copier(dir.path(), nom);
        let avant = std::fs::read(&cible).unwrap();
        let plan = plan_balises_edition(cible.to_str().unwrap(), &balises()).unwrap();
        let champs: Vec<&str> = plan.iter().map(|c| c.champ).collect();
        for attendu in [
            "ALBUM",
            "TITLE",
            "DISCNUMBER",
            "DISCSUBTITLE",
            "COMPILATION",
        ] {
            assert!(champs.contains(&attendu), "{nom} : {champs:?}");
        }
        let album = plan.iter().find(|c| c.champ == "ALBUM").unwrap();
        assert_eq!(album.apres.as_deref(), Some("Köln Concert"));
        assert_eq!(
            std::fs::read(&cible).unwrap(),
            avant,
            "{nom} : le plan a écrit"
        );
    }
}

/// COMPILATION : `Retrait` enlève la balise, `Faux` pose 0, `None` n'y touche
/// pas ; un nom de disque effacé en base est retiré du fichier.
#[test]
fn drapeau_de_compilation_et_nom_de_disque_retires() {
    let dir = tempfile::tempdir().unwrap();
    let cible = copier(dir.path(), "test.flac");
    let chemin = cible.to_str().unwrap();
    ecrire_balises_edition(chemin, &balises()).unwrap();

    let mut b = balises();
    b.compilation = None;
    b.nom_disque = None;
    let plan = plan_balises_edition(chemin, &b).unwrap();
    assert_eq!(plan.len(), 1, "{plan:?}");
    assert_eq!(plan[0].champ, "DISCSUBTITLE");
    assert_eq!(plan[0].apres, None);
    ecrire_balises_edition(chemin, &b).unwrap();
    let m = tune_core::metadata::read_metadata(&cible).unwrap();
    assert_eq!(m.disc_subtitle, None);
    assert_eq!(
        m.compilation,
        Some(true),
        "None ne doit pas toucher au drapeau"
    );

    b.compilation = Some(DrapeauAEcrire::Faux);
    ecrire_balises_edition(chemin, &b).unwrap();
    assert_eq!(
        tune_core::metadata::read_metadata(&cible)
            .unwrap()
            .compilation,
        Some(false)
    );

    b.compilation = Some(DrapeauAEcrire::Retrait);
    ecrire_balises_edition(chemin, &b).unwrap();
    assert_eq!(
        tune_core::metadata::read_metadata(&cible)
            .unwrap()
            .compilation,
        None
    );
}

/// Contre-épreuve du garde-fou de format : un DSF (que lofty ne sait pas
/// écrire) est refusé AVANT toute copie, et le fichier ne bouge pas.
#[test]
fn dsf_refuse_sans_toucher() {
    let dir = tempfile::tempdir().unwrap();
    let cible = copier(dir.path(), "dsd/ref_dsd64_stereo.dsf");
    let avant = std::fs::read(&cible).unwrap();
    let r = ecrire_balises_edition(cible.to_str().unwrap(), &balises());
    assert!(r.is_err(), "{r:?}");
    assert_eq!(std::fs::read(&cible).unwrap(), avant);
}
