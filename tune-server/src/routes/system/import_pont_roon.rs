//! `POST /system/import/roon` quand le fichier téléversé est un EXPORT DU
//! MOISSONNEUR (`{"source":"roon","artistes":[…]}`) — phase 2 du pont Roon
//! (#3914). Le CSV de l'interface Roon garde son chemin historique dans
//! `import.rs` ; ici on n'importe pas des pistes, on ENRICHIT celles qu'on a.
//!
//! L'écriture vit dans `tune_core::library::pont_roon_import` : l'extension
//! « Pont Roon » (greffon `tune-pont-roon`, qui importe aussi les IMAGES d'une
//! archive) applique exactement le même corps.
//!
//! 🔴 Premium (`Feature::PontRoon`, Bertrand 16/09/2026 : « en plugin
//! PREMIUM ») : cette porte-ci aussi, sans quoi le JSON seul contournerait la
//! décision par l'écran d'import historique.

use axum::Json;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::{info, warn};
use tune_core::library::pont_roon::ExportRoon;
use tune_core::library::pont_roon_import::appliquer;
pub(crate) use tune_core::library::pont_roon_import::est_un_export_du_pont;

use crate::state::AppState;

/// La réponse HTTP : le rapport, en aperçu ou après écriture. 402 sans Premium.
pub(crate) async fn repondre(
    state: &AppState,
    headers: &HeaderMap,
    texte: &str,
    apercu: bool,
) -> Response {
    if let Err(refus) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::PontRoon,
        headers,
    )
    .await
    {
        return refus;
    }
    let export = match ExportRoon::lire(texte) {
        Ok(e) => e,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({"error": "export_pont_roon_illisible", "detail": e})),
            )
                .into_response();
        }
    };
    let rapport = appliquer(&state.backend, &export, apercu, None);
    if apercu {
        info!(?rapport, "pont_roon_apercu_rendu_sans_ecriture");
    } else {
        info!(?rapport, "pont_roon_importe");
    }
    if rapport.artistes_apparies == 0 {
        warn!(
            artistes = rapport.artistes_total,
            "pont_roon_aucun_artiste_apparie — l'export vient-il de la même bibliothèque ?"
        );
    }
    let mut v = serde_json::to_value(&rapport).unwrap_or_default();
    v["preview"] = json!(apercu);
    v["source"] = json!("roon_pont");
    v["absent_de_l_api"] = json!(export.absent_de_l_api);
    (StatusCode::OK, Json(v)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn etat() -> AppState {
        AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// Sans Premium, la porte JSON refuse aussi : 402, code `pont_roon`.
    #[tokio::test]
    async fn sans_premium_la_porte_json_refuse() {
        let s = etat();
        let rep = repondre(
            &s,
            &HeaderMap::new(),
            r#"{"source":"roon","artistes":[]}"#,
            true,
        )
        .await;
        assert_eq!(rep.status(), StatusCode::PAYMENT_REQUIRED);
        let corps = axum::body::to_bytes(rep.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&corps).unwrap();
        assert_eq!(v["code"], "pont_roon");
    }

    /// 🔴 LA garde du chantier : ce qui vient de Roon reste LOCAL. Le sync
    /// cloud pousse artistes (nom, bio, MBID) et albums — jamais
    /// `track_credits`, jamais `track_metadata`, jamais une image.
    #[test]
    fn le_sync_cloud_ne_pousse_ni_credits_ni_images() {
        const SYNC: &str = include_str!("../../../../tune-core/src/cloud/library_sync.rs");
        let sans = SYNC
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !sans.contains("track_credits"),
            "le sync cloud lit track_credits"
        );
        assert!(
            !sans.contains("track_metadata"),
            "le sync cloud lit track_metadata"
        );
        assert!(
            !sans.contains("image_path"),
            "le sync cloud pousse des images"
        );
    }
}
