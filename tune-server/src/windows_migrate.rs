//! Démarrage Windows : détecter une installation sous *Program Files* et
//! amener les données dans `%LOCALAPPDATA%\TuneServer`.
//!
//! Appelé une fois au démarrage, avant l'initialisation de la base.
//!
//! # Ce que la migration perdait
//!
//! Le geste était `std::fs::copy(&old_db, &new_db)` : le `.db` SEUL. Une base
//! SQLite n'est pas un fichier — le `-wal` porte les transactions pas encore
//! repliées, le `-shm` l'index de ce journal. Copier la base sans son journal
//! rend une base amputée des dernières écritures ; y laisser un journal
//! étranger fait rejouer ce qui n'est pas le sien, et
//! `tune_core::db_backup::replace_database` le dit depuis longtemps : « sans
//! cela SQLite rejoue le journal par-dessus le fichier fraîchement copié et
//! rend un mélange des deux bases ». Ce sont la bibliothèque et les zones d'un
//! utilisateur Windows, au premier démarrage après une mise à jour.
//!
//! La copie est désormais celle de [`tune_core::db_backup::copier_base_sqlite`],
//! qui emporte les annexes et ne laisse jamais de demi-résultat.
//!
//! # Pourquoi la règle ne vit plus sous `#[cfg(target_os = "windows")]`
//!
//! Tout ce fichier portait ce `cfg` : sur la machine de compilation (Linux) il
//! n'existait pour aucun compilateur, et un test qui aurait porté le même `cfg`
//! aurait été **vert contre rien**. La règle — quels fichiers, dans quel ordre,
//! que faire quand la cible existe déjà — et **tous ses effets de bord** vivent
//! maintenant dans [`plan_migration_windows`] et
//! [`appliquer_plan_migration_windows`], sans `cfg`, éprouvées sur Linux par
//! `tests/migration_windows_annexes_sqlite.rs`. Seul [`check_and_migrate`]
//! reste sous `cfg` : il ne fait que lire ce que la règle ne peut pas savoir —
//! le chemin de l'exécutable et `%LOCALAPPDATA%`.
//!
//! Le patron est celui de `tune_core::config::resolve_local_audio_backend`,
//! qui prend son `lookup` en paramètre « pour que la règle soit vérifiable sans
//! toucher à l'environnement du processus ».

use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

/// Le dossier de données de Tune sous `%LOCALAPPDATA%`.
pub const WINDOWS_DATA_SUBDIR: &str = "TuneServer";

/// Le nom de la base migrée, tel qu'il est posé par les installeurs Windows.
pub const NOM_BASE: &str = "tune.db";

/// Ce que le démarrage doit faire, une fois la règle appliquée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionMigrationWindows {
    /// L'exécutable ne tourne pas depuis un dossier restreint : rien à faire.
    HorsProgramFiles,
    /// `%LOCALAPPDATA%` est introuvable : on ne fabrique pas un chemin au hasard.
    LocalappdataAbsent,
    /// Aucune base à côté de l'exécutable — rien à migrer.
    Aucune,
    /// Une base vit à côté de l'exécutable et **aucune** dans le dossier de
    /// données : elle doit y être recopiée, annexes comprises.
    Migrer { source: PathBuf, cible: PathBuf },
    /// Les DEUX existent. La base du dossier de données l'emporte, et l'autre
    /// est laissée EN PLACE, intacte, nommée dans le journal. **On ne détruit
    /// jamais une base existante.**
    CibleDejaPresente { source: PathBuf, cible: PathBuf },
}

/// Le plan rendu par la règle : le dossier de données visé, et ce qu'il faut
/// faire de la base trouvée à côté de l'exécutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanMigrationWindows {
    /// Le dossier de l'exécutable, gardé pour le journal.
    pub exe_dir: PathBuf,
    /// Le dossier de données à créer, quand il y en a un.
    pub data_dir: Option<PathBuf>,
    pub action: ActionMigrationWindows,
}

/// L'installation tourne-t-elle depuis un dossier restreint ?
///
/// `Program Files (x86)` est testé d'abord parce qu'il contient `program
/// files` : l'ordre n'a aucun effet sur le résultat, il rend seulement
/// l'intention lisible.
pub fn dans_program_files(exe_dir: &Path) -> bool {
    let bas = exe_dir.to_string_lossy().to_lowercase();
    bas.contains("program files (x86)") || bas.contains("program files")
}

/// La règle : où doit vivre la base, et que faire de celle qu'on trouve à côté
/// de l'exécutable.
///
/// Fonction **pure**, et volontairement **sans `cfg`**. Tout ce qu'elle a
/// besoin de savoir lui est passé : le dossier de l'exécutable,
/// `%LOCALAPPDATA%`, et un prédicat d'existence.
pub fn plan_migration_windows(
    exe_dir: &Path,
    localappdata: Option<&str>,
    existe: impl Fn(&Path) -> bool,
) -> PlanMigrationWindows {
    let nu = |action| PlanMigrationWindows {
        exe_dir: exe_dir.to_path_buf(),
        data_dir: None,
        action,
    };
    if !dans_program_files(exe_dir) {
        return nu(ActionMigrationWindows::HorsProgramFiles);
    }
    let Some(localappdata) = localappdata.filter(|v| !v.is_empty()) else {
        return nu(ActionMigrationWindows::LocalappdataAbsent);
    };
    let data_dir = PathBuf::from(localappdata).join(WINDOWS_DATA_SUBDIR);
    let source = exe_dir.join(NOM_BASE);
    let cible = data_dir.join(NOM_BASE);
    // Cas dégénéré : l'exécutable tourne DEPUIS le dossier de données. Les deux
    // chemins désignent alors le même fichier — rien à migrer, et surtout rien
    // à « délaisser ».
    let action = if source == cible {
        ActionMigrationWindows::Aucune
    } else {
        match (existe(&source), existe(&cible)) {
            (true, false) => ActionMigrationWindows::Migrer { source, cible },
            (true, true) => ActionMigrationWindows::CibleDejaPresente { source, cible },
            (false, _) => ActionMigrationWindows::Aucune,
        }
    };
    PlanMigrationWindows {
        exe_dir: exe_dir.to_path_buf(),
        data_dir: Some(data_dir),
        action,
    }
}

/// Applique le plan : crée le dossier de données, puis migre s'il le faut.
///
/// Sans `cfg`, comme la règle : **les effets de bord aussi** sont ainsi
/// éprouvés sur la machine de compilation. La copie est déléguée à
/// [`tune_core::db_backup::copier_base_sqlite`] — un seul geste, un seul
/// endroit à corriger la prochaine fois.
///
/// Rend `true` quand une base a effectivement été migrée, pour que l'appelant
/// — et le test — n'aient pas à relire le disque pour le savoir.
pub fn appliquer_plan_migration_windows(plan: &PlanMigrationWindows) -> bool {
    match &plan.action {
        ActionMigrationWindows::HorsProgramFiles => return false,
        ActionMigrationWindows::LocalappdataAbsent => {
            warn!("windows_migrate_LOCALAPPDATA_not_set");
            return false;
        }
        _ => {}
    }
    let Some(data_dir) = plan.data_dir.as_ref() else {
        return false;
    };

    warn!(
        exe_dir = %plan.exe_dir.display(),
        data_dir = %data_dir.display(),
        "running_from_program_files — data will be stored in %LOCALAPPDATA%\\TuneServer"
    );

    if !data_dir.exists() {
        match std::fs::create_dir_all(data_dir) {
            Ok(_) => info!(path = %data_dir.display(), "windows_migrate_data_dir_created"),
            Err(e) => {
                warn!(path = %data_dir.display(), error = %e, "windows_migrate_data_dir_create_failed");
                return false;
            }
        }
    }

    match &plan.action {
        ActionMigrationWindows::Migrer { source, cible } => {
            info!(
                from = %source.display(),
                to = %cible.display(),
                "migrating database to %LOCALAPPDATA%\\TuneServer"
            );
            match tune_core::db_backup::copier_base_sqlite(source, cible) {
                Ok(octets) => {
                    info!(
                        octets,
                        from = %source.display(),
                        to = %cible.display(),
                        "windows_migrate_db_copied"
                    );
                    true
                }
                Err(e) => {
                    error!(
                        from = %source.display(),
                        to = %cible.display(),
                        error = %e,
                        "windows_migrate_db_copy_failed"
                    );
                    false
                }
            }
        }
        // La cible existe : on ne l'écrase pas, et — c'est le changement — on
        // ne se tait plus. L'ancienne base reste EN PLACE, intacte, et le
        // journal la nomme pour que l'utilisateur puisse la récupérer.
        ActionMigrationWindows::CibleDejaPresente { source, cible } => {
            warn!(
                retenue = %cible.display(),
                delaissee = %source.display(),
                "windows_migrate_base_deja_presente_l_ancienne_est_laissee_intacte"
            );
            false
        }
        _ => false,
    }
}

/// Le câblage, et lui seul : ce que la règle ne peut pas savoir.
#[cfg(target_os = "windows")]
pub fn check_and_migrate() {
    let exe_path = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "windows_migrate_cannot_resolve_exe_path");
            return;
        }
    };
    let Some(exe_dir) = exe_path.parent() else {
        return;
    };
    let localappdata = std::env::var("LOCALAPPDATA").ok();
    let plan = plan_migration_windows(exe_dir, localappdata.as_deref(), |chemin| chemin.exists());
    appliquer_plan_migration_windows(&plan);
}
