//! #6635 — « Graver tags » (`POST /library/write-tags`) écrit TOUS les genres
//! de l'album, chacun en valeur séparée selon le conteneur, et non plus le
//! seul `tracks.genre` (« Pop » pour un album « Pop/Rock »).
//!
//! Par la fonction de route, comme le bouton de la fiche album
//! (`writeAlbumTags` : `album_id`, `only_missing: false`), sur de VRAIS
//! fichiers copiés des fixtures de `tune-core` dans un dossier
//! `test_scratch`. Relus ensuite deux fois : par le conteneur CONCRET de
//! lofty (champs `GENRE` du FLAC, trame `TCON` de l'ID3v2.4, atome `©gen` du
//! M4A), puis par le lecteur du scan (`metadata::read_metadata`).
use super::*;
use std::path::{Path, PathBuf};

use lofty::config::ParseOptions;
use lofty::file::AudioFile;
use tune_core::test_scratch::{ScratchDir, scratch_dir};

const ATTENDU: [&str; 2] = ["Pop", "Rock"];

fn fixture(nom: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures")
        .join(nom)
}

struct Banc {
    state: AppState,
    dir: ScratchDir,
}

/// Un album « Pop/Rock » de trois pistes — FLAC, MP3, M4A — tel que la fiche
/// « Modifier » le laisse en base (#5314) : `genre` = le premier genre,
/// `genres` = le tableau JSON de tous.
fn banc() -> Banc {
    let dir = scratch_dir("tune-6635-genres");
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);
    let b = &state.backend;
    b.execute("INSERT INTO artists (id, name) VALUES (1, 'Bruce')", &[])
        .unwrap();
    b.execute(
        "INSERT INTO albums (id, title, artist_id, genre, genres) \
         VALUES (1, 'The River', 1, 'Pop/Rock', '[\"Pop\",\"Rock\"]')",
        &[],
    )
    .unwrap();
    for (id, src, nom) in [
        (11i64, "test.flac", "01.flac"),
        (12, "test.mp3", "02.mp3"),
        (13, "test.m4a", "03.m4a"),
    ] {
        let dst = dir.join(nom);
        std::fs::copy(fixture(src), &dst).unwrap();
        b.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number, \
             duration_ms, file_path, source, genre, genres) \
             VALUES (?, ?, 1, 1, 1, ?, 1000, ?, 'local', 'Pop', '[\"Pop\",\"Rock\"]')",
            &[
                &id as &dyn ToSqlValue,
                &nom.to_string(),
                &(id - 10),
                &dst.to_string_lossy().into_owned(),
            ],
        )
        .unwrap();
    }
    Banc { state, dir }
}

/// Lance la passe et attend son statut « done » (elle tourne dans un
/// `tokio::spawn`).
async fn graver(state: &AppState, only_missing: bool) -> Value {
    let r = write_tags_to_files(
        State(state.clone()),
        Json(WriteTagsRequest {
            only_missing,
            track_ids: None,
            album_id: Some(1),
        }),
    )
    .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    for _ in 0..400 {
        let statut: Value = reglages
            .get("write_tags_status")
            .unwrap()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(Value::Null);
        if statut["status"] == "done" {
            return statut;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("la passe « Graver tags » ne s'est pas terminée");
}

fn ouvrir(p: &Path) -> std::fs::File {
    std::fs::File::open(p).unwrap()
}

/// Les valeurs de `GENRE` du FLAC, champ par champ.
fn genres_flac(p: &Path) -> Vec<String> {
    let f = lofty::flac::FlacFile::read_from(&mut ouvrir(p), ParseOptions::new()).unwrap();
    f.vorbis_comments()
        .map(|vc| vc.get_all("GENRE").map(str::to_string).collect())
        .unwrap_or_default()
}

/// Les valeurs de la trame `TCON`, et la version de l'étiquette ID3v2.
fn genres_mp3(p: &Path) -> (Vec<String>, Option<lofty::id3::v2::Id3v2Version>) {
    let f = lofty::mpeg::MpegFile::read_from(&mut ouvrir(p), ParseOptions::new()).unwrap();
    let Some(tag) = f.id3v2() else {
        return (Vec::new(), None);
    };
    (
        tag.genres()
            .map(|g| g.map(str::to_string).collect())
            .unwrap_or_default(),
        Some(tag.original_version()),
    )
}

/// Les valeurs des atomes `©gen` du M4A, tous atomes confondus : lofty écrit
/// un atome par genre (`Ilst::merge_tag`), et `Ilst::get` ne rendrait que le
/// premier.
fn genres_m4a(p: &Path) -> Vec<String> {
    use lofty::mp4::{AtomData, AtomIdent};
    let f = lofty::mp4::Mp4File::read_from(&mut ouvrir(p), ParseOptions::new()).unwrap();
    let Some(ilst) = f.ilst() else {
        return Vec::new();
    };
    let cle_genre = AtomIdent::Fourcc(*b"\xa9gen");
    ilst.into_iter()
        .filter(|atome| *atome.ident() == cle_genre)
        .flat_map(|atome| atome.data())
        .filter_map(|d| match d {
            AtomData::UTF8(s) | AtomData::UTF16(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

fn relu_par_le_scan(p: &Path) -> Vec<String> {
    tune_core::metadata::read_metadata(p).unwrap().genres
}

/// Le bouton de la fiche album : `only_missing: false`. Les trois conteneurs
/// portent « Pop » ET « Rock », chacun en valeur séparée, et le scan les
/// relit tels quels.
#[tokio::test]
async fn graver_tags_ecrit_tous_les_genres_de_l_album_6635() {
    let banc = banc();
    let statut = graver(&banc.state, false).await;
    assert_eq!(
        statut["written"], 3,
        "#6635 : la passe doit écrire les trois pistes de l'album (0 : la \
         sélection des pistes a échoué) — {statut}"
    );
    assert_eq!(statut["errors"], 0, "{statut}");

    let flac = banc.dir.join("01.flac");
    let mp3 = banc.dir.join("02.mp3");
    let m4a = banc.dir.join("03.m4a");

    assert_eq!(
        genres_flac(&flac),
        ATTENDU,
        "#6635 : le FLAC doit porter un champ GENRE par genre de l'album"
    );
    let (tcon, version) = genres_mp3(&mp3);
    assert_eq!(
        tcon, ATTENDU,
        "#6635 : la trame TCON du MP3 doit porter chaque genre de l'album"
    );
    assert_eq!(version, Some(lofty::id3::v2::Id3v2Version::V4));
    assert_eq!(
        genres_m4a(&m4a),
        ATTENDU,
        "#6635 : l'atome ©gen du M4A doit porter chaque genre de l'album"
    );
    for p in [&flac, &mp3, &m4a] {
        assert_eq!(
            relu_par_le_scan(p),
            ATTENDU,
            "#6635 : le scan relit {} sans perte",
            p.display()
        );
    }

    // Seconde passe : idempotente, aucun genre doublé.
    graver(&banc.state, false).await;
    assert_eq!(genres_flac(&flac), ATTENDU);
    assert_eq!(genres_mp3(&mp3).0, ATTENDU);
    assert_eq!(genres_m4a(&m4a), ATTENDU);
}

/// `only_missing: true` : un fichier SANS genre reçoit tous ceux de l'album ;
/// un fichier qui en porte déjà un le garde (rien n'est « manquant »).
#[tokio::test]
async fn sans_genre_dans_le_fichier_le_remplissage_pose_tous_les_genres_6635() {
    let banc = banc();
    let flac = banc.dir.join("01.flac");
    let m4a = banc.dir.join("03.m4a");
    // Le FLAC perd son genre ; le M4A reçoit « Soul », que la base ignore.
    {
        use lofty::tag::{Accessor, ItemKey, TagExt};
        let mut f =
            lofty::flac::FlacFile::read_from(&mut ouvrir(&flac), ParseOptions::new()).unwrap();
        if let Some(vc) = f.vorbis_comments_mut() {
            vc.remove("GENRE").for_each(drop);
        }
        let mut fichier = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&flac)
            .unwrap();
        f.save_to(&mut fichier, lofty::config::WriteOptions::default())
            .unwrap();
        let mut t = lofty::read_from_path(&m4a).unwrap();
        let tag = lofty::file::TaggedFileExt::primary_tag_mut(&mut t).unwrap();
        tag.remove_key(ItemKey::Genre);
        tag.set_genre("Soul".into());
        tag.save_to_path(&m4a, lofty::config::WriteOptions::default())
            .unwrap();
    }
    assert!(genres_flac(&flac).is_empty(), "témoin : FLAC sans genre");
    assert_eq!(genres_m4a(&m4a), ["Soul"], "témoin : M4A « Soul »");

    graver(&banc.state, true).await;
    assert_eq!(
        genres_flac(&flac),
        ATTENDU,
        "#6635 : un FLAC sans genre reçoit tous les genres de l'album"
    );
    assert_eq!(genres_m4a(&m4a), ["Soul"], "le genre déjà présent reste");
}

// ---------------------------------------------------------------------------
// #6635 — l'écriture SÛRE, par la route : l'audio reste identique, aucune
// copie de travail ne traîne, le fichier joué et le fichier en lecture seule
// sont refusés avec leur motif, les droits sont conservés, la ligne de la
// piste reprend la taille et la date du fichier réécrit.
// ---------------------------------------------------------------------------

fn empreinte(p: &Path) -> [u8; 32] {
    tune_core::metadata::empreinte_audio::empreinte_audio(p)
        .unwrap()
        .unwrap()
}

/// Les fichiers du dossier qui ne sont pas les trois pistes : copies de
/// travail laissées, quel que soit leur préfixe.
fn restes(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !["01.flac", "02.mp3", "03.m4a"].contains(&n.as_str()))
        .collect()
}

fn motifs(statut: &Value) -> Vec<(String, String)> {
    statut["refus"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|r| {
            (
                r["file"].as_str().unwrap_or("").to_string(),
                r["reason"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn graver_tags_garde_l_audio_identique_et_ne_laisse_aucune_copie_6635() {
    let banc = banc();
    let fichiers = ["01.flac", "02.mp3", "03.m4a"].map(|n| banc.dir.join(n));
    let avant: Vec<_> = fichiers.iter().map(|p| empreinte(p)).collect();
    let statut = graver(&banc.state, false).await;
    assert_eq!(statut["written"], 3, "{statut}");
    for (p, a) in fichiers.iter().zip(&avant) {
        assert_eq!(
            &empreinte(p),
            a,
            "#6635 : l'audio de {} doit rester identique après « Graver tags »",
            p.display()
        );
    }
    assert!(
        restes(banc.dir.path()).is_empty(),
        "#6635 : aucune copie de travail ne doit rester : {:?}",
        restes(banc.dir.path())
    );
    // La ligne de la piste reprend la date et la taille du fichier réécrit.
    let flac = &fichiers[0];
    let (taille, mtime) = tune_core::audio::iso9660::taille_et_mtime(flac).unwrap();
    let ligne = tune_core::db::track_repo::TrackRepo::with_backend(banc.state.backend.clone())
        .get(11)
        .unwrap()
        .unwrap();
    assert_eq!(ligne.file_size, Some(taille as i64));
    assert_eq!(ligne.file_mtime, Some(mtime));
}

#[tokio::test]
async fn graver_tags_refuse_le_fichier_en_cours_de_lecture_6635() {
    let banc = banc();
    let flac = banc.dir.join("01.flac");
    let octets = std::fs::read(&flac).unwrap();
    banc.state
        .playback
        .play(
            6635,
            tune_core::playback::NowPlaying {
                track_id: Some(11),
                title: "piste jouée".into(),
                source: "local".into(),
                ..Default::default()
            },
        )
        .await;
    let statut = graver(&banc.state, false).await;
    assert_eq!(
        std::fs::read(&flac).unwrap(),
        octets,
        "#6635 : le fichier joué ne doit pas être écrit — {statut}"
    );
    assert_eq!(
        statut["written"], 2,
        "les deux autres pistes s'écrivent — {statut}"
    );
    assert!(
        motifs(&statut)
            .iter()
            .any(|(f, m)| f.ends_with("01.flac") && m == MOTIF_FICHIER_EN_LECTURE),
        "#6635 : le refus doit être remonté avec son motif — {statut}"
    );
    assert!(restes(banc.dir.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn graver_tags_refuse_le_fichier_en_lecture_seule_6635() {
    use std::os::unix::fs::PermissionsExt;
    let banc = banc();
    let flac = banc.dir.join("01.flac");
    std::fs::set_permissions(&flac, std::fs::Permissions::from_mode(0o444)).unwrap();
    let octets = std::fs::read(&flac).unwrap();
    let statut = graver(&banc.state, false).await;
    std::fs::set_permissions(&flac, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        std::fs::read(&flac).unwrap(),
        octets,
        "#6635 : un fichier en lecture seule ne doit pas être écrit — {statut}"
    );
    assert_eq!(statut["errors"], 1, "{statut}");
    assert!(
        motifs(&statut).iter().any(|(f, m)| f.ends_with("01.flac")
            && m == tune_core::metadata::tag_writer::MOTIF_FICHIER_LECTURE_SEULE),
        "#6635 : le refus doit être remonté avec son motif — {statut}"
    );
    assert!(restes(banc.dir.path()).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn graver_tags_conserve_les_droits_du_fichier_6635() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let banc = banc();
    let mp3 = banc.dir.join("02.mp3");
    std::fs::set_permissions(&mp3, std::fs::Permissions::from_mode(0o640)).unwrap();
    let avant = std::fs::metadata(&mp3).unwrap();
    let statut = graver(&banc.state, false).await;
    assert_eq!(statut["written"], 3, "{statut}");
    let apres = std::fs::metadata(&mp3).unwrap();
    assert_ne!(
        avant.ino(),
        apres.ino(),
        "le fichier a été remplacé, pas écrit en place"
    );
    assert_eq!(apres.mode() & 0o7777, 0o640, "#6635 : droits conservés");
    assert_eq!((apres.uid(), apres.gid()), (avant.uid(), avant.gid()));
}
