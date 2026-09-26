//! Greffon natif « circle » (#5018, Tune Circle étape T1) : le cercle.
//!
//! Tune Circle partage avec des proches **invités, et qui ont accepté**. T1 ne
//! partage encore rien : elle construit la relation et sa **révocation**, par
//! laquelle passera tout ce que T2-T5 partageront.
//!
//! ## Architecture
//!
//! ```text
//! client ── /api/v1/ext/circle/… ──▶ routes.rs ──▶ relais.rs ── Bearer ──▶ mozaiklabs /api/v1/circle/…
//!                                                     │
//!                                                     └─ réglages : mozaik_access_token,
//!                                                        mozaik_refresh_token, mozaik_base_url
//! ```
//!
//! * **Le jeton** est celui de la session SSO du serveur
//!   (`tune_core::cloud::sso`, PKCE) : le greffon le relit dans les réglages à
//!   CHAQUE appel, comme `library_sync` et les routes `/cloud/*`, et le
//!   rafraîchit une fois sur un 401 par `MozaikAuth::refresh_token`, comme le
//!   battement de compte. Aucun second jeton n'est créé.
//! * **Aucun stockage local du cercle.** Le cloud porte le droit : chaque
//!   lecture repart vers lui. Une révocation faite par l'autre membre est donc
//!   vraie au prochain `GET /`, sans cache à invalider.
//! * **Journal** : route, statut, durée. Jamais le jeton, jamais un corps
//!   (les invitations envoyées portent l'adresse saisie par l'auteur).

pub mod relais;
pub mod routes;

use std::sync::Arc;

use async_trait::async_trait;
use tune_core::db::backend::DbBackend;
use tune_core::event_bus::TuneEvent;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

/// Ce que l'hôte passe au greffon, explicitement, à sa construction.
///
/// La base seule suffit : le jeton SSO, son jeton de rafraîchissement et
/// l'adresse du cloud y vivent déjà, sous les clés que lisent toutes les
/// fonctions cloud du serveur. L'interface hôte n'a pas eu à grandir.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
}

pub struct CirclePlugin {
    services: HostServices,
}

impl CirclePlugin {
    pub fn new(services: HostServices) -> Self {
        Self { services }
    }
}

#[async_trait]
impl TunePlugin for CirclePlugin {
    fn name(&self) -> &str {
        "circle"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Tune Circle — partage entre proches invités. Gratuit ; l'écoute à distance sera Premium."
    }
    /// Opt-in, comme `cd` : compilé partout, dormant tant qu'on ne l'installe pas.
    fn default_enabled(&self) -> bool {
        false
    }
    /// Au catalogue (#5018, décision de Bertrand du 25/09), comme `cd`
    /// (#4863) : le gestionnaire propose « Installer », puis
    /// `POST /api/v1/plugins/circle/install` et un redémarrage.
    /// Gratuit : absent de `premium_plugins`, aucun contrôle de droit. Seule
    /// l'écoute à distance (étape T4, à venir) sera Premium.
    fn catalogued(&self) -> bool {
        true
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(routes::router(Arc::new(relais::Relais::new(
            self.services.backend.clone(),
        ))));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }

    /// Le greffon n'observe pas le bus : il n'a rien à tenir à jour.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}
