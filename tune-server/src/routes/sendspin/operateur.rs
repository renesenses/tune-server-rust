//! Commandes de confiance soumises a la politique administrateur de Tune.
use super::ContexteSendspin;
use super::sessions::{Commande, ErreurCommande};
use crate::auth::RequireAdmin;
use crate::state::AppState;
use axum::extract::{DefaultBodyLimit, Path};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::sendspin::appairage::MethodeAppairage as M;
use tune_core::sendspin::jeton::{JetonPsk, lire_code};
use tune_core::sendspin::pake::FormatCode;

pub(crate) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/sendspin/{client_id}/pair",
            get(etat).post(demarrer).delete(annuler),
        )
        .route("/sendspin/{client_id}/pair/code", post(code))
        .route("/sendspin/{client_id}/credentials", delete(revoquer))
        .layer(DefaultBodyLimit::max(16 * 1024))
}
type Erreur = (StatusCode, Json<Value>);
fn erreur(e: ErreurCommande) -> Erreur {
    let (statut, message) = match e {
        ErreurCommande::Absent => (StatusCode::NOT_FOUND, "client non connecte"),
        ErreurCommande::Sature => (StatusCode::TOO_MANY_REQUESTS, "file de commandes pleine"),
        ErreurCommande::Invalide(s) => (StatusCode::BAD_REQUEST, s),
        ErreurCommande::Conflit(s) => (StatusCode::CONFLICT, s),
        ErreurCommande::Indisponible => (StatusCode::SERVICE_UNAVAILABLE, "connexion indisponible"),
    };
    (statut, Json(json!({"error":message})))
}
fn identite(id: &str) -> Result<(), Erreur> {
    tune_core::sendspin::identite::cle_publique_du_pair(id)
        .map(|_| ())
        .map_err(|_| erreur(ErreurCommande::Invalide("identite invalide")))
}
async fn etat(
    _: RequireAdmin,
    Path(id): Path<String>,
    Extension(c): Extension<ContexteSendspin>,
) -> Result<Json<Value>, Erreur> {
    identite(&id)?;
    c.sessions()
        .etat(&id)
        .map(Json)
        .ok_or_else(|| erreur(ErreurCommande::Absent))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Debut {
    method: String,
    format: Option<String>,
    token: Option<String>,
    code: Option<String>,
}
async fn demarrer(
    _: RequireAdmin,
    Path(id): Path<String>,
    Extension(c): Extension<ContexteSendspin>,
    Json(d): Json<Debut>,
) -> Result<Json<Value>, Erreur> {
    identite(&id)?;
    let mut psk = None;
    let methode = match (d.method.as_str(), d.format.as_deref()) {
        ("pairing_psk", None) if d.code.is_none() => {
            psk = Some(
                JetonPsk::lire(
                    d.token
                        .as_deref()
                        .ok_or_else(|| erreur(ErreurCommande::Invalide("jeton requis")))?,
                )
                .and_then(|j| j.pour_pair(&id))
                .map_err(|_| {
                    erreur(ErreurCommande::Invalide(
                        "jeton incompatible avec ce client",
                    ))
                })?,
            );
            M::Psk
        }
        ("static_pairing_code", None) if d.token.is_none() => {
            if let Some(code) = d.code.as_deref() {
                lire_code(code, FormatCode::Statique)
                    .map_err(|_| erreur(ErreurCommande::Invalide("code statique invalide")))?;
            }
            M::Statique
        }
        ("dynamic_pairing_code", Some("digits")) if d.token.is_none() && d.code.is_none() => {
            M::Dynamique
        }
        ("dynamic_pairing_code", Some("qr_code")) if d.token.is_none() && d.code.is_none() => M::Qr,
        _ => {
            return Err(erreur(ErreurCommande::Invalide(
                "methode ou format invalide",
            )));
        }
    };
    c.sessions()
        .commander(
            &id,
            Commande::Demarrer {
                methode,
                psk,
                code: d.code,
            },
        )
        .await
        .map(Json)
        .map_err(erreur)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Saisie {
    code: String,
}
async fn code(
    _: RequireAdmin,
    Path(id): Path<String>,
    Extension(c): Extension<ContexteSendspin>,
    Json(s): Json<Saisie>,
) -> Result<Json<Value>, Erreur> {
    identite(&id)?;
    c.sessions()
        .commander(&id, Commande::Saisir(s.code))
        .await
        .map(Json)
        .map_err(erreur)
}
async fn annuler(
    _: RequireAdmin,
    Path(id): Path<String>,
    Extension(c): Extension<ContexteSendspin>,
) -> Result<Json<Value>, Erreur> {
    identite(&id)?;
    c.sessions()
        .commander(&id, Commande::Annuler)
        .await
        .map(Json)
        .map_err(erreur)
}
async fn revoquer(
    _: RequireAdmin,
    Path(id): Path<String>,
    Extension(c): Extension<ContexteSendspin>,
) -> Result<Json<Value>, Erreur> {
    identite(&id)?;
    c.revoquer(&id)
        .await
        .map(|retire| Json(json!({"revoked":retire})))
        .map_err(|_| erreur(ErreurCommande::Indisponible))
}
