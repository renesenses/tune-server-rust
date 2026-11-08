//! Sauvegarde locale d'une base PostgreSQL (#7288).
//!
//! En mode PostgreSQL, le fichier SQLite de `db_path` n'est pas la base du
//! serveur : le copier rendrait une archive des mauvaises données. La
//! sauvegarde passe donc par `pg_dump`, au format custom (compressé, relu par
//! `pg_restore`), dans le même dossier `backups` que les sauvegardes SQLite.
//!
//! Les fichiers s'appellent `tune_pg_<horodatage>.dump` : ils se listent et
//! s'élaguent par le même motif `<stem>_*.<ext>` que les sauvegardes SQLite
//! (`tune_<horodatage>.db`), sans jamais toucher à celles-ci.

use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::BackupInfo;

/// Préfixe des fichiers de sauvegarde PostgreSQL, sans le `_` de l'horodatage.
pub const STEM_SAUVEGARDE_PG: &str = "tune_pg";
/// Extension des fichiers de sauvegarde PostgreSQL (format custom de pg_dump).
pub const EXTENSION_SAUVEGARDE_PG: &str = "dump";
/// Variable d'environnement qui donne le chemin de `pg_dump` quand il n'est
/// pas dans le `PATH` du service.
pub const VARIABLE_PG_DUMP: &str = "TUNE_PG_DUMP";
/// Variable d'environnement qui donne, en secondes, le délai au-delà duquel
/// un `pg_dump` est arrêté.
pub const VARIABLE_DELAI_PG_DUMP: &str = "TUNE_PG_DUMP_TIMEOUT_SECS";
/// Délai par défaut d'un `pg_dump` : une heure.
pub const DELAI_PG_DUMP_PAR_DEFAUT: Duration = Duration::from_secs(3600);

/// Une seule sauvegarde PostgreSQL à la fois : deux clics rapprochés ne
/// lancent pas deux `pg_dump` vers le même fichier.
static SAUVEGARDE_EN_COURS: Mutex<()> = Mutex::new(());

/// Pourquoi une sauvegarde PostgreSQL n'a pas abouti.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurSauvegardePg {
    /// `pg_dump` n'a pas pu être lancé : absent du système ou du `PATH`.
    PgDumpIntrouvable { programme: String },
    /// `pg_dump` existe mais le service n'a pas le droit de l'exécuter.
    PgDumpNonExecutable { programme: String },
    /// `pg_dump` n'a pas fini dans le délai : il a été arrêté.
    PgDumpDelaiDepasse { secondes: u64 },
    /// `pg_dump` a démarré puis échoué ; `detail` est sa sortie d'erreur,
    /// sans le mot de passe de la connexion.
    PgDumpEnEchec { detail: String },
    /// Le dossier ou le fichier de sauvegarde n'a pas pu être écrit.
    Ecriture { detail: String },
}

impl ErreurSauvegardePg {
    /// Code court, stable, pour le champ `error` de la réponse HTTP.
    pub fn code(&self) -> &'static str {
        match self {
            Self::PgDumpIntrouvable { .. } => "pg_dump_not_found",
            Self::PgDumpNonExecutable { .. } => "pg_dump_not_executable",
            Self::PgDumpDelaiDepasse { .. } => "pg_dump_timeout",
            Self::PgDumpEnEchec { .. } => "pg_dump_failed",
            Self::Ecriture { .. } => "backup_write_failed",
        }
    }
}

impl std::fmt::Display for ErreurSauvegardePg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PgDumpIntrouvable { programme } => write!(
                f,
                "PostgreSQL backups need pg_dump, which could not be started ({programme}). \
                 Install the PostgreSQL client tools (same major version as the server, \
                 e.g. package postgresql-client) or set {VARIABLE_PG_DUMP} to the full \
                 path of pg_dump, then try again."
            ),
            Self::PgDumpNonExecutable { programme } => write!(
                f,
                "pg_dump was found but cannot be executed by the Tune service ({programme}): \
                 permission denied. Make it executable for the service user, or set \
                 {VARIABLE_PG_DUMP} to a pg_dump it may run, then try again."
            ),
            Self::PgDumpDelaiDepasse { secondes } => write!(
                f,
                "pg_dump did not finish within {secondes} s and was stopped; no backup was \
                 kept. Check that the database is reachable and not locked, or raise \
                 {VARIABLE_DELAI_PG_DUMP}, then try again."
            ),
            Self::PgDumpEnEchec { detail } => write!(f, "pg_dump failed: {detail}"),
            Self::Ecriture { detail } => write!(f, "could not write the backup file: {detail}"),
        }
    }
}

/// Le programme `pg_dump` à lancer : `TUNE_PG_DUMP` s'il est posé, sinon
/// `pg_dump` cherché dans le `PATH`.
pub fn programme_pg_dump() -> PathBuf {
    std::env::var_os(VARIABLE_PG_DUMP)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("pg_dump"))
}

/// Le délai d'un `pg_dump` : `TUNE_PG_DUMP_TIMEOUT_SECS` s'il est posé et
/// positif, sinon [`DELAI_PG_DUMP_PAR_DEFAUT`].
pub fn delai_pg_dump() -> Duration {
    std::env::var(VARIABLE_DELAI_PG_DUMP)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(DELAI_PG_DUMP_PAR_DEFAUT)
}

/// Les sauvegardes PostgreSQL de `backup_dir`, de la plus récente à la plus
/// ancienne.
pub fn list_pg_backups(backup_dir: &Path) -> Vec<BackupInfo> {
    super::list_backups_in(backup_dir, STEM_SAUVEGARDE_PG, EXTENSION_SAUVEGARDE_PG)
}

/// Sépare le mot de passe d'une URL `postgres://user:motdepasse@hôte/base`.
///
/// Rend l'URL sans le mot de passe et le mot de passe décodé. Le mot de passe
/// passe ensuite à `pg_dump` par `PGPASSWORD` : sur la ligne de commande, il
/// serait lisible par tout utilisateur de la machine (`ps`, `/proc`). Un
/// mot de passe donné en paramètre (`?password=…`) est retiré de même.
fn separer_mot_de_passe(url: &str) -> (String, Option<String>) {
    let (url, du_parametre) = retirer_parametre_mot_de_passe(url);
    let (sans, de_l_autorite) = separer_mot_de_passe_de_l_autorite(&url);
    (sans, de_l_autorite.or(du_parametre))
}

/// Retire le paramètre `password=` de la partie requête d'une URL.
fn retirer_parametre_mot_de_passe(url: &str) -> (String, Option<String>) {
    let Some(debut) = url.find('?') else {
        return (url.to_string(), None);
    };
    let mut mot_de_passe = None;
    let gardes: Vec<&str> = url[debut + 1..]
        .split('&')
        .filter(|p| match p.strip_prefix("password=") {
            Some(brut) => {
                mot_de_passe = Some(
                    urlencoding::decode(brut)
                        .map(|s| s.into_owned())
                        .unwrap_or_else(|_| brut.to_string()),
                );
                false
            }
            None => true,
        })
        .collect();
    if mot_de_passe.is_none() {
        return (url.to_string(), None);
    }
    let mut sans = url[..debut].to_string();
    if !gardes.is_empty() {
        sans.push('?');
        sans.push_str(&gardes.join("&"));
    }
    (sans, mot_de_passe)
}

fn separer_mot_de_passe_de_l_autorite(url: &str) -> (String, Option<String>) {
    let Some(debut_hote) = url.find("://").map(|i| i + 3) else {
        return (url.to_string(), None);
    };
    let fin_autorite = url[debut_hote..]
        .find(['/', '?'])
        .map(|i| debut_hote + i)
        .unwrap_or(url.len());
    let autorite = &url[debut_hote..fin_autorite];
    let Some(arobase) = autorite.rfind('@') else {
        return (url.to_string(), None);
    };
    let identite = &autorite[..arobase];
    let Some(deux_points) = identite.find(':') else {
        return (url.to_string(), None);
    };
    let utilisateur = &identite[..deux_points];
    let brut = &identite[deux_points + 1..];
    let mot_de_passe = urlencoding::decode(brut)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| brut.to_string());
    let sans = format!(
        "{}{}{}",
        &url[..debut_hote],
        utilisateur,
        &url[debut_hote + arobase..]
    );
    (sans, Some(mot_de_passe))
}

/// Ce que `pg_dump` a rendu : son code de sortie et sa sortie d'erreur.
struct SortiePgDump {
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
}

/// Lance `commande` et attend sa fin au plus `delai`. Rend `Ok(None)` si le
/// délai est dépassé : le processus est alors tué. La sortie d'erreur est lue
/// à part, pour qu'un `pg_dump` bavard ne bloque pas sur un tube plein.
fn executer_avec_delai(
    commande: &mut Command,
    delai: Duration,
) -> std::io::Result<Option<SortiePgDump>> {
    let mut enfant = commande.spawn()?;
    let lecteur = enfant.stderr.take().map(|mut flux| {
        std::thread::spawn(move || {
            let mut octets = Vec::new();
            let _ = flux.read_to_end(&mut octets);
            octets
        })
    });
    let limite = Instant::now() + delai;
    let status = loop {
        if let Some(status) = enfant.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= limite {
            let _ = enfant.kill();
            let _ = enfant.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stderr = lecteur.and_then(|l| l.join().ok()).unwrap_or_default();
    Ok(status.map(|status| SortiePgDump { status, stderr }))
}

/// Sauvegarde la base PostgreSQL `database_url` dans `backup_dir` avec
/// `pg_dump`, et rend la fiche du fichier créé.
///
/// Le dump s'écrit d'abord dans un fichier `.partial`, renommé une fois
/// `pg_dump` terminé avec succès : une sauvegarde interrompue ne figure
/// jamais dans la liste. Au-delà de [`super::MAX_BACKUPS`], les plus anciennes
/// sauvegardes PostgreSQL sont supprimées, comme pour SQLite.
pub fn create_pg_backup(
    database_url: &str,
    backup_dir: &Path,
    pg_dump: &Path,
) -> Result<BackupInfo, ErreurSauvegardePg> {
    create_pg_backup_avec_delai(database_url, backup_dir, pg_dump, delai_pg_dump())
}

/// Le nom du prochain dump : `tune_pg_<horodatage>.dump`, suivi de `_2`,
/// `_3`… si une sauvegarde de la même seconde existe déjà. Les suffixes se
/// trient après le nom sans suffixe, l'élagage reste dans l'ordre.
fn nom_libre(backup_dir: &Path) -> String {
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let base = format!("{STEM_SAUVEGARDE_PG}_{timestamp}");
    let mut nom = format!("{base}.{EXTENSION_SAUVEGARDE_PG}");
    let mut rang = 2;
    while backup_dir.join(&nom).exists() || backup_dir.join(format!("{nom}.partial")).exists() {
        nom = format!("{base}_{rang}.{EXTENSION_SAUVEGARDE_PG}");
        rang += 1;
    }
    nom
}

/// [`create_pg_backup`] avec un délai donné, au-delà duquel `pg_dump` est
/// arrêté et le fichier partiel retiré.
pub fn create_pg_backup_avec_delai(
    database_url: &str,
    backup_dir: &Path,
    pg_dump: &Path,
    delai: Duration,
) -> Result<BackupInfo, ErreurSauvegardePg> {
    let _une_a_la_fois = SAUVEGARDE_EN_COURS
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    fs::create_dir_all(backup_dir).map_err(|e| ErreurSauvegardePg::Ecriture {
        detail: format!("{}: {e}", backup_dir.display()),
    })?;

    let nom = nom_libre(backup_dir);
    let cible = backup_dir.join(&nom);
    let partiel = backup_dir.join(format!("{nom}.partial"));

    let (url_sans_secret, mot_de_passe) = separer_mot_de_passe(database_url);
    let mut commande = Command::new(pg_dump);
    commande
        .arg("--format=custom")
        .arg("--no-owner")
        .arg("--no-privileges")
        .arg("--file")
        .arg(&partiel)
        .arg("--dbname")
        .arg(&url_sans_secret)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(mdp) = &mot_de_passe {
        commande.env("PGPASSWORD", mdp);
    }

    let debut = Instant::now();
    let sortie = match executer_avec_delai(&mut commande, delai) {
        Ok(Some(s)) => s,
        Ok(None) => {
            let _ = fs::remove_file(&partiel);
            warn!(secondes = delai.as_secs(), "pg_backup_timeout");
            return Err(ErreurSauvegardePg::PgDumpDelaiDepasse {
                secondes: delai.as_secs(),
            });
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Err(ErreurSauvegardePg::PgDumpIntrouvable {
                programme: pg_dump.display().to_string(),
            });
        }
        Err(e) if e.kind() == ErrorKind::PermissionDenied => {
            return Err(ErreurSauvegardePg::PgDumpNonExecutable {
                programme: pg_dump.display().to_string(),
            });
        }
        Err(e) => {
            let _ = fs::remove_file(&partiel);
            return Err(ErreurSauvegardePg::PgDumpEnEchec {
                detail: e.to_string(),
            });
        }
    };
    info!(
        duree_ms = debut.elapsed().as_millis() as u64,
        "pg_dump_finished"
    );

    if !sortie.status.success() {
        let _ = fs::remove_file(&partiel);
        let mut detail = String::from_utf8_lossy(&sortie.stderr).trim().to_string();
        if let Some(mdp) = mot_de_passe.as_deref().filter(|m| !m.is_empty()) {
            detail = detail.replace(mdp, "***");
        }
        if detail.is_empty() {
            detail = format!("exit status {}", sortie.status);
        }
        warn!(detail = %detail, "pg_backup_failed");
        return Err(ErreurSauvegardePg::PgDumpEnEchec { detail });
    }

    if let Err(e) = fs::rename(&partiel, &cible) {
        let _ = fs::remove_file(&partiel);
        return Err(ErreurSauvegardePg::Ecriture {
            detail: format!("{}: {e}", cible.display()),
        });
    }

    info!(path = %cible.display(), "pg_backup_created");
    super::prune_backups(backup_dir, STEM_SAUVEGARDE_PG, EXTENSION_SAUVEGARDE_PG);

    let meta = fs::metadata(&cible).map_err(|e| ErreurSauvegardePg::Ecriture {
        detail: format!("{}: {e}", cible.display()),
    })?;
    let created_at = meta
        .modified()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();
    Ok(BackupInfo {
        filename: nom,
        size: meta.len(),
        created_at,
    })
}

#[cfg(test)]
#[path = "db_backup_postgres_tests.rs"]
mod tests;
