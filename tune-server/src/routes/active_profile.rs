use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// The built-in default profile id (seeded by migration 6).
pub const DEFAULT_PROFILE_ID: i64 = 1;

/// The user profile a request acts on.
///
/// Resolution order:
/// 1. `X-Profile-Id` request header — lets a client act on a specific profile
///    without mutating shared server state (true per-client selection).
/// 2. The global `active_profile_id` setting — the pre-existing single-active
///    model that `/profiles/switch`, the orchestrator (history tagging) and the
///    per-profile metadata fields already use. Keeping this as the fallback
///    means every current client keeps working unchanged.
/// 3. `1` — the built-in "Default" profile.
#[derive(Debug, Clone, Copy)]
pub struct ActiveProfile(pub i64);

impl ActiveProfile {
    pub fn id(&self) -> i64 {
        self.0
    }
}

impl FromRequestParts<AppState> for ActiveProfile {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // 1. Explicit per-request override.
        if let Some(id) = parts
            .headers
            .get("X-Profile-Id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|&id| id > 0)
        {
            return Ok(ActiveProfile(id));
        }
        // 2. Global active profile (shared with the rest of the system).
        let id = SettingsRepo::with_backend(state.backend.clone())
            .get("active_profile_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|&id| id > 0)
            .unwrap_or(DEFAULT_PROFILE_ID);
        Ok(ActiveProfile(id))
    }
}
