//! #5916 — le dernier profil choisi, gardé par le SERVEUR, par appareil.
//!
//! Fil 2168 (gerard brot, 11/10, Linux) : à chaque fermeture de toutes les
//! fenêtres du navigateur, Tune revient sur le premier profil, et avec lui
//! perd le thème, le crête-mètre, la liste de gauche, le volume en dB. Le
//! profil actif n'existait que dans `localStorage['tune-profile-id']` du
//! client web : un navigateur qui efface les données des sites à la fermeture
//! (navigation privée, réglage de confidentialité) le perdait à chaque fois.
//!
//! Le client reprend désormais ce que dit cette route quand son
//! `localStorage` est vide, et l'informe de chaque changement de profil.
//!
//! # L'identifiant d'appareil : l'adresse IP de l'appelant
//!
//! C'est déjà l'identité qu'utilise la zone navigateur (« Cet ordinateur
//! (192.168.1.42) », `zones::create_zone_handler`). Un identifiant tiré au
//! sort par le client ne servirait à rien ici : il vivrait dans le même
//! stockage que celui qui s'efface. Et aucun cookie n'est posé.
//!
//! Derrière un mandataire (relais nuage, proxy inverse : voir
//! `ENTETES_MANDATAIRE`), le pair de la socket n'est plus l'appareil. Il n'y
//! a alors PAS d'identifiant d'appareil, et seul joue le repli :
//!
//! # Le repli « dernier profil choisi, tous appareils confondus »
//!
//! Chaque choix met aussi à jour une valeur commune. Elle sert quand
//! l'appareil n'a pas d'identifiant, ou n'a encore rien choisi. Sur une
//! installation à un seul appareil, c'est exactement le profil attendu ; sur
//! un foyer à plusieurs appareils, un appareil NEUF s'ouvre sur le dernier
//! profil choisi ailleurs, plutôt que sur le premier de la liste. Choix
//! accepté par Bertrand (12/10) et documenté ici.
//!
//! # Stockage
//!
//! Dans `settings`, sous le préfixe [`PREFIXE`] : aucune table, donc aucune
//! migration SQLite ni PostgreSQL. Ces lignes ne sortent pas par
//! `/system/config` (filtrées dans `system::config::get_config`).

use std::net::{IpAddr, SocketAddr};

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::auth::AuthUser;
use crate::routes::active_profile::header_allowed;
use crate::routes::library::reveler::ENTETES_MANDATAIRE;
use crate::state::AppState;

/// Le préfixe des lignes de `settings` tenues ici.
pub(crate) const PREFIXE: &str = "profil_choisi:";
/// La ligne du repli « tous appareils confondus ».
const CLE_TOUS_APPAREILS: &str = "profil_choisi:dernier";

fn cle_appareil(ip: IpAddr) -> String {
    format!("{PREFIXE}appareil:{ip}")
}

/// L'appareil appelant, ou `None` quand on ne peut pas le reconnaître.
pub(crate) struct Appareil(Option<IpAddr>);

impl<S: Send + Sync> FromRequestParts<S> for Appareil {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        if ENTETES_MANDATAIRE
            .iter()
            .any(|e| parts.headers.contains_key(*e))
        {
            return Ok(Self(None));
        }
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(a)| a.ip().to_canonical()),
        ))
    }
}

fn auth_activee(settings: &SettingsRepo) -> bool {
    settings
        .get("auth_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Un profil retenu, s'il existe encore et que l'appelant peut l'utiliser.
fn profil_valide(
    state: &AppState,
    settings: &SettingsRepo,
    cle: &str,
    caller: Option<&AuthUser>,
) -> Option<i64> {
    let id = settings
        .get(cle)
        .ok()
        .flatten()?
        .trim()
        .parse::<i64>()
        .ok()?;
    if id <= 0 || !header_allowed(auth_activee(settings), caller, id) {
        return None;
    }
    ProfileRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .map(|_| id)
}

/// `GET /profiles/dernier-choisi` — `{"profile_id": 3, "source": "appareil"}`,
/// `"tous_appareils"` pour le repli, ou `{"profile_id": null, "source": null}`.
pub(crate) async fn lire(
    State(state): State<AppState>,
    Appareil(appareil): Appareil,
    caller: Option<AuthUser>,
) -> Json<serde_json::Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Some(ip) = appareil
        && let Some(id) = profil_valide(&state, &settings, &cle_appareil(ip), caller.as_ref())
    {
        return Json(json!({ "profile_id": id, "source": "appareil" }));
    }
    if let Some(id) = profil_valide(&state, &settings, CLE_TOUS_APPAREILS, caller.as_ref()) {
        return Json(json!({ "profile_id": id, "source": "tous_appareils" }));
    }
    Json(json!({ "profile_id": null, "source": null }))
}

#[derive(Deserialize)]
pub(crate) struct Choix {
    profile_id: i64,
}

/// `PUT /profiles/dernier-choisi` `{"profile_id": 3}` — retient le choix pour
/// cet appareil et pour le repli. `204`, `404` si le profil n'existe pas,
/// `403` si, auth activée, l'appelant ne peut pas agir comme ce profil.
pub(crate) async fn retenir(
    State(state): State<AppState>,
    Appareil(appareil): Appareil,
    caller: Option<AuthUser>,
    Json(choix): Json<Choix>,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let id = choix.profile_id;
    if !header_allowed(auth_activee(&settings), caller.as_ref(), id) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match ProfileRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(_)) => {}
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
    let valeur = id.to_string();
    if let Some(ip) = appareil
        && let Err(e) = settings.set(&cle_appareil(ip), &valeur)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    if let Err(e) = settings.set(CLE_TOUS_APPAREILS, &valeur) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
#[path = "profil_par_appareil_tests_5916.rs"]
mod tests;
