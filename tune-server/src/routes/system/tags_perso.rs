//! #9158 (Rob) — réglage « Balises personnelles → tags Tune » :
//!
//! - `GET` / `PUT /api/v1/system/settings/personal-tags` : les
//!   correspondances (clé de fichier → tags) ;
//! - `GET /api/v1/system/settings/personal-tags/keys?sample=N` : l'aperçu des
//!   clés rencontrées dans un échantillon de fichiers, avec leur nombre ;
//! - `POST /api/v1/system/settings/personal-tags/apply` : « Appliquer à la
//!   bibliothèque existante », passe de fond (registre des tâches de fond,
//!   pausable, cédant à la lecture) ; `GET` sur la même route rend son
//!   avancement.
//!
//! Le réglage et la lecture vivent dans `tune_core::metadata::tags_perso`,
//! la pose et la marque d'origine dans `tune_core::db::tags_perso_repo`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use tune_core::db::tags_perso_repo as repo;
use tune_core::metadata::tags_perso as tp;
use tune_core::taches_de_fond::{Tache, attendre_son_tour};

use crate::state::AppState;

/// L'identifiant de la passe au registre des tâches de fond.
pub(crate) const TACHE: &str = "tags_perso";
/// Pistes lues par lot.
const TAILLE_DU_LOT: i64 = 100;
/// La respiration entre deux lots.
const PAUSE_ENTRE_LOTS: Duration = Duration::from_millis(300);
/// L'échantillon de l'aperçu, par défaut et au plus.
const ECHANTILLON_DEFAUT: i64 = 200;
const ECHANTILLON_MAX: i64 = 2000;
/// Le temps de lecture de l'aperçu, au plus.
const DELAI_APERCU: Duration = Duration::from_secs(20);

/// L'avancement de la passe « Appliquer », lu par le client.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Avancement {
    pub running: bool,
    pub processed: u64,
    pub total: u64,
    pub added: u64,
    pub removed: u64,
    pub unreadable: u64,
    pub error: Option<String>,
    pub finished: bool,
}

static AVANCEMENT: Mutex<Option<Avancement>> = Mutex::new(None);

fn avancement() -> Avancement {
    AVANCEMENT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

fn modifier(f: impl FnOnce(&mut Avancement)) {
    let mut g = AVANCEMENT.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Avancement::default));
}

fn reponse(reglage: &tp::TagsPerso) -> Json<Value> {
    Json(json!({
        "personal_tags": reglage,
        "max_mappings": tp::MAX_CORRESPONDANCES,
        "apply": avancement(),
    }))
}

pub(super) async fn get_personal_tags(State(state): State<AppState>) -> Json<Value> {
    reponse(&tp::lire(&state.backend))
}

/// Pose le réglage. Réservé à l'administrateur : il change le scan de toute
/// la bibliothèque. Ne relance RIEN : les fichiers déjà importés suivent au
/// prochain scan d'un fichier modifié, ou par « Appliquer ».
pub(super) async fn set_personal_tags(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(saisie): Json<tp::TagsPerso>,
) -> Response {
    let backend = state.backend.clone();
    match tokio::task::spawn_blocking(move || tp::ecrire(&backend, saisie)).await {
        Ok(Ok(nouveau)) => reponse(&nouveau).into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e, "code": "invalid_personal_tags" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ParamsApercu {
    sample: Option<i64>,
}

/// L'aperçu des clés vues dans un échantillon de fichiers, au hasard.
pub(super) async fn personal_tag_keys(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Query(p): Query<ParamsApercu>,
) -> Response {
    let taille = p
        .sample
        .unwrap_or(ECHANTILLON_DEFAUT)
        .clamp(1, ECHANTILLON_MAX);
    let backend = state.backend.clone();
    let fait = tokio::task::spawn_blocking(move || -> Result<repo::Apercu, String> {
        let chemins = repo::echantillon(&backend, taille)?;
        let reglage = tp::lire(&backend);
        let echeance = Instant::now() + DELAI_APERCU;
        Ok(repo::apercu(
            &chemins,
            &reglage,
            lire_balises_bornee,
            || Instant::now() < echeance,
        ))
    })
    .await;
    match fait {
        Ok(Ok(a)) => Json(a).into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// Les balises d'un fichier, sous délai : un partage qui ne répond plus ne
/// fige ni l'aperçu ni la passe.
fn lire_balises_bornee(
    chemin: &std::path::Path,
) -> Option<tune_core::metadata::balises_brutes::BalisesDuFichier> {
    let reel =
        tune_core::library::local_path::resolve_existing_local_path(&chemin.to_string_lossy())?;
    let chemin = std::path::PathBuf::from(reel);
    match crate::lecture_bornee::lire_avec_delai(
        "tags_perso",
        move || tune_core::metadata::balises_brutes::lire_balises_du_fichier(&chemin).ok(),
        crate::lecture_bornee::DELAI_LECTURE_DISQUE,
        &|| false,
    ) {
        crate::lecture_bornee::Lecture::Lue(b) => b,
        _ => None,
    }
}

pub(super) async fn get_apply_status() -> Json<Avancement> {
    Json(avancement())
}

/// « Appliquer à la bibliothèque existante ». `202` : la passe part ; `409` :
/// elle tourne déjà.
pub(super) async fn apply_personal_tags(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Response {
    if avancement().running {
        return (StatusCode::CONFLICT, Json(json!({ "apply": avancement() }))).into_response();
    }
    let Some(garde) =
        state
            .background_tasks
            .try_begin(TACHE, "Balises personnelles → tags Tune…", "maintenance")
    else {
        return (StatusCode::CONFLICT, Json(json!({ "apply": avancement() }))).into_response();
    };
    modifier(|a| {
        *a = Avancement {
            running: true,
            ..Avancement::default()
        }
    });
    let state2 = state.clone();
    tokio::spawn(async move {
        let _garde = garde;
        executer(&state2).await;
    });
    (StatusCode::ACCEPTED, Json(json!({ "apply": avancement() }))).into_response()
}

/// La passe entière, selon le réglage EN VIGUEUR. Réglage vide : retire
/// d'un coup toutes les poses du scan, sans lire un fichier.
async fn executer(state: &AppState) {
    let reglage = tp::courante();
    let b = state.backend.clone();
    if reglage.est_vide() {
        let fait = tokio::task::spawn_blocking(move || repo::retirer_tout(&b)).await;
        modifier(|a| {
            match fait {
                Ok(Ok(n)) => a.removed = n,
                Ok(Err(e)) => a.error = Some(e),
                Err(e) => a.error = Some(e.to_string()),
            }
            a.running = false;
            a.finished = true;
        });
        return;
    }
    let total = {
        let b = b.clone();
        tokio::task::spawn_blocking(move || repo::restantes(&b, 0))
            .await
            .unwrap_or(0)
    };
    modifier(|a| a.total = total);
    state
        .background_tasks
        .update_progress(TACHE, 0, total, crate::background_tasks::Phase::Tags);
    let mut apres = 0i64;
    loop {
        attendre_son_tour(Tache::Enrichissement).await;
        let b = b.clone();
        let r = reglage.clone();
        let lot = tokio::task::spawn_blocking(move || {
            repo::traiter_lot(&b, apres, TAILLE_DU_LOT, &r, |chemin| {
                lire_balises_bornee(chemin).map(|x| x.paires())
            })
        })
        .await;
        match lot {
            Ok(Ok(lot)) if lot.fini => break,
            Ok(Ok(lot)) => {
                apres = lot.dernier;
                let mut fait = 0;
                modifier(|a| {
                    a.processed += lot.lues;
                    a.added += lot.bilan.ajoutes;
                    a.removed += lot.bilan.retires;
                    a.unreadable += lot.illisibles;
                    a.total = a.total.max(a.processed);
                    fait = a.processed;
                });
                state.background_tasks.update_progress(
                    TACHE,
                    fait,
                    total.max(fait),
                    crate::background_tasks::Phase::Tags,
                );
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "tags_perso_application_echec");
                modifier(|a| a.error = Some(e));
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "tags_perso_application_tache_perdue");
                modifier(|a| a.error = Some(e.to_string()));
                break;
            }
        }
        tokio::time::sleep(PAUSE_ENTRE_LOTS).await;
    }
    let bilan = avancement();
    tracing::info!(
        pistes = bilan.processed,
        ajoutes = bilan.added,
        retires = bilan.removed,
        "tags_perso_application_terminee"
    );
    modifier(|a| {
        a.running = false;
        a.finished = true;
    });
}

#[cfg(test)]
mod tests {
    //! Aucun témoin ne pose un réglage NON vide par la route : le miroir est
    //! global au binaire de test, et d'autres témoins y scannent des
    //! fichiers.
    use super::*;

    #[tokio::test]
    async fn le_get_rend_un_reglage_vide_et_le_put_invalide_est_refuse_9158() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let Json(v) = get_personal_tags(State(state.clone())).await;
        assert_eq!(v["personal_tags"]["mappings"], json!([]));
        assert_eq!(v["max_mappings"], json!(tp::MAX_CORRESPONDANCES));

        // Un tag fixe sans nom est refusé, et rien n'est installé.
        let saisie: tp::TagsPerso = serde_json::from_value(json!({
            "mappings": [{ "key": "TXXX:FAVORI", "mode": "fixed" }]
        }))
        .unwrap();
        let r = set_personal_tags(
            crate::auth::RequireAdmin,
            State(state.clone()),
            Json(saisie),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        assert!(tp::courante().est_vide());

        // Le vide reposé : accepté.
        let r = set_personal_tags(
            crate::auth::RequireAdmin,
            State(state.clone()),
            Json(tp::TagsPerso::default()),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }
}
