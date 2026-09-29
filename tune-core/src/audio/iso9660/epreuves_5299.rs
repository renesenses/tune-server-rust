//! #5299 — de bout en bout : une image ISO de données porte un FLAC et un MP3
//! dans des sous-dossiers, un nom long Joliet et une `cover.jpg`. Le parcours
//! les indexe, les balises se lisent, la lecture et le déplacement rendent
//! les mêmes échantillons que le fichier hors de l'image, la pochette est
//! reprise ; une image sans audio reste signalée.

use super::fabrique::{self, Noms};
use super::*;
use std::path::{Path, PathBuf};

const NOM_LONG_FLAC: &str =
    "Les Artistes - Un album/CD1/01 - Un titre bien trop long pour un nom ISO 9660.flac";
const CHEMIN_MP3: &str = "Les Artistes - Un album/CD2/02 - Deuxième.mp3";
const CHEMIN_POCHETTE: &str = "Les Artistes - Un album/CD1/cover.jpg";
const POCHETTE: &[u8] = b"\xFF\xD8\xFF\xE0\x00\x10JFIF-pochette-de-test";

/// Une copie balisée d'une fixture du dépôt.
fn balisee(dossier: &Path, fixture: &str, titre: &str) -> Vec<u8> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, Tag, TagExt};

    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let copie = dossier.join(fixture);
    std::fs::copy(&source, &copie).unwrap();
    let fichier = lofty::read_from_path(&copie).unwrap();
    let mut tag = Tag::new(fichier.primary_tag_type());
    tag.set_title(titre.to_string());
    tag.set_artist("Les Artistes".to_string());
    tag.set_album("Un album".to_string());
    tag.save_to_path(&copie, WriteOptions::default()).unwrap();
    std::fs::read(&copie).unwrap()
}

struct Banc {
    _dossier: tempfile::TempDir,
    racine: PathBuf,
    image: PathBuf,
    flac: Vec<u8>,
    mp3: Vec<u8>,
    /// Les mêmes fichiers, hors de l'image : la référence de la lecture.
    flac_nu: PathBuf,
    mp3_nu: PathBuf,
}

fn banc() -> Banc {
    // Sous `target/`, jamais sous le dossier temporaire du système : le
    // parcours écarte tout ce qui y vit (`is_tune_temp_file`). Nom unique : la
    // machine de compilation est partagée.
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    std::fs::create_dir_all(&parent).unwrap();
    let dossier = tempfile::Builder::new()
        .prefix("tune-iso-donnees-5299-")
        .tempdir_in(&parent)
        .unwrap();
    let travail = dossier.path().join("travail");
    let racine = dossier.path().join("bibliotheque");
    std::fs::create_dir_all(&travail).unwrap();
    std::fs::create_dir_all(&racine).unwrap();
    let flac = balisee(&travail, "test.flac", "Premier titre");
    let mp3 = balisee(&travail, "test.mp3", "Deuxième titre");
    let contenu = vec![
        (NOM_LONG_FLAC.to_string(), flac.clone()),
        (CHEMIN_POCHETTE.to_string(), POCHETTE.to_vec()),
        (CHEMIN_MP3.to_string(), mp3.clone()),
        ("LISEZMOI.TXT".to_string(), b"pas de l'audio".to_vec()),
    ];
    let image = racine.join("Disque de donnees.iso");
    std::fs::write(&image, fabrique::iso(&contenu, Noms::Joliet)).unwrap();
    // Une seconde image, sans aucun fichier audio.
    std::fs::write(
        racine.join("Sauvegarde.iso"),
        fabrique::iso(
            &vec![("DOCS/NOTE.TXT".to_string(), b"note".to_vec())],
            Noms::Joliet,
        ),
    )
    .unwrap();
    Banc {
        flac_nu: travail.join("test.flac"),
        mp3_nu: travail.join("test.mp3"),
        _dossier: dossier,
        racine,
        image,
        flac,
        mp3,
    }
}

fn virtuel(b: &Banc, interne: &str) -> PathBuf {
    PathBuf::from(chemin_virtuel(&b.image, interne))
}

/// Le parcours indexe les fichiers audio de l'image, sous leur chemin virtuel,
/// et laisse la seconde image — sans audio — signalée comme avant.
#[test]
fn le_parcours_indexe_l_audio_d_une_image_de_donnees_5299() {
    let b = banc();
    let r = crate::scanner::walker::list_audio_files(&[b.racine.to_string_lossy().into_owned()]);
    let mut vus: Vec<PathBuf> = r.files.clone();
    vus.sort();
    let mut attendus = vec![virtuel(&b, NOM_LONG_FLAC), virtuel(&b, CHEMIN_MP3)];
    attendus.sort();
    assert_eq!(
        vus, attendus,
        "les pistes de l'image doivent entrer sous `image.iso!/…`, nom long Joliet compris"
    );
    assert_eq!(
        r.skipped_by_ext
            .get(super::super::iso_sacd::CLE_RAPPORT_ISO_DONNEES),
        Some(&1),
        "l'image SANS audio reste comptée et nommée : {:?}",
        r.skipped_by_ext
    );
    assert!(
        r.skipped_paths.iter().any(|p| p.contains("Sauvegarde.iso")),
        "{:?}",
        r.skipped_paths
    );
    assert!(
        !r.skipped_paths
            .iter()
            .any(|p| p.contains("Disque de donnees.iso")),
        "l'image qui porte de l'audio n'est plus écartée : {:?}",
        r.skipped_paths
    );
}

/// Les balises, la taille et l'empreinte se lisent DANS l'image.
#[test]
fn les_metadonnees_se_lisent_dans_l_image_5299() {
    let b = banc();
    let fichiers = vec![virtuel(&b, NOM_LONG_FLAC), virtuel(&b, CHEMIN_MP3)];
    let (lus, stats) = crate::scanner::walker::scan_files_parallel(&fichiers, true, None);
    assert_eq!(stats.metadata_ok, 2, "échecs : {:?}", stats.failed_paths);
    let date_image = std::fs::metadata(&b.image)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    for (lu, (titre, octets, format)) in lus.iter().zip([
        ("Premier titre", &b.flac, "flac"),
        ("Deuxième titre", &b.mp3, "mp3"),
    ]) {
        let meta = lu.metadata.as_ref().unwrap();
        assert_eq!(meta.title.as_deref(), Some(titre), "{}", lu.path);
        assert_eq!(meta.artist.as_deref(), Some("Les Artistes"));
        assert_eq!(meta.album.as_deref(), Some("Un album"));
        assert_eq!(meta.sample_rate, Some(44_100), "{}", lu.path);
        assert!(
            meta.duration_ms.is_some_and(|d| d > 500),
            "durée : {:?}",
            meta.duration_ms
        );
        assert!(
            meta.format.as_deref().is_some_and(|f| f.contains(format)),
            "format : {:?}",
            meta.format
        );
        assert_eq!(
            lu.file_size,
            octets.len() as u64,
            "taille propre du fichier interne"
        );
        assert_eq!(lu.mtime, date_image, "la date est celle de l'image");
        assert!(
            lu.audio_hash.is_some(),
            "l'empreinte se calcule dans l'image"
        );
    }
}

/// La lecture rend, octet pour octet, les échantillons du fichier hors de
/// l'image — du début, puis après un déplacement.
#[test]
fn la_lecture_et_le_deplacement_rendent_les_memes_echantillons_5299() {
    let b = banc();
    for (interne, nu) in [(NOM_LONG_FLAC, &b.flac_nu), (CHEMIN_MP3, &b.mp3_nu)] {
        let v = virtuel(&b, interne);
        let v = v.to_str().unwrap();
        for seek_s in [0.0, 0.5] {
            let dans = crate::audio::decode::decode_to_pcm(v, None, None, seek_s, 0.0)
                .unwrap_or_else(|e| panic!("{interne} à {seek_s} s : {e}"));
            let hors =
                crate::audio::decode::decode_to_pcm(nu.to_str().unwrap(), None, None, seek_s, 0.0)
                    .unwrap();
            assert!(!dans.samples_i32.is_empty(), "{interne} : rien de décodé");
            assert_eq!(
                (dans.sample_rate, dans.channels),
                (hors.sample_rate, hors.channels),
                "{interne}"
            );
            assert!(
                dans.samples_i32 == hors.samples_i32,
                "{interne} à {seek_s} s : {} échantillons dans l'image, {} hors de l'image",
                dans.samples_i32.len(),
                hors.samples_i32.len()
            );
        }
    }
}

/// La résolution de lecture trouve le chemin virtuel ; la pochette du dossier
/// interne est reprise ; les balises ne se réécrivent pas dans une image.
#[test]
fn la_resolution_et_la_pochette_suivent_l_image_5299() {
    let b = banc();
    let flac = virtuel(&b, NOM_LONG_FLAC);
    assert_eq!(
        crate::library::local_path::resolve_existing_local_path(flac.to_str().unwrap()),
        Some(flac.to_string_lossy().into_owned()),
        "la lecture doit trouver la piste rangée dans l'image"
    );
    assert_eq!(
        crate::library::local_path::resolve_existing_local_path(&chemin_virtuel(
            &b.image,
            "Les Artistes - Un album/CD1/absent.flac"
        )),
        None
    );
    let pochette = crate::library::artwork::find_folder_cover(&flac).expect("cover.jpg de CD1");
    assert_eq!(pochette, virtuel(&b, CHEMIN_POCHETTE));
    assert_eq!(
        crate::library::artwork::lire_l_image(&pochette).unwrap(),
        POCHETTE
    );
    // CD2 n'a pas de pochette, ni la racine de l'image, ni le dossier de l'image.
    assert_eq!(
        crate::library::artwork::find_folder_cover(&virtuel(&b, CHEMIN_MP3)),
        None
    );
    std::fs::write(b.racine.join("folder.jpg"), b"a cote").unwrap();
    assert_eq!(
        crate::library::artwork::find_folder_cover(&virtuel(&b, CHEMIN_MP3)),
        Some(b.racine.join("folder.jpg")),
        "à défaut, la pochette posée à côté de l'image"
    );
}

/// Une image est en lecture seule : l'écriture de balises le DIT, et ne
/// touche pas à l'image.
#[tokio::test]
async fn les_balises_ne_se_reecrivent_pas_dans_une_image_5299() {
    let b = banc();
    let avant = std::fs::read(&b.image).unwrap();
    let flac = virtuel(&b, NOM_LONG_FLAC);
    let refus = crate::metadata::tag_writer::write_tags(
        flac.to_str().unwrap(),
        &crate::metadata::tag_writer::TagUpdate {
            title: Some("Réécrit".into()),
            ..Default::default()
        },
    )
    .await
    .expect_err("écrire dans une image doit être refusé");
    assert_eq!(
        refus,
        crate::metadata::tag_writer::MOTIF_IMAGE_ISO_LECTURE_SEULE
    );
    assert!(
        std::fs::read(&b.image).unwrap() == avant,
        "l'image a été modifiée"
    );
}

/// Les passes qui écrivent (paroles, « Écrire dans les fichiers ») tiennent
/// une piste d'image pour un format non inscriptible, et s'en abstiennent.
#[test]
fn une_piste_d_image_n_est_pas_un_format_inscriptible_5299() {
    let chemin = chemin_virtuel(Path::new("/m/d.iso"), "A/01.flac");
    assert!(crate::metadata::tag_writer::is_unsupported_format(&chemin));
    assert!(!crate::metadata::tag_writer::format_balises_edition(
        &chemin
    ));
    assert!(!crate::metadata::tag_writer::is_unsupported_format(
        "/m/A/01.flac"
    ));
}
