//! `GET /library/query?q=…&limit=…&offset=…` — la requête avancée de la
//! Bibliothèque (#7872, seconde partie de #7848).
//!
//! Le texte est analysé par `tune_core::library::requete::analyser`, puis
//! compilé en condition SQL paramétrée (aucune valeur tapée n'entre dans le
//! SQL). La route rend les pistes qui y répondent, page par page, sous la
//! même forme que `GET /library/tracks` (`items`, `total`, `limit`,
//! `offset`) et avec la même charge par piste (`tracks::joindre_dr_par_piste`,
//! le seam de toutes les listes de pistes).
//!
//! Une requête mal formée rend **400** avec `{position, attendu, message}`
//! (position en caractères, comptée à partir de 0), plus `error` et `code`
//! comme toute erreur de l'API. Jamais une 500 pour une faute de frappe.
//!
//! Droits : ceux de `GET /library/search`. Même routeur, même extracteur de
//! profil actif ; le profil sert aux champs `rating` et `loved`.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::requete::{self, ErreurDeRequete, LIMITE_MAX};

/// La page par défaut, celle de la liste des pistes.
const LIMITE_PAR_DEFAUT: i64 = 50;

#[derive(Deserialize)]
pub(super) struct RequeteAvancee {
    /// Absente, elle vaut une requête vide, refusée en 400 avec sa position
    /// comme toute autre requête mal formée.
    #[serde(default)]
    pub q: String,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Le corps 400 d'une requête mal formée. Les collections
/// intelligentes (`tune-smart-http`) rendent la même forme.
fn refus_de_requete(e: &ErreurDeRequete) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": e.message,
            "code": "requete_invalide",
            "position": e.position,
            "attendu": e.attendu,
            "message": e.message,
        })),
    )
        .into_response()
}

/// `GET /library/query`.
pub(super) async fn requete_avancee(
    State(state): State<AppState>,
    profile: crate::routes::active_profile::ActiveProfile,
    Query(params): Query<RequeteAvancee>,
) -> Response {
    let arbre = match requete::analyser(&params.q) {
        Ok(arbre) => arbre,
        Err(e) => return refus_de_requete(&e),
    };
    let limite = params
        .limit
        .unwrap_or(LIMITE_PAR_DEFAUT)
        .clamp(0, LIMITE_MAX);
    let decalage = params.offset.unwrap_or(0).max(0);
    let profil = profile.id();
    // Lectures synchrones : hors de l'exécuteur, comme `/library/search`
    // (fil 1936) — un compte sur toute la bibliothèque ne doit pas tenir le
    // fil qui sert le flux.
    let travail = tokio::task::spawn_blocking(move || {
        pistes_de_la_requete(&state, &arbre, profil, limite, decalage)
    })
    .await;
    match travail {
        Ok(Ok(corps)) => Json(corps).into_response(),
        Ok(Err(e)) => AppError::internal(e).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "library_query_tache_bloquante_perdue");
            AppError::internal("library_query_interrompue").into_response()
        }
    }
}

/// Le corps de la réponse, exécuté HORS de l'exécuteur async.
fn pistes_de_la_requete(
    state: &AppState,
    arbre: &requete::Expr,
    profil: i64,
    limite: i64,
    decalage: i64,
) -> Result<Value, String> {
    let db = &*state.backend;
    let total = requete::nombre_de_pistes(db, arbre, profil)?;
    let ids = requete::pistes_correspondantes(db, arbre, profil, limite, decalage)?;
    let pistes = TrackRepo::with_backend(state.backend.clone())
        .get_multiple(&ids)
        .map_err(|e| e.to_string())?;
    let rendues = pistes.len() as i64;
    let items = super::tracks::joindre_dr_par_piste(state, profil, pistes);
    Ok(json!({
        "items": items,
        "total": total,
        "limit": limite,
        "offset": decalage,
        "has_more": decalage.saturating_add(rendues) < total,
    }))
}
