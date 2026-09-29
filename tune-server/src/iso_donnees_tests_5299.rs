//! #5299 — le scan de production sur une image ISO de données : les pistes
//! entrent en base sous leur chemin virtuel, avec leurs balises et la pochette
//! du dossier interne ; un second scan ne les relit pas et ne les élague pas.
//! Base sur fichier, pas `:memory:` : le pool de lecture d'une base en mémoire
//! ne voit pas les écritures du scan.
use super::pochettes_disque_tests_5034::{racine, scan_manuel};
use crate::state::AppState;
use std::path::Path;
use tune_core::audio::iso9660::{self, fabrique};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::artwork::content_hash;

const FLAC: &str =
    "Les Artistes - Un album/CD1/01 - Un titre bien trop long pour un nom ISO 9660.flac";
const MP3: &str = "Les Artistes - Un album/CD1/02 - Deuxième.mp3";
const POCHETTE: &[u8] = b"\xFF\xD8\xFF\xE0\x00\x10JFIF-pochette-5299";

fn balisee(dossier: &Path, fixture: &str, titre: &str, numero: u32) -> Vec<u8> {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, Tag, TagExt};
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures")
        .join(fixture);
    let copie = dossier.join(fixture);
    std::fs::copy(&source, &copie).unwrap();
    let fichier = lofty::read_from_path(&copie).unwrap();
    let mut tag = Tag::new(fichier.primary_tag_type());
    tag.set_title(titre.to_string());
    tag.set_artist("Les Artistes".to_string());
    tag.set_album("Un album".to_string());
    tag.set_track(numero);
    tag.save_to_path(&copie, WriteOptions::default()).unwrap();
    std::fs::read(&copie).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_scan_indexe_une_image_iso_de_donnees_5299() {
    let r = racine("iso-donnees-5299");
    let musique = r.join("musique");
    let travail = r.join("travail");
    std::fs::create_dir_all(&musique).unwrap();
    std::fs::create_dir_all(&travail).unwrap();
    let flac = balisee(&travail, "test.flac", "Premier titre", 1);
    let mp3 = balisee(&travail, "test.mp3", "Deuxième titre", 2);
    let contenu = vec![
        (FLAC.to_string(), flac.clone()),
        (MP3.to_string(), mp3),
        (
            "Les Artistes - Un album/cover.jpg".to_string(),
            POCHETTE.to_vec(),
        ),
    ];
    let image = musique.join("Disque de donnees.iso");
    std::fs::write(&image, fabrique::iso(&contenu, fabrique::Noms::Joliet)).unwrap();

    let etat = AppState::new(&r.join("tune.db").to_string_lossy(), 0, Default::default()).unwrap();
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    scan_manuel(&etat, true, None).await;

    let repo = TrackRepo::with_backend(etat.backend.clone());
    let chemin_flac = iso9660::chemin_virtuel(&image, FLAC);
    let piste = repo
        .get_by_path(&chemin_flac)
        .unwrap()
        .unwrap_or_else(|| panic!("#5299 : {chemin_flac} absent de la base après le scan"));
    assert_eq!(piste.title, "Premier titre");
    assert_eq!(piste.album_title.as_deref(), Some("Un album"));
    assert_eq!(
        piste.file_size,
        Some(flac.len() as i64),
        "taille propre du FLAC interne"
    );
    let deuxieme = repo
        .get_by_path(&iso9660::chemin_virtuel(&image, MP3))
        .unwrap()
        .expect("#5299 : le MP3 de l'image doit être en base");
    assert_eq!(deuxieme.title, "Deuxième titre");
    assert_eq!(
        deuxieme.album_id, piste.album_id,
        "un seul album pour le dossier interne"
    );

    let album = AlbumRepo::with_backend(etat.backend.clone())
        .get(piste.album_id.expect("album"))
        .unwrap()
        .unwrap();
    assert_eq!(
        album.cover_path.as_deref(),
        Some(content_hash(POCHETTE).as_str()),
        "#5299 : la pochette rangée dans l'image doit être reprise"
    );

    // Le scan rapide suivant ne relit pas les pistes inchangées…
    let carte = repo.get_all_file_info_by_path().unwrap();
    assert!(
        !crate::routes::system::scan::file_needs_scan(Path::new(&chemin_flac), &carte),
        "#5299 : une piste d'image inchangée serait relue à chaque scan"
    );
    // … et ne les élague pas.
    scan_manuel(&etat, false, None).await;
    assert_eq!(
        repo.get_by_path(&chemin_flac).unwrap().map(|t| t.id),
        Some(piste.id),
        "#5299 : le second scan a supprimé ou recréé la piste de l'image"
    );
}
