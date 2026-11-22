//! #8955 — la mise à jour intégrée remplace aussi les greffons livrés.
//!
//! Les archives de release posent, à côté de `tune-server`, un dossier
//! `plugins/` (greffons WASM livrés : `party`, `playlists-converter`), que le
//! serveur lit en premier (`plugins_host::resolve_plugins_dir`). Jusqu'ici la
//! mise à jour par le bouton basculait le binaire, `web/` et les exécutables
//! livrés (#8944), mais pas `plugins/` : les greffons livrés restaient à la
//! version de la première installation.
//!
//! Seuls les greffons PRÉSENTS dans l'archive sont touchés, un par un
//! (`plugins/<nom>/`, avec un `manifest.json`) : un greffon que
//! l'utilisateur a installé lui-même dans `plugins/` n'est ni retiré ni
//! modifié. Un `plugins/<nom>` qui est un lien symbolique est laissé.
//!
//! Même discipline que le binaire et `web/` (#7756) : chaque greffon neuf est
//! copié durablement et vérifié (taille de chaque fichier) AVANT toute
//! bascule ; la bascule met l'ancien de côté puis renomme le neuf ; un échec
//! remet les anciens.
//!
//! La copie neuve et la sauvegarde vivent HORS de `plugins/`
//! (`plugins.new/<nom>`, `plugins.old/<nom>`) : `plugins/` est parcouru au
//! démarrage, et un `plugins/party.old` porterait le même identifiant que
//! `plugins/party` — l'ancien pourrait être chargé à la place du neuf.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::installation_sure;

/// Le dossier des greffons livrés, à côté du binaire et dans l'archive.
pub(super) const DOSSIER: &str = "plugins";
/// Où les copies neuves attendent leur bascule (hors de `plugins/`).
pub(super) const ATTENTE: &str = "plugins.new";
/// Où les anciens sont gardés après la bascule (hors de `plugins/`).
pub(super) const SAUVEGARDE: &str = "plugins.old";

/// Un greffon livré à installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GreffonLivre {
    /// `plugins/<nom>` dans l'archive extraite.
    pub source: PathBuf,
    /// `plugins/<nom>` à côté du binaire.
    pub cible: PathBuf,
    /// `plugins.new/<nom>` : la copie neuve, avant bascule.
    pub pose: PathBuf,
    /// `plugins.old/<nom>` : l'ancien, après bascule.
    pub ancien: PathBuf,
}

/// Les greffons livrés de l'archive extraite `archive`, avec leurs chemins
/// dans `exe_dir`. Un dossier sans `manifest.json`, un lien, ou une cible
/// qui est un lien symbolique sont laissés.
pub(super) fn a_installer(archive: &Path, exe_dir: &Path) -> Vec<GreffonLivre> {
    let Ok(entrees) = std::fs::read_dir(archive.join(DOSSIER)) else {
        return Vec::new();
    };
    let mut greffons: Vec<GreffonLivre> = entrees
        .flatten()
        .filter_map(|entree| {
            let source = entree.path();
            let dossier = std::fs::symlink_metadata(&source).is_ok_and(|m| m.is_dir());
            if !dossier || !source.join("manifest.json").is_file() {
                return None;
            }
            let nom = entree.file_name();
            let cible = exe_dir.join(DOSSIER).join(&nom);
            if std::fs::symlink_metadata(&cible).is_ok_and(|m| m.file_type().is_symlink()) {
                warn!(cible = %cible.display(), "update_greffon_livre_lien_garde");
                return None;
            }
            Some(GreffonLivre {
                source,
                cible,
                pose: exe_dir.join(ATTENTE).join(&nom),
                ancien: exe_dir.join(SAUVEGARDE).join(&nom),
            })
        })
        .collect();
    greffons.sort_by(|a, b| a.cible.cmp(&b.cible));
    greffons
}

/// Taille cumulée des greffons à poser (pour la vérification de place).
pub(super) fn taille(greffons: &[GreffonLivre]) -> u64 {
    greffons
        .iter()
        .map(|g| installation_sure::taille_du_dossier(&g.source))
        .sum()
}

/// Chaque fichier de `source` est dans `copie`, à la même taille.
fn verifier_la_copie(source: &Path, copie: &Path) -> Result<(), String> {
    let entrees = std::fs::read_dir(source)
        .map_err(|e| format!("invalid plugin: cannot read {}: {e}", source.display()))?;
    for entree in entrees {
        let entree = entree.map_err(|e| format!("invalid plugin: {e}"))?;
        let vers = copie.join(entree.file_name());
        if entree.path().is_dir() {
            verifier_la_copie(&entree.path(), &vers)?;
            continue;
        }
        let attendue = entree.metadata().map(|m| m.len()).unwrap_or(0);
        let ecrite = std::fs::metadata(&vers)
            .map_err(|_| format!("invalid plugin: {} missing", vers.display()))?
            .len();
        if ecrite != attendue {
            return Err(format!(
                "invalid plugin: {}: {ecrite} bytes written, {attendue} expected",
                vers.display()
            ));
        }
    }
    Ok(())
}

/// Pose et vérifie chaque `plugins.new/<nom>`, sans toucher à `plugins/`. Au
/// premier échec, tout ce qui a été posé est retiré.
pub(super) fn preparer(
    greffons: &[GreffonLivre],
    copier: fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), String> {
    for (i, g) in greffons.iter().enumerate() {
        let resultat = (|| {
            if let Some(parent) = g.pose.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            if std::fs::symlink_metadata(&g.pose).is_ok() {
                std::fs::remove_dir_all(&g.pose)
                    .map_err(|e| format!("clear staged plugin {}: {e}", g.pose.display()))?;
            }
            installation_sure::copier_le_dossier(&g.source, &g.pose, copier)
                .map_err(|e| format!("stage plugin {}: {e}", g.pose.display()))?;
            verifier_la_copie(&g.source, &g.pose)
        })();
        if let Err(e) = resultat {
            std::fs::remove_dir_all(&g.pose).ok();
            abandonner(&greffons[..i]);
            return Err(e);
        }
    }
    if let Some(parent) = greffons.first().and_then(|g| g.pose.parent()) {
        installation_sure::synchroniser_dossier(parent);
    }
    Ok(())
}

/// Retire les copies posées et non basculées (et `plugins.new/` s'il est
/// vide).
pub(super) fn abandonner(greffons: &[GreffonLivre]) {
    for g in greffons {
        std::fs::remove_dir_all(&g.pose).ok();
    }
    if let Some(parent) = greffons.first().and_then(|g| g.pose.parent()) {
        std::fs::remove_dir(parent).ok();
    }
}

/// Un greffon basculé : de quoi le remettre.
#[derive(Debug)]
pub(super) struct GreffonBascule {
    pub cible: PathBuf,
    pub ancien: PathBuf,
    /// L'ancien a été mis de côté en `plugins.old/<nom>`.
    pub ancien_garde: bool,
}

/// Bascule un greffon posé : l'ancien part en `plugins.old/<nom>` (la
/// sauvegarde précédente est retirée), le neuf prend sa place. Un échec
/// laisse la cible comme avant.
pub(super) fn basculer(
    g: &GreffonLivre,
    renommer: fn(&Path, &Path) -> io::Result<()>,
) -> Result<GreffonBascule, String> {
    for dossier in [g.cible.parent(), g.ancien.parent()].into_iter().flatten() {
        std::fs::create_dir_all(dossier)
            .map_err(|e| format!("create {}: {e}", dossier.display()))?;
    }
    if std::fs::symlink_metadata(&g.ancien).is_ok() {
        std::fs::remove_dir_all(&g.ancien)
            .map_err(|e| format!("clear plugin backup {}: {e}", g.ancien.display()))?;
    }
    let ancien_garde = std::fs::symlink_metadata(&g.cible).is_ok();
    if ancien_garde {
        renommer(&g.cible, &g.ancien)
            .map_err(|e| format!("park old plugin {}: {e}", g.cible.display()))?;
    }
    if let Err(e) = renommer(&g.pose, &g.cible) {
        if ancien_garde {
            std::fs::rename(&g.ancien, &g.cible).ok();
        }
        return Err(format!("swap new plugin into {}: {e}", g.cible.display()));
    }
    if let Some(parent) = g.cible.parent() {
        installation_sure::synchroniser_dossier(parent);
    }
    Ok(GreffonBascule {
        cible: g.cible.clone(),
        ancien: g.ancien.clone(),
        ancien_garde,
    })
}

/// Remet les anciens greffons basculés, du dernier au premier ; rend ce qui
/// n'a pas pu être remis.
pub(super) fn remettre_tous(bascules: &[GreffonBascule]) -> Vec<String> {
    let mut non_remis = Vec::new();
    for b in bascules.iter().rev() {
        let resultat = (|| {
            std::fs::remove_dir_all(&b.cible).map_err(|e| e.to_string())?;
            if b.ancien_garde {
                std::fs::rename(&b.ancien, &b.cible).map_err(|e| e.to_string())?;
            }
            Ok::<(), String>(())
        })();
        if let Err(e) = resultat {
            tracing::error!(dir = %b.cible.display(), error = %e, "update_plugin_rollback_failed");
            non_remis.push(format!("plugin {} NOT restored ({e})", b.cible.display()));
        }
    }
    non_remis
}

/// Journalise la bascule réussie.
pub(super) fn journaliser(bascules: &[GreffonBascule]) {
    if !bascules.is_empty() {
        info!(n = bascules.len(), "update_greffons_livres_remplaces");
    }
}

#[cfg(test)]
#[path = "greffons_livres_8955_tests.rs"]
mod tests;
