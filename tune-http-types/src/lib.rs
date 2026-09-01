//! Shared HTTP contracts used by the server and its route crates.

pub mod error;
pub mod panne_sql;

pub use error::AppError;

/// The authenticated identity injected into request extensions.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: i64,
    pub role: String,
}

/// The built-in default profile id (seeded by migration 6).
pub const DEFAULT_PROFILE_ID: i64 = 1;

/// The user profile a request acts on.
#[derive(Debug, Clone, Copy)]
pub struct ActiveProfile(pub i64);

impl ActiveProfile {
    pub fn id(&self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::{ActiveProfile, AppError, AuthUser};

    #[test]
    fn shared_identity_contracts_keep_their_values() {
        let user = AuthUser {
            user_id: 42,
            role: "admin".into(),
        };

        assert_eq!(user.clone().user_id, 42);
        assert_eq!(user.role, "admin");
        assert_eq!(ActiveProfile(7).id(), 7);
    }

    #[test]
    fn app_error_keeps_the_http_contract() {
        let error = AppError::bad_request("invalid request");

        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.message, "invalid request");
        assert_eq!(error.code.as_deref(), Some("bad_request"));
    }
}
