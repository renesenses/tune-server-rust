//! #8676 — « Remplacer le chemin » (Réglages › Bibliothèque).
//!
//! `POST /system/music-dirs/move/preview` : ce que l'opération ferait — pistes
//! sous l'ancien préfixe, combien existent sur disque au nouveau (échantillon
//! régulier au-delà de [`deplacer_racine::ECHANTILLON_MAX`]), exemples
//! manquants, collisions, colonnes et réglages touchés. N'écrit rien.
//!
//! `POST /system/music-dirs/move` : applique, en une transaction, si l'aperçu
//! refait à l'instant l'autorise ET si `confirm_tracks` est le nombre de pistes
//! qu'il annonce — même contrat que `confirm_purge` (#1943) : une confirmation
//! prise sur un écran périmé ne vaut pas pour une autre ampleur. Refusé
//! pendant un scan : le droit de scanner est tenu tout le temps de l'écriture,
//! aucun scan ne peut donc démarrer au milieu.
//!
//! Les deux routes sont réservées à l'administrateur. Pas de rescan : les
//! chemins réécrits sont ceux du disque. L'opération est journalisée (journal
//! `tracing` et réglage `library_root_moves_journal`) et se défait par la même
//! opération en sens inverse.
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use tune_core::library::deplacer_racine::{self, Apercu, Prefixes};

use crate::auth::RequireAdmin;
use crate::state::AppState;

#[derive(Deserialize)]
pub(crate) struct CorpsDeplacement {
    old_prefix: String,
    new_prefix: String,
    /// Le nombre de pistes annoncé par l'aperçu. Exigé par l'application.
    #[serde(default)]
    confirm_tracks: Option<u64>,
}

fn refus(code: StatusCode, refusal: deplacer_racine::Refus) -> Response {
    (
        code,
        Json(json!({ "refusal": refusal, "can_apply": false })),
    )
        .into_response()
}

async fn calculer_l_apercu(state: &AppState, p: Prefixes) -> Result<Apercu, String> {
    let db = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        deplacer_racine::apercu(
            db.as_ref(),
            &p,
            deplacer_racine::ECHANTILLON_MAX,
            &|chemin| std::path::Path::new(chemin).exists(),
        )
    })
    .await
    .map_err(|e| e.to_string())?
}

/// `POST /system/music-dirs/move/preview`.
pub(super) async fn apercu(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(corps): Json<CorpsDeplacement>,
) -> Response {
    let p = match Prefixes::nouveaux(&corps.old_prefix, &corps.new_prefix) {
        Ok(p) => p,
        Err(r) => return refus(StatusCode::BAD_REQUEST, r),
    };
    match calculer_l_apercu(&state, p).await {
        Ok(a) => Json(a).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

/// `POST /system/music-dirs/move`.
pub(super) async fn appliquer(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(corps): Json<CorpsDeplacement>,
) -> Response {
    let p = match Prefixes::nouveaux(&corps.old_prefix, &corps.new_prefix) {
        Ok(p) => p,
        Err(r) => return refus(StatusCode::BAD_REQUEST, r),
    };
    // Le droit de scanner, tenu jusqu'à la fin de l'écriture.
    let Some(_droit) = super::scan::try_begin_scan() else {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "scan_running", "can_apply": false })),
        )
            .into_response();
    };
    let a = match calculer_l_apercu(&state, p.clone()).await {
        Ok(a) => a,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };
    if let Some(r) = a.refusal {
        tracing::info!(motif = ?r, "library_root_move_refused (#8676)");
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(a)).into_response();
    }
    if corps.confirm_tracks != Some(a.tracks) {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "confirmation_mismatch",
                "confirm_tracks_required": a.tracks,
                "preview": a,
            })),
        )
            .into_response();
    }
    let db = state.backend.clone();
    let p2 = p.clone();
    let resultat = tokio::task::spawn_blocking(move || deplacer_racine::appliquer(&db, &p2))
        .await
        .map_err(|e| e.to_string())
        .and_then(|r| r);
    match resultat {
        Ok(bilan) => {
            tracing::warn!(
                ancien = %bilan.old_prefix,
                nouveau = %bilan.new_prefix,
                pistes = bilan.tracks_updated,
                colonnes = ?bilan.columns,
                reglages = ?bilan.settings,
                "library_root_moved (#8676) — chemins réécrits en une transaction, \
                 identifiants conservés. Réversible : même opération en sens inverse."
            );
            // Le veilleur de fichiers suit les racines : elles ont changé.
            if bilan.settings.contains(&"music_dirs") {
                crate::auto_scan::signaler_racines_modifiees();
            }
            let inverse = p.inverse();
            Json(json!({
                "applied": true,
                "old_prefix": bilan.old_prefix,
                "new_prefix": bilan.new_prefix,
                "tracks_updated": bilan.tracks_updated,
                "columns": bilan.columns,
                "settings": bilan.settings,
                "dirs": super::get_music_dirs_list(&state.backend),
                "reverse": { "old_prefix": inverse.ancien, "new_prefix": inverse.nouveau },
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(
                ancien = %p.ancien,
                nouveau = %p.nouveau,
                erreur = %e,
                "library_root_move_failed (#8676) — transaction annulée, rien n'a changé"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e, "applied": false })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
#[path = "deplacer_racine_tests_8676.rs"]
mod tests;
