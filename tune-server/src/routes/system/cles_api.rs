//! Réglages ▸ Clés d'API (#4805, lot 1 « Enrichissement MetaRust ») : la clé
//! AcoustID et le jeton Discogs PERSONNELS de l'utilisateur.
//!
//! * `GET    /system/api-keys`                 — l'état des clés, sans valeur ;
//! * `PUT    /system/api-keys/{service}`       — `{"value": "…"}`, scellée ;
//! * `DELETE /system/api-keys/{service}`       — la clé intégrée reprend ;
//! * `POST   /system/api-keys/{service}/test`  — « Tester » : `{"value"}`
//!   facultatif (tester AVANT d'enregistrer), sinon la clé en service.
//!
//! Aucune réponse ne porte une clé : l'état ne rend que ses quatre derniers
//! caractères. Aucun journal non plus : les erreurs de transport ne sont pas
//! tracées (un `reqwest::Error` affiche son URL), et la clé AcoustID part
//! dans le CORPS d'un POST, jamais dans l'URL.
//!
//! Le stockage est celui de [`tune_core::cles_api`] — le coffre partagé avec
//! le chantier Assistant IA ([`tune_core::coffre`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::cles_api::{self, CleResolue, Service, VerdictTest};
use tune_core::coffre::Coffre;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// Le réglage HISTORIQUE de la clé AcoustID du serveur (la « clé intégrée »).
pub(crate) const REGLAGE_ACOUSTID_INTEGRE: &str = "acoustid_api_key";
/// Le réglage HISTORIQUE du jeton Discogs du serveur.
pub(crate) const REGLAGE_DISCOGS_INTEGRE: &str = "discogs_token";

/// Le coffre du serveur : celui installé au démarrage, sinon celui qui vit à
/// côté de la base de cet état (essais).
pub(crate) fn coffre(state: &AppState) -> Coffre {
    Coffre::du_serveur().unwrap_or_else(|| Coffre::a_cote_de_la_base(&state.config.db_path))
}

fn non_vide(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// La clé INTÉGRÉE d'un service : le réglage historique, puis `tune.toml` /
/// l'environnement.
pub(crate) fn cle_integree(state: &AppState, service: Service) -> Option<String> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match service {
        Service::AcoustId => non_vide(settings.get(REGLAGE_ACOUSTID_INTEGRE).ok().flatten())
            .or_else(|| non_vide(state.config.acoustid_api_key.clone())),
        Service::Discogs => non_vide(settings.get(REGLAGE_DISCOGS_INTEGRE).ok().flatten())
            .or_else(|| non_vide(state.config.discogs_token.clone()))
            .or_else(|| non_vide(std::env::var("TUNE_DISCOGS_TOKEN").ok()))
            .or_else(|| non_vide(std::env::var("DISCOGS_TOKEN").ok())),
    }
}

/// La clé à utiliser pour appeler le service : la PERSONNELLE d'abord.
pub(crate) fn cle(state: &AppState, service: Service) -> Option<CleResolue> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    cles_api::resoudre(
        &settings,
        &coffre(state),
        service,
        cle_integree(state, service),
    )
}

// ── Une seule réserve pour le jeton Discogs personnel ─────────────────────────
//
// Le même jeton se saisit à DEUX endroits : Réglages ▸ Clés d'API (ici,
// scellé dans le coffre) et Réglages ▸ « Jetons de service »
// (`routes::service_tokens`, ligne `streaming_auth` « discogs » plus le
// réglage historique `discogs_token`). Avant le 31/10/2026, le second était
// lu comme une clé INTÉGRÉE : l'enrichissement s'en servait, mais
// « Ma collection physique » (#7195) n'y voyait pas de jeton personnel et
// `GET /system/api-keys` disait `personnelle: false`.
//
// Désormais le coffre est la seule réserve du jeton PERSONNEL : « Jetons de
// service » y range ce qu'on lui confie ([`ranger_le_jeton_discogs_des_jetons_de_service`]),
// une installation existante l'y adopte au démarrage
// ([`adopter_le_jeton_discogs_des_jetons_de_service`]), et tout le monde le
// lit par [`cle`] → [`cles_api::resoudre`].

/// Le service et le champ du jeton Discogs dans « Jetons de service ».
pub(crate) const JETON_DE_SERVICE_DISCOGS: (&str, &str) = ("discogs", "token");

/// Range, SCELLÉ, comme jeton personnel, le jeton Discogs saisi dans
/// « Jetons de service ». La valeur ne passe dans aucun journal.
pub(crate) fn ranger_le_jeton_discogs_des_jetons_de_service(
    state: &AppState,
    valeur: &str,
) -> Result<(), String> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    cles_api::poser(&settings, &coffre(state), Service::Discogs, valeur)
}

/// Au démarrage : une installation dont le jeton Discogs n'a été saisi que
/// dans « Jetons de service » (avant l'unification) l'adopte comme jeton
/// personnel, scellé. Rien si un jeton personnel est déjà enregistré (même
/// illisible : c'est à l'utilisateur de le ressaisir). Rend `true` s'il a
/// été adopté.
pub(crate) fn adopter_le_jeton_discogs_des_jetons_de_service(state: &AppState) -> bool {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let deja = settings
        .get(Service::Discogs.reglage_scelle())
        .ok()
        .flatten()
        .is_some_and(|s| !s.trim().is_empty());
    if deja {
        return false;
    }
    let (service, champ) = JETON_DE_SERVICE_DISCOGS;
    let Some(jeton) =
        tune_core::services_manager::ServicesManager::with_backend(state.backend.clone())
            .get_credential(service, champ)
            .filter(|j| cles_api::valider(j).is_ok())
    else {
        return false;
    };
    match cles_api::poser(&settings, &coffre(state), Service::Discogs, &jeton) {
        Ok(()) => {
            tracing::info!("discogs_jeton_de_service_adopte_comme_personnel");
            true
        }
        Err(motif) => {
            // Le motif décrit le coffre ou la base, jamais la valeur.
            tracing::warn!(motif = %motif, "discogs_jeton_de_service_non_adopte");
            false
        }
    }
}

/// Retirer le jeton Discogs personnel le retire AUSSI de « Jetons de
/// service » (sinon le démarrage suivant le réadopterait) et efface le
/// réglage historique `discogs_token` s'il porte la même valeur — un jeton
/// différent (celui du serveur) reste.
fn retirer_le_jeton_discogs_des_jetons_de_service(state: &AppState, retire: Option<&str>) {
    let (service, champ) = JETON_DE_SERVICE_DISCOGS;
    let svc = tune_core::services_manager::ServicesManager::with_backend(state.backend.clone());
    let du_service = svc.get_credential(service, champ);
    if retire.is_some() && du_service.as_deref().map(str::trim) == retire.map(str::trim) {
        svc.delete_token(service).ok();
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let historique = settings.get(REGLAGE_DISCOGS_INTEGRE).ok().flatten();
    if retire.is_some() && historique.as_deref().map(str::trim) == retire.map(str::trim) {
        settings.delete(REGLAGE_DISCOGS_INTEGRE).ok();
    }
}

fn etat_json(state: &AppState, service: Service) -> Value {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    json!(cles_api::etat(
        &settings,
        &coffre(state),
        service,
        cle_integree(state, service).is_some()
    ))
}

fn service_inconnu() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "unknown_service", "services": ["acoustid", "discogs"]})),
    )
        .into_response()
}

/// `GET /system/api-keys`
pub(crate) async fn lister(State(state): State<AppState>) -> Json<Value> {
    let st = state.clone();
    let cles = tokio::task::spawn_blocking(move || {
        Service::TOUS
            .iter()
            .map(|s| etat_json(&st, *s))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(json!({ "keys": cles }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CorpsCle {
    #[serde(default)]
    value: Option<String>,
}

/// `PUT /system/api-keys/{service}`
pub(crate) async fn poser(
    State(state): State<AppState>,
    Path(code): Path<String>,
    Json(corps): Json<CorpsCle>,
) -> Response {
    let Some(service) = Service::depuis_code(&code) else {
        return service_inconnu();
    };
    let valeur = corps.value.unwrap_or_default();
    if let Err(motif) = cles_api::valider(&valeur) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"error": motif})),
        )
            .into_response();
    }
    let st = state.clone();
    let fait = tokio::task::spawn_blocking(move || {
        let settings = SettingsRepo::with_backend(st.backend.clone());
        cles_api::poser(&settings, &coffre(&st), service, &valeur).map(|_| etat_json(&st, service))
    })
    .await;
    match fait {
        Ok(Ok(etat)) => {
            tracing::info!(service = service.code(), "cle_api_personnelle_enregistree");
            Json(etat).into_response()
        }
        // Le motif décrit la panne (coffre, base), jamais la valeur.
        Ok(Err(motif)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "key_not_saved", "detail": motif})),
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// `DELETE /system/api-keys/{service}`
pub(crate) async fn retirer(State(state): State<AppState>, Path(code): Path<String>) -> Response {
    let Some(service) = Service::depuis_code(&code) else {
        return service_inconnu();
    };
    let st = state.clone();
    let fait = tokio::task::spawn_blocking(move || {
        let settings = SettingsRepo::with_backend(st.backend.clone());
        let retire = (service == Service::Discogs)
            .then(|| cles_api::lire_personnelle(&settings, &coffre(&st), service).ok())
            .flatten()
            .flatten();
        cles_api::retirer(&settings, service)?;
        if service == Service::Discogs {
            retirer_le_jeton_discogs_des_jetons_de_service(&st, retire.as_deref());
        }
        Ok::<_, String>(etat_json(&st, service))
    })
    .await;
    match fait {
        Ok(Ok(etat)) => Json(etat).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// L'appel « Tester » à AcoustID : un `lookup` de la piste exemple de leur
/// documentation, clé dans le CORPS.
async fn tester_acoustid(client: &reqwest::Client, cle: &str) -> VerdictTest {
    use tune_core::http::fetch::{ACOUSTID, CLE_ACOUSTID};
    ACOUSTID.acquire(CLE_ACOUSTID).await;
    let Ok(r) = client
        .post("https://api.acoustid.org/v2/lookup")
        .timeout(std::time::Duration::from_secs(15))
        .form(&[
            ("client", cle),
            ("trackid", cles_api::ACOUSTID_PISTE_EXEMPLE),
            ("format", "json"),
        ])
        .send()
        .await
    else {
        return VerdictTest::Indetermine;
    };
    ACOUSTID.constater_reponse(CLE_ACOUSTID, &r);
    let statut = r.status().as_u16();
    let corps = r.json::<Value>().await.unwrap_or(Value::Null);
    cles_api::verdict_acoustid(statut, &corps)
}

/// L'appel « Tester » à Discogs : `GET /oauth/identity`.
async fn tester_discogs(client: &reqwest::Client, jeton: &str) -> VerdictTest {
    use tune_core::http::fetch::{CLE_DISCOGS, DISCOGS};
    DISCOGS.acquire(CLE_DISCOGS).await;
    let Ok(r) = client
        .get("https://api.discogs.com/oauth/identity")
        .header(
            reqwest::header::USER_AGENT,
            tune_core::metadata::musicbrainz_release::MB_UA,
        )
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Discogs token={jeton}"),
        )
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
    else {
        return VerdictTest::Indetermine;
    };
    DISCOGS.constater_reponse(CLE_DISCOGS, &r);
    let statut = r.status().as_u16();
    let corps = r.json::<Value>().await.unwrap_or(Value::Null);
    cles_api::verdict_discogs(statut, &corps)
}

/// `POST /system/api-keys/{service}/test`
pub(crate) async fn tester(
    State(state): State<AppState>,
    Path(code): Path<String>,
    corps: Option<Json<CorpsCle>>,
) -> Response {
    let Some(service) = Service::depuis_code(&code) else {
        return service_inconnu();
    };
    let saisie = corps
        .and_then(|Json(c)| c.value)
        .filter(|v| !v.trim().is_empty());
    let (valeur, origine) = match saisie {
        Some(v) => match cles_api::valider(&v) {
            Ok(v) => (v, "input"),
            Err(motif) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({"error": motif})),
                )
                    .into_response();
            }
        },
        None => {
            let st = state.clone();
            match tokio::task::spawn_blocking(move || cle(&st, service))
                .await
                .ok()
                .flatten()
            {
                Some(c) => {
                    let o = match c.origine {
                        cles_api::Origine::Personnelle => "personal",
                        cles_api::Origine::Integree => "built_in",
                    };
                    (c.into_valeur(), o)
                }
                None => {
                    return (
                        StatusCode::PRECONDITION_FAILED,
                        Json(json!({"error": "no_key_configured"})),
                    )
                        .into_response();
                }
            }
        }
    };
    let verdict = match service {
        Service::AcoustId => tester_acoustid(&state.http_client, &valeur).await,
        Service::Discogs => tester_discogs(&state.http_client, &valeur).await,
    };
    tracing::info!(service = service.code(), ?verdict, "cle_api_testee");
    Json(json!({ "service": service.code(), "tested": origine, "verdict": verdict }))
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::state::AppState;

    const CLE: &str = "PERSO-discogs-0f1e2d3c4b5a6978";

    fn etat(dir: &std::path::Path) -> AppState {
        let config = crate::config::TuneConfig {
            db_path: dir.join("tune.db").to_string_lossy().to_string(),
            ..Default::default()
        };
        // Base en mémoire ; la clé maîtresse du coffre dans `dir`.
        AppState::new(":memory:", 0, config).expect("état en mémoire")
    }

    async fn appel(
        etat: &AppState,
        methode: &str,
        chemin: &str,
        corps: Option<&str>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder().method(methode).uri(chemin);
        if corps.is_some() {
            req = req.header("content-type", "application/json");
        }
        let req = req
            .body(
                corps
                    .map(|c| Body::from(c.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let r = crate::routes::system::router()
            .with_state(etat.clone())
            .oneshot(req)
            .await
            .unwrap();
        let statut = r.status();
        let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (statut, String::from_utf8_lossy(&octets).to_string())
    }

    /// Une clé n'apparaît JAMAIS dans une réponse : ni à l'enregistrement, ni
    /// dans l'état, ni dans `GET /system/config`, ni dans l'export.
    #[tokio::test]
    async fn une_cle_n_apparait_jamais_dans_une_reponse_d_api() {
        let dir = tempfile::tempdir().unwrap();
        let e = etat(dir.path());
        let (s, pose) = appel(
            &e,
            "PUT",
            "/api-keys/discogs",
            Some(&format!("{{\"value\":\"{CLE}\"}}")),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{pose}");
        assert!(!pose.contains(CLE), "{pose}");
        let v: Value = serde_json::from_str(&pose).unwrap();
        assert_eq!(v["fin"], "6978");
        assert_eq!(v["utilisee"], "personnelle");
        for chemin in ["/api-keys", "/config", "/config/export"] {
            let (_, corps) = appel(&e, "GET", chemin, None).await;
            assert!(!corps.contains(CLE), "{chemin} a rendu la clé : {corps}");
            assert!(!corps.contains("0f1e2d3c4b5a"), "{chemin} : {corps}");
        }
        // La personnelle est bien celle qui sert.
        let c = super::cle(&e, tune_core::cles_api::Service::Discogs).unwrap();
        assert_eq!(c.valeur(), CLE);
    }

    #[tokio::test]
    async fn une_saisie_vide_est_refusee_et_un_service_inconnu_aussi() {
        let dir = tempfile::tempdir().unwrap();
        let e = etat(dir.path());
        let (s, _) = appel(&e, "PUT", "/api-keys/acoustid", Some("{\"value\":\"  \"}")).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        let (s, _) = appel(&e, "PUT", "/api-keys/lastfm", Some("{\"value\":\"x1234\"}")).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // Sans aucune clé, « Tester » le dit au lieu d'appeler le service.
        let (s, _) = appel(&e, "POST", "/api-keys/acoustid/test", None).await;
        assert_eq!(s, StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn retirer_la_cle_personnelle_rend_la_main_a_l_integree() {
        let dir = tempfile::tempdir().unwrap();
        let e = etat(dir.path());
        tune_core::db::settings_repo::SettingsRepo::with_backend(e.backend.clone())
            .set(super::REGLAGE_ACOUSTID_INTEGRE, "INTEGREE-1234")
            .unwrap();
        appel(
            &e,
            "PUT",
            "/api-keys/acoustid",
            Some("{\"value\":\"PERSO-abcdef\"}"),
        )
        .await;
        assert_eq!(
            super::cle(&e, tune_core::cles_api::Service::AcoustId)
                .unwrap()
                .valeur(),
            "PERSO-abcdef"
        );
        let (s, corps) = appel(&e, "DELETE", "/api-keys/acoustid", None).await;
        assert_eq!(s, StatusCode::OK);
        let v: Value = serde_json::from_str(&corps).unwrap();
        assert_eq!(v["utilisee"], "integree");
        assert_eq!(
            super::cle(&e, tune_core::cles_api::Service::AcoustId)
                .unwrap()
                .valeur(),
            "INTEGREE-1234"
        );
    }
}
