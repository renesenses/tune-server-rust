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
//! Sous Unix, l'ancien est mis de côté par un LIEN DUR (`<nom>.old`), puis
//! le neuf remplace la cible par `rename` : le nom installé n'est jamais
//! absent, même si la machine s'arrête entre les deux (relecture de #8954).
//! Une bascule coupée par un arrêt est finie au démarrage suivant
//! ([`finir_les_bascules_interrompues`]). Les `<nom>.old` sont retirés au
//! premier démarrage de la version neuve ([`retirer_les_sauvegardes`]).
//!
//! Un exécutable que l'utilisateur veut garder (le sien, ou aucun) se
//! protège par un marqueur vide `.no-bundled-<nom>` dans le dossier du
//! binaire : la mise à jour ne crée ni ne remplace alors ce nom. Sans
//! marqueur, un exécutable livré remplacé à la main par un fichier ordinaire
//! est remplacé (un lien symbolique, lui, est toujours laissé).
//!
//! Arrêt et relance des démons : à la relance (`exec`), chaque
//! `airplay-daemon` voit son entrée standard se fermer (tube `CLOEXEC`) et
//! sort ; la relance de la mise à jour arrête en plus les sorties AirPlay 2
//! (et leur démon) et `librespot` avant l'`exec`. Le
//! serveur relancé les démarre à nouveau, depuis les fichiers neufs.

use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

use super::installation_sure::{self, Controles, a_cote};

/// Les exécutables livrés à côté de `tune-server` dans les archives de
/// release, sans suffixe de plateforme. Ajouter un exécutable à l'archive
/// (`release.yml`) sans l'ajouter ici le laisse figé chez les utilisateurs
/// mis à jour par le bouton.
pub(crate) const EXECUTABLES_LIVRES: [&str; 4] =
    ["airplay-daemon", "librespot", "ffmpeg", "fpcalc"];

/// Le marqueur qui dit de laisser `nom` tel qu'il est (ou absent).
pub(super) fn marqueur_de_retrait(exe_dir: &Path, nom: &str) -> PathBuf {
    exe_dir.join(format!(".no-bundled-{nom}"))
}

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
/// du système) est laissée telle quelle, de même qu'un nom protégé par son
/// marqueur [`marqueur_de_retrait`].
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
            if marqueur_de_retrait(exe_dir, nom).exists() {
                info!(cible = %cible.display(), "update_executable_livre_marqueur_garde");
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

/// Libère `<cible>.old` : un fichier est supprimé ; un dossier, ou un
/// fichier qui ne se supprime pas, est renommé sous un nom unique
/// (`<cible>.old.<pid>-<n>`) plutôt que de faire échouer la mise à jour.
fn liberer_la_sauvegarde(ancien: &Path) -> Result<(), String> {
    let Ok(meta) = std::fs::symlink_metadata(ancien) else {
        return Ok(());
    };
    if !meta.is_dir() && std::fs::remove_file(ancien).is_ok() {
        return Ok(());
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let ecarte = a_cote(ancien, &format!("{}-{unique}", std::process::id()));
    std::fs::rename(ancien, &ecarte)
        .map_err(|e| format!("clear backup {}: {e}", ancien.display()))?;
    warn!(de = %ancien.display(), vers = %ecarte.display(), "update_sauvegarde_ecartee");
    Ok(())
}

/// Bascule un exécutable préparé : l'ancien est gardé en `<cible>.old` (le
/// précédent est libéré), le `.new` prend son nom. Un échec laisse la cible
/// comme avant.
///
/// L'ancien est gardé par un lien dur, puis `rename(.new, cible)` remplace la
/// cible d'un coup : il n'y a pas d'instant où la cible manque. Sur un
/// système de fichiers sans liens durs, repli sur le renommage.
pub(super) fn basculer(
    l: &ExecutableLivre,
    renommer: fn(&Path, &Path) -> io::Result<()>,
) -> Result<LivreBascule, String> {
    let pose = a_cote(&l.cible, "new");
    let ancien = a_cote(&l.cible, "old");
    liberer_la_sauvegarde(&ancien)?;
    let ancien_garde = std::fs::symlink_metadata(&l.cible).is_ok();
    if ancien_garde && let Err(e) = std::fs::hard_link(&l.cible, &ancien) {
        warn!(cible = %l.cible.display(), error = %e, "update_lien_dur_impossible — renommage");
        renommer(&l.cible, &ancien).map_err(|e| format!("park old {}: {e}", l.cible.display()))?;
    }
    if let Err(e) = renommer(&pose, &l.cible) {
        if ancien_garde {
            if std::fs::symlink_metadata(&l.cible).is_ok() {
                // Lien dur : la cible n'a pas bougé, seul le lien est retiré.
                std::fs::remove_file(&ancien).ok();
            } else {
                std::fs::rename(&ancien, &l.cible).ok();
            }
        }
        return Err(format!("swap new {} into place: {e}", l.cible.display()));
    }
    if let Some(dir) = l.cible.parent() {
        installation_sure::synchroniser_dossier(dir);
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

/// Finit, au démarrage, une bascule coupée par un arrêt de la machine
/// (Unix).
///
/// Les `<nom>.new` sont tous écrits AVANT la bascule du binaire, qui
/// consomme `tune-server.new`. Donc :
/// - `tune-server.new` encore là : le binaire n'a pas basculé, les `.new`
///   des exécutables livrés sont d'une mise à jour abandonnée → retirés ;
/// - sinon, un `<nom>.new` qui traîne est la suite d'une bascule coupée : il
///   prend la place de la cible s'il est valide (non vide, ELF de cette
///   machine sous Linux) et que la cible est absente ou plus ancienne que
///   lui ; sinon il est retiré.
///
/// `binaire` : le chemin installé du serveur (jamais `.old`). Rend les
/// cibles remplacées.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) fn finir_les_bascules_interrompues(
    binaire: &Path,
    controles: &Controles,
) -> Vec<PathBuf> {
    let Some(exe_dir) = binaire.parent() else {
        return Vec::new();
    };
    // Le même nom que le `.new` d'`install_unix`.
    let binaire_abandonne = binaire.with_extension("new").exists();
    let mut finies = Vec::new();
    for nom in EXECUTABLES_LIVRES {
        let cible = exe_dir.join(nom);
        let pose = a_cote(&cible, "new");
        if !std::fs::symlink_metadata(&pose).is_ok_and(|m| m.file_type().is_file()) {
            continue;
        }
        let refus = if binaire_abandonne {
            Some("binary was not swapped".to_string())
        } else if marqueur_de_retrait(exe_dir, nom).exists() {
            Some("kept by its .no-bundled marker".to_string())
        } else if std::fs::symlink_metadata(&cible).is_ok_and(|m| m.file_type().is_symlink()) {
            Some("target is a symlink".to_string())
        } else {
            let sans_essai = Controles {
                taille_attendue: None,
                essai: |_| Ok(()),
                ..*controles
            };
            match installation_sure::valider_binaire(&pose, &sans_essai) {
                Err(e) => Some(e),
                Ok(()) => {
                    let date = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
                    match (date(&cible), date(&pose)) {
                        (Some(c), Some(n)) if c > n => Some("target is newer".to_string()),
                        _ => None,
                    }
                }
            }
        };
        if let Some(motif) = refus {
            warn!(pose = %pose.display(), motif = %motif, "update_bascule_interrompue_abandonnee");
            std::fs::remove_file(&pose).ok();
            continue;
        }
        // Exécutable quoi qu'ait laissé la coupure.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&pose, std::fs::Permissions::from_mode(0o755))
            {
                warn!(pose = %pose.display(), error = %e, "update_bascule_interrompue_droits");
                std::fs::remove_file(&pose).ok();
                continue;
            }
        }
        let livre = ExecutableLivre {
            source: pose.clone(),
            cible: cible.clone(),
        };
        match basculer(&livre, |de, vers| std::fs::rename(de, vers)) {
            Ok(_) => {
                warn!(cible = %cible.display(), "update_bascule_interrompue_finie");
                finies.push(cible);
            }
            Err(e) => {
                warn!(cible = %cible.display(), error = %e, "update_bascule_interrompue_echec");
            }
        }
    }
    finies
}

/// Retire les sauvegardes des exécutables livrés (`<nom>.old` et ceux
/// écartés en `<nom>.old.<…>`) une fois la version neuve démarrée : elles
/// ne servent qu'à revenir de CETTE mise à jour, et une sauvegarde périmée
/// ne doit pas être reprise par un retour manuel. Seuls des fichiers sont
/// retirés, jamais un dossier. Rend les chemins retirés.
pub(super) fn retirer_les_sauvegardes(exe_dir: &Path, suffixe: &str) -> Vec<PathBuf> {
    let Ok(entrees) = std::fs::read_dir(exe_dir) else {
        return Vec::new();
    };
    let prefixes: Vec<String> = EXECUTABLES_LIVRES
        .iter()
        .map(|nom| format!("{nom}{suffixe}.old"))
        .collect();
    let mut retirees = Vec::new();
    for entree in entrees.flatten() {
        let nom = entree.file_name().to_string_lossy().into_owned();
        let sauvegarde = prefixes.iter().any(|p| {
            nom == *p
                || nom
                    .strip_prefix(p.as_str())
                    .is_some_and(|r| r.starts_with('.'))
        });
        if !sauvegarde || !entree.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if std::fs::remove_file(entree.path()).is_ok() {
            retirees.push(entree.path());
        }
    }
    if !retirees.is_empty() {
        info!(
            n = retirees.len(),
            "update_sauvegardes_des_executables_livres_retirees"
        );
    }
    retirees
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
