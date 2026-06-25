use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tracing::{info, warn};

use tune_core::db::history_repo::HistoryRepo;
use tune_core::license::Feature;
use tune_core::playback::PlayState;
use tune_core::social::{
    NowListeningCard, PublicProfileData, SharingProfile, TopArtist, load_profile,
    render_now_listening_svg, save_profile,
};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/profile", get(get_profile).post(update_profile))
        .route("/now-listening", get(now_listening))
        .route("/public", get(public_profile))
        .route("/share", post(share_now_playing))
        .route("/embed.svg", get(embed_svg))
}

// ---------------------------------------------------------------------------
// GET /social/profile — Premium. Returns current sharing profile settings
// ---------------------------------------------------------------------------

async fn get_profile(State(state): State<AppState>) -> Response {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::SocialSharing).await
    {
        return resp;
    }

    let profile = load_profile(&state.backend);
    Json(json!({
        "profile": profile,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /social/profile — Premium. Update sharing preferences
// ---------------------------------------------------------------------------

async fn update_profile(
    State(state): State<AppState>,
    Json(body): Json<SharingProfile>,
) -> Response {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::SocialSharing).await
    {
        return resp;
    }

    if let Err(e) = save_profile(&state.backend, &body) {
        warn!(error = %e, "social_profile_save_failed");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))).into_response();
    }

    info!(
        display_name = %body.display_name,
        enabled = body.enabled,
        "social_profile_updated"
    );

    Json(json!({
        "ok": true,
        "profile": body,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /social/now-listening — Premium. Returns current NowListeningCard
// ---------------------------------------------------------------------------

async fn now_listening(State(state): State<AppState>) -> Response {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::SocialSharing).await
    {
        return resp;
    }

    let profile = load_profile(&state.backend);
    if !profile.enabled || !profile.share_now_playing {
        return Json(json!({
            "sharing_enabled": false,
            "now_listening": null,
        }))
        .into_response();
    }

    let card = build_now_listening_card(&state).await;

    Json(json!({
        "sharing_enabled": true,
        "now_listening": card,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /social/public — Public (no auth). Returns PublicProfileData
// ---------------------------------------------------------------------------

async fn public_profile(State(state): State<AppState>) -> Response {
    let profile = load_profile(&state.backend);

    if !profile.enabled {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "sharing disabled"})),
        )
            .into_response();
    }

    let card = if profile.share_now_playing {
        build_now_listening_card(&state).await
    } else {
        None
    };

    let top_artists = if profile.share_top_artists {
        build_top_artists(&state)
    } else {
        vec![]
    };

    let total_plays = if profile.share_history {
        HistoryRepo::with_backend(state.backend.clone())
            .count()
            .unwrap_or(0)
    } else {
        0
    };

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let member_since = settings.get("instance_created_at").ok().flatten();

    let data = PublicProfileData {
        display_name: profile.display_name,
        now_listening: card,
        top_artists,
        total_plays,
        member_since,
    };

    Json(json!(data)).into_response()
}

// ---------------------------------------------------------------------------
// POST /social/share — Premium. Push now-playing to mozaiklabs.fr community
// ---------------------------------------------------------------------------

async fn share_now_playing(State(state): State<AppState>) -> Response {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::SocialSharing).await
    {
        return resp;
    }

    let profile = load_profile(&state.backend);
    if !profile.enabled || !profile.share_now_playing {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "sharing is not enabled"})),
        )
            .into_response();
    }

    let card = match build_now_listening_card(&state).await {
        Some(c) => c,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "nothing playing"})),
            )
                .into_response();
        }
    };

    // Push to mozaiklabs.fr community API
    let payload = json!({
        "display_name": profile.display_name,
        "now_listening": card,
    });

    match state
        .http_client
        .post("https://mozaiklabs.fr/api/v1/community/now-listening")
        .json(&payload)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                display_name = %profile.display_name,
                title = %card.title,
                "social_share_pushed"
            );
            Json(json!({
                "ok": true,
                "shared": card,
            }))
            .into_response()
        }
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            warn!(status, body = %body, "social_share_upstream_error");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": "upstream error", "status": status})),
            )
                .into_response()
        }
        Err(e) => {
            warn!(error = %e, "social_share_request_failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("request failed: {e}")})),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// GET /social/embed.svg — Public. Embeddable SVG badge
// ---------------------------------------------------------------------------

async fn embed_svg(State(state): State<AppState>) -> Response {
    let profile = load_profile(&state.backend);

    let card = if profile.enabled && profile.share_now_playing {
        build_now_listening_card(&state).await
    } else {
        None
    };

    let svg = render_now_listening_svg(card.as_ref());

    (
        [(
            axum::http::header::CONTENT_TYPE,
            "image/svg+xml; charset=utf-8",
        )],
        svg,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a NowListeningCard from the currently playing zone (zone 1 default).
async fn build_now_listening_card(state: &AppState) -> Option<NowListeningCard> {
    // Check all zones, return the first one that is actively playing.
    let states = state.playback.all_states().await;
    states
        .iter()
        .find(|z| z.state == PlayState::Playing)
        .and_then(|z| z.now_playing.as_ref())
        .map(NowListeningCard::from_now_playing)
}

/// Build the top artists list from playback history.
fn build_top_artists(state: &AppState) -> Vec<TopArtist> {
    let repo = HistoryRepo::with_backend(state.backend.clone());
    repo.top_artists(10)
        .unwrap_or_default()
        .into_iter()
        .map(|(name, plays)| TopArtist { name, plays })
        .collect()
}
