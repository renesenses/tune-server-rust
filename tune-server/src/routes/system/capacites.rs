//! `GET /api/v1/system/capabilities` — ce que CE serveur sait faire (#8189).
//!
//! Les écrans des télécommandes devinaient jusqu'ici par un 404 (« la route
//! n'existe pas, donc la fonction non plus »), ce qui ne distingue ni un
//! serveur ancien d'une panne, ni un serveur de bureau d'un serveur embarqué
//! dans un téléphone. Cette route le dit.
//!
//! Le contrat est additif : un client lit les clés qu'il connaît et ignore les
//! autres ; une clé absente vaut « non ».

use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use crate::state::AppState;

/// Les types de sources distantes que le moteur sait lire sans montage
/// système (WebDAV, SMB, NFS : étapes 3, 4 et 6 de #8189) : ceux dont une
/// fabrique est déclarée (`sources_reseau::installer_la_fabrique`). Vide
/// tant qu'aucun protocole réel n'est branché.
pub(crate) fn types_de_sources_distantes() -> Vec<String> {
    tune_core::sources_reseau::types_pris_en_charge()
}

pub(crate) fn capacites_payload(mobile: bool) -> Value {
    json!({
        "version": tune_core::version(),
        "mobile": mobile,
        "sources_reseau": {
            // L'écran « Sources du réseau » : serveurs UPnP/DLNA
            // (`/network/media-servers*`) et autres serveurs Tune (`/peers`).
            "disponible": true,
            "serveurs_upnp": true,
            "serveurs_tune": true,
            // Montage de partages par le système (`/network/mounts`,
            // `/network/smb/*`) : jamais sur un téléphone.
            "montage_systeme": !mobile,
            // Sources lues par le moteur lui-même, sans montage.
            "types": types_de_sources_distantes(),
        },
        "decouverte_reseau": {
            // Faux tant que l'application a différé la découverte et que
            // personne ne l'a encore demandée (iOS).
            "active": tune_core::discovery::porte_multicast::est_ouverte(),
        },
    })
}

pub(super) async fn capacites(State(state): State<AppState>) -> Json<Value> {
    Json(capacites_payload(state.config.mobile))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn bureau_et_mobile_ne_disent_pas_la_meme_chose() {
        let bureau = capacites_payload(false);
        let mobile = capacites_payload(true);
        assert_eq!(bureau["sources_reseau"]["disponible"], true);
        assert_eq!(mobile["sources_reseau"]["disponible"], true);
        assert_eq!(bureau["sources_reseau"]["montage_systeme"], true);
        assert_eq!(mobile["sources_reseau"]["montage_systeme"], false);
        assert_eq!(mobile["mobile"], true);
        assert!(mobile["sources_reseau"]["types"].is_array());
    }

    #[tokio::test]
    async fn la_route_est_servie_par_le_routeur() {
        let config = crate::config::TuneConfig {
            mobile: true,
            ..Default::default()
        };
        let state = AppState::new(":memory:", 0, config).unwrap();
        let app = crate::routes::router(state);
        let rep = app
            .oneshot(
                Request::get("/api/v1/system/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::OK);
        let corps = axum::body::to_bytes(rep.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&corps).unwrap();
        assert_eq!(v["sources_reseau"]["disponible"], true);
        assert_eq!(v["sources_reseau"]["montage_systeme"], false);
    }
}
