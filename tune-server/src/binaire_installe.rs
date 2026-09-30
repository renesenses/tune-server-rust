//! #5461 — Le binaire INSTALLÉ, qu'il ne faut pas confondre avec le binaire
//! qui TOURNE.
//!
//! Sous Linux, `std::env::current_exe()` lit `/proc/self/exe`, qui désigne le
//! FICHIER en cours d'exécution et suit donc ses renommages. Or la mise à jour
//! Unix renomme le binaire en cours en `<exe>.old` avant de poser le nouveau
//! sous le nom normal. Relu après l'installation, `current_exe()` rend
//! `/opt/tune-server.old` : la relance ré-exécutait l'ANCIEN binaire, et la
//! mise à jour suivante, calculant sa sauvegarde à partir de ce chemin,
//! supprimait le binaire en cours puis échouait en `ENOENT` (FabienM, fil 2037,
//! 0.9.167 → 0.9.168). Une fois le fichier supprimé, Linux suffixe encore le
//! chemin de ` (deleted)`.
//!
//! Ce module dit, à partir du chemin lu, quel est le chemin d'installation
//! (celui qu'on remplace et qu'on relance), et reconnaît au démarrage un
//! processus lancé depuis la sauvegarde. Les fonctions sont pures ou ne font
//! que lire le disque, pour être éprouvées sans processus ni `exec`.

use std::path::{Path, PathBuf};

/// Suffixe du binaire mis de côté par la mise à jour Unix.
pub const SUFFIXE_DE_SAUVEGARDE: &str = ".old";

/// Ce que Linux ajoute à `/proc/self/exe` quand le fichier a été supprimé.
const MARQUE_DE_SUPPRESSION: &str = " (deleted)";

/// Variable posée avant la relance de réparation, pour qu'un démarrage
/// n'en tente jamais deux de suite.
pub const VARIABLE_REPARATION_TENTEE: &str = "TUNE_RELANCE_DEPUIS_OLD_TENTEE";

/// Le chemin lu, sans la marque « (deleted) » de Linux.
pub fn sans_marque_de_suppression(exe_lu: &Path) -> PathBuf {
    match exe_lu.to_str() {
        Some(s) => PathBuf::from(s.strip_suffix(MARQUE_DE_SUPPRESSION).unwrap_or(s)),
        None => exe_lu.to_path_buf(),
    }
}

/// `X` → `X.old`. Le suffixe est AJOUTÉ au nom (et non substitué à une
/// extension) pour que [`nom_installe_de_la_sauvegarde`] en soit l'inverse
/// exact ; pour un nom sans point, c'est le même chemin qu'avant.
pub fn chemin_de_sauvegarde(binaire: &Path) -> PathBuf {
    let mut nom = binaire.as_os_str().to_os_string();
    nom.push(SUFFIXE_DE_SAUVEGARDE);
    PathBuf::from(nom)
}

/// `X.old` → `Some(X)` ; tout autre nom → `None`.
pub fn nom_installe_de_la_sauvegarde(exe: &Path) -> Option<PathBuf> {
    let nom = exe.file_name()?.to_str()?;
    let base = nom.strip_suffix(SUFFIXE_DE_SAUVEGARDE)?;
    if base.is_empty() {
        return None;
    }
    Some(exe.with_file_name(base))
}

/// Le chemin à REMPLACER par une mise à jour, déduit du chemin lu.
///
/// Un processus qui tourne depuis `X.old` installe sous `X`, jamais sous
/// `X.old` : c'est ce dernier cas qui supprimait le binaire en cours.
pub fn chemin_d_installation(exe_lu: &Path) -> PathBuf {
    let en_cours = sans_marque_de_suppression(exe_lu);
    nom_installe_de_la_sauvegarde(&en_cours).unwrap_or(en_cours)
}

/// Le chemin à relancer pour un simple redémarrage : `X` quand on tourne
/// depuis `X.old` et que `X` est là, sinon le binaire en cours.
pub fn chemin_de_relance(exe_lu: &Path) -> PathBuf {
    let en_cours = sans_marque_de_suppression(exe_lu);
    match nom_installe_de_la_sauvegarde(&en_cours) {
        Some(installe) if installe.is_file() => installe,
        _ => en_cours,
    }
}

/// Le chemin de relance du processus courant (voir [`chemin_de_relance`]).
pub fn chemin_de_relance_du_processus() -> std::io::Result<PathBuf> {
    std::env::current_exe().map(|exe| chemin_de_relance(&exe))
}

/// Ce que le démarrage constate sur le binaire qui tourne.
#[derive(Debug, PartialEq, Eq)]
pub enum Demarrage {
    /// Lancé sous son nom normal.
    Normal,
    /// Lancé depuis la sauvegarde `X.old` d'une mise à jour.
    DepuisLaSauvegarde {
        en_cours: PathBuf,
        installe: PathBuf,
        /// `Ok` : relancer `installe` est sûr. `Err` : pourquoi on ne répare pas.
        reparation: Result<(), String>,
    },
}

/// Identité d'un fichier (périphérique, inode), pour savoir si deux chemins
/// désignent le même binaire.
#[cfg(unix)]
pub fn identite(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

/// Diagnostic du démarrage.
///
/// `binaire_en_cours` : les métadonnées du fichier qui tourne (sous Linux,
/// celles de `/proc/self/exe`, lisibles même quand le fichier est supprimé).
/// `deja_tente` : une relance de réparation a déjà eu lieu pour ce processus.
///
/// La réparation n'est jugée sûre que si le binaire installé existe, est un
/// fichier exécutable, n'est PAS le fichier qui tourne (un lien vers `.old`
/// ferait boucler la relance) et n'est pas plus ancien que lui : la mise à
/// jour pose le neuf sous `X` APRÈS avoir renommé l'ancien en `X.old`.
#[cfg(unix)]
pub fn diagnostiquer(
    exe_lu: &Path,
    binaire_en_cours: Option<&std::fs::Metadata>,
    deja_tente: bool,
) -> Demarrage {
    let en_cours = sans_marque_de_suppression(exe_lu);
    let Some(installe) = nom_installe_de_la_sauvegarde(&en_cours) else {
        return Demarrage::Normal;
    };
    let reparation = verdict_de_reparation(&installe, binaire_en_cours, deja_tente);
    Demarrage::DepuisLaSauvegarde {
        en_cours,
        installe,
        reparation,
    }
}

#[cfg(unix)]
fn verdict_de_reparation(
    installe: &Path,
    binaire_en_cours: Option<&std::fs::Metadata>,
    deja_tente: bool,
) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if deja_tente {
        return Err("une relance de réparation a déjà été tentée".to_string());
    }
    let meta = std::fs::metadata(installe)
        .map_err(|e| format!("binaire installé illisible ({}) : {e}", installe.display()))?;
    if !meta.is_file() {
        return Err(format!("{} n'est pas un fichier", installe.display()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("{} n'est pas exécutable", installe.display()));
    }
    if let Some(courant) = binaire_en_cours {
        if identite(&meta) == identite(courant) {
            return Err(format!(
                "{} désigne le binaire qui tourne",
                installe.display()
            ));
        }
        if let (Ok(neuf), Ok(ancien)) = (meta.modified(), courant.modified())
            && neuf < ancien
        {
            return Err(format!(
                "{} est plus ancien que le binaire qui tourne",
                installe.display()
            ));
        }
    }
    Ok(())
}

/// Au démarrage : journalise un lancement depuis `X.old` et, si c'est sûr,
/// relance sur place (`exec`, même PID) le binaire installé `X`. Ne rend la
/// main que si rien n'est à faire ou si la relance n'a pas eu lieu.
///
/// À appeler tôt, journal posé, avant d'ouvrir le port et la base.
#[cfg(unix)]
pub fn reparer_un_lancement_depuis_la_sauvegarde() {
    let Ok(exe_lu) = std::env::current_exe() else {
        return;
    };
    // Sous Linux, `/proc/self/exe` se lit même quand le fichier est supprimé.
    let courant = std::fs::metadata("/proc/self/exe")
        .or_else(|_| std::fs::metadata(sans_marque_de_suppression(&exe_lu)))
        .ok();
    let deja_tente = std::env::var_os(VARIABLE_REPARATION_TENTEE).is_some();
    match diagnostiquer(&exe_lu, courant.as_ref(), deja_tente) {
        Demarrage::Normal => {}
        Demarrage::DepuisLaSauvegarde {
            en_cours: _,
            installe,
            reparation: Err(motif),
        } => {
            tracing::warn!(
                en_cours = %exe_lu.display(),
                installe = %installe.display(),
                motif = %motif,
                "binaire_lance_depuis_la_sauvegarde_non_repare — relancer Tune à la main \
                 depuis le binaire installé (#5461)"
            );
        }
        Demarrage::DepuisLaSauvegarde {
            en_cours,
            installe,
            reparation: Ok(()),
        } => {
            use std::os::unix::process::CommandExt;
            tracing::warn!(
                en_cours = %en_cours.display(),
                installe = %installe.display(),
                "binaire_lance_depuis_la_sauvegarde — relance dans le binaire installé (#5461)"
            );
            let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
            let err = std::process::Command::new(&installe)
                .args(&args)
                .env(VARIABLE_REPARATION_TENTEE, "1")
                .exec();
            tracing::warn!(
                error = %err,
                installe = %installe.display(),
                "binaire_lance_depuis_la_sauvegarde_relance_impossible — on continue sur la sauvegarde"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn poser(chemin: &Path, contenu: &str, mode: u32) {
        std::fs::write(chemin, contenu).unwrap();
        std::fs::set_permissions(chemin, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn la_marque_de_suppression_de_linux_est_retiree() {
        assert_eq!(
            sans_marque_de_suppression(Path::new("/opt/tune-server.old (deleted)")),
            PathBuf::from("/opt/tune-server.old")
        );
        assert_eq!(
            sans_marque_de_suppression(Path::new("/opt/tune-server")),
            PathBuf::from("/opt/tune-server")
        );
    }

    #[test]
    fn sauvegarde_et_nom_installe_sont_inverses() {
        for nom in ["/opt/tune-server", "/opt/tune-server-rust/tune-server-bin"] {
            let binaire = PathBuf::from(nom);
            let sauvegarde = chemin_de_sauvegarde(&binaire);
            assert_eq!(sauvegarde, PathBuf::from(format!("{nom}.old")));
            assert_eq!(nom_installe_de_la_sauvegarde(&sauvegarde), Some(binaire));
        }
        assert_eq!(
            nom_installe_de_la_sauvegarde(Path::new("/opt/tune-server")),
            None
        );
        assert_eq!(nom_installe_de_la_sauvegarde(Path::new("/opt/.old")), None);
    }

    /// Le cas de FabienM : le processus tourne depuis `/opt/tune-server.old`,
    /// supprimé ou non. L'installation vise `/opt/tune-server`, jamais `.old`.
    #[test]
    fn un_processus_lance_depuis_old_installe_sous_le_nom_normal() {
        for lu in ["/opt/tune-server.old", "/opt/tune-server.old (deleted)"] {
            assert_eq!(
                chemin_d_installation(Path::new(lu)),
                PathBuf::from("/opt/tune-server"),
                "{lu}"
            );
        }
        assert_eq!(
            chemin_d_installation(Path::new("/opt/tune-server")),
            PathBuf::from("/opt/tune-server")
        );
    }

    #[test]
    fn le_redemarrage_vise_le_binaire_installe_s_il_existe() {
        let dir = tempfile::tempdir().unwrap();
        let installe = dir.path().join("tune-server");
        let sauvegarde = dir.path().join("tune-server.old");
        poser(&sauvegarde, "v1", 0o755);
        // Sans binaire installé : on relance ce qui tourne, pour ne pas mourir.
        assert_eq!(chemin_de_relance(&sauvegarde), sauvegarde);
        poser(&installe, "v2", 0o755);
        assert_eq!(chemin_de_relance(&sauvegarde), installe);
        assert_eq!(chemin_de_relance(&installe), installe);
    }

    #[test]
    fn demarrage_depuis_old_reparable_quand_le_neuf_est_en_place() {
        let dir = tempfile::tempdir().unwrap();
        let installe = dir.path().join("tune-server");
        let sauvegarde = dir.path().join("tune-server.old");
        poser(&sauvegarde, "v1", 0o755);
        poser(&installe, "v2", 0o755);
        let courant = std::fs::metadata(&sauvegarde).unwrap();
        assert_eq!(
            diagnostiquer(&sauvegarde, Some(&courant), false),
            Demarrage::DepuisLaSauvegarde {
                en_cours: sauvegarde.clone(),
                installe: installe.clone(),
                reparation: Ok(()),
            }
        );
        // Et le démarrage normal ne déclenche rien.
        let normal = std::fs::metadata(&installe).unwrap();
        assert_eq!(
            diagnostiquer(&installe, Some(&normal), false),
            Demarrage::Normal
        );
    }

    #[test]
    fn demarrage_depuis_old_non_repare_quand_ce_n_est_pas_sur() {
        let dir = tempfile::tempdir().unwrap();
        let installe = dir.path().join("tune-server");
        let sauvegarde = dir.path().join("tune-server.old");
        poser(&sauvegarde, "v1", 0o755);
        let courant = std::fs::metadata(&sauvegarde).unwrap();
        let refus = |d: Demarrage| match d {
            Demarrage::DepuisLaSauvegarde {
                reparation: Err(m), ..
            } => m,
            autre => panic!("réparation attendue refusée, rendu : {autre:?}"),
        };

        // Binaire installé absent.
        let m = refus(diagnostiquer(&sauvegarde, Some(&courant), false));
        assert!(m.contains("illisible"), "{m}");

        // Présent mais non exécutable.
        poser(&installe, "v2", 0o644);
        let m = refus(diagnostiquer(&sauvegarde, Some(&courant), false));
        assert!(m.contains("exécutable"), "{m}");

        // Relance déjà tentée : jamais deux de suite.
        poser(&installe, "v2", 0o755);
        let m = refus(diagnostiquer(&sauvegarde, Some(&courant), true));
        assert!(m.contains("déjà"), "{m}");

        // Le nom normal est un lien vers la sauvegarde : relancer bouclerait.
        std::fs::remove_file(&installe).unwrap();
        std::os::unix::fs::symlink(&sauvegarde, &installe).unwrap();
        let m = refus(diagnostiquer(&sauvegarde, Some(&courant), false));
        assert!(m.contains("qui tourne"), "{m}");
    }
}
