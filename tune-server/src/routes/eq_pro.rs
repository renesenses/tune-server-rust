use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;

use crate::error::AppError;
use crate::state::AppState;

/// Ce module ne garde QUE ce qui atteint le son ou sert reellement un client.
///
/// Cinq routes ont ete retirees — `/status`, `/bands`, `/parametric`,
/// `/graphic` et `/room-correction`. Elles
/// persistaient leur etat dans cinq cles de reglages — `eq_current_bands`,
/// `eq_parametric`, `eq_graphic`, `eq_active_preset`, `eq_room_correction` —
/// **qu'aucun code hors de ce fichier ne lisait**. Le chemin audio ne connait
/// qu'une cle, `zone_{id}_eq_profile`, et ces six-la n'y menaient pas.
///
/// `set_bands` repondait pourtant `"applied": true`, et `/room-correction`
/// ecrivait dans sa propre reponse « Actual convolution requires DSP pipeline
/// integration ». Une API qui annonce un succes qu'elle ne peut pas tenir est
/// pire qu'une API absente : elle fait chercher la panne ailleurs (#1718).
///
/// Verifie avant retrait : le client web n'appelle que `/expert-settings` et
/// `/presets` ; tune-ios, tune-macos, tune-remote*, tune-server-flutter,
/// tune-server-ipados, Alexa, Home Assistant et Endpoint n'en appellent
/// aucune. Le plugin EQ Pro vit sous `/api/v1/eq-pro/`, autre espace de noms.
/// Le plugin Room Calibration pousse bien vers `/api/v1/eq/bands`, mais en
/// PUT sur le port 8200 — l'API du plugin EQ Advanced, pas celle-ci, qui
/// n'expose que GET et POST sur 8888.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/presets", get(list_presets).post(create_preset))
        .route(
            "/presets/{id}",
            get(get_preset).put(update_preset).delete(delete_preset),
        )
        // Conservee : contrairement aux six retirees, celle-ci ecrit
        // `zone_{id}_eq_profile` — la cle que le chemin audio lit — puis
        // rafraichit la sortie en cours. Elle atteint donc le son.
        .route("/presets/{id}/activate", post(activate_preset))
        .route(
            "/expert-settings",
            get(get_expert_settings).post(set_expert_settings),
        )
}

/// Résolution du mode Expert (nombre de bandes de l'égaliseur graphique).
/// Stockée SERVEUR — pas dans le navigateur — pour que web, iPad et mobile
/// partagent la même grille. Valeurs : 10 (octave), 15 (2/3), 31 (1/3 ISO).
const EQ_EXPERT_BAND_CHOICES: [u32; 3] = [10, 15, 31];

async fn get_expert_settings(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let bands = settings
        .get("eq_expert_bands")
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|n| EQ_EXPERT_BAND_CHOICES.contains(n))
        .unwrap_or(10);
    Json(json!({ "expert_bands": bands }))
}

#[derive(Deserialize)]
struct ExpertSettingsBody {
    expert_bands: u32,
}

async fn set_expert_settings(
    State(state): State<AppState>,
    Json(body): Json<ExpertSettingsBody>,
) -> Result<Json<Value>, AppError> {
    if !EQ_EXPERT_BAND_CHOICES.contains(&body.expert_bands) {
        return Err(AppError::bad_request(format!(
            "expert_bands doit être 10, 15 ou 31 (reçu {})",
            body.expert_bands
        )));
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("eq_expert_bands", &body.expert_bands.to_string())?;
    Ok(Json(json!({ "expert_bands": body.expert_bands })))
}

fn load_presets(state: &AppState) -> Vec<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .get("eq_presets")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_presets(state: &AppState, presets: &[Value]) -> Result<(), AppError> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set("eq_presets", &serde_json::to_string(presets)?)
        .ok();
    Ok(())
}

/// List all EQ presets.
async fn list_presets(State(state): State<AppState>) -> Json<Value> {
    let presets = load_presets(&state);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let active_id = settings.get("eq_active_preset").ok().flatten();

    Json(json!({
        "presets": presets,
        "active_preset_id": active_id,
    }))
}

#[derive(Deserialize)]
struct CreatePresetBody {
    name: String,
    #[serde(default)]
    bands: Vec<EqBand>,
    /// "parametric", "graphic", or "custom"
    eq_type: Option<String>,
    /// Zone ID this preset is for (None = global)
    zone_id: Option<String>,
}

#[derive(Deserialize, Clone)]
struct EqBand {
    /// Center frequency in Hz
    freq: f64,
    /// Gain in dB (-12 to +12 typical)
    gain: f64,
    /// Q factor (0.1 to 30)
    q: Option<f64>,
    /// Filter type: "peak", "low_shelf", "high_shelf", "low_pass", "high_pass", "notch"
    #[serde(rename = "type", default = "default_band_type")]
    band_type: String,
}

fn default_band_type() -> String {
    "peak".into()
}

impl EqBand {
    fn to_json(&self) -> Value {
        json!({
            "freq": self.freq,
            "gain": self.gain,
            "q": self.q.unwrap_or(1.0),
            "type": self.band_type,
        })
    }
}

/// Create a new EQ preset.
async fn create_preset(
    State(state): State<AppState>,
    Json(body): Json<CreatePresetBody>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::DspEq)
            .await
    {
        return Ok(resp);
    }

    let mut presets = load_presets(&state);
    let id = uuid::Uuid::new_v4().to_string();

    let bands_json: Vec<Value> = body.bands.iter().map(|b| b.to_json()).collect();

    let preset = json!({
        "id": id,
        "name": body.name,
        "eq_type": body.eq_type.unwrap_or_else(|| "parametric".into()),
        "zone_id": body.zone_id,
        "bands": bands_json,
        "created_at": epoch_secs(),
    });

    presets.push(preset.clone());
    save_presets(&state, &presets)?;

    Ok((StatusCode::CREATED, Json(preset)).into_response())
}

/// Get a single preset by ID.
async fn get_preset(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    let presets = load_presets(&state);
    match presets.iter().find(|p| p["id"].as_str() == Some(&id)) {
        Some(preset) => Json(preset.clone()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response(),
    }
}

/// Update an existing preset.
async fn update_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreatePresetBody>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::DspEq)
            .await
    {
        return Ok(resp);
    }

    let mut presets = load_presets(&state);
    let idx = presets.iter().position(|p| p["id"].as_str() == Some(&id));

    match idx {
        Some(i) => {
            let bands_json: Vec<Value> = body.bands.iter().map(|b| b.to_json()).collect();
            presets[i]["name"] = json!(body.name);
            presets[i]["bands"] = json!(bands_json);
            if let Some(t) = &body.eq_type {
                presets[i]["eq_type"] = json!(t);
            }
            if let Some(z) = &body.zone_id {
                presets[i]["zone_id"] = json!(z);
            }
            let updated = presets[i].clone();
            save_presets(&state, &presets)?;
            Ok(Json(updated).into_response())
        }
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response()),
    }
}

/// Delete a preset.
async fn delete_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::DspEq)
            .await
    {
        return Ok(resp);
    }

    let mut presets = load_presets(&state);
    let before = presets.len();
    presets.retain(|p| p["id"].as_str() != Some(&id));

    if presets.len() == before {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response());
    }

    save_presets(&state, &presets)?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    if settings.get("eq_active_preset").ok().flatten().as_deref() == Some(&id) {
        settings.delete("eq_active_preset").ok();
    }

    Ok(StatusCode::NO_CONTENT.into_response())
}

// --- Advanced EQ routes ---

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Quelle zone égaliser en activant ce preset : `?zone_id=` d'abord, à défaut
/// le `zone_id` que le preset porte lui-même, `None` si aucun des deux.
///
/// Le paramètre d'URL prime volontairement : un preset « global »
/// (`zone_id: null`) doit pouvoir être activé sur n'importe quelle zone, et un
/// preset lié à une zone doit pouvoir être essayé ailleurs sans être modifié.
///
/// `None` conduit à un 400. C'est le point de l'issue #1718 : mieux vaut
/// refuser que répondre `activated: true` sans savoir quoi régler.
fn resolve_activation_zone(query: Option<&str>, preset: &Value) -> Option<i64> {
    query
        .and_then(|z| z.trim().parse::<i64>().ok())
        .or_else(|| {
            preset["zone_id"]
                .as_str()
                .and_then(|z| z.trim().parse().ok())
        })
        // Un preset créé depuis un client qui envoie un nombre JSON plutôt
        // qu'une chaîne — `CreatePresetBody.zone_id` est `Option<String>`, mais
        // le preset est restocké en JSON libre et rien ne garantit le type.
        .or_else(|| preset["zone_id"].as_i64())
}

/// Activer un preset SUR UNE ZONE — et l'entendre.
///
/// Cette route ne faisait qu'écrire `eq_active_preset` et répondre
/// `activated: true`. Le chemin audio ne lit pas cette clé : le preset
/// n'atteignait jamais le son (#1718). Elle écrit désormais les bandes du
/// preset dans `zone_{id}_eq_profile` — la seule clé que
/// `Orchestrator::load_eq_processor` connaisse — puis rafraîchit la sortie qui
/// joue, comme `POST /zones/{id}/eq`.
///
/// La zone vient de `?zone_id=`, à défaut du `zone_id` que le preset porte
/// déjà (`CreatePresetBody`, « None = global »). Sans l'une ni l'autre on ne
/// peut pas savoir QUOI égaliser : 400 plutôt qu'un succès sans effet.
///
/// Les champs de macro-réglage du profil (tilt graves/médiums/aigus, pièce,
/// placement) sont PRÉSERVÉS : activer un preset remplace les bandes expertes,
/// pas l'environnement d'écoute. Même règle que `POST /zones/{id}/eq`.
async fn activate_preset(
    State(state): State<AppState>,
    Path(id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::DspEq)
            .await
    {
        return resp;
    }

    let presets = load_presets(&state);
    let preset = presets.iter().find(|p| p["id"].as_str() == Some(&id));

    match preset {
        Some(p) => {
            let Some(zone_id) =
                resolve_activation_zone(params.get("zone_id").map(|s| s.as_str()), p)
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": "zone_id required",
                        "detail": "Ce preset n'est lié à aucune zone : préciser \
                                   ?zone_id=N. Sans zone, l'égaliseur ne saurait \
                                   pas quelle sortie régler.",
                    })),
                )
                    .into_response();
            };

            let settings = SettingsRepo::with_backend(state.backend.clone());
            let key = format!("zone_{zone_id}_eq_profile");
            let mut profile: tune_core::audio::eq::EqProfile = settings
                .get(&key)
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            profile.bands = p["bands"]
                .as_array()
                .map(|bands| {
                    bands
                        .iter()
                        .filter_map(|v| serde_json::from_value(v.clone()).ok())
                        .collect()
                })
                .unwrap_or_default();
            profile.enabled = true;
            let _ = settings.set(&key, &serde_json::to_string(&profile).unwrap_or_default());

            settings.set("eq_active_preset", &id).ok();
            settings.set("eq_enabled", "true").ok();

            // Persister ne suffit pas : sans ceci le preset n'atteindrait le son
            // qu'à la piste suivante sur une zone locale (#1725).
            let applique_a_chaud = state.orchestrator.refresh_zone_eq(zone_id).await;

            Json(json!({
                "active_preset_id": id,
                "active_preset_name": p["name"],
                "zone_id": zone_id,
                "band_count": profile.bands.len(),
                "activated": true,
                // « persisté » d'un côté, « entendu maintenant » de l'autre.
                // Faux ne signale pas un échec : rien ne joue, zone non locale,
                // ou mode PURE.
                "applied_live": applique_a_chaud,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "preset not found"})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_activation_zone;
    use serde_json::json;

    #[test]
    fn le_parametre_durl_prime_sur_la_zone_du_preset() {
        // Un preset lié à la zone 3 doit pouvoir être essayé sur la 7 sans
        // etre modifié.
        let preset = json!({"id": "a", "zone_id": "3"});
        assert_eq!(resolve_activation_zone(Some("7"), &preset), Some(7));
    }

    #[test]
    fn a_defaut_on_prend_la_zone_du_preset() {
        let preset = json!({"id": "a", "zone_id": "3"});
        assert_eq!(resolve_activation_zone(None, &preset), Some(3));
    }

    #[test]
    fn un_preset_global_active_sur_la_zone_demandee() {
        // `zone_id: null` = preset global (cf. CreatePresetBody).
        let preset = json!({"id": "a", "zone_id": null});
        assert_eq!(resolve_activation_zone(Some("2"), &preset), Some(2));
    }

    #[test]
    fn sans_zone_nulle_part_on_refuse() {
        // Le coeur de #1718 : mieux vaut un 400 qu'un `activated: true` qui ne
        // sait pas quoi regler.
        let preset = json!({"id": "a", "zone_id": null});
        assert_eq!(resolve_activation_zone(None, &preset), None);
        assert_eq!(resolve_activation_zone(Some(""), &preset), None);
        assert_eq!(resolve_activation_zone(Some("salon"), &preset), None);
    }

    #[test]
    fn un_zone_id_numerique_est_accepte_aussi() {
        // Le preset est restocke en JSON libre : rien ne garantit que zone_id
        // soit reste une chaine.
        let preset = json!({"id": "a", "zone_id": 5});
        assert_eq!(resolve_activation_zone(None, &preset), Some(5));
    }

    #[test]
    fn les_espaces_ne_font_pas_echouer_la_resolution() {
        let preset = json!({"id": "a", "zone_id": " 4 "});
        assert_eq!(resolve_activation_zone(None, &preset), Some(4));
        assert_eq!(resolve_activation_zone(Some(" 9 "), &preset), Some(9));
    }
}

#[cfg(test)]
mod routes_mortes_tests {
    /// #1718 — verrouille le retrait, par le CONTENU du fichier.
    ///
    /// Ces six routes persistaient leur etat dans des cles qu'aucun code hors
    /// de ce module ne lisait : elles annoncaient un succes que le son ne
    /// suivait jamais. Si l'une revient, c'est ce test qui doit le dire — pas
    /// un utilisateur qui cherche pourquoi son egaliseur reste muet.
    ///
    /// Le controle porte sur le ROUTEUR : les fonctions peuvent reapparaitre
    /// pour d'autres usages, ce qui ne doit pas etre interdit ; ce qui ne doit
    /// pas revenir, c'est leur exposition en HTTP.
    #[test]
    fn les_routes_sans_effet_sur_le_son_ne_reviennent_pas() {
        let source = include_str!("eq_pro.rs");
        let routeur = source
            .split("pub fn router()")
            .nth(1)
            .expect("routeur introuvable")
            .split("\n}")
            .next()
            .expect("fin du routeur introuvable");

        for mortes in [
            "\"/status\"",
            "\"/bands\"",
            "\"/parametric\"",
            "\"/graphic\"",
            "\"/room-correction\"",
        ] {
            assert!(
                !routeur.contains(mortes),
                "la route {mortes} est revenue dans le routeur : elle ecrivait une cle \
                 que le chemin audio ne lit pas (#1718). Si elle doit exister, elle doit \
                 d'abord ecrire zone_{{id}}_eq_profile."
            );
        }
    }

    /// Les deux routes que le client web utilise reellement doivent rester.
    #[test]
    fn les_routes_utilisees_par_le_client_restent_exposees() {
        let source = include_str!("eq_pro.rs");
        let routeur = source.split("pub fn router()").nth(1).unwrap();
        assert!(routeur.contains("\"/presets\""), "liste des presets perdue");
        assert!(
            routeur.contains("\"/expert-settings\""),
            "resolution du mode Expert perdue"
        );
        // Retiree par erreur lors du premier jet de #1718, rattrapee par le
        // garde-fou `every_route_writing_the_eq_profile_refreshes_the_live_output` :
        // contrairement aux cinq autres, elle ecrit `zone_{id}_eq_profile` et
        // rafraichit la sortie. Elle atteint le son.
        assert!(
            routeur.contains("activate"),
            "activation d'un preset perdue : elle ecrit la vraie cle du chemin audio"
        );
    }
}
