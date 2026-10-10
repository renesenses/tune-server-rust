//! #5299 — le surveillant suit les images `.iso` de données.
//!
//! Il ne relayait que les fichiers audio et les feuilles CUE : une image de
//! données déposée Tune lancé n'était indexée qu'au scan suivant, et une
//! image retirée gardait ses pistes en base jusque-là.
//!
//! Ces épreuves jouent la voie de production : événements `notify` bruts
//! traduits par le gestionnaire du surveillant, attente d'écriture stable
//! (`settle_partition`), puis `traiter_le_lot_du_surveillant`. Les images sont
//! fabriquées par `iso9660::fabrique`, sans outil externe.
//!
//! ⚠️ Base de FICHIER : sur `:memory:`, le pool de lecture clone la connexion
//! d'écriture et voit ce qu'une base réelle ne verrait pas.
use super::surveillant_retouche_tests_4896::flac_8_canaux;
use super::{
    ReglagesDuSurveillant, fichier_conforme_a_la_base, settle_partition,
    traiter_le_lot_du_surveillant,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::audio::iso9660::chemin_virtuel;
use tune_core::audio::iso9660::fabrique::{self, Noms};
use tune_core::db::backend::DbBackend;
use tune_core::scanner::watcher::notify::Event;
use tune_core::scanner::watcher::notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};
use tune_core::scanner::watcher::{ChangeType, FileChange, rejouer_evenements_notify};

fn base_fichier(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let dossier = tune_core::test_scratch::scratch_dir(&format!("iso-5299-base-{epreuve}"));
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

struct Banc {
    _racine: tune_core::test_scratch::ScratchDir,
    racine: PathBuf,
    image: PathBuf,
}

/// Racine sous le dossier courant et NON sous le dossier temporaire :
/// `is_tune_temp_file` écarte tout ce qui vit sous ce dernier.
fn banc(epreuve: &str) -> Banc {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("surveillant-iso-5299-{epreuve}"),
    );
    let dossier = racine.join("Gravures");
    std::fs::create_dir_all(&dossier).unwrap();
    Banc {
        racine: racine.to_path_buf(),
        image: dossier.join("Disque de donnees.iso"),
        _racine: racine,
    }
}

const PISTE_1: &str = "Un album/CD1/01 - Premier titre au nom bien long.flac";
const PISTE_2: &str = "Un album/CD2/02 - Second.flac";
const PISTE_3: &str = "Un album/CD2/03 - Ajout.flac";

fn graver(b: &Banc, pistes: &[&str]) {
    let mut contenu: fabrique::Contenu = pistes
        .iter()
        .map(|p| (p.to_string(), flac_8_canaux()))
        .collect();
    contenu.push(("LISEZMOI.TXT".into(), b"pas de l'audio".to_vec()));
    std::fs::write(&b.image, fabrique::iso(&contenu, Noms::Joliet)).unwrap();
}

fn ev(kind: EventKind, chemin: &Path) -> Event {
    Event::new(kind).add_path(chemin.to_path_buf())
}

fn un_lot(db: &Arc<dyn DbBackend>, b: &Banc, evenements: Vec<Event>) -> Vec<FileChange> {
    let racines = vec![chaine(&b.racine)];
    let (changes, en_ecriture) = settle_partition(rejouer_evenements_notify(evenements), &[]);
    assert!(en_ecriture.is_empty(), "écriture stable : {en_ecriture:?}");
    let mut attente = Vec::new();
    traiter_le_lot_du_surveillant(
        db,
        changes,
        &ReglagesDuSurveillant {
            exclusions: &[],
            racines: &racines,
            quality_split: true,
        },
        &mut attente,
    )
}

fn chemins(db: &Arc<dyn DbBackend>) -> Vec<String> {
    db.query_many("SELECT file_path FROM tracks ORDER BY file_path", &[])
        .unwrap()
        .iter()
        .filter_map(|r| r[0].as_string())
        .collect()
}

fn virtuels(b: &Banc, pistes: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = pistes.iter().map(|p| chemin_virtuel(&b.image, p)).collect();
    v.sort();
    v
}

#[test]
fn le_surveillant_relaie_l_image_iso_5299() {
    let b = banc("relaie");
    graver(&b, &[PISTE_1]);
    let changes =
        rejouer_evenements_notify(vec![ev(EventKind::Create(CreateKind::File), &b.image)]);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].path, chaine(&b.image));
    assert_eq!(changes[0].change_type, ChangeType::Added);
    std::fs::remove_file(&b.image).unwrap();
    let changes = rejouer_evenements_notify(vec![ev(EventKind::Remove(RemoveKind::Any), &b.image)]);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(
        changes[0].change_type,
        ChangeType::Deleted,
        "une image supprimée est un fichier supprimé, pas un dossier disparu"
    );
}

#[test]
fn une_image_deposee_est_indexee_sans_attendre_le_scan_5299() {
    let (_d, db) = base_fichier("deposee");
    let b = banc("deposee");
    graver(&b, &[PISTE_1, PISTE_2]);
    un_lot(
        &db,
        &b,
        vec![
            ev(EventKind::Create(CreateKind::File), &b.image),
            ev(EventKind::Modify(ModifyKind::Any), &b.image),
        ],
    );
    assert_eq!(chemins(&db), virtuels(&b, &[PISTE_1, PISTE_2]));
    // La garde « fichier inchangé » reconnaît une piste d'image : un nouvel
    // événement sur l'image intacte ne relit rien.
    assert!(fichier_conforme_a_la_base(
        &db,
        &chemin_virtuel(&b.image, PISTE_1)
    ));
}

#[test]
fn une_image_regravee_suit_son_contenu_5299() {
    let (_d, db) = base_fichier("regravee");
    let b = banc("regravee");
    graver(&b, &[PISTE_1, PISTE_2]);
    un_lot(
        &db,
        &b,
        vec![ev(EventKind::Create(CreateKind::File), &b.image)],
    );
    assert_eq!(chemins(&db), virtuels(&b, &[PISTE_1, PISTE_2]));
    // Même image, contenu changé : PISTE_2 disparaît, PISTE_3 arrive.
    std::thread::sleep(std::time::Duration::from_millis(20));
    graver(&b, &[PISTE_1, PISTE_3]);
    un_lot(
        &db,
        &b,
        vec![ev(EventKind::Modify(ModifyKind::Any), &b.image)],
    );
    assert_eq!(chemins(&db), virtuels(&b, &[PISTE_1, PISTE_3]));
}

#[test]
fn une_image_retiree_retire_ses_pistes_5299() {
    let (_d, db) = base_fichier("retiree");
    let b = banc("retiree");
    graver(&b, &[PISTE_1, PISTE_2]);
    un_lot(
        &db,
        &b,
        vec![ev(EventKind::Create(CreateKind::File), &b.image)],
    );
    assert_eq!(chemins(&db).len(), 2);
    std::fs::remove_file(&b.image).unwrap();
    un_lot(
        &db,
        &b,
        vec![ev(EventKind::Remove(RemoveKind::File), &b.image)],
    );
    assert!(chemins(&db).is_empty(), "{:?}", chemins(&db));
}

/// Une image présente mais illisible ne retire rien : seul un fichier absent
/// fait disparaître des pistes.
#[test]
fn une_image_illisible_ne_retire_rien_5299() {
    let (_d, db) = base_fichier("illisible");
    let b = banc("illisible");
    graver(&b, &[PISTE_1]);
    un_lot(
        &db,
        &b,
        vec![ev(EventKind::Create(CreateKind::File), &b.image)],
    );
    assert_eq!(chemins(&db).len(), 1);
    std::fs::write(&b.image, vec![0u8; 64 * 2048]).unwrap();
    un_lot(
        &db,
        &b,
        vec![ev(EventKind::Modify(ModifyKind::Any), &b.image)],
    );
    assert_eq!(chemins(&db), virtuels(&b, &[PISTE_1]));
}
