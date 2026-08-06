//! Metadata reports — "this cover is wrong", "this conductor is not on the
//! record", "this bio is about another artist".
//!
//! Local-first: the row in `metadata_reports` is the record of truth and the
//! local side effect (clearing a wrong artist image) happens immediately. The
//! community push is best-effort on top and only fires when the user opted
//! into community sync — a report is useful on its own even offline.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::metadata_report_repo::{MetadataReportRepo, REPORT_ENTITIES};
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

use super::now_iso_utc;

#[derive(Debug, Deserialize)]
pub(super) struct ReportBody {
    /// One of REPORT_ENTITIES.
    entity: String,
    /// Local row id (track/album/artist). Absent for pure-MBID reports.
    entity_id: Option<i64>,
    mbid: Option<String>,
    /// Metadata key being contested (extra reports).
    field: Option<String>,
    /// Contested value (extra reports) — what the community should unpublish.
    value: Option<String>,
    reason: Option<String>,
    comment: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReportListQuery {
    limit: Option<i64>,
}

/// POST /api/v1/library/reports
pub(super) async fn create_report(
    State(state): State<AppState>,
    Json(body): Json<ReportBody>,
) -> impl IntoResponse {
    let entity = body.entity.trim().to_lowercase();
    if !REPORT_ENTITIES.contains(&entity.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unknown entity",
                "allowed": REPORT_ENTITIES,
            })),
        )
            .into_response();
    }
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("unspecified")
        .to_string();

    let repo = MetadataReportRepo::with_backend(state.backend.clone());
    let created_at = now_iso_utc();
    if let Err(e) = repo.insert(
        &entity,
        body.entity_id,
        body.mbid.as_deref(),
        body.field.as_deref(),
        body.value.as_deref(),
        &reason,
        body.comment.as_deref(),
        &created_at,
    ) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }

    // Local effect: a wrong artist image is dropped straight away so the UI
    // falls back to a placeholder and the next enrichment re-fetches one.
    let mut image_cleared = false;
    if entity == "artist_image" {
        if let Some(id) = body.entity_id {
            image_cleared = ArtistRepo::with_backend(state.backend.clone())
                .clear_image(id)
                .is_ok();
        }
    }

    let pushed = push_report_to_community(&state, &entity, &body, &reason).await;

    Json(json!({
        "reported": true,
        "entity": entity,
        "image_cleared": image_cleared,
        "pushed": pushed,
    }))
    .into_response()
}

/// Forward the report to mozaiklabs.fr. Gated on `community_sync_enabled`:
/// reporting is local-first and we never phone home for users who opted out.
async fn push_report_to_community(
    state: &AppState,
    entity: &str,
    body: &ReportBody,
    reason: &str,
) -> bool {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let enabled = settings
        .get("community_sync_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1");
    if !enabled {
        return false;
    }
    let Some(instance_id) = settings.get("instance_id").ok().flatten() else {
        return false;
    };
    let base_url = settings
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .unwrap_or_else(|| "https://mozaiklabs.fr".into());

    let submission = tune_core::cloud::community::ReportSubmission {
        entity,
        mbid: body.mbid.as_deref(),
        field: body.field.as_deref(),
        value: body.value.as_deref(),
        reason,
        comment: body.comment.as_deref(),
    };
    match tune_core::cloud::community::submit_report(&base_url, &instance_id, &submission).await {
        Ok(()) => true,
        Err(e) => {
            // Never fail the user's action on a cloud hiccup: the row stays
            // unpushed and is retried by the next community sync cycle.
            warn!(entity, error = %e, "metadata_report_push_failed");
            false
        }
    }
}

/// GET /api/v1/library/reports
pub(super) async fn list_reports(
    State(state): State<AppState>,
    Query(q): Query<ReportListQuery>,
) -> Json<Value> {
    let repo = MetadataReportRepo::with_backend(state.backend.clone());
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let reports = repo.list(limit).unwrap_or_default();
    Json(json!({
        "reports": reports
            .into_iter()
            .map(|r| json!({
                "id": r.id,
                "entity": r.entity,
                "entity_id": r.entity_id,
                "mbid": r.mbid,
                "field": r.field,
                "value": r.value,
                "reason": r.reason,
                "comment": r.comment,
                "created_at": r.created_at,
                "pushed_at": r.pushed_at,
            }))
            .collect::<Vec<_>>(),
    }))
}
