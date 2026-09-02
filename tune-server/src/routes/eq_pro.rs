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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(eq_status))
        .route("/presets", get(list_presets).post(create_preset))
        .route(
            "/presets/{id}",
            get(get_preset).put(update_preset).delete(delete_preset),
        )
        .route("/presets/{id}/activate", post(activate_preset))
        .route("/import/autoeq", post(import_autoeq))
        .route("/bands", get(get_bands))
        .route(
            "/expert-settings",
            get(get_expert_settings).post(set_expert_settings),
        )
    // RETIRÉ ici : POST /bands, GET+POST /parametric, GET+POST /graphic et
    // POST /room-correction (#1718).
    //
    // Ces routes persistaient `eq_current_bands`, `eq_parametric`, `eq_graphic`
    // et `eq_room_correction` — quatre clés que RIEN ne lisait, dans aucun
    // crate. Le chemin audio ne connaît que `zone_{id}_eq_profile`. Elles
    // répondaient pourtant `"applied": true`, et `/room-correction` avouait
    // même dans son propre corps que la convolution n'était pas branchée.
    //
    // Elles n'avaient aucun client : le client web n'appelle que
    // `/eq/presets` (liste, création, suppression). Et elles doublonnaient des
    // chemins qui, eux, atteignent le son — `POST /zones/{id}/eq` pour les
    // bandes, `POST /room-correction/profiles/{zone}/apply` pour la pièce.
    //
    // `GET /bands` reste : il lit les bandes du preset actif, ce qui est vrai
    // et utile. Seule l'écriture mentait.
}

/// Résolution du mode Expert (nombre de bandes de l'égaliseur graphique).
/// Stockée SERVEUR — pas dans le navigateur — pour que web, iPad et mobile
/// partagent la même grille. Valeurs : 10 (octave), 15 (2/3), 31 (1/3 ISO).
const EQ_EXPERT_BAND_CHOICES: [u32; 3] = [10, 15, 31];

/// Le nombre de bandes qu'un préréglage peut porter.
///
/// C'est le `max_bands` qu'annonce `GET /eq/status`, et la borne que l'import
/// AutoEq fait respecter. Les deux lisent la même constante pour qu'aucun
/// client ne se voie promettre une limite que l'import applique différemment.
const MAX_BANDS: usize = 31;

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

/// EQ subsystem status.
async fn eq_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let presets = load_presets(&state);
    let active_id = settings.get("eq_active_preset").ok().flatten();
    let enabled = settings
        .get("eq_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);

    let active_preset = active_id
        .as_ref()
        .and_then(|id| presets.iter().find(|p| p["id"].as_str() == Some(id)));

    Json(json!({
        "enabled": enabled,
        "preset_count": presets.len(),
        "active_preset_id": active_id,
        "active_preset_name": active_preset.and_then(|p| p["name"].as_str()),
        "supported_types": ["parametric", "graphic", "room_correction"],
        "max_bands": MAX_BANDS,
    }))
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
    /// Canal ciblé, `None` pour tous. Même contrat que EqBandSpec : les
    /// anciens presets sans ce champ restent globaux.
    #[serde(default)]
    channel: Option<u16>,
}

fn default_band_type() -> String {
    "peak".into()
}

impl EqBand {
    fn to_json(&self) -> Value {
        let mut value = json!({
            "freq": self.freq,
            "gain": self.gain,
            "q": self.q.unwrap_or(1.0),
            "type": self.band_type,
        });
        if let Some(channel) = self.channel {
            value["channel"] = json!(channel);
        }
        value
    }
}

/// Create a new EQ preset.
async fn create_preset(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreatePresetBody>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
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
    headers: axum::http::HeaderMap,
    Json(body): Json<CreatePresetBody>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
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
    headers: axum::http::HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
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
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Premium gate: DSP & EQ mutations require Premium
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
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
            let applique_a_chaud = state.orchestrator.apply_eq_change(zone_id).await;

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

// --- Import d'un profil AutoEq (#1405) ---

/// Un profil AutoEq tient en une quinzaine de lignes. 64 Kio laissent une marge
/// confortable pour un copier-coller maladroit tout en refusant qu'on pousse un
/// fichier arbitraire dans un réglage persisté.
const TAILLE_MAX_PROFIL: usize = 64 * 1024;

#[derive(Deserialize)]
struct ImportAutoEqBody {
    /// Le texte du fichier `… ParametricEQ.txt`, collé ou déposé tel quel.
    text: String,
    /// Le nom du préréglage — en pratique le modèle de casque. AutoEq ne le
    /// met pas DANS le fichier, il est dans le nom du fichier : c'est donc au
    /// client de le fournir.
    name: Option<String>,
    /// Zone visée. Une correction de casque vise une sortie précise ; sans
    /// zone le préréglage reste global et devra recevoir `?zone_id=` à
    /// l'activation.
    zone_id: Option<String>,
}

/// Importer un profil AutoEq et en faire un préréglage.
///
/// ## Ce que cette route fait
///
/// Elle analyse le format ParametricEQ ([`tune_core::audio::autoeq`]) et
/// enregistre le résultat comme un préréglage ordinaire, dans le même stockage
/// que `POST /eq/presets`. Rien de plus : les bandes obtenues sont des
/// `EqBandSpec` comme les autres, et c'est `POST /eq/presets/{id}/activate` qui
/// les envoie au son.
///
/// ## Le `Preamp` n'est PAS appliqué, et la réponse le dit
///
/// AutoEq préfixe ses profils d'un `Preamp` négatif pour que ses gains positifs
/// n'écrêtent pas. Tune réserve déjà cette marge, et davantage : le pré-gain
/// automatique de l'égaliseur vaut la somme de tous les gains positifs de la
/// cascade (`EqProfile::automatic_headroom_db`, d423c16b). Appliquer en plus le
/// `Preamp` du fichier atténuerait deux fois.
///
/// La conséquence s'entend et doit être affichée : sur l'Etymotic ER4SR, le
/// fichier demande −6,4 dB et Tune en réserve −22,2. Le préréglage joue donc
/// nettement plus bas que le même profil dans un lecteur qui suit le `Preamp`.
/// Ce n'est pas un défaut — rien n'écrête, et le timbre est celui d'AutoEq —
/// mais l'utilisateur doit pouvoir rattraper au volume en sachant pourquoi.
/// D'où `preamp_db`, `reserved_headroom_db` et `preamp_applied` dans la
/// réponse.
///
/// La couverture n'est pas supposée, elle est **vérifiée à chaque import** :
/// `preamp_covered_by_headroom` compare la marge réellement réservée au
/// `Preamp` demandé. Elle est vraie sur tout profil publié par AutoEq (le
/// maximum d'une réponse combinée ne dépasse jamais la somme de ses gains
/// positifs) ; un fichier bricolé pourrait la mettre en défaut, et la réponse
/// porte alors un `warning` plutôt que de se taire.
async fn import_autoeq(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ImportAutoEqBody>,
) -> Result<impl IntoResponse, AppError> {
    // Même porte que la création d'un préréglage : c'est ce que cette route est.
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
    .await
    {
        return Ok(resp);
    }

    if body.text.len() > TAILLE_MAX_PROFIL {
        return Err(AppError::bad_request(format!(
            "profil trop volumineux ({} octets) : un fichier AutoEq ParametricEQ \
             en fait quelques centaines",
            body.text.len()
        )));
    }

    // Un fichier malformé est REFUSÉ, et l'erreur nomme la ligne fautive
    // (`ErreurAutoEq`). Jamais de préréglage à moitié construit : rien n'est
    // écrit avant que le fichier entier soit lu sans faute.
    let profil = tune_core::audio::autoeq::analyser(&body.text)
        .map_err(|e| AppError::bad_request(format!("profil AutoEq illisible — {e}")))?;

    // Dépassement : un refus chiffré, jamais une troncature silencieuse. Perdre
    // les dernières bandes d'une correction, c'est en changer le timbre sans
    // le dire.
    if profil.bandes.len() > MAX_BANDS {
        return Err(AppError::bad_request(format!(
            "{} bandes actives : l'égaliseur en accepte {MAX_BANDS} au plus. \
             Aucune bande n'a été tronquée et rien n'a été enregistré — \
             désactivez des filtres dans le fichier (« OFF ») et réimportez.",
            profil.bandes.len()
        )));
    }

    let bands_json: Vec<Value> = profil
        .bandes
        .iter()
        .map(|b| {
            json!({
                "freq": b.freq,
                "gain": b.gain,
                "q": b.q,
                "type": b.band_type,
            })
        })
        .collect();

    let mut presets = load_presets(&state);
    let id = uuid::Uuid::new_v4().to_string();
    let preset = json!({
        "id": id,
        "name": body.name.unwrap_or_else(|| "AutoEq".into()),
        "eq_type": "parametric",
        "zone_id": body.zone_id,
        "bands": bands_json,
        "created_at": epoch_secs(),
        "source": "autoeq",
    });
    presets.push(preset.clone());
    save_presets(&state, &presets)?;

    let reserved = profil.marge_reservee_db();
    let couvert = profil.marge_de_tune_couvre_le_preamp();
    let mut corps = json!({
        "preset": preset,
        "band_count": profil.bandes.len(),
        // Écartés, mais comptés : l'écart entre le fichier et le préréglage
        // s'explique dans la réponse, pas à l'oreille.
        "ignored_filter_count": profil.filtres_ignores,
        // Ce que le fichier demande…
        "preamp_db": profil.preamp_db,
        // …ce que Tune réserve réellement, et le fait qu'il ne cumule pas.
        "reserved_headroom_db": reserved,
        "preamp_applied": false,
        "preamp_covered_by_headroom": couvert,
        "detail": format!(
            "Le pré-gain automatique de l'égaliseur réserve {reserved:.1} dB, \
             soit au moins la marge du Preamp AutoEq ({:.1} dB) : celui-ci \
             n'est donc pas appliqué en plus. Le préréglage joue plus bas \
             qu'un lecteur qui suit le Preamp ; rattraper au volume.",
            profil.preamp_db
        ),
    });
    if !couvert {
        corps["warning"] = json!(format!(
            "Ce fichier demande un Preamp de {:.1} dB alors que la somme de ses \
             gains positifs n'en justifie que {reserved:.1} : ce n'est pas un \
             export AutoEq standard. Le préréglage est importé tel quel ; \
             baissez le volume avant de l'activer.",
            profil.preamp_db
        ));
    }
    Ok((StatusCode::CREATED, Json(corps)).into_response())
}

/// Get current active EQ bands.
async fn get_bands(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let active_id = settings.get("eq_active_preset").ok().flatten();
    let presets = load_presets(&state);

    let bands = active_id
        .and_then(|id| {
            presets
                .iter()
                .find(|p| p["id"].as_str() == Some(&id))
                .and_then(|p| p["bands"].as_array())
                .cloned()
        })
        .unwrap_or_default();

    Json(json!({
        "bands": bands,
        "count": bands.len(),
        "active_preset_id": settings.get("eq_active_preset").ok().flatten(),
    }))
}

// --- Advanced EQ routes ---

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::{EqBand, resolve_activation_zone};
    use serde_json::json;

    #[test]
    fn un_preset_preserve_le_canal_de_sa_bande() {
        let band: EqBand = serde_json::from_value(json!({
            "freq": 120.0,
            "gain": -3.5,
            "q": 1.2,
            "type": "peak",
            "channel": 1,
        }))
        .unwrap();
        let stored = band.to_json();
        assert_eq!(stored["channel"], 1);

        let audio_band: tune_core::audio::eq::EqBandSpec = serde_json::from_value(stored).unwrap();
        assert_eq!(audio_band.channel, Some(1));
    }

    #[test]
    fn un_ancien_preset_sans_canal_reste_global() {
        let band: EqBand = serde_json::from_value(json!({
            "freq": 1000.0,
            "gain": 2.0,
            "q": 1.0,
            "type": "peak",
        }))
        .unwrap();
        let stored = band.to_json();
        assert!(stored.get("channel").is_none());
        let audio_band: tune_core::audio::eq::EqBandSpec = serde_json::from_value(stored).unwrap();
        assert_eq!(audio_band.channel, None);
    }

    /// La chaîne complète de l'import AutoEq : texte → JSON stocké → bande du
    /// chemin audio.
    ///
    /// `activate_preset` relit les bandes du préréglage avec
    /// `serde_json::from_value::<EqBandSpec>`. Si la forme écrite par l'import
    /// cessait de correspondre à ce que `EqBandSpec` attend, le `filter_map`
    /// de l'activation les jetterait EN SILENCE et le préréglage serait activé
    /// sans une seule bande. Ce test relie les deux bouts.
    #[test]
    fn un_profil_autoeq_importe_traverse_le_stockage_jusqu_a_la_bande_audio() {
        let profil = tune_core::audio::autoeq::analyser(
            "Preamp: -6.1 dB\nFilter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70\n",
        )
        .expect("profil AutoEq valide");

        // Exactement la forme que `import_autoeq` persiste.
        let stocke = json!({
            "freq": profil.bandes[0].freq,
            "gain": profil.bandes[0].gain,
            "q": profil.bandes[0].q,
            "type": profil.bandes[0].band_type,
        });

        let relue: tune_core::audio::eq::EqBandSpec =
            serde_json::from_value(stocke).expect("la bande stockee doit se relire");
        assert_eq!(relue.freq, 105.0);
        assert_eq!(relue.gain, 6.4);
        assert_eq!(relue.q, 0.70);
        assert_eq!(relue.band_type, "low_shelf");
        // Une correction de casque vaut pour les deux oreilles.
        assert_eq!(relue.channel, None);
    }

    /// Un texte qui n'est pas un profil AutoEq doit produire un message
    /// exploitable, pas un préréglage vide.
    #[test]
    fn un_texte_qui_nest_pas_un_profil_autoeq_donne_un_message_lisible() {
        let erreur = tune_core::audio::autoeq::analyser("mes reglages perso")
            .expect_err("ce texte n'est pas un profil AutoEq");
        let message = erreur.to_string();
        assert!(message.contains("ligne 1"), "message : {message}");
    }

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
