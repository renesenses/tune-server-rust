//! La migration Windows emporte la base **et ses annexes**, ou échoue
//! proprement — jamais un demi-résultat.
//!
//! # Ce que ce fichier garde
//!
//! `tune-server/src/windows_migrate.rs` recopiait la base de *Program Files*
//! vers `%LOCALAPPDATA%\TuneServer` avec un `std::fs::copy(&old_db, &new_db)`
//! nu : le `.db` SEUL, sans son `-wal` ni son `-shm`. Une base SQLite n'est pas
//! un fichier — le `-wal` porte les transactions pas encore repliées. La base
//! arrivait donc amputée des dernières écritures, au premier démarrage après
//! une mise à jour, c'est-à-dire au moment où l'utilisateur s'y attend le
//! moins. Le dépôt savait déjà ce que ça coûte :
//! `tune_core::db_backup::replace_database` traite les deux suffixes ensemble
//! depuis longtemps, « sans cela SQLite rejoue le journal par-dessus le fichier
//! fraîchement copié et rend un mélange des deux bases ».
//!
//! # Pourquoi ce test n'a AUCUN `cfg`
//!
//! Tout `windows_migrate.rs` vivait sous `#[cfg(target_os = "windows")]` : sur
//! la machine de compilation (Linux) il n'existait pour aucun compilateur, et
//! un test qui aurait porté le même `cfg` aurait été **vert contre rien** —
//! c'est la famille de faux verts qui a déjà coûté cher ici. La règle et
//! **tous ses effets de bord** vivent désormais dans des fonctions sans `cfg` ;
//! seule la lecture de `%LOCALAPPDATA%` et du chemin de l'exécutable reste sous
//! `cfg`, et c'est la porte `Windows + ASIO` de la CI qui la compile.
//!
//! Les chemins fabriqués ici (« Program Files ») sont de simples noms de
//! dossiers : sous Linux ils sont parfaitement légitimes, et c'est tout ce que
//! la règle regarde.

use std::path::{Path, PathBuf};
use tune_core::db_backup::copier_base_sqlite;
use tune_core::test_scratch::{ScratchDir, scratch_dir_in};
use tune_server::windows_migrate::{
    ActionMigrationWindows, appliquer_plan_migration_windows, dans_program_files,
    plan_migration_windows,
};

/// Un dossier à soi, sous la cible de compilation et non sous `/tmp`, supprimé
/// à la sortie de portée (panique comprise).
fn bac(etiquette: &str) -> ScratchDir {
    scratch_dir_in(env!("CARGO_TARGET_TMPDIR"), etiquette)
}

/// Recueille le journal `tracing` du fil courant.
///
/// `set_default` est **par fil** : les tests d'un même binaire tournent en
/// parallèle sans se marcher dessus.
#[derive(Clone, Default)]
struct Journal(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Journal {
        self.clone()
    }
}

fn capter_le_journal() -> (Journal, tracing::subscriber::DefaultGuard) {
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let garde = tracing::subscriber::set_default(abonne);
    (journal, garde)
}

/// Une VRAIE base SQLite en mode WAL, dont le journal porte une écriture que le
/// `.db` ne contient pas encore.
///
/// La connexion est **rendue à l'appelant et doit rester ouverte** : SQLite
/// replie et efface le `-wal` à la fermeture de la dernière connexion. C'est
/// exactement la situation d'un serveur qu'on met à jour.
fn base_avec_wal_vivant(chemin: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(chemin).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal", "la base doit être en mode WAL");
    conn.execute_batch(
        "CREATE TABLE zones (nom TEXT NOT NULL);
         INSERT INTO zones (nom) VALUES ('salon');",
    )
    .unwrap();
    // Ce premier lot part dans le `.db` : sans lui, la contre-épreuve du témoin
    // ne prouverait rien (la copie nue n'aurait même pas la table).
    conn.query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()))
        .unwrap();
    // À partir d'ici plus rien ne se replie tout seul : la ligne suivante reste
    // dans le `-wal`, et nulle part ailleurs.
    conn.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
    conn.execute("INSERT INTO zones (nom) VALUES ('cuisine')", [])
        .unwrap();
    conn
}

fn zones(chemin: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(chemin).unwrap();
    let mut requete = conn.prepare("SELECT nom FROM zones ORDER BY nom").unwrap();
    let lignes: Vec<String> = requete
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    lignes
}

fn entrees(dossier: &Path) -> Vec<String> {
    let mut noms: Vec<String> = std::fs::read_dir(dossier)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    noms.sort();
    noms
}

// ---------------------------------------------------------------------------
// 1. Le défaut lui-même : les annexes suivent, et le contenu relu est complet.
// ---------------------------------------------------------------------------

#[test]
fn les_annexes_suivent_la_base_et_le_contenu_relu_est_complet() {
    let bac = bac("winmig-a7c254-annexes");
    let source_dir = bac.join("Program Files/Tune");
    let cible_dir = bac.join("AppData/Local/TuneServer");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&cible_dir).unwrap();

    let source = source_dir.join("tune.db");
    let _vivante = base_avec_wal_vivant(&source);

    // Témoin d'amont : le défaut n'existe que si le `-wal` porte vraiment
    // quelque chose. Sans cette assertion le test pourrait passer sur une base
    // déjà repliée, donc contre rien.
    let wal = source_dir.join("tune.db-wal");
    let shm = source_dir.join("tune.db-shm");
    assert!(wal.exists(), "témoin : la source a un -wal");
    assert!(
        std::fs::metadata(&wal).unwrap().len() > 0,
        "témoin : le -wal n'est pas vide"
    );
    assert!(shm.exists(), "témoin : la source a un -shm");

    // La copie NUE, celle d'avant le correctif : le `.db` seul.
    let cible_nue = cible_dir.join("copie_nue.db");
    std::fs::copy(&source, &cible_nue).unwrap();
    assert_eq!(
        zones(&cible_nue),
        vec!["salon".to_string()],
        "contre-épreuve : la copie du seul .db PERD l'écriture restée dans le -wal"
    );

    // La copie du correctif.
    let cible = cible_dir.join("tune.db");
    let octets = copier_base_sqlite(&source, &cible).expect("la copie doit réussir");
    assert!(octets > 0);

    assert!(cible.exists(), "le .db est à destination");
    assert!(
        cible_dir.join("tune.db-wal").exists(),
        "le -wal doit suivre la base"
    );
    assert!(
        cible_dir.join("tune.db-shm").exists(),
        "le -shm doit suivre la base"
    );

    assert_eq!(
        zones(&cible),
        vec!["cuisine".to_string(), "salon".to_string()],
        "la base migrée doit porter AUSSI l'écriture qui n'était que dans le -wal"
    );

    // La source n'est jamais touchée.
    assert!(source.exists() && wal.exists() && shm.exists());
}

// ---------------------------------------------------------------------------
// 2. Le témoin : une base sans annexes se copie comme avant.
// ---------------------------------------------------------------------------

#[test]
fn temoin_une_base_sans_annexes_se_copie_comme_avant() {
    let bac = bac("winmig-a7c254-temoin");
    let source_dir = bac.join("Program Files/Tune");
    let cible_dir = bac.join("AppData/Local/TuneServer");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&cible_dir).unwrap();

    let source = source_dir.join("tune.db");
    std::fs::write(&source, b"une base repliee, sans journal").unwrap();
    assert!(!source_dir.join("tune.db-wal").exists());
    assert!(!source_dir.join("tune.db-shm").exists());

    let cible = cible_dir.join("tune.db");
    let octets = copier_base_sqlite(&source, &cible).expect("la copie doit réussir");

    assert_eq!(octets, b"une base repliee, sans journal".len() as u64);
    assert_eq!(
        std::fs::read(&cible).unwrap(),
        b"une base repliee, sans journal"
    );
    assert_eq!(
        entrees(&cible_dir),
        vec!["tune.db".to_string()],
        "rien d'autre que la base — ni annexe fabriquée, ni temporaire oublié"
    );
    assert!(source.exists(), "la source est intacte");
}

// ---------------------------------------------------------------------------
// 3. La cible existe déjà : rien n'est écrasé, et c'est journalisé.
// ---------------------------------------------------------------------------

#[test]
fn cible_deja_presente_rien_n_est_ecrase_et_l_ancienne_est_nommee_au_journal() {
    let bac = bac("winmig-a7c254-deja");
    let exe_dir = bac.join("Program Files/Tune");
    let localappdata = bac.join("AppData/Local");
    let data_dir = localappdata.join("TuneServer");
    std::fs::create_dir_all(&exe_dir).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();

    let ancienne = exe_dir.join("tune.db");
    let en_place = data_dir.join("tune.db");
    std::fs::write(&ancienne, b"celle de Program Files").unwrap();
    std::fs::write(&en_place, b"celle de LOCALAPPDATA, la vraie").unwrap();

    let plan = plan_migration_windows(
        &exe_dir,
        Some(localappdata.to_str().unwrap()),
        |c: &Path| c.exists(),
    );
    assert_eq!(
        plan.action,
        ActionMigrationWindows::CibleDejaPresente {
            source: ancienne.clone(),
            cible: en_place.clone(),
        }
    );

    let (journal, _garde) = capter_le_journal();
    let migre = appliquer_plan_migration_windows(&plan);
    assert!(!migre, "rien n'est migré quand la cible existe");

    assert_eq!(
        std::fs::read(&en_place).unwrap(),
        b"celle de LOCALAPPDATA, la vraie",
        "la base en place ne doit JAMAIS être écrasée"
    );
    assert_eq!(
        std::fs::read(&ancienne).unwrap(),
        b"celle de Program Files",
        "l'ancienne est laissée EN PLACE, intacte"
    );
    assert_eq!(entrees(&data_dir), vec!["tune.db".to_string()]);

    let texte = journal.texte();
    assert!(
        texte.contains("windows_migrate_base_deja_presente_l_ancienne_est_laissee_intacte"),
        "le cas doit être journalisé, il ne l'était pas — journal :\n{texte}"
    );
    assert!(
        texte.contains(ancienne.to_str().unwrap()),
        "le journal doit NOMMER la base délaissée — journal :\n{texte}"
    );
}

// ---------------------------------------------------------------------------
// 4. Échec à mi-chemin : pas de temporaire, source intacte, cible inchangée.
// ---------------------------------------------------------------------------

#[test]
fn echec_a_mi_chemin_ne_laisse_ni_temporaire_ni_demi_base() {
    let bac = bac("winmig-a7c254-mi-chemin");
    let source_dir = bac.join("Program Files/Tune");
    let cible_dir = bac.join("AppData/Local/TuneServer");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&cible_dir).unwrap();

    let source = source_dir.join("tune.db");
    let wal = source_dir.join("tune.db-wal");
    let shm = source_dir.join("tune.db-shm");
    std::fs::write(&source, b"la base").unwrap();
    std::fs::write(&wal, b"le journal").unwrap();
    // Un `-shm` qui est un DOSSIER : `fs::copy` échoue dessus quel que soit
    // l'utilisateur qui exécute le test — un `chmod` ne prouverait rien sous
    // root, et la CI n'est pas la seule machine à jouer cette suite.
    std::fs::create_dir(&shm).unwrap();

    let cible = cible_dir.join("tune.db");
    let erreur = copier_base_sqlite(&source, &cible).expect_err("la copie doit échouer");
    assert!(
        erreur.contains("tune.db-shm"),
        "l'erreur doit nommer le fichier fautif : {erreur}"
    );

    assert_eq!(
        entrees(&cible_dir),
        Vec::<String>::new(),
        "aucun fichier — ni base à moitié posée, ni temporaire oublié"
    );
    assert_eq!(
        std::fs::read(&source).unwrap(),
        b"la base",
        "source intacte"
    );
    assert_eq!(std::fs::read(&wal).unwrap(), b"le journal", "-wal intact");
    assert!(shm.is_dir(), "-shm intact");
}

// ---------------------------------------------------------------------------
// 5. La règle : détection de Program Files et lecture de %LOCALAPPDATA%.
// ---------------------------------------------------------------------------

#[test]
fn program_files_est_detecte_dans_ses_trois_ecritures() {
    for chemin in [
        r"C:\Program Files\Tune",
        r"C:\Program Files (x86)\Tune",
        r"c:\program files\tune",
    ] {
        assert!(
            dans_program_files(Path::new(chemin)),
            "{chemin} doit être vu comme restreint"
        );
    }
    for chemin in [r"D:\Tune", r"C:\Users\jp\Tune", r"C:\ProgramData\Tune"] {
        assert!(
            !dans_program_files(Path::new(chemin)),
            "{chemin} n'est pas restreint"
        );
    }
}

#[test]
fn hors_program_files_le_plan_ne_fait_rien() {
    let plan = plan_migration_windows(
        Path::new(r"D:\Tune"),
        Some(r"C:\Users\jp\AppData\Local"),
        |_| panic!("le prédicat d'existence ne doit même pas être consulté"),
    );
    assert_eq!(plan.action, ActionMigrationWindows::HorsProgramFiles);
    assert_eq!(plan.data_dir, None);
    assert!(!appliquer_plan_migration_windows(&plan));
}

#[test]
fn sans_localappdata_aucun_chemin_n_est_fabrique() {
    for valeur in [None, Some("")] {
        let plan = plan_migration_windows(Path::new(r"C:\Program Files\Tune"), valeur, |_| true);
        assert_eq!(plan.action, ActionMigrationWindows::LocalappdataAbsent);
        assert_eq!(plan.data_dir, None);
    }
}

#[test]
fn sans_base_a_cote_de_l_executable_il_n_y_a_rien_a_migrer() {
    let plan = plan_migration_windows(
        Path::new(r"C:\Program Files\Tune"),
        Some(r"C:\Users\jp\AppData\Local"),
        |_| false,
    );
    assert_eq!(plan.action, ActionMigrationWindows::Aucune);
    assert!(plan.data_dir.is_some());
}

#[test]
fn une_base_a_cote_et_pas_de_cible_donne_migrer() {
    let exe_dir = PathBuf::from(r"C:\Program Files\Tune");
    let plan = plan_migration_windows(&exe_dir, Some(r"C:\Users\jp\AppData\Local"), |c: &Path| {
        c.starts_with(&exe_dir)
    });
    match plan.action {
        ActionMigrationWindows::Migrer { source, cible } => {
            assert!(source.ends_with("tune.db"));
            assert!(cible.ends_with("tune.db"));
            assert!(cible.to_string_lossy().contains("TuneServer"));
        }
        autre => panic!("attendu Migrer, obtenu {autre:?}"),
    }
}

// ---------------------------------------------------------------------------
// 6. Le chemin complet, effets de bord compris — sans aucun `cfg`.
// ---------------------------------------------------------------------------

#[test]
fn la_migration_complete_pose_la_base_et_ses_annexes_et_cree_le_dossier() {
    let bac = bac("winmig-a7c254-complet");
    let exe_dir = bac.join("Program Files/Tune");
    let localappdata = bac.join("AppData/Local");
    std::fs::create_dir_all(&exe_dir).unwrap();
    std::fs::create_dir_all(&localappdata).unwrap();
    // Le dossier de données n'existe pas encore : c'est `appliquer_…` qui doit
    // le créer, comme au premier démarrage après une mise à jour.
    let data_dir = localappdata.join("TuneServer");
    assert!(!data_dir.exists());

    let source = exe_dir.join("tune.db");
    let _vivante = base_avec_wal_vivant(&source);

    let plan = plan_migration_windows(
        &exe_dir,
        Some(localappdata.to_str().unwrap()),
        |c: &Path| c.exists(),
    );
    assert!(matches!(plan.action, ActionMigrationWindows::Migrer { .. }));

    let (journal, _garde) = capter_le_journal();
    assert!(
        appliquer_plan_migration_windows(&plan),
        "la migration doit réussir"
    );

    assert_eq!(
        entrees(&data_dir),
        vec![
            "tune.db".to_string(),
            "tune.db-shm".to_string(),
            "tune.db-wal".to_string(),
        ],
        "les TROIS fichiers, et rien d'autre : aucun temporaire ne survit"
    );
    assert_eq!(
        zones(&data_dir.join("tune.db")),
        vec!["cuisine".to_string(), "salon".to_string()],
        "la base migrée est complète, écriture du -wal comprise"
    );
    assert!(journal.texte().contains("windows_migrate_db_copied"));
}

#[test]
fn une_migration_echouee_laisse_la_base_d_origine_intacte() {
    let bac = bac("winmig-a7c254-echec");
    let exe_dir = bac.join("Program Files/Tune");
    let localappdata = bac.join("AppData/Local");
    std::fs::create_dir_all(&exe_dir).unwrap();
    std::fs::create_dir_all(&localappdata).unwrap();

    let source = exe_dir.join("tune.db");
    std::fs::write(&source, b"la base").unwrap();
    std::fs::write(exe_dir.join("tune.db-wal"), b"le journal").unwrap();
    std::fs::create_dir(exe_dir.join("tune.db-shm")).unwrap();

    let plan = plan_migration_windows(
        &exe_dir,
        Some(localappdata.to_str().unwrap()),
        |c: &Path| c.exists(),
    );
    let (journal, _garde) = capter_le_journal();
    assert!(!appliquer_plan_migration_windows(&plan));

    assert_eq!(
        entrees(&localappdata.join("TuneServer")),
        Vec::<String>::new(),
        "rien n'est laissé à destination"
    );
    assert_eq!(std::fs::read(&source).unwrap(), b"la base");
    assert!(journal.texte().contains("windows_migrate_db_copy_failed"));
}
