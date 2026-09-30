//! #5202 — les pochettes d'un lot de scan : hors de la transaction, et bornées.
//!
//! Le « fichier muet » est un TUBE NOMMÉ (`mkfifo`) déguisé en `.flac` :
//! l'ouvrir en lecture bloque dans le noyau tant que personne ne l'ouvre en
//! écriture. C'est un vrai appel système qui ne rend pas la main, comme une
//! lecture sur un partage SMB tombé, sans rien injecter dans le code lu.
use super::*;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tune_core::db::sqlite::SqliteDb;
use tune_core::metadata::TrackMetadata;

/// Un tube nommé. `Drop` l'ouvre en écriture, sans bloquer, puis le referme :
/// tout lecteur resté pendu reçoit une fin de fichier et rend la main — même
/// quand le témoin a rougi.
struct Tube(std::path::PathBuf);

impl Tube {
    fn new(chemin: std::path::PathBuf) -> Self {
        let ok = std::process::Command::new("mkfifo")
            .arg(&chemin)
            .status()
            .expect("mkfifo")
            .success();
        assert!(ok, "mkfifo {}", chemin.display());
        Self(chemin)
    }
}

impl Drop for Tube {
    fn drop(&mut self) {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.0);
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    Arc::new(db)
}

fn fichier(chemin: &std::path::Path, piste: u32) -> ScannedFile {
    ScannedFile {
        path: chemin.to_string_lossy().into_owned(),
        metadata: Some(TrackMetadata {
            title: Some(format!("Symphonie {piste}")),
            artist: Some("Leonard Bernstein".into()),
            album: Some("The Symphony Edition".into()),
            album_artist: Some("Leonard Bernstein".into()),
            track_number: Some(piste),
            ..Default::default()
        }),
        unsupported: None,
        audio_hash: None,
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

/// Ce que le fil de travail annonce, dans l'ordre.
enum Etape {
    Importe,
    Pochettes {
        a_poser: usize,
        expirees: usize,
        annule: bool,
    },
}

/// Importe `fichiers` en mode différé, puis traite leurs pochettes, sur un
/// fil à part : un témoin qui rougit ne pend pas le banc.
fn lancer(
    db: Arc<dyn DbBackend>,
    cache: std::path::PathBuf,
    fichiers: Vec<ScannedFile>,
    delai: Duration,
    arret: Arc<std::sync::atomic::AtomicBool>,
) -> mpsc::Receiver<Etape> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut imp =
            TrackImporter::new(db, true, cache, PorteeDuScan::TOUT).avec_pochettes_differees();
        imp.begin_batch(&fichiers);
        for f in &fichiers {
            imp.import(f).expect("import");
        }
        let _ = tx.send(Etape::Importe);
        let arret = move || arret.load(std::sync::atomic::Ordering::SeqCst);
        let mut lectures = crate::lecture_bornee::LecturesBornees::new(delai, &arret);
        let a_poser = imp.traiter_les_pochettes_differees(&mut lectures, 0);
        let _ = tx.send(Etape::Pochettes {
            a_poser: a_poser.len(),
            expirees: lectures.expirees,
            annule: lectures.annule,
        });
    });
    rx
}

fn attendre_l_import(rx: &mpsc::Receiver<Etape>) {
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Etape::Importe) => {}
        _ => panic!(
            "#5202 : l'import, qui tourne DANS la transaction du lot, relit la pochette d'un \
             fichier muet et ne rend pas la main"
        ),
    }
}

fn attendre_les_pochettes(rx: &mpsc::Receiver<Etape>, message: &str) -> (usize, usize, bool) {
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Etape::Pochettes {
            a_poser,
            expirees,
            annule,
        }) => (a_poser, expirees, annule),
        _ => panic!("{message}"),
    }
}

#[test]
fn un_fichier_muet_ne_retient_ni_la_transaction_ni_le_lot_pour_sa_pochette_5202() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("Bernstein").join("Disc 4");
    std::fs::create_dir_all(&dossier).unwrap();
    let tube = Tube::new(dossier.join("01.flac"));
    let rx = lancer(
        base(),
        tmp.path().join("cache"),
        vec![fichier(&tube.0, 1)],
        Duration::from_millis(200),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    attendre_l_import(&rx);
    let (a_poser, expirees, _) = attendre_les_pochettes(
        &rx,
        "#5202 : la pochette d'un fichier muet fige le lot, faute de délai",
    );
    assert_eq!(a_poser, 0, "rien n'a été lu : rien à poser");
    assert_eq!(
        expirees, 1,
        "le fichier muet est sauté au bout du délai, et compté"
    );
    drop(tube);
}

#[test]
fn un_stockage_muet_n_est_plus_lu_pour_les_pochettes_apres_trois_expirations_5202() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("Bernstein").join("Disc 5");
    std::fs::create_dir_all(&dossier).unwrap();
    let tubes: Vec<Tube> = (1..=6)
        .map(|i| Tube::new(dossier.join(format!("{i:02}.flac"))))
        .collect();
    let fichiers = tubes
        .iter()
        .enumerate()
        .map(|(i, t)| fichier(&t.0, i as u32 + 1))
        .collect();
    let debut = Instant::now();
    let rx = lancer(
        base(),
        tmp.path().join("cache"),
        fichiers,
        Duration::from_millis(200),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );
    attendre_l_import(&rx);
    let (_, expirees, _) = attendre_les_pochettes(
        &rx,
        "#5202 : les pochettes d'un stockage muet figent le lot, faute de délai",
    );
    assert_eq!(
        expirees,
        crate::lecture_bornee::EXPIRATIONS_AVANT_ABANDON,
        "#5202 : un stockage qui ne répond plus doit être abandonné après trois expirations, \
         pas attendu fichier après fichier ({:?})",
        debut.elapsed()
    );
    drop(tubes);
}

#[test]
fn arreter_pendant_une_lecture_de_pochette_bloquee_rend_la_main_5202() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("Bernstein").join("Disc 6");
    std::fs::create_dir_all(&dossier).unwrap();
    let tube = Tube::new(dossier.join("01.flac"));
    let arret = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rx = lancer(
        base(),
        tmp.path().join("cache"),
        vec![fichier(&tube.0, 1)],
        // Un délai bien plus long que l'attente du témoin : seul l'arrêt
        // peut rendre la main à temps.
        Duration::from_secs(60),
        arret.clone(),
    );
    attendre_l_import(&rx);
    std::thread::sleep(Duration::from_millis(300));
    arret.store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, _, annule) = attendre_les_pochettes(
        &rx,
        "#5202 : Arrêter doit agir pendant une lecture de pochette bloquée, pas après son délai",
    );
    assert!(annule, "l'arrêt est constaté");
    drop(tube);
}

#[test]
fn en_mode_differe_la_pochette_de_l_album_est_posee_apres_l_import_5202() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("Bernstein").join("Disc 7");
    std::fs::create_dir_all(&dossier).unwrap();
    std::fs::write(dossier.join("cover.jpg"), b"IMAGE-DISC-7").unwrap();
    let chemin = dossier.join("01.flac");
    std::fs::write(&chemin, b"pas-du-vrai-audio").unwrap();
    let db = base();
    let mut imp = TrackImporter::new(
        db.clone(),
        true,
        tmp.path().join("cache"),
        PorteeDuScan::TOUT,
    )
    .avec_pochettes_differees();
    let f = fichier(&chemin, 1);
    imp.begin_batch(std::slice::from_ref(&f));
    let (_, album_id) = imp.import(&f).expect("import");
    let aid = album_id.expect("album");
    let albums = AlbumRepo::with_backend(db.clone());
    assert_eq!(
        albums.get(aid).unwrap().unwrap().cover_path,
        None,
        "la pochette ne se pose plus pendant l'import (dans la transaction)"
    );
    let jamais = || false;
    let mut lectures =
        crate::lecture_bornee::LecturesBornees::new(Duration::from_secs(30), &jamais);
    imp.traiter_les_pochettes_differees(&mut lectures, 0);
    assert!(
        albums.get(aid).unwrap().unwrap().cover_path.is_some(),
        "#5202 : le travail différé doit poser la pochette de l'album depuis l'image du dossier"
    );
    assert_eq!(imp.artwork_extracted(), 1);
}
