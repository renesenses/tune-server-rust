//! Le surveillant de fichiers écrivait DANS la transaction d'un lot de scan.
//!
//! SQLite n'a qu'une connexion d'écriture pour tout le serveur. Un lot de scan
//! y ouvre `BEGIN IMMEDIATE` et la tient, à travers de nombreux appels, jusqu'à
//! son `COMMIT` ; le verrou interne de la connexion est relâché entre deux
//! appels. Seuls le scan et la file d'attente passaient par la porte
//! `sqlite_write_gate` qui couvre cet intervalle : le surveillant, non. Ses
//! écritures s'intercalaient donc entre deux instructions du lot et entraient
//! dans SA transaction, encore ouverte.
//!
//! Or le surveillant relit ensuite par le POOL DE LECTURE, fait de connexions
//! séparées, qui ne voient pas ce que cette transaction n'a pas encore validé :
//!
//! - **piste réenregistrée à l'identique** (date changée, contenu intact) :
//!   `delete_by_path` part dans la transaction du lot ; la recherche de doublon
//!   (`paths_by_audio_hash_and_album`, pool de lecture) retrouve encore
//!   l'ANCIENNE ligne, donc le fichier lui-même ; la comparaison octet par
//!   octet le déclare identique à lui-même, la relecture est sautée
//!   (`watcher_skip_duplicate_audio_hash`)… et la suppression est validée par
//!   le `COMMIT` du lot. La piste disparaît de la bibliothèque.
//! - **dossier d'album renommé** : `deplacer_fichiers` passe par `write_tx`,
//!   qui échoue (« cannot start a transaction within a transaction ») ; les
//!   pistes sont retirées puis réimportées comme neuves — identifiants,
//!   favoris, écoutes perdus.
//!
//! ⚠️ Base de FICHIER obligatoire : `SqliteDb::open_in_memory` donne au pool de
//! lecture des clones de la connexion d'écriture, qui voient la transaction en
//! cours. Sur `:memory:`, le premier défaut ne se montre pas.
use super::surveillant_retouche_tests_4896::coffret_indexe;
use super::{ReglagesDuSurveillant, settle_partition, traiter_le_lot_du_surveillant};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, SystemTime};
use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::notify::Event;
use tune_core::scanner::watcher::notify::event::{EventKind, ModifyKind, RenameMode};
use tune_core::scanner::watcher::{ChangeType, FileChange, rejouer_evenements_notify};

/// Une base SQLite de FICHIER : pool de lecture fait de connexions séparées,
/// comme en production.
fn base_fichier(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let dossier = tune_core::test_scratch::scratch_dir(&format!("lot-de-scan-{epreuve}"));
    let chemin = dossier.join("tune-epreuve.db");
    let db =
        tune_core::db::sqlite::SqliteDb::open(&chemin.to_string_lossy()).expect("base de fichier");
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    (dossier, Arc::new(db))
}

fn chaine(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Un lot de scan en vol, tel que `routes/system/scan.rs` le tient : la porte,
/// puis `BEGIN IMMEDIATE` sur la connexion d'écriture partagée, et le `COMMIT`
/// plus tard. `surveillant` tourne pendant ce temps. Le lot garde sa
/// transaction jusqu'à ce que le surveillant ait fini — ou, s'il attend la
/// porte, au plus `ATTENTE` — puis valide.
fn pendant_un_lot_de_scan(db: &Arc<dyn DbBackend>, surveillant: impl FnOnce()) {
    const ATTENTE: Duration = Duration::from_secs(5);
    let (ouvert, lot_ouvert) = mpsc::channel::<()>();
    let (fini, surveillant_fini) = mpsc::channel::<()>();
    let db_du_scan = db.clone();
    let scan = std::thread::spawn(move || {
        let _porte = crate::sqlite_write_gate::scan_batch();
        db_du_scan
            .execute_batch("BEGIN IMMEDIATE")
            .expect("BEGIN du lot");
        ouvert.send(()).unwrap();
        let _ = surveillant_fini.recv_timeout(ATTENTE);
        db_du_scan.execute_batch("COMMIT").expect("COMMIT du lot");
    });
    lot_ouvert.recv().expect("le lot a ouvert sa transaction");
    surveillant();
    let _ = fini.send(());
    scan.join().expect("fil du scan");
}

/// UN lot du surveillant, dans l'ordre de la boucle de `spawn_file_watcher`.
fn un_tour(
    db: &Arc<dyn DbBackend>,
    racine: &Path,
    a_suivre: Vec<FileChange>,
    attente: &mut Vec<String>,
) -> Vec<FileChange> {
    let racines = vec![chaine(racine)];
    let (changes, mut en_ecriture) = settle_partition(a_suivre, &[]);
    let a_relire = traiter_le_lot_du_surveillant(
        db,
        changes,
        &ReglagesDuSurveillant {
            exclusions: &[],
            racines: &racines,
            quality_split: true,
        },
        attente,
    );
    en_ecriture.extend(a_relire);
    en_ecriture
}

/// Réenregistre le fichier À L'IDENTIQUE : même contenu, date avancée. C'est
/// ce que fait un éditeur de balises qui enregistre sans rien changer, un
/// `touch`, une synchronisation qui recopie le fichier sur lui-même.
fn reenregistrer_a_l_identique(piste: &Path) {
    std::fs::File::options()
        .write(true)
        .open(piste)
        .and_then(|f| f.set_modified(SystemTime::now()))
        .expect("date de modification");
}

fn modifie(piste: &Path) -> Vec<FileChange> {
    vec![FileChange {
        change_type: ChangeType::Modified,
        path: chaine(piste),
    }]
}

fn ligne(db: &Arc<dyn DbBackend>, piste: &Path) -> Option<i64> {
    TrackRepo::with_backend(db.clone())
        .get_by_path(&chaine(piste))
        .unwrap()
        .and_then(|t| t.id)
}

/// Témoin, hors de tout lot de scan : la piste réenregistrée à l'identique est
/// relue et reste en base. C'est ce qui prouve que la perte mesurée plus bas
/// vient du lot de scan, et de rien d'autre.
#[test]
fn temoin_hors_scan_une_piste_reenregistree_a_l_identique_reste_en_base() {
    let (_base, db) = base_fichier("temoin");
    let (racine, pistes) = coffret_indexe(&db, "lot-de-scan-temoin");
    assert!(ligne(&db, &pistes[0]).is_some(), "montage : piste indexée");
    reenregistrer_a_l_identique(&pistes[0]);
    un_tour(&db, &racine, modifie(&pistes[0]), &mut Vec::new());
    assert!(
        ligne(&db, &pistes[0]).is_some(),
        "hors scan, une piste réenregistrée à l'identique reste en base"
    );
}

/// La cause. Un lot de scan tient sa transaction ; le surveillant relit une
/// piste réenregistrée à l'identique. Sans la porte, la suppression part dans
/// la transaction du lot, la recherche de doublon (pool de lecture) retrouve
/// le fichier lui-même, la relecture est sautée, et le `COMMIT` du lot valide
/// la suppression.
#[test]
fn pendant_un_lot_de_scan_une_piste_reenregistree_a_l_identique_reste_en_base() {
    let (_base, db) = base_fichier("identique");
    let (racine, pistes) = coffret_indexe(&db, "lot-de-scan-identique");
    assert!(ligne(&db, &pistes[0]).is_some(), "montage : piste indexée");
    reenregistrer_a_l_identique(&pistes[0]);
    pendant_un_lot_de_scan(&db, || {
        un_tour(&db, &racine, modifie(&pistes[0]), &mut Vec::new());
    });
    assert!(
        ligne(&db, &pistes[0]).is_some(),
        "une piste réenregistrée à l'identique PENDANT un lot de scan doit rester en base : \
         sans la porte du scan, le surveillant supprime la ligne dans la transaction du lot, \
         la recherche de doublon (pool de lecture, aveugle à cette transaction) retrouve le \
         fichier lui-même, la relecture est sautée, et le COMMIT du lot valide la suppression"
    );
}

/// Second effet de la même cause : un dossier d'album renommé pendant un lot
/// de scan. `deplacer_fichiers` passe par `write_tx`, refusé tant que le lot
/// tient la connexion : les pistes perdaient leur ligne (identifiants,
/// favoris, écoutes) et revenaient comme neuves.
#[test]
fn pendant_un_lot_de_scan_un_dossier_renomme_garde_ses_pistes() {
    let (_base, db) = base_fichier("dossier");
    let (racine, pistes) = coffret_indexe(&db, "lot-de-scan-dossier");
    let avant: Vec<Option<i64>> = pistes.iter().map(|p| ligne(&db, p)).collect();
    assert!(
        avant.iter().all(Option::is_some),
        "montage : coffret indexé"
    );
    let ancien = pistes[0].parent().unwrap().to_path_buf();
    let nouveau = ancien.with_file_name("Multichannel 7.1 (2023)");
    std::fs::rename(&ancien, &nouveau).unwrap();
    let nouvelles: Vec<PathBuf> = pistes
        .iter()
        .map(|p| nouveau.join(p.file_name().unwrap()))
        .collect();
    // inotify : MOVED_FROM, MOVED_TO.
    let nom = |mode| EventKind::Modify(ModifyKind::Name(mode));
    let evenements = vec![
        Event::new(nom(RenameMode::From)).add_path(ancien.clone()),
        Event::new(nom(RenameMode::To)).add_path(nouveau.clone()),
    ];
    let mut attente = Vec::new();
    pendant_un_lot_de_scan(&db, || {
        let mut a_suivre = rejouer_evenements_notify(evenements);
        for _ in 0..3 {
            a_suivre = un_tour(&db, &racine, a_suivre, &mut attente);
        }
    });
    assert_eq!(
        nouvelles.iter().map(|p| ligne(&db, p)).collect::<Vec<_>>(),
        avant,
        "un dossier renommé PENDANT un lot de scan garde ses lignes de piste sous le nouveau \
         nom : sans la porte du scan, `deplacer_fichiers` (write_tx) est refusé tant que le lot \
         tient la connexion, et les pistes reviennent comme neuves"
    );
}
