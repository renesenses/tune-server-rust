//! « Sortie AES67 » (#7336) — greffon natif, Premium.
//!
//! Diffuse une zone Tune en flux AES67 : RTP, PCM L24 gros-boutiste, 48 kHz,
//! paquets de 1 ms, multicast 239.x.y.z, annoncé par SAP/SDP. Chaque flux
//! déclaré par l'utilisateur devient une sortie (et donc une zone) par
//! `register_output_provider`.
//!
//! Étape E1 de l'étude (coordination/etude-aes67-7336.md) :
//! - horodatage sur l'HORLOGE SYSTÈME, sans PTP. Le suiveur PTP (`statime`,
//!   étape E2) se branchera derrière [`horloge::SourceHorloge`] ;
//! - lecture, pause, reprise, arrêt ; ni saut ni enchaînement sans blanc
//!   (étape E3) ;
//! - volume logiciel facultatif, éteint par défaut ;
//! - aucun écran : le greffon est HORS CATALOGUE (`catalogued() == false`)
//!   tant que le client web ne le consomme pas.
//!
//! Routes, sous `/api/v1/ext/aes67` :
//! - `GET  /`     → droit Premium, étape, flux déclarés ;
//! - `PUT  /flux` → remplace la liste des flux (402 sans Premium).

pub mod banc;
pub mod cadenceur;
pub mod config;
pub mod droit;
pub mod fournisseur;
pub mod horloge;
pub mod l24;
pub mod paquetisation;
pub mod producteur;
pub mod reseau;
pub mod rtp;
pub mod sap;
pub mod sdp;
pub mod sortie;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde_json::json;

use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::license::Feature;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

use crate::config::FluxAes67;
use crate::droit::{DroitDeLaLicence, DroitPremium};
use crate::fournisseur::FournisseurAes67;
use crate::horloge::{HorlogeSysteme, SourceHorloge};
use crate::sortie::ReglagesEmission;

/// Le nom du greffon, donc le préfixe de ses réglages (`plugin_aes67_*`) et
/// de ses routes (`/api/v1/ext/aes67`).
pub const NOM: &str = "aes67";
/// Réglage où la liste des flux est gardée (JSON), via `PluginContext`.
pub const CLE_DES_FLUX: &str = "flux";
const URL_OFFRE: &str = "https://mozaiklabs.fr/pricing";

pub struct Aes67Plugin;

impl Aes67Plugin {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Aes67Plugin {
    fn default() -> Self {
        Self::new()
    }
}

/// Relit la liste des flux enregistrée ; illisible ou absente : vide.
pub fn lire_les_flux(texte: Option<&str>) -> Vec<FluxAes67> {
    texte
        .and_then(|t| serde_json::from_str::<Vec<FluxAes67>>(t).ok())
        .unwrap_or_default()
}

#[async_trait]
impl TunePlugin for Aes67Plugin {
    fn name(&self) -> &str {
        NOM
    }
    fn display_name(&self) -> &str {
        "Sortie AES67"
    }
    fn version(&self) -> &str {
        tune_core::version()
    }
    fn description(&self) -> &str {
        "Sortie AES67 (Premium) : diffuse une zone en flux RTP multicast vers des récepteurs AES67"
    }
    /// Opt-in : on l'installe quand on a un récepteur AES67 sur un réseau
    /// filaire qui gère le multicast.
    fn default_enabled(&self) -> bool {
        false
    }
    /// Aucun écran ne le consomme encore (étape E3).
    fn catalogued(&self) -> bool {
        false
    }
    fn required_feature(&self) -> Option<Feature> {
        Some(Feature::Aes67)
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let flux = Arc::new(RwLock::new(lire_les_flux(
            ctx.get_config(CLE_DES_FLUX).as_deref(),
        )));
        let droit: Arc<dyn DroitPremium> = Arc::new(DroitDeLaLicence(ctx.license.clone()));
        let horloge: Arc<dyn SourceHorloge> = Arc::new(HorlogeSysteme::new());
        ctx.register_output_provider(Arc::new(FournisseurAes67::new(
            flux.clone(),
            droit.clone(),
            horloge,
            ReglagesEmission::default(),
        )));
        ctx.register_router(router(Etat {
            flux,
            droit,
            db: ctx.db(),
        }));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }

    async fn on_event(&mut self, _event: &TuneEvent) {}
}

#[derive(Clone)]
struct Etat {
    flux: Arc<RwLock<Vec<FluxAes67>>>,
    droit: Arc<dyn DroitPremium>,
    db: Option<Arc<dyn DbBackend>>,
}

fn router(etat: Etat) -> Router<()> {
    Router::new()
        .route("/", get(etat_du_greffon))
        .route("/flux", put(enregistrer_les_flux))
        .with_state(etat)
}

/// Le refus Premium, même forme que `premium_guard` du serveur.
fn refus_premium() -> Response {
    (
        StatusCode::PAYMENT_REQUIRED,
        Json(json!({
            "error": "premium_required",
            "code": Feature::Aes67.code(),
            "feature": Feature::Aes67.display_name(),
            "upgrade_url": URL_OFFRE,
        })),
    )
        .into_response()
}

async fn etat_du_greffon(State(etat): State<Etat>) -> Response {
    let flux = etat.flux.read().unwrap_or_else(|p| p.into_inner()).clone();
    Json(json!({
        "premium": etat.droit.autorise().await,
        "etape": "E1",
        "reference_horloge": "local",
        "ptp": false,
        "flux": flux,
    }))
    .into_response()
}

async fn enregistrer_les_flux(
    State(etat): State<Etat>,
    Json(flux): Json<Vec<FluxAes67>>,
) -> Response {
    if !etat.droit.autorise().await {
        return refus_premium();
    }
    if let Err(e) = config::valider_la_liste(&flux) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response();
    }
    let Some(db) = etat.db.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "base indisponible" })),
        )
            .into_response();
    };
    let texte = match serde_json::to_string(&flux) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let cle = format!("plugin_{NOM}_{CLE_DES_FLUX}");
    if let Err(e) =
        tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone()).set(&cle, &texte)
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    *etat.flux.write().unwrap_or_else(|p| p.into_inner()) = flux.clone();
    Json(json!({ "flux": flux })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    use tune_core::license::LicenseManager;

    fn base() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn etat(backend: &Arc<dyn DbBackend>, licence: Arc<LicenseManager>) -> Etat {
        Etat {
            flux: Arc::new(RwLock::new(Vec::new())),
            droit: Arc::new(DroitDeLaLicence(Some(licence))),
            db: Some(backend.clone()),
        }
    }

    fn put_flux(corps: serde_json::Value) -> Request<Body> {
        Request::put("/flux")
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap()
    }

    async fn json_de(rep: Response) -> serde_json::Value {
        serde_json::from_slice(
            &axum::body::to_bytes(rep.into_body(), 1 << 20)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn le_greffon_est_premium_opt_in_et_hors_catalogue() {
        let p = Aes67Plugin::new();
        assert_eq!(p.name(), "aes67");
        assert_eq!(p.display_name(), "Sortie AES67");
        assert_eq!(p.required_feature(), Some(Feature::Aes67));
        assert!(!p.default_enabled());
        assert!(!p.catalogued());
        assert!(!p.description().to_lowercase().contains("dante"));
    }

    #[tokio::test]
    async fn sans_premium_l_enregistrement_est_refuse_en_402() {
        let b = base();
        let e = etat(&b, Arc::new(LicenseManager::new(b.clone())));
        let rep = router(e.clone())
            .oneshot(put_flux(
                json!([{ "id": "salon", "nom": "Salon", "adresse": "239.69.1.10" }]),
            ))
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(json_de(rep).await["code"], "aes67_output");
        assert!(e.flux.read().unwrap().is_empty());
        let rep = router(e)
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(json_de(rep).await["premium"], false);
    }

    #[tokio::test]
    async fn avec_premium_les_flux_valides_sont_gardes_et_relus() {
        let b = base();
        let licence = Arc::new(LicenseManager::new(b.clone()));
        licence.set_account_premium(true, None).await;
        let e = etat(&b, licence);
        let rep = router(e.clone())
            .oneshot(put_flux(
                json!([{ "id": "salon", "nom": "Salon", "adresse": "192.168.1.2" }]),
            ))
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::BAD_REQUEST, "unicast refusé");
        let rep = router(e.clone())
            .oneshot(put_flux(
                json!([{ "id": "salon", "nom": "Salon", "adresse": "239.69.1.10" }]),
            ))
            .await
            .unwrap();
        assert_eq!(rep.status(), StatusCode::OK);
        assert_eq!(e.flux.read().unwrap().len(), 1);
        let garde = tune_core::db::settings_repo::SettingsRepo::with_backend(b.clone())
            .get("plugin_aes67_flux")
            .unwrap();
        let relus = lire_les_flux(garde.as_deref());
        assert_eq!(relus.len(), 1);
        assert_eq!(relus[0].device_id(), "aes67:salon");
        assert_eq!(relus[0].port, 5004);
    }

    #[test]
    fn une_liste_illisible_vaut_une_liste_vide() {
        assert!(lire_les_flux(None).is_empty());
        assert!(lire_les_flux(Some("pas du json")).is_empty());
    }
}
