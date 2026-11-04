//! #7600 — ce que la route de suivi dit d'une mise à jour qui n'aboutit pas.
//!
//! Signalé en rc8.1 sous Linux : après « Mettre à jour », le bouton reste sur
//! « Chargement » et rien ne se passe. L'écran ne lit, pendant toute
//! l'attente, que `GET /system/update/status`. Or trois familles de cas n'y
//! laissaient rien de lisible :
//!
//! - les refus définitifs faits AVANT le téléchargement (Docker, paquet ou
//!   `.no-auto-update`, dossier non inscriptible, vérification impossible,
//!   pas d'archive pour la plateforme) ne rendaient leur raison que dans la
//!   réponse au POST — la route de suivi restait sur `phase: null` ;
//! - les échecs en arrière-plan écrivaient `failed: <phrase>` sans raison
//!   lisible par une machine ;
//! - une tâche d'arrière-plan morte (panique, annulation) laissait la phase
//!   figée sur `downloading` ou `installing` : « en cours » pour toujours, et
//!   chaque nouveau clic refusé en `already_in_progress`.
//!
//! Ce module tient la raison à côté de la phase et la rend sur la route de
//! suivi, sous `status` / `reason` / `message`. La phase garde son libellé
//! `failed: …`, que les clients existants savent déjà lire. Il ne touche ni
//! au remplacement du binaire ni à celui de `web/`.

use std::sync::Mutex;

use serde_json::{Value, json};
use tracing::warn;

/// Raisons stables rendues dans `reason` : l'écran les traduit, il ne parse
/// pas la phrase anglaise de la phase.
pub(crate) mod raison {
    /// Serveur en conteneur : la mise à jour passe par l'image.
    pub(crate) const DOCKER: &str = "docker";
    /// Installation gérée par un gestionnaire de paquets (Homebrew) ou mise à
    /// jour intégrée interdite (`.no-auto-update`, posé par les paquets).
    pub(crate) const PACKAGE_MANAGED: &str = "package_managed";
    /// Le dossier du binaire n'est pas inscriptible par le compte de Tune.
    pub(crate) const NOT_WRITABLE: &str = "not_writable";
    /// La liste des versions n'a pas pu être lue.
    pub(crate) const CHECK_FAILED: &str = "check_failed";
    /// La version n'a pas d'archive pour cette plateforme.
    pub(crate) const NO_ASSET: &str = "no_asset";
    /// Le téléchargement de l'archive a échoué.
    pub(crate) const DOWNLOAD_FAILED: &str = "download_failed";
    /// Signature ou empreinte non vérifiée : installation refusée.
    pub(crate) const SIGNATURE_CHECK_FAILED: &str = "signature_check_failed";
    /// Dossier temporaire ou extraction de l'archive en échec.
    pub(crate) const EXTRACT_FAILED: &str = "extract_failed";
    /// Copie, renommage ou contrôle du nouveau binaire refusé.
    pub(crate) const INSTALL_FAILED: &str = "install_failed";
    /// L'installation a paniqué (attrapée par `catch_unwind`).
    pub(crate) const INSTALL_CRASHED: &str = "install_crashed";
    /// La tâche d'arrière-plan s'est arrêtée sans rendre de résultat.
    pub(crate) const TASK_ABORTED: &str = "task_aborted";
    /// Échec dont l'étape n'est pas connue.
    pub(crate) const UPDATE_FAILED: &str = "update_failed";
    /// Pas un échec : la version installée est déjà la dernière.
    pub(crate) const UP_TO_DATE: &str = "up_to_date";
}

/// L'issue de la dernière demande de mise à jour qui n'a rien installé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueDeMiseAJour {
    /// `"failed"` ou `"up_to_date"`.
    pub(crate) statut: &'static str,
    pub(crate) raison: &'static str,
    pub(crate) message: String,
}

pub type Phase = Mutex<Option<String>>;
pub type Issue = Mutex<Option<IssueDeMiseAJour>>;

fn verrou<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Pose un échec explicite : la phase passe à `failed: <message>` (lu par les
/// clients actuels, et qui libère la garde `already_in_progress`), et la
/// raison est retenue pour la route de suivi.
pub(crate) fn noter_l_echec(phase: &Phase, issue: &Issue, raison: &'static str, message: &str) {
    warn!(reason = raison, message, "update_failed_reason");
    let mut p = verrou(phase);
    *verrou(issue) = Some(IssueDeMiseAJour {
        statut: "failed",
        raison,
        message: message.to_string(),
    });
    *p = Some(format!("failed: {message}"));
}

/// « Déjà à jour » : pas un échec, mais une fin que l'écran doit pouvoir lire
/// au lieu d'attendre une phase qui ne viendra jamais.
pub(crate) fn noter_a_jour(phase: &Phase, issue: &Issue, message: &str) {
    let mut p = verrou(phase);
    *verrou(issue) = Some(IssueDeMiseAJour {
        statut: "up_to_date",
        raison: raison::UP_TO_DATE,
        message: message.to_string(),
    });
    *p = None;
}

/// Début de la phrase de la garde Docker de secours de la tâche
/// (`update.rs`) : seule phrase reconnue telle quelle, gardée par un témoin.
pub(crate) const PREFIXE_DOCKER: &str = "Running in Docker";

/// La garde `already_in_progress` de `update_install` : une phase posée et
/// non `failed…` refuse une nouvelle tentative.
pub(crate) fn bloque_une_nouvelle_tentative(phase: Option<&str>) -> bool {
    phase.is_some_and(|p| !p.starts_with("failed"))
}

/// La raison d'un échec posé en arrière-plan, d'après l'étape où il survient.
///
/// Les échecs de la tâche écrivent `failed: <phrase>` juste après avoir posé
/// leur étape (`downloading`, `verifying`, `extracting`, `installing`) : c'est
/// l'étape qui dit la famille, sans avoir à reconnaître chaque phrase.
pub(crate) fn raison_de_l_etape(precedente: Option<&str>, nouvelle: &str) -> &'static str {
    let detail = nouvelle.strip_prefix("failed:").unwrap_or(nouvelle).trim();
    if detail.starts_with(PREFIXE_DOCKER) {
        return raison::DOCKER;
    }
    if detail.starts_with("Install crashed") {
        return raison::INSTALL_CRASHED;
    }
    match precedente {
        Some("downloading") => raison::DOWNLOAD_FAILED,
        Some("verifying") => raison::SIGNATURE_CHECK_FAILED,
        Some("extracting") => raison::EXTRACT_FAILED,
        Some("installing") | Some("installing_bundle") => raison::INSTALL_FAILED,
        _ => raison::UPDATE_FAILED,
    }
}

/// Pose une phase de la tâche d'arrière-plan. Une phase `failed…` retient sa
/// raison ; toute autre phase efface l'issue précédente (une nouvelle
/// tentative est en cours).
pub(crate) fn poser_la_phase(phase: &Phase, issue: &Issue, nouvelle: &str) {
    let mut p = verrou(phase);
    if nouvelle.starts_with("failed") {
        let raison = raison_de_l_etape(p.as_deref(), nouvelle);
        let message = nouvelle
            .strip_prefix("failed:")
            .unwrap_or(nouvelle)
            .trim()
            .to_string();
        *verrou(issue) = Some(IssueDeMiseAJour {
            statut: "failed",
            raison,
            message,
        });
    } else {
        *verrou(issue) = None;
    }
    *p = Some(nouvelle.to_string());
}

/// Attend la fin de la tâche d'arrière-plan. Si elle s'arrête — panique,
/// annulation, retour — sans avoir posé ni échec ni relance, la phase serait
/// restée « en cours » pour toujours : on pose l'échec `task_aborted`.
pub(crate) async fn surveiller_la_tache(
    tache: tokio::task::JoinHandle<()>,
    phase: std::sync::Arc<Phase>,
    issue: std::sync::Arc<Issue>,
) {
    let fin = tache.await;
    let etape = verrou(&phase).clone();
    let Some(etape) = etape else { return };
    if etape.starts_with("failed") || etape == "restarting" {
        return;
    }
    let comment = match &fin {
        Err(e) if e.is_panic() => "crashed",
        Err(_) => "was cancelled",
        Ok(()) => "ended without a result",
    };
    noter_l_echec(
        &phase,
        &issue,
        raison::TASK_ABORTED,
        &format!("Update task {comment} during '{etape}'. Retry the update."),
    );
}

/// Les champs `status`, `reason` et `message` de `GET /system/update/status`.
///
/// `status` : `idle`, `in_progress`, `restart_pending`, `failed` ou
/// `up_to_date`. `reason` n'est renseigné que pour `failed` et `up_to_date`.
pub(crate) fn etat_de_suivi(phase: Option<&str>, issue: Option<&IssueDeMiseAJour>) -> Value {
    match phase {
        None => match issue {
            Some(i) if i.statut == "up_to_date" => {
                json!({"status": "up_to_date", "reason": i.raison, "message": i.message})
            }
            _ => json!({"status": "idle", "reason": null, "message": null}),
        },
        Some(p) if p.starts_with("failed") => {
            let (raison, message) = match issue {
                Some(i) if i.statut == "failed" => (i.raison, i.message.clone()),
                _ => (
                    raison::UPDATE_FAILED,
                    p.strip_prefix("failed:").unwrap_or(p).trim().to_string(),
                ),
            };
            json!({"status": "failed", "reason": raison, "message": message})
        }
        Some("restart_pending_playback") => {
            json!({"status": "restart_pending", "reason": null, "message": null})
        }
        Some(_) => json!({"status": "in_progress", "reason": null, "message": null}),
    }
}

#[cfg(test)]
#[path = "suivi_de_mise_a_jour_7600_tests.rs"]
mod tests;
