//! Où le démon AirPlay 2 range les identités d'appairage (#8347).
//!
//! Depuis renesenses/airplay2-rs#4, le démon relit l'identité qu'un
//! pair-setup a sauvegardée (`.airplay_sender_identity_<appareil>.json`) et
//! fait un pair-verify au lieu de refaire un pair-setup à chaque connexion.
//! Le fichier allait dans le dossier courant du démon, c'est-à-dire celui du
//! serveur : `/` ou un dossier en lecture seule sous systemd, le dossier
//! d'installation sous Windows. Tune lui donne désormais un dossier à lui
//! dans ses données, par la variable `AIRPLAY2_IDENTITY_DIR`.

use std::path::{Path, PathBuf};

/// Variable lue par le démon (`airplay_client::connection::IDENTITY_DIR_ENV`).
pub(super) const VARIABLE: &str = "AIRPLAY2_IDENTITY_DIR";

/// Le dossier des identités : `<TUNE_DATA_DIR>/airplay2`, sinon
/// `<dossier de TUNE_DB_PATH>/airplay2` ; un chemin relatif se lit sous
/// `cwd`. `None` : rien de connu, le démon garde son dossier courant.
pub(super) fn dossier_des_identites(
    tune_data_dir: Option<&str>,
    db_path: Option<&str>,
    cwd: &Path,
) -> Option<PathBuf> {
    let absolu = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };
    if let Some(d) = tune_data_dir.map(str::trim).filter(|d| !d.is_empty()) {
        return Some(absolu(Path::new(d)).join("airplay2"));
    }
    let base = db_path.map(str::trim).filter(|d| !d.is_empty())?;
    let parent = Path::new(base)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())?;
    Some(absolu(parent).join("airplay2"))
}

/// Le dossier à donner au démon lancé maintenant, sauf si l'environnement
/// en impose déjà un.
pub(super) fn pour_ce_serveur() -> Option<PathBuf> {
    if std::env::var_os(VARIABLE).is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    dossier_des_identites(
        std::env::var("TUNE_DATA_DIR").ok().as_deref(),
        std::env::var("TUNE_DB_PATH").ok().as_deref(),
        &cwd,
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::dossier_des_identites;

    #[test]
    fn les_donnees_de_tune_d_abord() {
        let cwd = Path::new("/srv/tune");
        assert_eq!(
            dossier_des_identites(Some("/var/lib/tune"), Some("/data/tune.db"), cwd),
            Some(PathBuf::from("/var/lib/tune/airplay2"))
        );
        assert_eq!(
            dossier_des_identites(Some("data"), None, cwd),
            Some(PathBuf::from("/srv/tune/data/airplay2"))
        );
    }

    #[test]
    fn sinon_a_cote_de_la_base() {
        let cwd = Path::new("/srv/tune");
        // Dockerfile.dist : TUNE_DB_PATH=/data/tune.db, TUNE_DATA_DIR absent.
        assert_eq!(
            dossier_des_identites(None, Some("/data/tune.db"), cwd),
            Some(PathBuf::from("/data/airplay2"))
        );
        assert_eq!(
            dossier_des_identites(Some("  "), Some("db/tune.db"), cwd),
            Some(PathBuf::from("/srv/tune/db/airplay2"))
        );
    }

    #[test]
    fn rien_de_connu_le_demon_garde_son_dossier() {
        let cwd = Path::new("/srv/tune");
        assert_eq!(dossier_des_identites(None, None, cwd), None);
        assert_eq!(dossier_des_identites(None, Some("tune.db"), cwd), None);
        assert_eq!(dossier_des_identites(Some(""), Some(""), cwd), None);
    }
}
