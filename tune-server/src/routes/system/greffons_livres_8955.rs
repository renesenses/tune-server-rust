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

/// Fil 3014 — combien de fois une opération refusée sur un greffon est tentée.
const ESSAIS: u32 = 5;

/// L'attente avant le `essai`-ième nouvel essai (courte en test).
fn attente(essai: u32) -> std::time::Duration {
    if cfg!(test) {
        std::time::Duration::from_millis(1)
    } else {
        std::time::Duration::from_millis(200 * u64::from(essai))
    }
}

/// Un refus qui peut n'être que passager sous Windows : un antivirus ou
/// l'indexeur tient un fichier tout juste écrit (ERROR_ACCESS_DENIED, ou
/// ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION).
fn refus_passager(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
        || (cfg!(windows) && matches!(e.raw_os_error(), Some(32 | 33)))
}

/// `op`, retentée quelques fois, avec une courte attente, tant qu'elle
/// rencontre un refus passager.
fn avec_essais<T>(quoi: &Path, mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut essai = 1;
    loop {
        match op() {
            Err(e) if essai < ESSAIS && refus_passager(&e) => {
                warn!(
                    chemin = %quoi.display(),
                    essai,
                    error = %e,
                    "update_greffon_livre_refus_nouvel_essai"
                );
                std::thread::sleep(attente(essai));
                essai += 1;
            }
            resultat => return resultat,
        }
    }
}

/// Retire l'attribut lecture seule de `chemin` et de tout ce qu'il contient :
/// sous Windows, un fichier en lecture seule ne se supprime ni ne s'écrase.
/// Sans effet ailleurs (c'est le dossier parent qui y décide).
fn retirer_la_lecture_seule(chemin: &Path) {
    #[cfg(windows)]
    {
        let Ok(meta) = std::fs::symlink_metadata(chemin) else {
            return;
        };
        if meta.is_dir() {
            if let Ok(entrees) = std::fs::read_dir(chemin) {
                for entree in entrees.flatten() {
                    retirer_la_lecture_seule(&entree.path());
                }
            }
        }
        let mut droits = meta.permissions();
        if droits.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            droits.set_readonly(false);
            std::fs::set_permissions(chemin, droits).ok();
        }
    }
    #[cfg(not(windows))]
    let _ = chemin;
}

/// Supprime le dossier `chemin` s'il existe, lecture seule retirée d'abord,
/// avec quelques essais sur un refus passager.
pub(super) fn effacer(chemin: &Path) -> io::Result<()> {
    avec_essais(chemin, || {
        if std::fs::symlink_metadata(chemin).is_err() {
            return Ok(());
        }
        retirer_la_lecture_seule(chemin);
        std::fs::remove_dir_all(chemin)
    })
}

/// Pose et vérifie un `plugins.new/<nom>`, sans toucher à `plugins/`.
fn poser(g: &GreffonLivre, copier: fn(&Path, &Path) -> io::Result<()>) -> Result<(), String> {
    if let Some(parent) = g.pose.parent() {
        avec_essais(parent, || std::fs::create_dir_all(parent))
            .map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    effacer(&g.pose).map_err(|e| format!("clear staged plugin {}: {e}", g.pose.display()))?;
    // Une copie refusée est reprise depuis le début : `copier_le_dossier`
    // crée le dossier, il faut donc retirer la copie partielle d'abord.
    avec_essais(&g.pose, || {
        effacer(&g.pose)?;
        installation_sure::copier_le_dossier(&g.source, &g.pose, copier)
    })
    .map_err(|e| format!("stage plugin {}: {e}", g.pose.display()))?;
    verifier_la_copie(&g.source, &g.pose)
}

/// Ce que la préparation a posé, et ce qu'elle a dû laisser.
#[derive(Debug, Default)]
pub(super) struct Preparation {
    /// Les greffons posés en `plugins.new/<nom>`, prêts à basculer.
    pub poses: Vec<GreffonLivre>,
    /// Une ligne par greffon qui n'a PAS pu être posé : il garde sa version
    /// actuelle.
    pub gardes: Vec<String>,
}

/// Pose et vérifie chaque `plugins.new/<nom>`, sans toucher à `plugins/`.
///
/// Fil 3014 — un greffon ne bloque JAMAIS la mise à jour : celui qui ne peut
/// pas être posé (après quelques essais) est retiré de `plugins.new/`, garde
/// sa version actuelle et figure dans [`Preparation::gardes`] ; le serveur,
/// `web/` et les autres greffons sont mis à jour quand même.
pub(super) fn preparer(
    greffons: &[GreffonLivre],
    copier: fn(&Path, &Path) -> io::Result<()>,
) -> Preparation {
    let mut preparation = Preparation::default();
    for g in greffons {
        match poser(g, copier) {
            Ok(()) => preparation.poses.push(g.clone()),
            Err(e) => {
                effacer(&g.pose).ok();
                let nom = g.cible.file_name().unwrap_or_default().to_string_lossy();
                warn!(
                    greffon = %nom,
                    error = %e,
                    "update_greffon_livre_garde_a_sa_version — la mise à jour continue sans lui"
                );
                preparation
                    .gardes
                    .push(format!("plugin {nom} kept at its current version ({e})"));
            }
        }
    }
    if let Some(parent) = greffons.first().and_then(|g| g.pose.parent()) {
        if preparation.poses.is_empty() {
            std::fs::remove_dir(parent).ok();
        } else {
            installation_sure::synchroniser_dossier(parent);
        }
    }
    preparation
}

/// Fil 3014 — le marqueur qui dit, au démarrage suivant, quels greffons
/// livrés ont gardé leur version (lu par `record_post_update_result`).
pub(super) const MARQUE_GARDES: &str = "tune-update-plugins-kept.txt";

/// Écrit (ou retire, s'il n'y a rien) le marqueur des greffons gardés.
pub(super) fn noter_les_gardes(exe_dir: &Path, gardes: &[String]) {
    let marque = exe_dir.join(MARQUE_GARDES);
    if gardes.is_empty() {
        std::fs::remove_file(marque).ok();
    } else if let Err(e) = std::fs::write(&marque, gardes.join("\n")) {
        warn!(marque = %marque.display(), error = %e, "update_greffons_gardes_marque_non_ecrite");
    }
}

/// Retire les copies posées et non basculées (et `plugins.new/` s'il est
/// vide).
pub(super) fn abandonner(greffons: &[GreffonLivre]) {
    for g in greffons {
        effacer(&g.pose).ok();
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
    effacer(&g.ancien)
        .map_err(|e| format!("clear plugin backup {}: {e}", g.ancien.display()))?;
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
            effacer(&b.cible).map_err(|e| e.to_string())?;
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
