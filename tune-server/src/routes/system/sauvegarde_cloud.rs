//! Routes de la sauvegarde cloud des personnalisations (#5654,
//! tune-web-client#902). Le métier est dans
//! [`tune_core::cloud::sauvegarde_config`] ; ici, la garde (admin, Premium)
//! et la forme des réponses.
//!
//! Toutes sont réservées à l'administrateur : elles lisent ou réécrivent la
//! configuration entière. Toutes exigent le Premium, sauf `status`, pour que
//! l'écran puisse dire « Premium requis » au lieu d'une erreur.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

use tune_core::cloud::sauvegarde_config as sc;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::auth::RequireAdmin;
use crate::state::AppState;

async fn garde_premium(state: &AppState) -> Result<(), Response> {
    crate::premium_guard::require_premium(&state.license, Feature::CloudConfigBackup).await
}

fn erreur(statut: StatusCode, code: &str) -> Response {
    (statut, Json(json!({ "error": code }))).into_response()
}

/// Une erreur du site, rendue au client sans corps ni adresse.
fn erreur_site(e: sc::ErreurSite) -> Response {
    match e {
        sc::ErreurSite::NonRelie => erreur(StatusCode::PRECONDITION_FAILED, "account_not_linked"),
        sc::ErreurSite::Http { statut, code } => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": code.unwrap_or_else(|| "cloud_error".into()),
                "status": statut,
            })),
        )
            .into_response(),
        sc::ErreurSite::Reseau(_) => erreur(StatusCode::BAD_GATEWAY, "cloud_unreachable"),
        sc::ErreurSite::Illisible => erreur(StatusCode::BAD_GATEWAY, "cloud_bad_response"),
    }
}

// ── GET /system/config-backup/cloud/status ──────────────────────────

pub(super) async fn status(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut v = sc::statut(&settings);
    v["premium"] = json!(
        state
            .license
            .check_feature(Feature::CloudConfigBackup)
            .await
    );
    Json(v).into_response()
}

// ── POST /system/config-backup/cloud/enable ─────────────────────────

#[derive(Deserialize, Default)]
pub(super) struct ActiverCorps {
    #[serde(default)]
    passphrase: Option<String>,
}

/// Active la sauvegarde automatique. Crée la clé à la première activation et
/// rend la clé de secours UNE fois ; ensuite, aucun corps n'est requis.
/// Dépose aussitôt un premier instantané si le compte est relié.
pub(super) async fn enable(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    corps: Option<Json<ActiverCorps>>,
) -> Response {
    if let Err(r) = garde_premium(&state).await {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let passphrase = corps.and_then(|Json(c)| c.passphrase).unwrap_or_default();
    let (key_id, secours) = match sc::cle_locale(&settings) {
        Ok(Some(c)) => (c.key_id, None),
        Ok(None) => {
            if passphrase.chars().count() < sc::PHRASE_MIN {
                return erreur(StatusCode::BAD_REQUEST, "passphrase_too_short");
            }
            match sc::creer_la_cle(&settings, &passphrase) {
                Ok((id, rk)) => (id, Some(rk.into_string())),
                Err(e) => {
                    warn!(error = %e, "sauvegarde_cloud_cle_refusee");
                    return erreur(StatusCode::BAD_REQUEST, "key_setup_failed");
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "sauvegarde_cloud_cle_illisible");
            return erreur(StatusCode::INTERNAL_SERVER_ERROR, "key_unreadable");
        }
    };
    if let Err(e) = sc::activer(&settings, true) {
        warn!(error = %e, "sauvegarde_cloud_activation_echec");
        return erreur(StatusCode::INTERNAL_SERVER_ERROR, "settings_write_failed");
    }
    // Premier instantané tout de suite, sans attendre la passe de fond. Un
    // échec ici n'annule pas l'activation : il est noté et la passe réessaie.
    let premier = sc::passe(&state.backend, &state.http_client, true, chrono::Utc::now()).await;
    let mut v = json!({
        "success": true,
        "key_id": key_id,
        "first_backup": issue_json(&premier),
    });
    if let Some(rk) = secours {
        v["recovery_key"] = json!(rk);
    }
    Json(v).into_response()
}

fn issue_json(i: &sc::Issue) -> Value {
    match i {
        sc::Issue::Rien(raison) => json!({ "done": false, "reason": raison }),
        sc::Issue::Deposee(m) => json!({ "done": true, "backup": m }),
        sc::Issue::Echec(e) => json!({ "done": false, "error": e }),
    }
}

// ── POST /system/config-backup/cloud/disable ────────────────────────

pub(super) async fn disable(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    if let Err(r) = garde_premium(&state).await {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match sc::activer(&settings, false) {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(_) => erreur(StatusCode::INTERNAL_SERVER_ERROR, "settings_write_failed"),
    }
}

// ── POST /system/config-backup/cloud/backup-now ─────────────────────

pub(super) async fn backup_now(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    if let Err(r) = garde_premium(&state).await {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = sc::compte(&settings) {
        return erreur_site(e);
    }
    match sc::passe(&state.backend, &state.http_client, true, chrono::Utc::now()).await {
        sc::Issue::Deposee(m) => Json(json!({
            "success": true,
            "skipped_unchanged": false,
            "backup": m,
        }))
        .into_response(),
        sc::Issue::Rien("unchanged") => Json(json!({
            "success": true,
            "skipped_unchanged": true,
            "backup": null,
        }))
        .into_response(),
        sc::Issue::Rien("no_key") => erreur(StatusCode::CONFLICT, "not_enabled"),
        sc::Issue::Rien(r) => erreur(StatusCode::CONFLICT, r),
        sc::Issue::Echec(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "backup_failed", "detail": e })),
        )
            .into_response(),
    }
}

// ── GET /system/config-backup/cloud/snapshots ───────────────────────

pub(super) async fn snapshots(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    if let Err(r) = garde_premium(&state).await {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let compte = match sc::compte(&settings) {
        Ok(c) => c,
        Err(e) => return erreur_site(e),
    };
    let locale = sc::cle_locale(&settings).ok().flatten().map(|c| c.key_id);
    match sc::lister(&settings, &state.http_client).await {
        Ok(liste) => {
            let backups: Vec<Value> = liste
                .into_iter()
                .map(|m| {
                    let mut v = json!(m);
                    v["this_server"] = json!(m.server_id == compte.server_id);
                    v["local_key"] = json!(locale.as_deref() == Some(m.key_id.as_str()));
                    v
                })
                .collect();
            Json(json!({
                "backups": backups,
                "max": sc::MAX_INSTANTANES,
                "max_per_server": sc::MAX_INSTANTANES,
                "max_machines": sc::MAX_MACHINES,
            }))
            .into_response()
        }
        Err(e) => erreur_site(e),
    }
}

// ── POST /system/config-backup/cloud/restore ────────────────────────

#[derive(Deserialize)]
pub(super) struct RestaurerCorps {
    id: i64,
    mode: sc::Mode,
    /// Phrase de passe ou clé de secours, quand la clé locale n'ouvre pas.
    #[serde(default)]
    secret: Option<String>,
}

/// Restaure un instantané. Le secret, s'il faut, voyage dans le CORPS, jamais
/// dans l'URL, et n'est ni journalisé ni rendu.
pub(super) async fn restore(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(corps): Json<RestaurerCorps>,
) -> Response {
    if let Err(r) = garde_premium(&state).await {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let (_, blob) = match sc::telecharger(&settings, &state.http_client, corps.id).await {
        Ok(x) => x,
        Err(sc::ErreurSite::Http { statut: 404, .. }) => {
            return erreur(StatusCode::NOT_FOUND, "backup_not_found");
        }
        Err(e) => return erreur_site(e),
    };
    let locale = match sc::cle_locale(&settings) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "sauvegarde_cloud_cle_illisible");
            None
        }
    };
    let (contenu, retrouvee) = match sc::dechiffrer(&blob, locale.as_ref(), corps.secret.as_deref())
    {
        Ok(x) => x,
        Err(sc::ErreurOuverture::SecretRequis) => {
            return erreur(StatusCode::BAD_REQUEST, "secret_required");
        }
        Err(sc::ErreurOuverture::MauvaisSecret) => {
            return erreur(StatusCode::BAD_REQUEST, "wrong_secret");
        }
        Err(sc::ErreurOuverture::Illisible(e)) => {
            warn!(error = %e, "sauvegarde_cloud_illisible");
            return erreur(StatusCode::UNPROCESSABLE_ENTITY, "backup_unreadable");
        }
    };
    let rapport = match sc::restaurer(&state.backend, &contenu, corps.mode) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "sauvegarde_cloud_restauration_echec");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "restore_failed", "detail": e })),
            )
                .into_response();
        }
    };
    // Adopter la clé : les sauvegardes suivantes de cette machine s'ouvriront
    // avec les secrets que l'utilisateur détient déjà, et la sauvegarde
    // automatique reprend (elle l'était là d'où vient l'instantané).
    let mut adoptee = false;
    if let Some(r) = retrouvee {
        match sc::adopter(&settings, &r) {
            Ok(true) => {
                adoptee = true;
                sc::activer(&settings, true).ok();
            }
            Ok(false) => {}
            Err(e) => warn!(error = %e, "sauvegarde_cloud_adoption_echec"),
        }
    }
    Json(json!({
        "success": true,
        "key_adopted": adoptee,
        // Les mots de passe des profils ne voyagent jamais.
        "passwords_restored": false,
        "report": rapport,
    }))
    .into_response()
}

/// La passe de fond : toutes les [`sc::CADENCE_MINUTES`], premier tour deux
/// minutes après le démarrage. Sans licence Premium, rien.
pub fn spawn(state: &AppState) {
    let backend = state.backend.clone();
    let http = state.http_client.clone();
    let license = state.license.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(
            sc::CADENCE_MINUTES as u64 * 60,
        ));
        loop {
            ticker.tick().await;
            if !license.check_feature(Feature::CloudConfigBackup).await {
                continue;
            }
            match sc::passe(&backend, &http, false, chrono::Utc::now()).await {
                sc::Issue::Deposee(m) => {
                    tracing::info!(id = m.id, "sauvegarde_cloud_automatique_deposee");
                }
                sc::Issue::Echec(e) => {
                    tracing::debug!(error = %e, "sauvegarde_cloud_automatique_echec")
                }
                sc::Issue::Rien(_) => {}
            }
        }
    });
}
