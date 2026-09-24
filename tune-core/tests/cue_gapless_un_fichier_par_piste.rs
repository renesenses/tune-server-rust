//! Une feuille CUE « gapless » — un `FILE` par piste — jusqu'en base.
//!
//! Le défaut, mesuré le 24/09/2026 sur les deux serveurs de Bertrand : dans
//! une feuille écrite par EAC, le pré-gap `INDEX 00` d'une piste vit à la FIN
//! du fichier précédent, si bien que la piste est DÉCLARÉE sous ce fichier-là
//! et ne commence réellement que dans le `FILE` annoncé juste après. Le
//! lecteur rattachait chaque piste à sa déclaration : tout l'album glissait
//! d'un fichier, la piste 1 entrait en collision avec la piste 2 (même
//! fichier, même `start_ms = 0`) et disparaissait, et le dernier fichier,
//! réclamé par personne, était indexé à part avec ses propres balises.
//!
//! Sur *Shaking the Tree* de Peter Gabriel (serveur .15) cela donnait quinze
//! titres faux et un juste, pour seize fichiers.
//!
//! Ce banc part du disque et finit en base : il monte un vrai dossier, une
//! vraie feuille de cette forme, de vrais WAV, et relit `tracks`. Les témoins
//! de `scanner::cue` prouvent la règle de lecture ; ceux-ci prouvent ce que
//! l'auditeur finit par voir.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::cue_bibliotheque::inventorier_et_ecrire;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    Arc::new(db)
}

/// Un vrai WAV : le plan écarte une image qu'aucun décodeur ne sait lire, et
/// une fixture vide rendrait ce banc vert sans rien prouver.
fn ecrire_wav(chemin: &Path, millisecondes: u32) {
    const TAUX: u32 = 44_100;
    let trames = TAUX * millisecondes / 1000;
    let octets = trames * 4;
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + octets).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&2u16.to_le_bytes());
    f.extend_from_slice(&TAUX.to_le_bytes());
    f.extend_from_slice(&(TAUX * 4).to_le_bytes());
    f.extend_from_slice(&4u16.to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&octets.to_le_bytes());
    for n in 0..trames {
        let v = ((n as f32 / 40.0).sin() * 8000.0) as i16;
        f.extend_from_slice(&v.to_le_bytes());
        f.extend_from_slice(&v.to_le_bytes());
    }
    fs::write(chemin, f).unwrap();
}

/// La forme exacte relevée sur le `.cue` de *Shaking the Tree*, réduite à
/// trois pistes : chemins Windows dans le `FILE`, `INDEX 00` en fin de fichier
/// précédent, `INDEX 01 00:00:00` sous le `FILE` suivant.
const GAPLESS: &str = "REM GENRE Unknown\r\n\
REM DATE 1990\r\n\
REM COMMENT \"ExactAudioCopy v0.99pb4\"\r\n\
PERFORMER \"Peter Gabriel\"\r\n\
TITLE \"Shaking the Tree: Sixteen Golden Greats\"\r\n\
FILE \"Peter Gabriel\\1990\\01. Solsbury Hill.wav\" WAVE\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"Solsbury Hill\"\r\n\
    PERFORMER \"Peter Gabriel\"\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"I Don't Remember\"\r\n\
    PERFORMER \"Peter Gabriel\"\r\n\
    INDEX 00 04:20:49\r\n\
FILE \"Peter Gabriel\\1990\\02. I Don't Remember.wav\" WAVE\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 03 AUDIO\r\n\
    TITLE \"Sledgehammer\"\r\n\
    PERFORMER \"Peter Gabriel\"\r\n\
    INDEX 00 03:48:27\r\n\
FILE \"Peter Gabriel\\1990\\03. Sledgehammer.wav\" WAVE\r\n\
    INDEX 01 00:00:00\r\n";

const FICHIERS: [&str; 3] = [
    "01. Solsbury Hill.wav",
    "02. I Don't Remember.wav",
    "03. Sledgehammer.wav",
];

/// Monte le dossier de l'album et rend (racine, dossier, fichiers).
fn album_gapless(racine: &Path) -> (PathBuf, Vec<PathBuf>) {
    let dossier = racine.join("Peter Gabriel - Shaking the Tree");
    fs::create_dir_all(&dossier).unwrap();
    let mut fichiers = Vec::new();
    for (i, nom) in FICHIERS.iter().enumerate() {
        let chemin = dossier.join(nom);
        ecrire_wav(&chemin, 2_000 + 500 * i as u32);
        fichiers.push(chemin);
    }
    fs::write(dossier.join("album.cue"), GAPLESS).unwrap();
    (dossier, fichiers)
}

fn racines(racine: &Path) -> Vec<String> {
    vec![racine.to_string_lossy().into_owned()]
}

/// 🔴 CHAQUE PISTE SUR LE FICHIER QUI PORTE SON `INDEX 01`.
///
/// Avant : « I Don't Remember » pointait sur `01. Solsbury Hill.wav` et
/// « Sledgehammer » sur `02. I Don't Remember.wav`. L'auditeur lisait un titre
/// et en entendait un autre, sur tout l'album.
#[test]
fn chaque_piste_atterrit_sur_le_fichier_de_son_index_01() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-gapless-mapping");
    let (dossier, fichiers) = album_gapless(scratch.path());
    let db = base();

    let (inv, bilan, images) =
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(scratch.path()));

    assert_eq!(inv.albums, 1, "inventaire : {inv:?}");
    assert_eq!(inv.pistes, 3, "inventaire : {inv:?}");
    assert_eq!(bilan.pistes_creees, 3, "bilan : {bilan:?}");
    assert_eq!(bilan.echecs, 0, "bilan : {bilan:?}");

    let repo = TrackRepo::with_backend(db);
    let attendu = [
        ("Solsbury Hill", 1usize),
        ("I Don't Remember", 2),
        ("Sledgehammer", 3),
    ];
    for (titre, rang) in attendu {
        let media = fichiers[rang - 1].to_string_lossy().to_string();
        let piste = repo
            .get_by_cue_identity(&media, 0)
            .unwrap()
            .unwrap_or_else(|| panic!("aucune piste sur {media}"));
        assert_eq!(
            piste.title, titre,
            "{media} porte « {} » au lieu de « {titre} » : l'album a glissé d'un fichier",
            piste.title
        );
        assert_eq!(piste.track_number, rang as i32);
    }

    // Aucun fichier de la feuille n'est laissé sans piste : sinon le scan
    // ordinaire l'indexerait à part, avec ses propres balises, et l'album
    // montrerait un titre juste au milieu des faux.
    assert!(
        images.is_empty(),
        "aucun de ces fichiers n'est DÉCOUPÉ : ils ne doivent pas sortir du scan ordinaire ({images:?})"
    );
}

/// AUCUNE PISTE PERDUE.
///
/// Les pistes 1 et 2 tombaient sur le même fichier au même `start_ms = 0` :
/// l'identité `(cue_media_path, cue_start_ms)` les confondait et la seconde
/// écrasait la première. Sur les seize pistes de *Shaking the Tree*, la piste 1
/// avait purement disparu.
#[test]
fn la_premiere_piste_ne_disparait_pas() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-gapless-piste1");
    let (dossier, _) = album_gapless(scratch.path());
    let db = base();

    inventorier_et_ecrire(db.clone(), &[dossier], &racines(scratch.path()));

    let repo = TrackRepo::with_backend(db);
    assert_eq!(
        repo.count().unwrap(),
        3,
        "une piste a été écrasée par une autre sur la même identité"
    );
}

/// 🔴 LE CHEMIN MANQUANT — une piste qui occupe un fichier ENTIER le porte.
///
/// `file_path = NULL` rendait ces lignes introuvables par chemin : invisibles
/// au pré-filtre incrémental du scan (qui s'indexe sur `file_path`), donc
/// doublables par le scan ordinaire, et ignorées de toutes les passes qui
/// filtrent `file_path IS NOT NULL`.
#[test]
fn une_piste_qui_occupe_tout_son_fichier_porte_son_chemin() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-gapless-chemin");
    let (dossier, fichiers) = album_gapless(scratch.path());
    let db = base();

    inventorier_et_ecrire(db.clone(), &[dossier], &racines(scratch.path()));

    let repo = TrackRepo::with_backend(db);
    for fichier in &fichiers {
        let media = fichier.to_string_lossy().to_string();
        let piste = repo.get_by_cue_identity(&media, 0).unwrap().unwrap();
        assert_eq!(
            piste.file_path.as_deref(),
            Some(media.as_str()),
            "la piste qui occupe {media} en entier doit le porter comme file_path"
        );
        // Et de quoi reconnaître le fichier inchangé au scan suivant : sans
        // ces deux-là, `file_needs_scan` conclurait « modifié » et le scan
        // ordinaire relirait les balises par-dessus le titre de la feuille.
        assert!(piste.file_size.is_some(), "file_size manquant sur {media}");
        assert!(
            piste.file_mtime.is_some(),
            "file_mtime manquant sur {media}"
        );
        // Le lien vers la feuille reste, lui : c'est encore par lui qu'on
        // élague la ligne si le fichier disparaît.
        assert_eq!(piste.cue_media_path.as_deref(), Some(media.as_str()));
        assert_eq!(piste.cue_start_ms, Some(0));
    }
    // La carte des chemins — celle du pré-filtre incrémental — les voit toutes.
    let carte = repo.get_all_file_info_by_path().unwrap();
    for fichier in &fichiers {
        assert!(
            carte.contains_key(fichier.to_string_lossy().as_ref()),
            "{} reste introuvable par chemin",
            fichier.display()
        );
    }
}

/// LE DOUBLON — une ligne posée par le scan ordinaire est ADOPTÉE, pas doublée.
///
/// C'est l'état du serveur .18, où 58 titres existaient DEUX fois : une ligne
/// de fichier héritée d'un scan antérieur, et une ligne CUE posée à côté
/// d'elle, invisibles l'une à l'autre faute de `file_path` commun.
#[test]
fn une_ligne_de_fichier_deja_en_base_est_adoptee_et_non_doublee() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-gapless-doublon");
    let (dossier, fichiers) = album_gapless(scratch.path());
    let db = base();
    let repo = TrackRepo::with_backend(db.clone());

    // L'état d'AVANT : le scan ordinaire avait indexé les fichiers, chacun
    // titré de ses propres balises, et enrichi depuis.
    for (i, fichier) in fichiers.iter().enumerate() {
        let mut t = tune_core::db::models::Track::new(format!("Titre de balise {i}"));
        t.file_path = Some(fichier.to_string_lossy().into_owned());
        t.audio_hash = Some(format!("empreinte-{i}"));
        t.musicbrainz_recording_id = Some(format!("mbid-{i}"));
        repo.create(&t).unwrap();
    }
    assert_eq!(repo.count().unwrap(), 3);

    let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(scratch.path()));

    assert_eq!(
        repo.count().unwrap(),
        3,
        "le scan CUE a posé des doublons à côté des lignes de fichier : {bilan:?}"
    );
    assert_eq!(bilan.pistes_creees, 0, "bilan : {bilan:?}");
    assert_eq!(bilan.pistes_mises_a_jour, 3, "bilan : {bilan:?}");
    assert_eq!(bilan.echecs, 0, "bilan : {bilan:?}");

    let media = fichiers[1].to_string_lossy().to_string();
    let piste = repo.get_by_path(&media).unwrap().unwrap();
    assert_eq!(
        piste.title, "I Don't Remember",
        "le titre de la feuille doit l'emporter sur la balise du fichier"
    );
    // Et ce qu'une feuille ne sait pas dire n'a pas été effacé au passage.
    assert_eq!(piste.audio_hash.as_deref(), Some("empreinte-1"));
    assert_eq!(piste.musicbrainz_recording_id.as_deref(), Some("mbid-1"));
}

/// L'ÉTAT DU .18 — une ligne de fichier ET une ligne CUE pour le même titre.
///
/// 58 titres y existaient deux fois : une ligne posée par le scan ordinaire
/// (avec `file_path`) et une ligne posée par la feuille (sans), invisibles
/// l'une à l'autre. Un scan doit en garder UNE — celle qui porte le chemin,
/// parce que c'est elle que protège `file_path UNIQUE` et elle que le
/// pré-filtre incrémental voit.
#[test]
fn deux_lignes_pour_un_meme_fichier_sont_ramenees_a_une() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-gapless-18");
    let (dossier, fichiers) = album_gapless(scratch.path());
    let db = base();
    let repo = TrackRepo::with_backend(db.clone());

    for (i, fichier) in fichiers.iter().enumerate() {
        // La ligne du scan ordinaire.
        let mut fichier_row = tune_core::db::models::Track::new(format!("Balise {i}"));
        fichier_row.file_path = Some(fichier.to_string_lossy().into_owned());
        repo.create(&fichier_row).unwrap();
        // Et la ligne CUE posée à côté d'elle, sans chemin.
        let mut cue_row = tune_core::db::models::Track::new(format!("Feuille {i}"));
        cue_row.cue_media_path = Some(fichier.to_string_lossy().into_owned());
        cue_row.cue_start_ms = Some(0);
        repo.create(&cue_row).unwrap();
    }
    assert_eq!(repo.count().unwrap(), 6, "l'état d'avant doit être doublé");

    let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(scratch.path()));

    assert_eq!(bilan.doublons_resorbes, 3, "bilan : {bilan:?}");
    assert_eq!(bilan.echecs, 0, "bilan : {bilan:?}");
    assert_eq!(
        repo.count().unwrap(),
        3,
        "il doit rester une seule ligne par fichier"
    );
    let media = fichiers[2].to_string_lossy().to_string();
    let piste = repo.get_by_path(&media).unwrap().unwrap();
    assert_eq!(piste.title, "Sledgehammer");
}

/// LA CONTRE-ÉPREUVE DE FORME — la feuille « image + cue » ordinaire, intacte.
///
/// Un seul `FILE`, plusieurs `TRACK`, de vrais décalages : c'est le cas pour
/// lequel le format a été inventé, et la très grande majorité des feuilles.
/// Ses pistes restent des TRANCHES — `file_path` NUL, `file_path UNIQUE`
/// interdisant d'en poser trois fois le même — et l'image sort du scan
/// ordinaire, sinon l'album existerait deux fois.
#[test]
fn la_feuille_image_ordinaire_ne_change_pas_de_comportement() {
    const IMAGE: &str = "PERFORMER \"Glenn Gould\"\nTITLE \"Goldberg Variations\"\n\
FILE \"image.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Aria\"\n    INDEX 00 00:00:00\n    INDEX 01 00:00:00\n\
  TRACK 02 AUDIO\n    TITLE \"Variatio 1\"\n    INDEX 01 00:01:00\n\
  TRACK 03 AUDIO\n    TITLE \"Variatio 2\"\n    INDEX 01 00:02:00\n";

    let scratch = tune_core::test_scratch::scratch_dir("cue-image-ordinaire");
    let dossier = scratch.path().join("Gould - Goldberg");
    fs::create_dir_all(&dossier).unwrap();
    let image = dossier.join("image.wav");
    ecrire_wav(&image, 4_000);
    fs::write(dossier.join("album.cue"), IMAGE).unwrap();
    let db = base();

    let (inv, bilan, images) =
        inventorier_et_ecrire(db.clone(), &[dossier], &racines(scratch.path()));

    assert_eq!(inv.albums, 1, "inventaire : {inv:?}");
    assert_eq!(bilan.pistes_creees, 3, "bilan : {bilan:?}");

    let repo = TrackRepo::with_backend(db);
    let media = image.to_string_lossy().to_string();
    for (debut, titre) in [(0i64, "Aria"), (1_000, "Variatio 1"), (2_000, "Variatio 2")] {
        let piste = repo.get_by_cue_identity(&media, debut).unwrap().unwrap();
        assert_eq!(piste.title, titre);
        assert_eq!(
            piste.file_path, None,
            "une TRANCHE ne peut pas porter de file_path : la contrainte UNIQUE en refuserait la deuxième"
        );
        assert_eq!(piste.cue_media_path.as_deref(), Some(media.as_str()));
    }
    let attendu: HashSet<PathBuf> = HashSet::from([image]);
    assert_eq!(
        images, attendu,
        "l'image DÉCOUPÉE doit sortir du scan ordinaire, sinon l'album existe deux fois"
    );
}
