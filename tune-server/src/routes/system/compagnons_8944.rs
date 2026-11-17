//! #8944 — la mise à jour intégrée remplace aussi les exécutables livrés à
//! côté de `tune-server`.
//!
//! Les archives de release posent, à côté du serveur, les démons et outils
//! qu'il lance : `airplay-daemon` (AirPlay 2), `librespot` (Spotify),
//! `ffmpeg` et `fpcalc` (`release.yml`, étapes « Package »). Le serveur les
//! cherche d'abord dans le dossier de son binaire. Jusqu'ici `install_unix`
//! ne basculait que `tune-server` et `web/` : une installation mise à jour
//! par le bouton gardait le démon AirPlay 2 de sa première installation, et
//! aucun correctif du démon (appairage #8347/#8375, SETUP #8420, volume
//! #8402/#8855) n'arrivait (fil 2878, rc12).
//!
//! La liste est FIXE ([`EXECUTABLES_LIVRES`]) : aucun fichier de l'archive
//! n'est installé parce qu'il s'y trouve, seuls ces noms le sont. Un nom
//! absent de l'archive (cible sans démon) laisse l'installé tel quel.
//!
//! Même discipline que pour le binaire (#7756) : chaque exécutable neuf est
//! écrit durablement en `<nom>.new` à côté de sa cible et validé (taille,
//! ELF de la bonne architecture sous Linux) AVANT toute bascule ; la bascule
//! met l'ancien en `<nom>.old` puis renomme le neuf (atomique, même dossier) ;
//! un échec remet les anciens. Un démon en cours garde son fichier ouvert
//! (inode) jusqu'à sa sortie : le renommer sous lui est sans effet sur lui.
//!
//! Arrêt et relance des démons : à la relance (`exec`), chaque
//! `airplay-daemon` voit son entrée standard se fermer (tube `CLOEXEC`) et
//! sort ; la relance de la mise à jour arrête `librespot` avant l'`exec`. Le
//! serveur relancé les démarre à nouveau, depuis les fichiers neufs.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::installation_sure::{self, Controles, a_cote};

/// Les exécutables livrés à côté de `tune-server` dans les archives de
/// release, sans suffixe de plateforme. Ajouter un exécutable à l'archive
/// (`release.yml`) sans l'ajouter ici le laisse figé chez les utilisateurs
/// mis à jour par le bouton.
pub(super) const EXECUTABLES_LIVRES: [&str; 4] =
    ["airplay-daemon", "librespot", "ffmpeg", "fpcalc"];

/// Un exécutable livré à installer : sa source dans l'archive extraite, sa
/// cible dans le dossier du binaire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ExecutableLivre {
    pub source: PathBuf,
    pub cible: PathBuf,
}

/// Les exécutables de [`EXECUTABLES_LIVRES`] présents dans l'archive extraite
/// `archive` (fichiers ordinaires, jamais un lien), avec leur cible dans
/// `exe_dir`. `suffixe` : `""` sous Unix, `".exe"` pour Windows.
///
/// Une cible qui est un lien symbolique (outil remplacé à la main par celui
/// du système) est laissée telle quelle.
pub(super) fn a_installer(archive: &Path, exe_dir: &Path, suffixe: &str) -> Vec<ExecutableLivre> {
    EXECUTABLES_LIVRES
        .iter()
        .filter_map(|nom| {
            let fichier = format!("{nom}{suffixe}");
            let source = archive.join(&fichier);
            let cible = exe_dir.join(&fichier);
            let ordinaire =
                std::fs::symlink_metadata(&source).is_ok_and(|m| m.file_type().is_file());
            if !ordinaire {
                return None;
            }
            if std::fs::symlink_metadata(&cible).is_ok_and(|m| m.file_type().is_symlink()) {
                warn!(cible = %cible.display(), "update_executable_livre_lien_garde");
                return None;
            }
            Some(ExecutableLivre { source, cible })
        })
        .collect()
}

/// Taille cumulée des sources (pour la vérification de place).
pub(super) fn taille(livres: &[ExecutableLivre]) -> u64 {
    livres
        .iter()
        .map(|l| std::fs::metadata(&l.source).map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Écrit chaque `<cible>.new` durablement (0755) et le valide contre sa
/// source, sans toucher aux cibles. Au premier échec, tout ce qui a été
/// posé est retiré. Pas d'essai d'exécution : ces outils n'ont pas tous un
/// `--version` sans effet.
pub(super) fn preparer(livres: &[ExecutableLivre], controles: &Controles) -> Result<(), String> {
    for (i, l) in livres.iter().enumerate() {
        let pose = a_cote(&l.cible, "new");
        let resultat = (|| {
            if std::fs::symlink_metadata(&pose).is_ok() {
                std::fs::remove_file(&pose)
                    .map_err(|e| format!("clear staged {}: {e}", pose.display()))?;
            }
            let attendue = std::fs::metadata(&l.source)
                .map_err(|e| format!("read {}: {e}", l.source.display()))?
                .len();
            installation_sure::ecrire_durablement(&l.source, &pose)?;
            // Taille ET architecture contrôlées contre la source extraite.
            let controles_du_livre = Controles {
                taille_attendue: Some(attendue),
                essai: |_| Ok(()),
                ..*controles
            };
            installation_sure::valider_binaire(&pose, &controles_du_livre)
                .map_err(|e| format!("{}: {e}", nom(&l.cible)))
        })();
        if let Err(e) = resultat {
            std::fs::remove_file(&pose).ok();
            abandonner(&livres[..i]);
            return Err(e);
        }
    }
    Ok(())
}

/// Retire les `<cible>.new` posés et non basculés.
pub(super) fn abandonner(livres: &[ExecutableLivre]) {
    for l in livres {
        std::fs::remove_file(a_cote(&l.cible, "new")).ok();
    }
}

/// Un exécutable livré basculé : de quoi le remettre.
#[derive(Debug)]
pub(super) struct LivreBascule {
    pub cible: PathBuf,
    /// L'ancien a été mis de côté en `<cible>.old`.
    pub ancien_garde: bool,
}

/// Bascule un exécutable préparé : l'ancien part en `<cible>.old` (le
/// précédent `.old` est retiré), le `.new` prend son nom. Un échec laisse la
/// cible comme avant.
pub(super) fn basculer(
    l: &ExecutableLivre,
    renommer: fn(&Path, &Path) -> io::Result<()>,
) -> Result<LivreBascule, String> {
    let pose = a_cote(&l.cible, "new");
    let ancien = a_cote(&l.cible, "old");
    if std::fs::symlink_metadata(&ancien).is_ok() {
        std::fs::remove_file(&ancien)
            .map_err(|e| format!("clear backup {}: {e}", ancien.display()))?;
    }
    let ancien_garde = std::fs::symlink_metadata(&l.cible).is_ok();
    if ancien_garde {
        renommer(&l.cible, &ancien).map_err(|e| format!("park old {}: {e}", l.cible.display()))?;
    }
    if let Err(e) = renommer(&pose, &l.cible) {
        if ancien_garde {
            std::fs::rename(&ancien, &l.cible).ok();
        }
        return Err(format!("swap new {} into place: {e}", l.cible.display()));
    }
    info!(cible = %l.cible.display(), "update_executable_livre_remplace");
    Ok(LivreBascule {
        cible: l.cible.clone(),
        ancien_garde,
    })
}

/// Remet l'ancien exécutable d'une cible basculée (le neuf est retiré ; s'il
/// n'y avait pas d'ancien, le neuf est simplement retiré).
pub(super) fn remettre(b: &LivreBascule) -> Result<(), String> {
    if b.ancien_garde {
        // `rename` remplace la cible d'un coup : jamais de nom vide.
        std::fs::rename(a_cote(&b.cible, "old"), &b.cible).map_err(|e| format!("{e}"))
    } else {
        std::fs::remove_file(&b.cible).map_err(|e| format!("{e}"))
    }
}

/// Remet tous les exécutables basculés, du dernier au premier. Rend la liste
/// de ceux qui n'ont pas pu l'être.
pub(super) fn remettre_tous(bascules: &[LivreBascule]) -> Vec<String> {
    let mut non_remis = Vec::new();
    for b in bascules.iter().rev() {
        if let Err(e) = remettre(b) {
            tracing::error!(cible = %b.cible.display(), error = %e, "update_executable_livre_rollback_failed");
            non_remis.push(format!("{} NOT restored ({e})", b.cible.display()));
        }
    }
    non_remis
}

fn nom(chemin: &Path) -> String {
    chemin
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "compagnons_8944_tests.rs"]
mod tests;
