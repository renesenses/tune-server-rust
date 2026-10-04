//! Fil 2134 (Levente Toth) — « la Bibliothèque se recharge toutes les 1-2 s ».
//!
//! Le journal montre `watcher_library_updated_emis` toutes les 0,96 s pendant
//! que le surveillant réimporte les 1 121 fichiers que la gravure des DR vient
//! de réécrire. Deux correctifs, deux familles d'épreuves :
//!
//! * la cadence de l'annonce (`CadenceDesAnnonces`) : au plus une toutes les
//!   `ESPACEMENT_DES_ANNONCES`, plus une finale quand l'activité retombe ;
//! * un fichier déjà indexé dont la taille et la date n'ont pas bougé ne
//!   compte plus comme un changement (`ecarter_les_fichiers_conformes`).
use super::{
    CadenceDesAnnonces, ESPACEMENT_DES_ANNONCES, ecarter_les_fichiers_conformes,
    fichier_conforme_a_la_base,
};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::scanner::watcher::{ChangeType, FileChange};

/// Joue une suite de lots (`true` = le lot change quelque chose) espacés de
/// `pas`, et rend les instants (depuis le départ) où l'annonce part.
fn jouer(lots: &[bool], pas: Duration) -> Vec<Duration> {
    let depart = Instant::now();
    let mut cadence = CadenceDesAnnonces::default();
    let mut annonces = Vec::new();
    for (i, &a_change) in lots.iter().enumerate() {
        let t = pas * i as u32;
        if cadence.apres_le_lot(a_change, depart + t).is_some() {
            annonces.push(t);
        }
    }
    annonces
}

/// LE CAS DU JOURNAL : 20 lots qui changent, un toutes les 0,96 s (19 s de
/// rafale), puis le calme. Avant : 20 annonces. Après : une au départ, une
/// toutes les 5 s au plus, et une finale au premier lot calme.
#[test]
fn une_rafale_de_lots_ne_s_annonce_qu_une_fois_toutes_les_5_s_2134() {
    let mut lots = vec![true; 20];
    lots.extend([false, false, false]);
    let annonces = jouer(&lots, Duration::from_millis(960));
    assert!(
        annonces.len() <= 6,
        "19 s de rafale : au plus 1 + 19/5 + la finale, obtenu {annonces:?}"
    );
    assert_eq!(
        annonces.first(),
        Some(&Duration::ZERO),
        "le premier lot s'annonce tout de suite"
    );
    for paire in annonces.windows(2).take(annonces.len().saturating_sub(2)) {
        assert!(
            paire[1] - paire[0] >= ESPACEMENT_DES_ANNONCES,
            "deux annonces de rafale trop proches : {annonces:?}"
        );
    }
    assert_eq!(
        annonces.last(),
        Some(&(Duration::from_millis(960) * 20)),
        "la finale part au premier lot calme : {annonces:?}"
    );
}

/// Rien ne reste sans annonce : le lot retenu en fin de rafale est annoncé
/// par la finale, et les lots retenus entre deux annonces sont comptés.
#[test]
fn les_lots_retenus_sont_regroupes_dans_la_finale_2134() {
    let t0 = Instant::now();
    let mut c = CadenceDesAnnonces::default();
    assert_eq!(c.apres_le_lot(true, t0), Some(1));
    assert_eq!(c.apres_le_lot(true, t0 + Duration::from_secs(1)), None);
    assert_eq!(c.apres_le_lot(true, t0 + Duration::from_secs(2)), None);
    assert_eq!(c.apres_le_lot(false, t0 + Duration::from_secs(4)), Some(2));
    // Calme sans rien de retenu : silence.
    assert_eq!(c.apres_le_lot(false, t0 + Duration::from_secs(6)), None);
    // Un dépôt isolé, longtemps après : annoncé tout de suite.
    assert_eq!(c.apres_le_lot(true, t0 + Duration::from_secs(60)), Some(1));
    assert_eq!(c.apres_le_lot(false, t0 + Duration::from_secs(62)), None);
}

fn base(dossier: &Path) -> Arc<dyn DbBackend> {
    let db = tune_core::db::sqlite::SqliteDb::open(&dossier.join("tune.db").to_string_lossy())
        .expect("base de fichier");
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Indexe `chemin` avec la taille et la date que le disque porte maintenant.
fn indexer(db: &Arc<dyn DbBackend>, chemin: &Path) {
    let (taille, mtime) = tune_core::audio::iso9660::taille_et_mtime(chemin).unwrap();
    let c = chemin.to_string_lossy().into_owned();
    let titre = "piste".to_string();
    let taille = taille as i64;
    db.execute(
        "INSERT INTO tracks (title, file_path, file_size, file_mtime) VALUES (?1, ?2, ?3, ?4)",
        &[&titre as &dyn ToSqlValue, &c, &taille, &mtime],
    )
    .unwrap();
}

fn changement(t: ChangeType, p: &Path) -> FileChange {
    FileChange {
        change_type: t,
        path: p.to_string_lossy().into_owned(),
    }
}

/// Un fichier indexé, intact, sort du lot ; un fichier retouché, un fichier
/// inconnu, une suppression et la feuille CUE du dossier y restent.
#[test]
fn seuls_les_fichiers_conformes_a_leur_ligne_sortent_du_lot_2134() {
    let d = tempfile::tempdir().unwrap();
    let db = base(d.path());
    let intact = d.path().join("01.flac");
    let retouche = d.path().join("02.flac");
    let inconnu = d.path().join("03.flac");
    let feuille = d.path().join("album.cue");
    for f in [&intact, &retouche, &inconnu, &feuille] {
        std::fs::write(f, b"0123456789").unwrap();
    }
    indexer(&db, &intact);
    indexer(&db, &retouche);
    std::fs::write(&retouche, b"0123456789-et-plus").unwrap();

    assert!(fichier_conforme_a_la_base(&db, &intact.to_string_lossy()));
    assert!(!fichier_conforme_a_la_base(
        &db,
        &retouche.to_string_lossy()
    ));
    assert!(!fichier_conforme_a_la_base(&db, &inconnu.to_string_lossy()));

    let restants = ecarter_les_fichiers_conformes(
        &db,
        vec![
            changement(ChangeType::Modified, &intact),
            changement(ChangeType::Added, &intact),
            changement(ChangeType::Modified, &retouche),
            changement(ChangeType::Added, &inconnu),
            changement(ChangeType::Modified, &feuille),
            changement(ChangeType::Deleted, &intact),
        ],
    );
    let vus: Vec<(ChangeType, String)> = restants
        .into_iter()
        .map(|c| {
            let nom = Path::new(&c.path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (c.change_type, nom)
        })
        .collect();
    assert_eq!(
        vus,
        vec![
            (ChangeType::Modified, "02.flac".to_string()),
            (ChangeType::Added, "03.flac".to_string()),
            (ChangeType::Modified, "album.cue".to_string()),
            (ChangeType::Deleted, "01.flac".to_string()),
        ]
    );
}
