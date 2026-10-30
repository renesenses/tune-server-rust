//! « Mon thème » — greffon natif du catalogue (tune-web-client#2527, go de
//! Bertrand du 30/10/2026 pour la rc10).
//!
//! Chaque profil choisit TOUTES les couleurs de l'interface (fonds, surfaces,
//! texte, accent et variantes) ; le thème est rangé sur le serveur, PAR
//! PROFIL, et le profil le retrouve sur tous ses appareils. Le client web le
//! découvre par `GET /api/v1/plugins` : la fiche de ce greffon porte
//! `provides.web_theme` ([`TunePlugin::theme_web`]).
//!
//! # Les routes
//!
//! Montées par l'hôte sous `/api/v1/ext/mon-theme` :
//!
//! | Route | Rôle |
//! |---|---|
//! | `GET /theme` | `{profile_id, actif, theme}` du profil de la requête ; `theme: null` s'il n'en a pas |
//! | `PUT /theme` | `{theme, actif?}` : juge ([`theme::valider`]) puis range ; `actif` vaut `true` par défaut |
//! | `PUT /theme/actif` | `{actif}` : revenir aux palettes standard sans perdre son thème ; 404 sans thème |
//! | `DELETE /theme` | efface le thème du profil |
//!
//! Refus : `422 {"error": "mon_theme.invalide", "champ"}` (la forme) ou
//! `422 {"error": "mon_theme.illisible", "paires": [{texte, fond, ratio,
//! minimum}]}` (WCAG AA). Un code, jamais une phrase : l'écran traduit.
//!
//! # Le profil
//!
//! Celui que juge l'hôte ([`Hote::profil_actif`]) : l'extracteur
//! `ActiveProfile` de toutes les routes, qui n'honore `X-Profile-Id` que pour
//! son titulaire quand l'authentification est active.
//!
//! # Le rangement
//!
//! Un réglage par profil, `mon_theme_profil_<id>`, valeur JSON `{actif,
//! theme}` — comme la localisation par profil de Concerts. Lu et écrit en
//! entier, jamais interrogé : une table n'apporterait rien, et éviterait mal
//! la double migration SQLite / PostgreSQL.
//!
//! Gratuit, pour tout le monde. Opt-in : il dort tant que l'utilisateur ne
//! l'a pas installé depuis le catalogue (Réglages › Extensions).

pub mod theme;

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

/// Le nom du greffon, et donc son préfixe `/api/v1/ext/mon-theme`.
pub const NOM: &str = "mon-theme";
/// La route du thème, relative au préfixe du greffon.
pub const ROUTE_THEME: &str = "/theme";

/// Ce que l'hôte juge pour le greffon.
#[async_trait]
pub trait Hote: Send + Sync {
    /// Le profil de la requête, jugé comme toutes les routes du serveur.
    async fn profil_actif(&self, parts: &mut axum::http::request::Parts) -> i64;
}

/// Ce que l'hôte fournit.
#[derive(Clone)]
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
    pub hote: Arc<dyn Hote>,
}

/// La clé du réglage d'un profil.
pub fn cle(profil: i64) -> String {
    format!("mon_theme_profil_{profil}")
}

#[derive(Clone)]
struct Etat {
    backend: Arc<dyn DbBackend>,
    hote: Arc<dyn Hote>,
}

impl Etat {
    fn settings(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.backend.clone())
    }

    /// Le `{actif, theme}` rangé, s'il est lisible. Un enregistrement abîmé
    /// vaut « pas de thème » : mieux vaut les palettes standard qu'une
    /// interface peinte à moitié.
    fn lire(&self, profil: i64) -> Option<(bool, Value)> {
        let brut = self.settings().get(&cle(profil)).ok().flatten()?;
        let v: Value = serde_json::from_str(&brut).ok()?;
        let t = theme::valider(v.get("theme")?).ok()?;
        Some((v.get("actif").and_then(Value::as_bool).unwrap_or(true), t))
    }

    fn ranger(&self, profil: i64, actif: bool, t: &Value) -> Result<(), String> {
        self.settings().set(
            &cle(profil),
            &json!({"actif": actif, "theme": t}).to_string(),
        )
    }
}

fn reponse(profil: i64, lu: Option<(bool, Value)>) -> Response {
    let (actif, t) = match lu {
        Some((a, t)) => (a, t),
        None => (false, Value::Null),
    };
    Json(json!({"profile_id": profil, "actif": actif, "theme": t})).into_response()
}

fn refus(statut: StatusCode, corps: Value) -> Response {
    (statut, Json(corps)).into_response()
}

fn echec_ecriture(e: String) -> Response {
    tracing::warn!(target: "plugin", erreur = %e, "mon_theme_ecriture_echouee");
    refus(
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": "mon_theme.ecriture"}),
    )
}

/// Le corps JSON, ou un 422 qui nomme `body`.
fn corps_json(corps: &[u8]) -> Result<Value, Response> {
    serde_json::from_slice::<Value>(corps)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(|| {
            refus(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"error": "mon_theme.invalide", "champ": "body"}),
            )
        })
}

async fn profil(etat: &Etat, req: Request) -> (i64, Bytes) {
    let (mut parts, body) = req.into_parts();
    let p = etat.hote.profil_actif(&mut parts).await;
    let octets = axum::body::to_bytes(body, 64 * 1024)
        .await
        .unwrap_or_default();
    (p, octets)
}

async fn lire_theme(State(etat): State<Etat>, req: Request) -> Response {
    let (p, _) = profil(&etat, req).await;
    reponse(p, etat.lire(p))
}

async fn poser_theme(State(etat): State<Etat>, req: Request) -> Response {
    let (p, octets) = profil(&etat, req).await;
    let corps = match corps_json(&octets) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let actif = match corps.get("actif") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return refus(
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"error": "mon_theme.invalide", "champ": "actif"}),
            );
        }
    };
    let t = match theme::valider(corps.get("theme").unwrap_or(&Value::Null)) {
        Ok(t) => t,
        Err(r) => return refus(StatusCode::UNPROCESSABLE_ENTITY, r.corps()),
    };
    if let Err(e) = etat.ranger(p, actif, &t) {
        return echec_ecriture(e);
    }
    tracing::info!(target: "plugin", profile_id = p, actif, "mon_theme_range");
    reponse(p, Some((actif, t)))
}

async fn poser_actif(State(etat): State<Etat>, req: Request) -> Response {
    let (p, octets) = profil(&etat, req).await;
    let corps = match corps_json(&octets) {
        Ok(c) => c,
        Err(r) => return r,
    };
    let Some(actif) = corps.get("actif").and_then(Value::as_bool) else {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({"error": "mon_theme.invalide", "champ": "actif"}),
        );
    };
    let Some((_, t)) = etat.lire(p) else {
        return refus(StatusCode::NOT_FOUND, json!({"error": "mon_theme.aucun"}));
    };
    if let Err(e) = etat.ranger(p, actif, &t) {
        return echec_ecriture(e);
    }
    reponse(p, Some((actif, t)))
}

async fn effacer_theme(State(etat): State<Etat>, req: Request) -> Response {
    let (p, _) = profil(&etat, req).await;
    if let Err(e) = etat.settings().delete(&cle(p)) {
        return echec_ecriture(e);
    }
    reponse(p, None)
}

async fn sans_cache(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Le routeur du greffon, prêt à être monté sous `/api/v1/ext/mon-theme`.
pub fn router(services: HostServices) -> Router<()> {
    let etat = Etat {
        backend: services.backend,
        hote: services.hote,
    };
    Router::new()
        .route(
            ROUTE_THEME,
            get(lire_theme).put(poser_theme).delete(effacer_theme),
        )
        .route("/theme/actif", put(poser_actif))
        .layer(middleware::map_response(sans_cache))
        .with_state(etat)
}

pub struct MonThemePlugin {
    services: HostServices,
}

impl MonThemePlugin {
    pub fn new(services: HostServices) -> Self {
        Self { services }
    }
}

#[async_trait]
impl TunePlugin for MonThemePlugin {
    fn name(&self) -> &str {
        // En littéral : `scripts/plugin-public-catalog.py` le lit tel quel.
        "mon-theme"
    }

    fn display_name(&self) -> &str {
        "Mon thème"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn description(&self) -> &str {
        "Choisir toutes les couleurs de l'interface, contraste WCAG AA vérifié en direct ; rangé par profil, partageable en fichier"
    }

    /// Au catalogue, à installer : on ne repeint pas l'interface de
    /// quelqu'un qui ne l'a pas demandé.
    fn default_enabled(&self) -> bool {
        false
    }

    fn theme_web(&self) -> Option<&str> {
        Some(ROUTE_THEME)
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.services.clone()));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}
