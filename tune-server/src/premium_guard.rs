use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::info;

use tune_core::license::{Feature, LicenseManager};

/// Check that a premium feature is enabled.  Returns `Ok(())` when the
/// feature is available, or an `Err(Response)` with HTTP 402 and a
/// structured JSON body when it is not.
pub async fn require_premium(license: &LicenseManager, feature: Feature) -> Result<(), Response> {
    if license.check_feature(feature).await {
        Ok(())
    } else {
        info!(feature = feature.display_name(), "premium_feature_blocked");
        Err((
            StatusCode::PAYMENT_REQUIRED,
            axum::Json(json!({
                "error": "premium_required",
                "feature": feature.display_name(),
                "message": format!("{} requires Tune Premium", feature.display_name()),
                "upgrade_url": "https://mozaiklabs.fr/pricing"
            })),
        )
            .into_response())
    }
}
