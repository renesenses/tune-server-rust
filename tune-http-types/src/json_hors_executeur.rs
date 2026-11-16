//! Les gros corps JSON ne doivent pas retenir les fils qui servent le flux audio.
use crate::AppError;
use axum::{
    Json,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// Encode ET libère le modèle sur le pool bloquant (#8555).
/// Construire `Json(corps)` hors de l'exécuteur ne suffit pas : Axum encode
/// seulement dans `into_response`. Le statut et le corps d'une erreur de
/// sérialisation restent ceux d'Axum ; une tâche perdue rend une erreur 500.
pub async fn json_hors_executeur<T>(corps: T) -> Result<Response, AppError>
where
    T: Serialize + Send + 'static,
{
    tokio::task::spawn_blocking(move || Json(corps).into_response())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "serialisation_json_tache_bloquante_perdue");
            AppError::internal("Sérialisation de la réponse interrompue")
        })
}
