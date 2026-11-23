use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

tokio::task_local! {
    static CHEMIN_REQUETE: String;
}

/// Conserve le gabarit de route pendant la requête, sans paramètres ni query.
/// Le contexte est propre à la tâche, y compris lorsque deux requêtes alternent.
pub async fn avec_chemin_requete(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| "route_non_reconnue".to_owned());
    CHEMIN_REQUETE.scope(path, next.run(request)).await
}

pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub code: Option<String>,
}

impl AppError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        if CHEMIN_REQUETE
            .try_with(|path| {
                tracing::warn!(error = %msg, path = %path, "not_found");
            })
            .is_err()
        {
            tracing::warn!(error = %msg, "not_found");
        }
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg,
            code: Some("not_found".into()),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::warn!(error = %msg, "bad_request");
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg,
            code: Some("bad_request".into()),
        }
    }

    /// #9343 — comme [`AppError::not_found`], le journal nomme la route quand
    /// l'erreur naît pendant une requête : « internal_error error=Read-only
    /// file system (os error 30) » seul ne disait pas quel bouton avait échoué.
    pub fn internal(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        if CHEMIN_REQUETE
            .try_with(|path| {
                tracing::warn!(error = %msg, path = %path, "internal_error");
            })
            .is_err()
        {
            tracing::warn!(error = %msg, "internal_error");
        }
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg,
            code: Some("internal_error".into()),
        }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::warn!(error = %msg, "unauthorized");
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: msg,
            code: Some("unauthorized".into()),
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::warn!(error = %msg, "conflict");
        Self {
            status: StatusCode::CONFLICT,
            message: msg,
            code: Some("conflict".into()),
        }
    }

    pub fn service_unavailable(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::warn!(error = %msg, "service_unavailable");
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: msg,
            code: Some("service_unavailable".into()),
        }
    }
}

/// #9343 — le message d'une erreur d'entrée-sortie, qui nomme TOUJOURS
/// l'opération et le chemin. `From<io::Error>` ne connaît ni l'un ni l'autre :
/// tout geste sur un fichier passe par ici plutôt que par `?`.
///
/// Un système de fichiers en lecture seule (EROFS, « os error 30 ») reçoit
/// un message dédié : c'est presque toujours un chemin mal ancré (relatif au
/// répertoire courant `/` d'un service macOS, volume monté en lecture seule),
/// et l'utilisateur doit savoir QUEL dossier refuse l'écriture.
pub fn message_d_entree_sortie(
    operation: &str,
    chemin: &std::path::Path,
    erreur: &std::io::Error,
) -> String {
    if erreur.kind() == std::io::ErrorKind::ReadOnlyFilesystem {
        format!(
            "{operation}: {} is on a read-only file system ({erreur}); \
             Tune cannot write there",
            chemin.display()
        )
    } else {
        format!("{operation}: {}: {erreur}", chemin.display())
    }
}

impl AppError {
    /// Erreur 500 d'entrée-sortie, avec l'opération et le chemin
    /// ([`message_d_entree_sortie`]).
    pub fn io(operation: &str, chemin: &std::path::Path, erreur: &std::io::Error) -> Self {
        Self::internal(message_d_entree_sortie(operation, chemin, erreur))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": self.message,
            "code": self.code,
        });
        (self.status, axum::Json(body)).into_response()
    }
}

impl From<String> for AppError {
    fn from(msg: String) -> Self {
        Self::internal(msg)
    }
}

impl From<&str> for AppError {
    fn from(msg: &str) -> Self {
        Self::internal(msg)
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        Self::internal(format!("database: {e}"))
    }
}

impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::bad_request(format!("json: {e}"))
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::internal(format!("io: {e}"))
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        Self::internal(format!("http: {e}"))
    }
}

#[cfg(test)]
mod chemin_8901_tests;
