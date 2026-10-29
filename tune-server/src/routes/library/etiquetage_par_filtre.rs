//! Étiqueter tout le résultat d'une vue filtrée d'Oxygen en un geste
//! (#7128, fil 2465 de r5yk5 : « Genre = Jazz ET Genre = Vocal », puis le tag
//! « Vocal Jazz » sur tout ce qui reste).
//!
//! Deux routes, qui lisent les MÊMES paramètres de facette que
//! `GET /library/tracks` (clés répétées, `all_of`, `collection`…) :
//!
//! - `GET  /library/tag-by-filter/preview?item_type=track&genre=…` →
//!   `{item_type, count, limit, over_limit}` : l'aperçu avant confirmation ;
//! - `POST /library/tag-by-filter?genre=…` avec le corps
//!   `{tag_id, item_type, remove?}` → `{tag_id, item_type, matched, changed}`.
//!
//! `item_type` vaut `track` (les morceaux du résultat) ou `album` (leurs
//! albums). Le serveur désigne l'ensemble lui-même
//! ([`TrackRepo::etiqueter_par_filtre`]) : aucune liste d'identifiants ne
//! voyage. Un filtre vide est refusé (`400`, `code: "empty_filter"`), un
//! résultat au-delà du plafond aussi (`422`, `code: "too_many_items"`).

use axum::Json;
use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tune_core::db::facet_filter::TrackFilter;
use tune_core::db::tag_repo::TagRepo;
use tune_core::db::track_repo::{
    CibleDuFiltre, PLAFOND_ETIQUETAGE_PAR_FILTRE, RefusEtiquetage, TrackRepo,
};

use super::facets::hors_executeur;
use super::query_multi::{track_filter_from_raw, values_of};
use crate::error::AppError;
use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct CorpsEtiquetage {
    tag_id: i64,
    item_type: String,
    #[serde(default)]
    remove: bool,
}

/// Le filtre de la vue, collection résolue comme `GET /library/tracks`.
fn filtre_de_la_vue(state: &AppState, raw: Option<&str>) -> Result<TrackFilter, AppError> {
    let mut filtre = track_filter_from_raw(raw)?;
    if let Some(nom) = values_of(raw, "collection")
        .into_iter()
        .map(|s| s.trim().to_string())
        .find(|s| !s.is_empty())
    {
        let scope = super::facets::resolve_collection(state, &nom);
        filtre.collection_ids = scope.albums;
        filtre.collection_track_ids = scope.tracks;
    }
    Ok(filtre)
}

fn cible(item_type: &str) -> Result<CibleDuFiltre, Response> {
    CibleDuFiltre::depuis_item_type(item_type).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("item_type must be `track` or `album`, got `{item_type}`"),
                "code": "bad_item_type",
            })),
        )
            .into_response()
    })
}

fn refus(r: RefusEtiquetage, item_type: &str) -> Response {
    match r {
        RefusEtiquetage::FiltreVide => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "no active filter: refusing to tag the whole library",
                "code": "empty_filter",
            })),
        )
            .into_response(),
        RefusEtiquetage::TropDObjets { nombre, plafond } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": format!("{nombre} items match, the limit is {plafond}"),
                "code": "too_many_items",
                "item_type": item_type,
                "count": nombre,
                "limit": plafond,
            })),
        )
            .into_response(),
    }
}

fn erreur_interne(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "etiquetage_par_filtre_echec");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
        .into_response()
}

/// `GET /library/tag-by-filter/preview` — combien d'objets le geste toucherait.
pub(super) async fn apercu(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
) -> Result<Response, AppError> {
    let item_type = values_of(raw.as_deref(), "item_type")
        .into_iter()
        .next()
        .unwrap_or_else(|| "track".to_string());
    let c = match cible(&item_type) {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let filtre = filtre_de_la_vue(&state, raw.as_deref())?;
    hors_executeur("tag_by_filter_preview", move || {
        let repo = TrackRepo::with_backend(state.backend.clone());
        match repo.compter_pour_etiquetage(&filtre, c) {
            Ok(Ok(n)) => Json(json!({
                "item_type": c.item_type(),
                "count": n,
                "limit": PLAFOND_ETIQUETAGE_PAR_FILTRE,
                "over_limit": n > PLAFOND_ETIQUETAGE_PAR_FILTRE,
            }))
            .into_response(),
            Ok(Err(r)) => refus(r, c.item_type()),
            Err(e) => erreur_interne(e),
        }
    })
    .await
}

/// `POST /library/tag-by-filter` — pose ou retire l'étiquette sur tout le
/// résultat, en une instruction SQL. `changed` compte les lignes réellement
/// posées (ou retirées) : relancer la pose rend `changed: 0`.
pub(super) async fn appliquer(
    State(state): State<AppState>,
    RawQuery(raw): RawQuery,
    Json(corps): Json<CorpsEtiquetage>,
) -> Result<Response, AppError> {
    let c = match cible(&corps.item_type) {
        Ok(c) => c,
        Err(r) => return Ok(r),
    };
    let filtre = filtre_de_la_vue(&state, raw.as_deref())?;
    hors_executeur("tag_by_filter", move || {
        let tags = TagRepo::with_backend(state.backend.clone());
        match tags.get(corps.tag_id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "tag not found", "code": "tag_not_found" })),
                )
                    .into_response();
            }
            Err(e) => return erreur_interne(e),
        }
        let repo = TrackRepo::with_backend(state.backend.clone());
        match repo.etiqueter_par_filtre(corps.tag_id, &filtre, c, corps.remove) {
            Ok(Ok(bilan)) => Json(json!({
                "tag_id": corps.tag_id,
                "item_type": c.item_type(),
                "removed": corps.remove,
                "matched": bilan.designes,
                "changed": bilan.modifies,
            }))
            .into_response(),
            Ok(Err(r)) => refus(r, c.item_type()),
            Err(e) => erreur_interne(e),
        }
    })
    .await
}
