//! Un corps JSON *vraiment* optionnel (#4447).
//!
//! `Option<Json<T>>` ne rend `None` QUE si l'en-tête `Content-Type` est
//! absent. S'il est présent — `application/json` — et que le corps est vide,
//! l'extracteur tente quand même la désérialisation, `serde_json` échoue sur
//! « EOF while parsing a value at line 1 column 0 », et le rejet remonte en
//! **400** au client. La lecture d'`Option` suggère l'inverse : c'est ce
//! contresens qui a fait rougir le bouton « Retrouver genres et années » de
//! l'écran Métadonnées, alors que la même route appelée sans en-tête répondait
//! 202.
//!
//! [`CorpsJsonOptionnel`] lit les **octets** et ne désérialise que s'il y en
//! a : l'en-tête ne décide plus de rien. Corps vide (ou blancs seuls) =
//! `None` = exactement le comportement historique « sans corps ». Corps
//! présent mais illisible = 400 franc, comme avant.

use axum::body::Bytes;
use axum::extract::{FromRequest, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde_json::json;

/// Corps JSON optionnel, insensible au `Content-Type`.
///
/// S'emploie en DERNIER argument du gestionnaire (il consomme le corps) :
///
/// ```ignore
/// async fn ma_route(
///     State(state): State<AppState>,
///     CorpsJsonOptionnel(corps): CorpsJsonOptionnel<MonCorps>,
/// ) -> impl IntoResponse { … }
/// ```
pub(crate) struct CorpsJsonOptionnel<T>(pub(crate) Option<T>);

impl<T, S> FromRequest<S> for CorpsJsonOptionnel<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let octets = Bytes::from_request(req, state)
            .await
            .map_err(IntoResponse::into_response)?;
        // Blancs seuls compris : un client qui envoie "\n" n'a pas plus de
        // charge utile qu'un client qui n'envoie rien.
        if octets.iter().all(|o| o.is_ascii_whitespace()) {
            return Ok(Self(None));
        }
        match serde_json::from_slice::<T>(&octets) {
            Ok(valeur) => Ok(Self(Some(valeur))),
            // Un corps PRÉSENT et invalide reste une erreur du client : on ne
            // le transforme pas en « pas de corps », ce qui ferait retomber la
            // route sur son comportement par défaut à l'insu de l'appelant.
            Err(e) => Err((
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "error": "invalid_json_body",
                    "detail": e.to_string(),
                })),
            )
                .into_response()),
        }
    }
}
