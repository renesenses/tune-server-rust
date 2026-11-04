//! Accès Subsonic / OpenSubsonic (#7855, lot 4, étape 1 sur 3 : le socle).
//!
//! Deux surfaces :
//!
//! - `/rest/*`, à la RACINE et hors de la couche d'authentification de
//!   `/api/v1` : un client Subsonic (Symfonium, DSub, Feishin…) n'a pas de
//!   session Tune. Chaque requête passe par [`garde`], qui contrôle dans
//!   l'ordre le réglage (404 s'il est éteint), la licence Premium, puis la
//!   clé `apiKey`. Aucun gestionnaire n'est atteint sans clé valide.
//! - `/api/v1/subsonic/*` ([`gestion`]), derrière l'authentification de
//!   Tune : le réglage « Accès Subsonic » et les clés des clients.
//!
//! Le routeur `/rest` est TOUJOURS monté et répond 404 lui-même quand le
//! réglage est éteint : sans lui, `/rest/ping` tomberait dans le repli
//! statique, qui sert `index.html` avec un statut 200.
//!
//! La clé voyage dans l'URL (`apiKey=`) : rien ici ne journalise l'URL ni la
//! clé.

pub mod gestion;
pub mod identifiants;
pub mod reponse;

use axum::extract::{Query, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get};
use axum::{Extension, Router};
use serde_json::json;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::cles_clients::ClesClientsRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::state::AppState;
use reponse::{CodeErreur, Format};

/// Le réglage « Accès Subsonic ». Absent = éteint.
pub const REGLAGE_ACCES: &str = "subsonic_enabled";

/// Le client authentifié par sa clé, posé par [`garde`] pour les méthodes.
#[derive(Debug, Clone)]
pub struct ClientSubsonic {
    pub profile_id: i64,
}

type Parametres = Vec<(String, String)>;

fn parametre<'a>(params: &'a Parametres, nom: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == nom)
        .map(|(_, v)| v.as_str())
}

fn format_de(params: &Parametres) -> Format {
    Format::depuis(parametre(params, "f"))
}

/// Le réglage est-il allumé ?
pub fn acces_active(state: &AppState) -> bool {
    SettingsRepo::with_backend(state.backend.clone())
        .get(REGLAGE_ACCES)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Le routeur `/rest`, à monter à la racine.
pub fn router_rest(state: AppState) -> Router {
    Router::new()
        .route("/ping", methode(ping))
        .route("/ping.view", methode(ping))
        .route("/getLicense", methode(get_license))
        .route("/getLicense.view", methode(get_license))
        .route("/getUser", methode(get_user))
        .route("/getUser.view", methode(get_user))
        .route("/getMusicFolders", methode(get_music_folders))
        .route("/getMusicFolders.view", methode(get_music_folders))
        .route(
            "/getOpenSubsonicExtensions",
            methode(get_open_subsonic_extensions),
        )
        .route(
            "/getOpenSubsonicExtensions.view",
            methode(get_open_subsonic_extensions),
        )
        .fallback(methode_inconnue)
        .layer(axum::middleware::from_fn_with_state(state.clone(), garde))
        .with_state(state)
}

fn methode<H, T>(h: H) -> MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState> + Clone,
    T: 'static,
{
    get(h.clone()).post(h)
}

/// La garde de toute requête `/rest` : réglage, licence, puis clé.
async fn garde(State(state): State<AppState>, mut requete: Request, suivant: Next) -> Response {
    // 1. Réglage éteint : la surface n'existe pas.
    if !acces_active(&state) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let params: Parametres = Query::try_from_uri(requete.uri())
        .map(|Query(p)| p)
        .unwrap_or_default();
    let format = format_de(&params);

    // 2. Licence : l'accès Subsonic est Premium.
    if !state.license.check_feature(Feature::Subsonic).await {
        return reponse::erreur(
            format,
            CodeErreur::NonAutorise,
            "Subsonic access requires Tune Premium",
        );
    }

    // 3. Authentification par clé (extension OpenSubsonic `apiKey`).
    match authentifier(&state, &params) {
        Ok(client) => {
            requete.extensions_mut().insert(client);
            suivant.run(requete).await
        }
        Err((code, message)) => reponse::erreur(format, code, message),
    }
}

/// Seule la clé `apiKey` authentifie. Le jeton `t`/`s` (md5 du mot de passe
/// et d'un sel) et le mot de passe `p` sont refusés : les mots de passe de
/// Tune sont en argon2, le md5 est invérifiable, et un mot de passe en clair
/// dans une URL n'est pas un mécanisme que Tune accepte.
fn authentifier(
    state: &AppState,
    params: &Parametres,
) -> Result<ClientSubsonic, (CodeErreur, &'static str)> {
    let cle = parametre(params, "apiKey");
    let jeton_ou_mot_de_passe = ["t", "s", "p"]
        .iter()
        .any(|n| parametre(params, n).is_some());

    if cle.is_some() && (jeton_ou_mot_de_passe || parametre(params, "u").is_some()) {
        return Err((
            CodeErreur::MecanismesEnConflit,
            "apiKey must not be combined with u, p, t or s",
        ));
    }
    if jeton_ou_mot_de_passe {
        return Err((
            CodeErreur::MecanismeNonPrisEnCharge,
            "Token and password authentication are not supported; use apiKey",
        ));
    }
    let Some(cle) = cle else {
        return Err((
            CodeErreur::ParametreManquant,
            "Required parameter is missing: apiKey",
        ));
    };
    match ClesClientsRepo::with_backend(state.backend.clone()).authentifier(cle) {
        Ok(Some(c)) => Ok(ClientSubsonic {
            profile_id: c.profile_id,
        }),
        Ok(None) => Err((CodeErreur::CleInvalide, "Invalid API key")),
        Err(e) => {
            tracing::warn!(error = %e, "subsonic_api_key_lookup_failed");
            Err((CodeErreur::Generique, "Authentication unavailable"))
        }
    }
}

async fn methode_inconnue(Query(params): Query<Parametres>) -> Response {
    reponse::erreur(
        format_de(&params),
        CodeErreur::Introuvable,
        "Unknown method",
    )
}

async fn ping(Query(params): Query<Parametres>) -> Response {
    reponse::ok(format_de(&params), json!({}))
}

async fn get_license(Query(params): Query<Parametres>) -> Response {
    // La garde a déjà vérifié la licence Premium : on n'expose ni courriel
    // ni échéance.
    reponse::ok(format_de(&params), json!({"license": {"valid": true}}))
}

async fn get_open_subsonic_extensions(Query(params): Query<Parametres>) -> Response {
    reponse::ok(
        format_de(&params),
        json!({"openSubsonicExtensions": [
            {"name": "apiKeyAuthentication", "versions": [1]},
        ]}),
    )
}

/// Le dossier unique de la bibliothèque : Tune n'expose pas ses dossiers
/// racines aux clients à cette étape.
pub const DOSSIER_MUSIQUE: i64 = 1;

async fn get_music_folders(Query(params): Query<Parametres>) -> Response {
    reponse::ok(
        format_de(&params),
        json!({"musicFolders": {"musicFolder": [
            {"id": DOSSIER_MUSIQUE, "name": "Tune"},
        ]}}),
    )
}

fn profil(state: &AppState, sql: &str, valeur: &dyn ToSqlValue) -> Option<(i64, String, bool)> {
    let row = state.backend.query_one(sql, &[valeur]).ok().flatten()?;
    Some((
        row.first()?.as_i64()?,
        row.get(1)?.as_string()?,
        row.get(2).and_then(|v| v.as_i64()).unwrap_or(0) != 0,
    ))
}

async fn get_user(
    State(state): State<AppState>,
    Extension(client): Extension<ClientSubsonic>,
    Query(params): Query<Parametres>,
) -> Response {
    let format = format_de(&params);
    let Some(appelant) = profil(
        &state,
        "SELECT id, username, is_admin FROM profiles WHERE id = ?",
        &client.profile_id,
    ) else {
        return reponse::erreur(format, CodeErreur::Introuvable, "User not found");
    };
    let cible = match parametre(&params, "username") {
        None => appelant.clone(),
        Some(nom) if nom == appelant.1 => appelant.clone(),
        // Un autre compte : réservé à l'administrateur.
        Some(_) if !appelant.2 => {
            return reponse::erreur(format, CodeErreur::NonAutorise, "Not authorized");
        }
        Some(nom) => match profil(
            &state,
            "SELECT id, username, is_admin FROM profiles WHERE username = ?",
            &nom,
        ) {
            Some(p) => p,
            None => return reponse::erreur(format, CodeErreur::Introuvable, "User not found"),
        },
    };
    reponse::ok(
        format,
        json!({"user": {
            "username": cible.1,
            "adminRole": cible.2,
            "scrobblingEnabled": false,
            "settingsRole": false,
            "streamRole": true,
            "downloadRole": true,
            "coverArtRole": true,
            "playlistRole": false,
            "commentRole": false,
            "podcastRole": false,
            "jukeboxRole": false,
            "shareRole": false,
            "uploadRole": false,
            "videoConversionRole": false,
            "folder": [DOSSIER_MUSIQUE],
        }}),
    )
}
