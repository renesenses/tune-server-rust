use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub struct AppError {
    pub status: StatusCode,
    pub message: String,
    pub code: Option<String>,
}

impl AppError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::warn!(error = %msg, "not_found");
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

    pub fn internal(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        tracing::warn!(error = %msg, "internal_error");
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
