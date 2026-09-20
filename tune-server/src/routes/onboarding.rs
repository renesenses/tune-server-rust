use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const STEPS: &[&str] = &[
    "welcome",
    "music-dirs",
    "streaming",
    "zones",
    "profile",
    "complete",
];

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(onboarding_status))
        .route("/step/welcome", post(step_welcome))
        .route("/step/music-dirs", post(step_music_dirs))
        .route("/step/streaming", post(step_streaming))
        .route("/step/zones", post(step_zones))
        .route("/step/profile", post(step_profile))
        .route("/step/complete", post(step_complete))
        .route("/skip", post(skip_onboarding))
}

/// Le statut « terminé » que l'on peut AFFIRMER de cette installation.
///
/// ## Pourquoi un rattrapage, et pourquoi il est exactement de cette forme
///
/// `onboarding_complete` ne devient « true » que par `POST /step/complete` ou
/// `POST /skip`. Or l'assistant du client web ne posait, en sortie, que son
/// drapeau `localStorage` — propre à l'appareil : il n'a jamais franchi cette
/// étape, sur aucune installation. `onboarding_complete` vaut donc « false »
/// partout, sur des serveurs en service depuis des mois. Mesuré le 20/09/2026
/// sur le .18 :
///
/// ```text
/// GET /api/v1/onboarding/status  → {"complete":false,"current_step":0}
/// GET /api/v1/library/stats      → {"tracks":47118,"albums":4363, …}
/// ```
///
/// Le client est réparé (il prévient désormais le serveur), mais les
/// installations déjà en place garderont leur « false » pour toujours :
/// personne n'ira repasser un assistant qu'il n'a jamais vu. Cette route
/// continuerait donc à affirmer une contre-vérité, et à piéger le prochain
/// qui la lit — c'est très exactement ce qui vient d'arriver.
///
/// D'où les deux conditions, et pas une :
///
/// - `etape_courante == 0` : l'assistant n'a **jamais été commencé**. C'est
///   ce qui protège le vrai cas neuf. Une installation en cours d'assistant,
///   dont le scan a déjà rempli la bibliothèque à l'étape « dossiers »,
///   affiche `etape_courante >= 1` : on ne lui vole pas la fin de son
///   assistant.
/// - `pistes > 0` : la bibliothèque est le témoin le plus dur. 47 118 pistes
///   ne s'indexent pas avant la première étape.
///
/// 🔴 Une bibliothèque VIDE ne donne jamais « terminé », quoi qu'il arrive :
/// une installation réellement vierge doit voir son assistant. Sans ce
/// revers, le rattrapage ne rattraperait rien — il éteindrait l'assistant
/// pour tout le monde.
fn onboarding_termine(complete_stocke: bool, etape_courante: i64, pistes: i64) -> bool {
    complete_stocke || (etape_courante == 0 && pistes > 0)
}

/// Check if onboarding is complete.
async fn onboarding_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let complete_stocke = settings
        .get("onboarding_complete")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let current_step: i64 = settings
        .get("onboarding_step")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // 🔴 Le compte n'est demandé que lorsqu'il peut changer la réponse : un
    // `COUNT(*)` à chaque appel d'une route appelée au montage de chaque
    // page, pour rien, serait payé par tout le monde.
    let mut rattrape = false;
    let complete = if onboarding_termine(complete_stocke, current_step, 0) {
        true
    } else {
        let pistes = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
            .count()
            .unwrap_or(0);
        rattrape = onboarding_termine(complete_stocke, current_step, pistes);
        if rattrape {
            tracing::info!(pistes, "onboarding_statut_rattrape_bibliotheque_pleine");
        }
        rattrape
    };

    let steps: Vec<Value> = STEPS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let idx = i as i64;
            json!({
                "index": idx,
                "name": name,
                "done": idx < current_step,
                "current": idx == current_step,
            })
        })
        .collect();

    // 🔴 Rien n'est ÉCRIT ici. Un GET n'a pas d'effet de bord, et le drapeau
    // stocké reste le témoin de ce que l'utilisateur a réellement fait ; le
    // rattrapage est une lecture, et il le dit.
    Json(json!({
        "complete": complete,
        "complete_rattrape": rattrape,
        "current_step": current_step,
        "steps": steps,
    }))
}

/// Step 1: Welcome - marks step 1 done.
async fn step_welcome(State(state): State<AppState>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    advance_step(&settings, 1);
    Json(json!({
        "step": "welcome",
        "status": "done",
        "next": "music-dirs",
    }))
}

#[derive(Deserialize)]
struct MusicDirsBody {
    dirs: Vec<String>,
}

/// Step 2: Configure music directories and trigger first scan.
async fn step_music_dirs(
    State(state): State<AppState>,
    Json(body): Json<MusicDirsBody>,
) -> impl IntoResponse {
    if body.dirs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "At least one music directory is required"})),
        )
            .into_response();
    }

    // Validate directories exist (normalizing paths for Windows compatibility)
    let mut valid_dirs = Vec::new();
    let mut invalid_dirs = Vec::new();
    for dir in &body.dirs {
        let normalized = tune_core::scanner::walker::normalize_path(dir);
        if std::path::Path::new(&normalized).is_dir() {
            valid_dirs.push(normalized);
        } else {
            invalid_dirs.push(dir.clone());
        }
    }

    if valid_dirs.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "No valid directories found",
                "invalid_dirs": invalid_dirs,
            })),
        )
            .into_response();
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let dirs_json = serde_json::to_string(&valid_dirs).unwrap_or_else(|_| "[]".into());
    settings.set("music_dirs", &dirs_json).ok();
    advance_step(&settings, 2);

    // Trigger library scan via the config (the scan system watches for music_dirs changes)
    Json(json!({
        "step": "music-dirs",
        "status": "done",
        "next": "streaming",
        "valid_dirs": valid_dirs,
        "invalid_dirs": invalid_dirs,
        "scan_triggered": true,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct StreamingBody {
    service: String,
    credentials: Option<Value>,
}

/// Step 3: Authenticate streaming service.
async fn step_streaming(
    State(state): State<AppState>,
    Json(body): Json<StreamingBody>,
) -> impl IntoResponse {
    let valid_services = ["tidal", "qobuz", "spotify", "deezer"];
    if !valid_services.contains(&body.service.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("Unknown service: {}", body.service),
                "valid_services": valid_services,
            })),
        )
            .into_response();
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Store credentials if provided (for services that use token auth)
    if let Some(creds) = &body.credentials {
        if let Some(obj) = creds.as_object() {
            for (key, value) in obj {
                let skey = format!("{}_{}", body.service, key);
                if let Some(sval) = value.as_str() {
                    settings.set(&skey, sval).ok();
                }
            }
        }
    }

    // Store which streaming service was configured during onboarding
    settings
        .set("onboarding_streaming_service", &body.service)
        .ok();
    advance_step(&settings, 3);

    // Return auth URL for OAuth-based services
    let auth_info = match body.service.as_str() {
        "tidal" | "spotify" => json!({
            "auth_type": "oauth",
            "auth_url": format!("/api/v1/streaming/{}/auth", body.service),
        }),
        "qobuz" => json!({
            "auth_type": "login_password",
            "auth_url": format!("/api/v1/streaming/{}/auth", body.service),
        }),
        "deezer" => json!({
            "auth_type": "arl_token",
            "auth_url": format!("/api/v1/streaming/{}/auth", body.service),
        }),
        _ => json!({"auth_type": "unknown"}),
    };

    Json(json!({
        "step": "streaming",
        "status": "done",
        "next": "zones",
        "service": body.service,
        "auth": auth_info,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ZonesBody {
    auto_discover: Option<bool>,
}

/// Step 4: Trigger device scan and create zones for discovered DLNA devices.
async fn step_zones(
    State(state): State<AppState>,
    Json(body): Json<ZonesBody>,
) -> impl IntoResponse {
    let auto_discover = body.auto_discover.unwrap_or(true);
    let settings = SettingsRepo::with_backend(state.backend.clone());

    if auto_discover {
        // Trigger SSDP discovery
        let scanner = &state.scanner;
        let discovered = scanner.rescan().await;
        tracing::info!(count = discovered.len(), "onboarding_zone_discovery");
    }

    settings
        .set(
            "onboarding_auto_discover",
            if auto_discover { "true" } else { "false" },
        )
        .ok();
    advance_step(&settings, 4);

    Json(json!({
        "step": "zones",
        "status": "done",
        "next": "profile",
        "auto_discover": auto_discover,
        "discovery_started": auto_discover,
        "zones_url": "/api/v1/zones",
        "devices_url": "/api/v1/devices",
    }))
}

#[derive(Deserialize)]
struct ProfileBody {
    name: String,
    avatar_color: Option<String>,
}

/// Step 5: Create first user profile.
async fn step_profile(
    State(state): State<AppState>,
    Json(body): Json<ProfileBody>,
) -> impl IntoResponse {
    if body.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Profile name is required"})),
        )
            .into_response();
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());

    // Create profile via profile_repo
    let profile_repo =
        tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone());
    let display_name = body.name.clone();
    let avatar_color = body.avatar_color.as_deref().unwrap_or("#6366f1");
    match profile_repo.create(&display_name, Some(&display_name), Some(avatar_color)) {
        Ok(profile_id) => {
            // Set as active profile
            settings
                .set("active_profile_id", &profile_id.to_string())
                .ok();
            advance_step(&settings, 5);

            Json(json!({
                "step": "profile",
                "status": "done",
                "next": "complete",
                "profile_id": profile_id,
                "name": display_name,
                "avatar_color": avatar_color,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "onboarding_profile_create_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("Failed to create profile: {e}")})),
            )
                .into_response()
        }
    }
}

/// Step 6: Mark onboarding complete. Returns summary.
async fn step_complete(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("onboarding_complete", "true").ok();
    settings
        .set("onboarding_step", &STEPS.len().to_string())
        .ok();

    // Build summary
    let music_dirs = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".into());
    let streaming_service = settings
        .get("onboarding_streaming_service")
        .ok()
        .flatten()
        .unwrap_or_default();
    let auto_discover = settings
        .get("onboarding_auto_discover")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    let active_profile_id = settings
        .get("active_profile_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    Json(json!({
        "step": "complete",
        "status": "done",
        "complete": true,
        "summary": {
            "music_dirs": serde_json::from_str::<Value>(&music_dirs).unwrap_or(json!([])),
            "streaming_service": streaming_service,
            "auto_discover": auto_discover,
            "active_profile_id": active_profile_id,
        },
    }))
}

/// Skip all steps, mark onboarding complete.
async fn skip_onboarding(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("onboarding_complete", "true").ok();
    settings
        .set("onboarding_step", &STEPS.len().to_string())
        .ok();

    Json(json!({
        "skipped": true,
        "complete": true,
    }))
}

fn advance_step(settings: &SettingsRepo, step: i64) {
    let current: i64 = settings
        .get("onboarding_step")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if step > current {
        settings.set("onboarding_step", &step.to_string()).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::onboarding_termine;

    /// Le terrain qui a produit le défaut : le .18, mesuré le 20/09/2026.
    ///
    /// `onboarding_complete` jamais posé, assistant jamais commencé
    /// (`current_step` = 0, les six étapes à `done:false`), et 47 118 pistes
    /// dans la bibliothèque. Le serveur affirmait « pas terminé », et chaque
    /// navigateur neuf recevait l'assistant de première installation.
    #[test]
    fn le_18_est_rattrape() {
        assert!(onboarding_termine(false, 0, 47_118));
    }

    /// 🔴 LA CONTRE-ÉPREUVE. Sans elle, le rattrapage ci-dessus n'aurait pas
    /// arbitré une contradiction : il aurait éteint l'assistant pour tout le
    /// monde. Une installation réellement vierge n'a aucune piste, et doit
    /// voir son assistant quoi qu'il arrive.
    #[test]
    fn une_installation_vierge_voit_toujours_son_assistant() {
        assert!(!onboarding_termine(false, 0, 0));
        // Et à chaque étape de l'assistant, tant qu'elle est vide.
        for etape in 0..=6 {
            assert!(
                !onboarding_termine(false, etape, 0),
                "bibliothèque vide à l'étape {etape} : l'assistant reste dû"
            );
        }
    }

    /// Une installation EN COURS d'assistant garde sa fin.
    ///
    /// L'étape « dossiers de musique » déclenche le scan : la bibliothèque
    /// peut être pleine avant que l'utilisateur n'ait vu les étapes zones et
    /// profil. C'est `etape_courante > 0` qui la protège — un rattrapage sur
    /// le seul compte de pistes lui aurait volé la fin de son installation.
    #[test]
    fn un_assistant_commence_nest_pas_rattrape() {
        for etape in 1..=5 {
            assert!(
                !onboarding_termine(false, etape, 47_118),
                "étape {etape} : l'assistant est commencé, on ne le termine pas à sa place"
            );
        }
    }

    /// Le drapeau réellement posé prime, et se suffit à lui-même : il n'a
    /// besoin ni d'une bibliothèque pleine ni d'une étape particulière. Une
    /// installation qui n'écoute que du streaming n'a aucune piste locale.
    #[test]
    fn le_drapeau_stocke_prime_et_se_suffit() {
        assert!(onboarding_termine(true, 0, 0));
        assert!(onboarding_termine(true, 6, 0));
        assert!(onboarding_termine(true, 3, 12));
    }
}
