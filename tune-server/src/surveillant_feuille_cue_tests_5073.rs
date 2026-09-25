//! #5073 (Gros Bidon, fil 1904) — un album « FLAC unique + feuille CUE »
//! déposé Tune lancé était importé en UNE piste de 39:37, titrée d'après le
//! fichier, « Unknown Artist ».
//!
//! Le surveillant ne relayait que les fichiers audio (`is_audio_file`) : le
//! `.cue` n'arrivait jamais jusqu'à `auto_scan`, et chaque FLAC était réimporté
//! seul (`reimporter_fichier_surveillant`). Le découpage n'avait lieu qu'à une
//! analyse complète, par `cue_bibliotheque`.
//!
//! Ces épreuves jouent la voie de production : événements `notify` bruts
//! traduits par le gestionnaire du surveillant, attente d'écriture stable
//! (`settle_partition`), puis `traiter_le_lot_du_surveillant`.
//!
//! ⚠️ Base de FICHIER : sur `:memory:`, le pool de lecture clone la connexion
//! d'écriture et voit ce qu'une base réelle ne verrait pas.
use super::surveillant_retouche_tests_4896::flac_8_canaux;
use super::{ReglagesDuSurveillant, settle_partition, traiter_le_lot_du_surveillant};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::scanner::watcher::notify::Event;
use tune_core::scanner::watcher::notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};
use tune_core::scanner::watcher::{FileChange, rejouer_evenements_notify};

fn base_fichier(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, Arc<dyn DbBackend>) {
    let dossier = tune_core::test_scratch::scratch_dir(&format!("cue-5073-base-{epreuve}"));
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

/// La feuille de Didier, réduite à deux pistes. Track 2 à `debut` (mm:ss:ff).
fn feuille(titre_2: &str, debut_2: &str) -> String {
    format!(
        "PERFORMER \"Alain Bashung\"\r\n\
         TITLE \"Osez Joséphine\"\r\n\
         FILE \"Osez Joséphine.flac\" WAVE\r\n\
         \x20 TRACK 01 AUDIO\r\n\
         \x20   TITLE \"J'écume\"\r\n\
         \x20   INDEX 01 00:00:00\r\n\
         \x20 TRACK 02 AUDIO\r\n\
         \x20   TITLE \"{titre_2}\"\r\n\
         \x20   INDEX 01 {debut_2}\r\n"
    )
}

struct Album {
    /// Racine de musique, à garder vivante.
    _racine: tune_core::test_scratch::ScratchDir,
    racine: PathBuf,
    image: PathBuf,
    cue: PathBuf,
}

/// Le dossier d'album, vide. Racine sous le dossier courant et NON sous le
/// dossier temporaire : `is_tune_temp_file` écarte tout ce qui vit sous ce
/// dernier.
fn album(epreuve: &str) -> Album {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("surveillant-cue-5073-{epreuve}"),
    );
    let dossier = racine
        .join("Alain Bashung")
        .join("1991 - Osez Joséphine [FLAC 16-44 - CD Barclay France]");
    std::fs::create_dir_all(&dossier).unwrap();
    Album {
        racine: racine.to_path_buf(),
        image: dossier.join("Osez Joséphine.flac"),
        cue: dossier.join("Osez Joséphine.cue"),
        _racine: racine,
    }
}

fn ev(kind: EventKind, chemin: &Path) -> Event {
    Event::new(kind).add_path(chemin.to_path_buf())
}

/// UN lot du surveillant, des événements bruts à l'écriture en base, dans
/// l'ordre de la boucle de `spawn_file_watcher`.
fn un_lot(db: &Arc<dyn DbBackend>, a: &Album, evenements: Vec<Event>) -> Vec<FileChange> {
    let racines = vec![chaine(&a.racine)];
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

/// `(titre, file_path, cue_media_path, cue_start_ms)`, triées par début.
type Ligne = (String, Option<String>, Option<String>, Option<i64>);

fn pistes(db: &Arc<dyn DbBackend>) -> Vec<Ligne> {
    db.query_many(
        "SELECT title, file_path, cue_media_path, cue_start_ms FROM tracks \
         ORDER BY cue_start_ms, title",
        &[],
    )
    .unwrap()
    .iter()
    .map(|r| {
        (
            r[0].as_string().unwrap_or_default(),
            r[1].as_string(),
            r[2].as_string(),
            r[3].as_i64(),
        )
    })
    .collect()
}

fn titres(db: &Arc<dyn DbBackend>) -> Vec<String> {
    pistes(db).into_iter().map(|p| p.0).collect()
}

/// Les deux pistes de la feuille, découpées dans l'image, et rien d'autre.
fn assert_decoupe(db: &Arc<dyn DbBackend>, a: &Album, titre_2: &str, debut_2_ms: i64) {
    let image = Some(chaine(&a.image));
    assert_eq!(
        pistes(db),
        vec![
            ("J'écume".to_string(), None, image.clone(), Some(0)),
            (titre_2.to_string(), None, image, Some(debut_2_ms)),
        ],
        "l'image est découpée par sa feuille, sans piste « image entière »"
    );
    let albums = db
        .query_many("SELECT title FROM albums a WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id)", &[])
        .unwrap();
    assert_eq!(albums.len(), 1, "un seul album");
    assert_eq!(albums[0][0].as_string().as_deref(), Some("Osez Joséphine"));
}

/// Le FLAC et sa feuille, copiés ensemble dans la bibliothèque.
fn deposer_ensemble(db: &Arc<dyn DbBackend>, a: &Album) {
    std::fs::write(&a.image, flac_8_canaux()).unwrap();
    std::fs::write(&a.cue, feuille("Volutes (remix)", "00:00:03")).unwrap();
    un_lot(
        db,
        a,
        vec![
            ev(EventKind::Create(CreateKind::File), &a.image),
            ev(EventKind::Modify(ModifyKind::Any), &a.image),
            ev(EventKind::Create(CreateKind::File), &a.cue),
            ev(EventKind::Modify(ModifyKind::Any), &a.cue),
        ],
    );
    assert_decoupe(db, a, "Volutes (remix)", PISTE_2_MS);
}

/// 3 trames CD = 40 ms.
const PISTE_2_MS: i64 = 40;

#[test]
fn le_surveillant_relaie_la_feuille_cue_5073() {
    let a = album("relaie");
    std::fs::write(&a.cue, feuille("Volutes (remix)", "00:00:03")).unwrap();
    let changes = rejouer_evenements_notify(vec![ev(EventKind::Create(CreateKind::File), &a.cue)]);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].path, chaine(&a.cue));
    assert_eq!(
        changes[0].change_type,
        tune_core::scanner::watcher::ChangeType::Added
    );
    std::fs::remove_file(&a.cue).unwrap();
    let changes = rejouer_evenements_notify(vec![ev(EventKind::Remove(RemoveKind::Any), &a.cue)]);
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(
        changes[0].change_type,
        tune_core::scanner::watcher::ChangeType::Deleted,
        "une feuille supprimée est un fichier supprimé, pas un dossier disparu"
    );
}

#[test]
fn un_flac_depose_avec_sa_feuille_est_decoupe_5073() {
    let (_b, db) = base_fichier("ensemble");
    let a = album("ensemble");
    deposer_ensemble(&db, &a);
    assert_decoupe(&db, &a, "Volutes (remix)", PISTE_2_MS);
}

#[test]
fn la_feuille_arrivee_apres_le_flac_remplace_la_piste_entiere_5073() {
    let (_b, db) = base_fichier("apres");
    let a = album("apres");
    std::fs::write(&a.image, flac_8_canaux()).unwrap();
    un_lot(
        &db,
        &a,
        vec![ev(EventKind::Create(CreateKind::File), &a.image)],
    );
    let avant = pistes(&db);
    assert_eq!(
        avant.len(),
        1,
        "sans feuille, le FLAC est une piste : {avant:?}"
    );
    assert_eq!(avant[0].1, Some(chaine(&a.image)));

    std::fs::write(&a.cue, feuille("Volutes (remix)", "00:00:03")).unwrap();
    un_lot(
        &db,
        &a,
        vec![ev(EventKind::Create(CreateKind::File), &a.cue)],
    );
    assert_decoupe(&db, &a, "Volutes (remix)", PISTE_2_MS);

    // Un nouvel événement sur le FLAC (réenregistré, lu par un éditeur) ne
    // le réimporte pas en piste entière à côté de ses tranches.
    un_lot(
        &db,
        &a,
        vec![ev(EventKind::Modify(ModifyKind::Any), &a.image)],
    );
    assert_decoupe(&db, &a, "Volutes (remix)", PISTE_2_MS);
}

#[test]
fn la_feuille_retouchee_est_relue_5073() {
    let (_b, db) = base_fichier("retouche");
    let a = album("retouche");
    deposer_ensemble(&db, &a);
    // Titre corrigé et INDEX déplacé : la tranche à 40 ms n'est plus décrite.
    std::fs::write(&a.cue, feuille("Volutes", "00:00:06")).unwrap();
    un_lot(
        &db,
        &a,
        vec![ev(EventKind::Modify(ModifyKind::Any), &a.cue)],
    );
    assert_decoupe(&db, &a, "Volutes", 80);
}

#[test]
fn la_feuille_supprimee_rend_le_flac_en_piste_entiere_5073() {
    let (_b, db) = base_fichier("supprimee");
    let a = album("supprimee");
    deposer_ensemble(&db, &a);
    std::fs::remove_file(&a.cue).unwrap();
    un_lot(
        &db,
        &a,
        vec![ev(EventKind::Remove(RemoveKind::File), &a.cue)],
    );
    let apres = pistes(&db);
    assert_eq!(apres.len(), 1, "une piste, l'image entière : {apres:?}");
    assert_eq!(apres[0].1, Some(chaine(&a.image)), "{apres:?}");
    assert_eq!(apres[0].2, None, "plus aucune tranche : {apres:?}");
}

#[test]
fn le_flac_efface_emporte_ses_tranches_5073() {
    let (_b, db) = base_fichier("image-effacee");
    let a = album("image-effacee");
    deposer_ensemble(&db, &a);
    std::fs::remove_file(&a.image).unwrap();
    un_lot(
        &db,
        &a,
        vec![ev(EventKind::Remove(RemoveKind::File), &a.image)],
    );
    assert!(titres(&db).is_empty(), "{:?}", pistes(&db));
}
