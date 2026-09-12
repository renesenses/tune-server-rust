use std::convert::Infallible;

use axum::extract::{FromRequestParts, OptionalFromRequestParts};
use axum::http::request::Parts;

use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;
pub use tune_http_types::DEFAULT_PROFILE_ID;

use crate::auth::AuthUser;
use crate::state::AppState;

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
///
/// # Convention: header = action identity, query param = view scope
///
/// Use this extractor for **actions** (a favorite, a note, a playlist) — the
/// `X-Profile-Id` header answers "who is doing this", so the write lands on the
/// caller's profile. Do **not** use it to default a **view scope** (dashboards,
/// history stats): those take an explicit `?profile_id=` query param and mean
/// "show me this profile's stats", absence meaning "show the household total".
/// A view handler must therefore ignore the header deliberately — never add a
/// header-if-no-param fallback there, or once the web client sends the header on
/// every request the default silently flips from global to per-profile.
///
/// # Authentication binds the header to the caller (audit item 4, BOLA)
///
/// The header alone is a claim, not proof. When auth is **enabled** the
/// `profiles` table doubles as the user table — rows carry `password_hash_v2`
/// and `is_admin`, and the JWT `sub` *is* the profile id — so honouring an
/// arbitrary `X-Profile-Id` would let any authenticated user act as any other
/// (favourites, playlists, notes written under someone else's identity). See
/// [`header_allowed`] for the rule.
///
/// When auth is **disabled** the server is fully open by choice (trusted LAN,
/// first run) and profiles are a convenience, not a security boundary: the
/// header is honoured exactly as before, so per-device profile selection keeps
/// working unchanged.
pub use tune_http_types::ActiveProfile;

/// Whether a caller may act as the profile named in `X-Profile-Id`.
///
/// Split out as a pure function so the rule is unit-testable: the extractor
/// itself needs a full [`AppState`] (database included) to run.
///
/// - Auth disabled → always allowed; the header is the only identity there is.
/// - `admin` → allowed to target any profile (household administration).
/// - `api-token` → allowed. Its `sub` is a token *name*, not a profile id, so it
///   has no identity of its own to be bound to; it can only be minted by an
///   admin (`POST /auth/token` is admin-gated), so it inherits that trust.
/// - any other authenticated caller → only their own profile.
/// - unauthenticated → refused. The middleware normally rejects these before we
///   get here; if the extractor runs outside that layer we must not let an
///   anonymous caller pick an identity.
fn header_allowed(auth_enabled: bool, caller: Option<&AuthUser>, requested: i64) -> bool {
    if !auth_enabled {
        return true;
    }
    match caller {
        Some(u) if u.role == "admin" || u.role == "api-token" => true,
        Some(u) => u.user_id == requested,
        None => false,
    }
}

impl FromRequestParts<AppState> for ActiveProfile {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let auth_enabled = settings
            .get("auth_enabled")
            .ok()
            .flatten()
            .map(|v| v == "true")
            .unwrap_or(false);

        // Identify the caller only when auth is on. With auth off there is no
        // credential to read and the lookup would be pure overhead on every
        // request.
        let caller = if auth_enabled {
            <AuthUser as OptionalFromRequestParts<AppState>>::from_request_parts(parts, state)
                .await
                .unwrap_or(None)
        } else {
            None
        };

        // 1. Explicit per-request override — honoured only if the caller is
        //    entitled to it (see `header_allowed`) and the profile still
        //    exists. The web client persists the id in
        //    `localStorage['tune-profile-id']`; after the profile is deleted on
        //    another device the stale id would otherwise silently scope every
        //    favorite/playlist/note under a phantom profile (there is no FK to
        //    stop it). On a miss we fall through.
        //
        //    A header the caller may not use is *ignored*, not rejected: this
        //    extractor is `Infallible` by design, and every handler taking it
        //    would otherwise need a 403 path. Falling back to the caller's own
        //    profile is the safe reading of "who is doing this".
        if let Some(id) = parts
            .headers
            .get("X-Profile-Id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|&id| id > 0)
        {
            if header_allowed(auth_enabled, caller.as_ref(), id) {
                let exists = ProfileRepo::with_backend(state.backend.clone())
                    .get(id)
                    .ok()
                    .flatten()
                    .is_some();
                if exists {
                    return Ok(ActiveProfile(id));
                }
            } else {
                tracing::debug!(
                    requested = id,
                    caller = caller.as_ref().map(|c| c.user_id).unwrap_or(0),
                    "active_profile_header_rejected"
                );
            }
        }

        // 2. An authenticated caller acts as themselves. Deliberately ahead of
        //    the global setting: once we know who is calling, the shared
        //    `active_profile_id` (which any other device can flip) must not
        //    decide what their writes are tagged with.
        if let Some(id) = caller.as_ref().map(|c| c.user_id).filter(|&id| id > 0) {
            return Ok(ActiveProfile(id));
        }

        // 3. Global active profile (shared with the rest of the system).
        let id = settings
            .get("active_profile_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|&id| id > 0)
            .unwrap_or(DEFAULT_PROFILE_ID);
        Ok(ActiveProfile(id))
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthUser, header_allowed};

    fn user(id: i64) -> AuthUser {
        AuthUser {
            user_id: id,
            role: "user".into(),
        }
    }

    fn admin(id: i64) -> AuthUser {
        AuthUser {
            user_id: id,
            role: "admin".into(),
        }
    }

    /// Auth off = trusted LAN by choice. The header is the only identity there
    /// is, so per-device profile selection must keep working exactly as before
    /// — including for a caller we know nothing about.
    #[test]
    fn header_is_honoured_when_auth_is_disabled() {
        assert!(header_allowed(false, None, 7));
        assert!(header_allowed(false, Some(&user(2)), 7));
    }

    /// The BOLA itself: profile 2 must not be able to act as profile 7.
    #[test]
    fn other_profiles_are_refused_when_auth_is_enabled() {
        assert!(!header_allowed(true, Some(&user(2)), 7));
    }

    #[test]
    fn own_profile_is_allowed_when_auth_is_enabled() {
        assert!(header_allowed(true, Some(&user(7)), 7));
    }

    /// Household administration — an admin legitimately acts for another
    /// profile (fixing someone's playlist, seeding favourites).
    #[test]
    fn admin_may_act_for_any_profile() {
        assert!(header_allowed(true, Some(&admin(1)), 7));
    }

    /// An api-token's `sub` is a token name, not a profile id, so `user_id`
    /// parses to 0 and it has no identity to be bound to. It is admin-minted
    /// (`POST /auth/token` is admin-gated), so it inherits that trust — a
    /// plain `user_id == requested` rule would lock every API token out of
    /// every profile.
    #[test]
    fn api_token_is_not_bound_to_a_profile() {
        let token = AuthUser {
            user_id: 0,
            role: "api-token".into(),
        };
        assert!(header_allowed(true, Some(&token), 7));
    }

    /// The middleware normally 401s these first. If the extractor ever runs
    /// outside that layer, an anonymous caller must not get to pick an
    /// identity by sending a header.
    #[test]
    fn anonymous_caller_may_not_pick_a_profile() {
        assert!(!header_allowed(true, None, 7));
    }
}
