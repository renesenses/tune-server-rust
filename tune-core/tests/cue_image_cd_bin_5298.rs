//! Images de CD audio BIN/CUE, du disque à la lecture (#5298).
//!
//! Un `FILE "album.bin" BINARY` porte le son brut du disque : 2352 octets par
//! secteur, PCM 16 bits little-endian, 44,1 kHz, stéréo, sans en-tête. Avant
//! ce lot, le plan des feuilles écartait l'image (`cue-image-non-decodable`)
//! et l'album n'existait pas.
//!
//! Chaque trame des images synthétiques porte son PROPRE RANG dans le fichier
//! (gauche = 16 bits bas, droite = 16 bits hauts). Relire le rang de chaque
//! trame servie prouve la borne à l'échantillon près, et une seule trame de
//! données jouée comme du son casse la suite des rangs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::cue_bibliotheque::inventorier_et_ecrire;

const TRAMES_PAR_SECTEUR: u64 = 588;
const OCTETS_PAR_SECTEUR: usize = 2352;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    Arc::new(db)
}

/// Des secteurs audio dont chaque trame porte son rang dans le fichier.
fn secteurs_audio(premier_secteur: u64, secteurs: u64) -> Vec<u8> {
    let mut o = Vec::with_capacity(secteurs as usize * OCTETS_PAR_SECTEUR);
    let debut = premier_secteur * TRAMES_PAR_SECTEUR;
    for n in debut..debut + secteurs * TRAMES_PAR_SECTEUR {
        o.extend_from_slice(&(n as u16).to_le_bytes());
        o.extend_from_slice(&((n >> 16) as u16).to_le_bytes());
    }
    o
}

/// Des secteurs de données : un motif qui ne ressemble à aucun rang voisin.
fn secteurs_de_donnees(secteurs: u64) -> Vec<u8> {
    let mut o = Vec::with_capacity(secteurs as usize * OCTETS_PAR_SECTEUR);
    for _ in 0..secteurs {
        let mut s = vec![0xA5u8; OCTETS_PAR_SECTEUR];
        s[0] = 0x00;
        s[1..11].fill(0xFF);
        s[11] = 0x00;
        o.extend_from_slice(&s);
    }
    o
}

fn rangs(pcm: &[u8]) -> Vec<u64> {
    pcm.chunks_exact(4)
        .map(|t| {
            u64::from(u16::from_le_bytes([t[0], t[1]]))
                | (u64::from(u16::from_le_bytes([t[2], t[3]])) << 16)
        })
        .collect()
}

/// Joue une tranche par le chemin de la sortie locale et de l'orchestrateur
/// (`decode_to_pcm_streaming_tranche`, 44,1 kHz / 16 bits / stéréo : aucune
/// conversion, les octets sortent tels quels) et rend le rang de chaque trame.
fn jouer(image: &Path, seek_s: f64, duree_s: Option<f64>) -> Vec<u64> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fp = image.to_string_lossy().into_owned();
    rt.block_on(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let pret = Arc::new(tokio::sync::Notify::new());
        let (niveaux, _niveaux_rx) = tokio::sync::mpsc::unbounded_channel();
        let tache = tokio::task::spawn_blocking(move || {
            tune_core::audio::decode::decode_to_pcm_streaming_tranche(
                &fp,
                Some(44_100),
                Some(2),
                Some(16),
                tx,
                32_768,
                pret,
                niveaux,
                seek_s,
                duree_s,
            )
        });
        let mut tout = Vec::new();
        while let Some(bloc) = rx.recv().await {
            tout.extend_from_slice(&bloc);
        }
        tache.await.unwrap().expect("décodage de l'image");
        assert_eq!(&tout[..4], b"RIFF", "l'en-tête WAV ouvre le flux");
        rangs(&tout[44..])
    })
}

/// Les bornes d'une piste telles que l'orchestrateur les tire de sa ligne.
fn bornes(piste: &Track) -> (f64, Option<f64>) {
    let debut = piste.cue_start_ms.unwrap() as u64;
    let duree = piste.cue_end_ms.map(|f| (f as u64 - debut) as f64 / 1000.0);
    (debut as f64 / 1000.0, duree)
}

fn assert_suite(rangs: &[u64], debut: u64, fin: u64, contexte: &str) {
    assert_eq!(
        rangs.first().copied(),
        Some(debut),
        "{contexte} : première trame"
    );
    assert_eq!(
        rangs.len() as u64,
        fin - debut,
        "{contexte} : nombre de trames (dernière servie : {:?})",
        rangs.last()
    );
    assert!(
        rangs.windows(2).all(|w| w[1] == w[0] + 1),
        "{contexte} : une trame étrangère s'est glissée dans la tranche"
    );
}

fn monter(racine: &Path, nom: &str, feuille: &str, images: &[(&str, Vec<u8>)]) -> PathBuf {
    let dossier = racine.join(nom);
    fs::create_dir_all(&dossier).unwrap();
    for (fichier, octets) in images {
        fs::write(dossier.join(fichier), octets).unwrap();
    }
    fs::write(dossier.join(format!("{nom}.cue")), feuille).unwrap();
    dossier
}

fn importer(db: &Arc<dyn DbBackend>, racine: &Path, dossier: &Path, pistes: usize) {
    let (inv, bilan, _) = inventorier_et_ecrire(
        db.clone(),
        std::slice::from_ref(&dossier.to_path_buf()),
        &[racine.to_string_lossy().into_owned()],
    );
    assert_eq!(inv.albums, 1, "inventaire : {inv:?}");
    assert_eq!(inv.pistes, pistes, "inventaire : {inv:?}");
    assert_eq!(bilan.pistes_creees, pistes, "bilan : {bilan:?}");
    assert_eq!(bilan.echecs, 0, "bilan : {bilan:?}");
}

fn piste(db: &Arc<dyn DbBackend>, image: &Path, debut_ms: i64) -> Track {
    TrackRepo::with_backend(db.clone())
        .get_by_cue_identity(&image.to_string_lossy(), debut_ms)
        .unwrap()
        .unwrap_or_else(|| panic!("aucune piste à {debut_ms} ms dans {}", image.display()))
}

/// Deux pistes, un prégap, du CD-Text. `INDEX 01 00:02:37` : 187 secteurs,
/// soit 109 956 trames — que la base range à 2493 ms, soit 109 941.
const DEUX_PISTES: &str = "REM GENRE Jazz\r\n\
REM DATE 1959\r\n\
PERFORMER \"Quintette d'essai\"\r\n\
TITLE \"Disque d'essai\"\r\n\
FILE \"disque.bin\" BINARY\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"Premier mouvement\"\r\n\
    PERFORMER \"Quintette d'essai\"\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Second mouvement\"\r\n\
    PERFORMER \"Soliste invité\"\r\n\
    INDEX 00 00:01:50\r\n\
    INDEX 01 00:02:37\r\n";

const SECTEURS_DEUX_PISTES: u64 = 313;
const DEBUT_PISTE_2: u64 = 187 * TRAMES_PAR_SECTEUR;

#[test]
fn un_bin_a_deux_pistes_devient_un_album_lu_au_cd_text() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-bin-5298-cdtext");
    let dossier = monter(
        scratch.path(),
        "disque",
        DEUX_PISTES,
        &[("disque.bin", secteurs_audio(0, SECTEURS_DEUX_PISTES))],
    );
    let image = dossier.join("disque.bin");
    let db = base();
    importer(&db, scratch.path(), &dossier, 2);

    let p1 = piste(&db, &image, 0);
    let p2 = piste(&db, &image, 2493);
    assert_eq!(p1.title, "Premier mouvement");
    assert_eq!(p2.title, "Second mouvement");
    assert_eq!(p1.track_number, 1);
    assert_eq!(p2.track_number, 2);
    assert_eq!(p1.album_title.as_deref(), Some("Disque d'essai"));
    assert_eq!(p1.artist_name.as_deref(), Some("Quintette d'essai"));
    assert_eq!(p2.artist_name.as_deref(), Some("Soliste invité"));
    assert_eq!(p1.genre.as_deref(), Some("Jazz"));
    assert_eq!(p1.year, Some(1959));
    // Le prégap de la piste 2 reste à la fin de la piste 1, comme pour toute
    // feuille CUE.
    assert_eq!(p1.cue_end_ms, Some(2493));
    assert_eq!(p2.cue_end_ms, None);
    // Le format du CD, et la durée de la dernière piste tirée de la TAILLE de
    // l'image (313 secteurs = 4173 ms).
    assert_eq!(p2.sample_rate, Some(44_100));
    assert_eq!(p2.bit_depth, Some(16));
    assert_eq!(p2.channels, 2);
    assert_eq!(p2.duration_ms, 4173 - 2493);
    assert_eq!(p1.duration_ms, 2493);
    // Une tranche : jamais de `file_path`, le scan ordinaire ne voit pas un
    // `.bin` et sa purge l'effacerait.
    assert_eq!(p1.file_path, None);
    assert_eq!(p2.file_path, None);
}

/// 🔴 LES BORNES À L'ÉCHANTILLON PRÈS, et le déplacement.
#[test]
fn les_pistes_d_un_bin_se_jouent_a_l_echantillon_pres() {
    let scratch = tune_core::test_scratch::scratch_dir("cue-bin-5298-bornes");
    let dossier = monter(
        scratch.path(),
        "disque",
        DEUX_PISTES,
        &[("disque.bin", secteurs_audio(0, SECTEURS_DEUX_PISTES))],
    );
    let image = dossier.join("disque.bin");
    let db = base();
    importer(&db, scratch.path(), &dossier, 2);
    let fin_image = SECTEURS_DEUX_PISTES * TRAMES_PAR_SECTEUR;

    let (debut, duree) = bornes(&piste(&db, &image, 0));
    assert_suite(&jouer(&image, debut, duree), 0, DEBUT_PISTE_2, "piste 1");

    let (debut, duree) = bornes(&piste(&db, &image, 2493));
    assert_suite(
        &jouer(&image, debut, duree),
        DEBUT_PISTE_2,
        fin_image,
        "piste 2",
    );

    // Déplacement d'une seconde dans la piste 2, comme le fait
    // l'orchestrateur : début de tranche + déplacement, durée diminuée.
    assert_suite(
        &jouer(&image, (2493.0 + 1000.0) / 1000.0, None),
        DEBUT_PISTE_2 + 44_100,
        fin_image,
        "piste 2 à +1 s",
    );
    // Déplacement d'une seconde dans la piste 1 : la fin reste exacte.
    assert_suite(
        &jouer(&image, 1.0, Some((2493.0 - 1000.0) / 1000.0)),
        44_100,
        DEBUT_PISTE_2,
        "piste 1 à +1 s",
    );
    // Un déplacement libre, hors frontière de secteur : à la milliseconde.
    let libre = jouer(&image, (2493.0 + 500.0) / 1000.0, None);
    let attendu = DEBUT_PISTE_2 + 22_050;
    let premier = libre[0];
    assert!(
        premier.abs_diff(attendu) <= 44,
        "déplacement de 500 ms : trame {premier}, attendu {attendu} à 1 ms près"
    );
    assert_suite(&libre, premier, fin_image, "piste 2 à +500 ms");

    // Le chemin d'ANALYSE (ReplayGain, DR, empreinte) lit la même tranche.
    let analyse =
        tune_core::audio::decode::decode_to_pcm(&image.to_string_lossy(), None, None, 0.0, 2.493)
            .unwrap();
    assert_eq!((analyse.sample_rate, analyse.channels), (44_100, 2));
    assert_suite(
        &rangs(&analyse.pcm_bytes()),
        0,
        DEBUT_PISTE_2,
        "analyse piste 1",
    );
}

/// CD mixte (« Mixed Mode ») : la piste 1 est une piste de DONNÉES, l'audio
/// suit dans le même BIN.
#[test]
fn la_piste_de_donnees_en_tete_d_un_cd_mixte_est_ignoree() {
    let feuille = "TITLE \"Jeu d'essai\"\r\n\
FILE \"mixte.bin\" BINARY\r\n\
  TRACK 01 MODE1/2352\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Thème\"\r\n\
    INDEX 00 00:02:00\r\n\
    INDEX 01 00:04:00\r\n\
  TRACK 03 AUDIO\r\n\
    TITLE \"Générique\"\r\n\
    INDEX 01 00:05:10\r\n";
    let mut octets = secteurs_de_donnees(150);
    octets.extend(secteurs_audio(150, 300));
    let scratch = tune_core::test_scratch::scratch_dir("cue-bin-5298-mixte");
    let dossier = monter(scratch.path(), "mixte", feuille, &[("mixte.bin", octets)]);
    let image = dossier.join("mixte.bin");
    let db = base();
    importer(&db, scratch.path(), &dossier, 2);

    assert!(
        TrackRepo::with_backend(db.clone())
            .get_by_cue_identity(&image.to_string_lossy(), 0)
            .unwrap()
            .is_none(),
        "la piste de données est entrée en base"
    );
    let theme = piste(&db, &image, 4000);
    assert_eq!(theme.title, "Thème");
    assert_eq!(theme.track_number, 2);
    let (debut, duree) = bornes(&theme);
    assert_suite(
        &jouer(&image, debut, duree),
        300 * TRAMES_PAR_SECTEUR,
        385 * TRAMES_PAR_SECTEUR,
        "piste 2 du CD mixte",
    );
}

/// CD Extra : les données viennent APRÈS l'audio. La dernière piste audio
/// s'arrête au prégap de la piste de données, déjà écrit en secteurs de
/// données.
#[test]
fn la_derniere_piste_audio_d_un_cd_extra_s_arrete_avant_les_donnees() {
    let feuille = "TITLE \"Single d'essai\"\r\n\
FILE \"extra.bin\" BINARY\r\n\
  TRACK 01 AUDIO\r\n\
    INDEX 01 00:00:00\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Face B\"\r\n\
    INDEX 01 00:01:00\r\n\
  TRACK 03 MODE2/2352\r\n\
    INDEX 00 00:02:00\r\n\
    INDEX 01 00:04:00\r\n";
    let mut octets = secteurs_audio(0, 150);
    octets.extend(secteurs_de_donnees(450));
    let scratch = tune_core::test_scratch::scratch_dir("cue-bin-5298-extra");
    let dossier = monter(scratch.path(), "extra", feuille, &[("extra.bin", octets)]);
    let image = dossier.join("extra.bin");
    let db = base();
    importer(&db, scratch.path(), &dossier, 2);

    let face_b = piste(&db, &image, 1000);
    assert_eq!(face_b.cue_end_ms, Some(2000));
    let (debut, duree) = bornes(&face_b);
    assert_suite(
        &jouer(&image, debut, duree),
        75 * TRAMES_PAR_SECTEUR,
        150 * TRAMES_PAR_SECTEUR,
        "dernière piste audio du CD Extra",
    );
}

/// Multi-fichiers : un BIN par piste, prégap en tête du second, et une piste
/// de données dans son propre BIN — absent du dossier, il ne doit pas faire
/// écarter l'album.
#[test]
fn une_feuille_a_un_bin_par_piste_est_lue() {
    let feuille = "TITLE \"Pistes séparées\"\r\n\
PERFORMER \"Duo d'essai\"\r\n\
FILE \"01.bin\" BINARY\r\n\
  TRACK 01 AUDIO\r\n\
    TITLE \"Un\"\r\n\
    INDEX 01 00:00:00\r\n\
FILE \"02.bin\" BINARY\r\n\
  TRACK 02 AUDIO\r\n\
    TITLE \"Deux\"\r\n\
    INDEX 00 00:00:00\r\n\
    INDEX 01 00:00:32\r\n\
FILE \"03-donnees.bin\" BINARY\r\n\
  TRACK 03 MODE1/2048\r\n\
    INDEX 01 00:00:00\r\n";
    let scratch = tune_core::test_scratch::scratch_dir("cue-bin-5298-multi");
    let dossier = monter(
        scratch.path(),
        "multi",
        feuille,
        &[
            ("01.bin", secteurs_audio(0, 100)),
            ("02.bin", secteurs_audio(0, 120)),
        ],
    );
    let db = base();
    importer(&db, scratch.path(), &dossier, 2);

    let un = piste(&db, &dossier.join("01.bin"), 0);
    assert_eq!(un.title, "Un");
    assert_eq!(un.file_path, None);
    let (debut, duree) = bornes(&un);
    assert_suite(
        &jouer(&dossier.join("01.bin"), debut, duree),
        0,
        100 * TRAMES_PAR_SECTEUR,
        "01.bin",
    );

    // 32 frames = 426 ms en base, 18 816 trames sur le disque.
    let deux = piste(&db, &dossier.join("02.bin"), 426);
    assert_eq!(deux.title, "Deux");
    assert_eq!(deux.artist_name.as_deref(), Some("Duo d'essai"));
    let (debut, duree) = bornes(&deux);
    assert_suite(
        &jouer(&dossier.join("02.bin"), debut, duree),
        32 * TRAMES_PAR_SECTEUR,
        120 * TRAMES_PAR_SECTEUR,
        "02.bin",
    );
}
