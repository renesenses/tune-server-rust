//! Propositions de correction venues de la communaute — la liste, la reponse,
//! et la bascule d'application automatique.
//!
//! L'interface presente ce que d'autres bibliotheques affirment ; l'utilisateur
//! tranche. Rien n'est applique sans lui, sauf s'il a active la bascule, qui
//! reste un reglage LOCAL : le cloud n'a jamais a savoir laquelle des deux
//! voies a ete empruntee.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::cloud::metadata_proposals::{self, AUTO_APPLY_SETTING};
use tune_core::db::metadata_proposal_repo::{MetadataProposal, MetadataProposalRepo};
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

use super::now_iso_utc;

#[derive(Debug, Deserialize)]
pub(super) struct ProposalListQuery {
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(super) struct DecisionBody {
    /// true = j'accepte la valeur de la communaute.
    accept: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct AutoApplyBody {
    enabled: bool,
}

fn to_json(p: &MetadataProposal) -> Value {
    json!({
        "id": p.id,
        "entity": p.entity,
        "local_id": p.local_id,
        "title": p.title,
        "artist": p.artist,
        "field": p.field,
        "current": p.current_value,
        "proposed": p.proposed_value,
        "servers_count": p.servers_count,
        "fetched_at": p.fetched_at,
    })
}

/// GET /api/v1/library/proposals
pub(super) async fn list_proposals(
    State(state): State<AppState>,
    Query(q): Query<ProposalListQuery>,
) -> Json<Value> {
    let repo = MetadataProposalRepo::with_backend(state.backend.clone());
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    let pending = repo.list_pending(limit).unwrap_or_default();

    let auto = SettingsRepo::with_backend(state.backend.clone())
        .get(AUTO_APPLY_SETTING)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1");

    Json(json!({
        "proposals": pending.iter().map(to_json).collect::<Vec<_>>(),
        "pending": repo.count_pending(),
        "auto_apply": auto,
    }))
}

/// POST /api/v1/library/proposals/{id}/decision
pub(super) async fn decide_proposal(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<DecisionBody>,
) -> impl IntoResponse {
    match metadata_proposals::decide(&state.backend, id, body.accept, &now_iso_utc()) {
        Ok(p) => Json(json!({
            "decided": true,
            "decision": p.decision,
            "applied": body.accept,
            "proposal": to_json(&p),
        }))
        .into_response(),
        // L'ecriture locale a echoue : la proposition reste en attente plutot
        // que d'etre comptee comme traitee sans que rien n'ait change.
        Err(e) => (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({"error": e}))).into_response(),
    }
}

/// POST /api/v1/library/proposals/auto-apply
///
/// La bascule ne rattrape pas l'arriere elle-meme : le prochain cycle
/// d'arbitrage s'en charge, et il traite les propositions deja en attente.
/// L'activer n'a donc jamais d'effet immediat sur la bibliotheque — ce qui
/// laisse le temps de la desactiver si on s'est trompe de bouton.
pub(super) async fn set_auto_apply(
    State(state): State<AppState>,
    Json(body): Json<AutoApplyBody>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match settings.set(
        AUTO_APPLY_SETTING,
        if body.enabled { "true" } else { "false" },
    ) {
        Ok(()) => Json(json!({ "auto_apply": body.enabled })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
