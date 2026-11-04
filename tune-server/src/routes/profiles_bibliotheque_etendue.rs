//! #7780 — réglage « Bibliothèque élargie aux favoris des services ».
//!
//! Activé, la Bibliothèque du client montre aussi les albums, artistes et
//! pistes mis en favori chez les services connectés (Qobuz, TIDAL…), et le
//! filtre Source permet d'en choisir un. Il remplace le bouton
//! « Bibliothèque + favoris » qui vivait sur l'appareil (`localStorage`) et ne
//! valait que pour les albums.
//!
//! Le réglage est tenu **par profil**, comme les favoris de service eux-mêmes,
//! sous la clé `profile_{id}_bibliotheque_etendue`. **Une clé absente vaut
//! `true`** : c'est le défaut décidé sur l'issue.
//!
//! La route des favoris de service (`/favorites/streaming`) sert aussi l'écran
//! Favoris : elle n'est PAS conditionnée au réglage en elle-même. Seul l'appel
//! que fait la Bibliothèque, marqué `?pour=bibliotheque`, rend une liste vide
//! quand le réglage est désactivé.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::settings_repo::SettingsRepo;

use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// La valeur de `?pour=` qui désigne l'appel de la Bibliothèque.
pub(super) const POUR_LA_BIBLIOTHEQUE: &str = "bibliotheque";

fn cle(profil: i64) -> String {
    format!("profile_{profil}_bibliotheque_etendue")
}

/// Le réglage du profil ; activé tant que rien n'a été écrit.
pub(super) fn est_activee(state: &AppState, profil: i64) -> bool {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match settings.get(&cle(profil)) {
        Ok(Some(v)) => v.trim() != "false",
        Ok(None) => true,
        Err(e) => {
            tracing::warn!(profile_id = profil, erreur = %e, "bibliotheque_etendue_illisible");
            true
        }
    }
}

#[derive(Deserialize)]
pub(super) struct Reglage {
    enabled: bool,
}

pub(super) async fn lire(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
) -> Response {
    if let Err(r) = super::profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    Json(json!({ "enabled": est_activee(&state, id) })).into_response()
}

pub(super) async fn ecrire(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    profil: ActiveProfile,
    Json(corps): Json<Reglage>,
) -> Response {
    if let Err(r) = super::profil_du_chemin_ou_404(id, profil) {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let valeur = if corps.enabled { "true" } else { "false" };
    match settings.set(&cle(id), valeur) {
        Ok(()) => Json(json!({ "enabled": corps.enabled })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response(),
    }
}

#[cfg(test)]
#[path = "profiles_bibliotheque_etendue_tests_7780.rs"]
mod tests_7780;
