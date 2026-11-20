use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;
use tune_core::room_correction::{
    CorrectionFilter, FilterType, FrequencyPoint, RoomProfile, delete_profile,
    generate_correction_from_measurements, list_profiles, load_profile, profil_eq_de_correction,
    save_profile,
};

use crate::error::AppError;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/profiles", get(list_profiles_handler))
        .route(
            "/profiles/{zone_id}",
            get(get_profile_handler)
                .post(save_profile_handler)
                .delete(delete_profile_handler),
        )
        .route("/analyze", post(analyze_handler))
        .route("/profiles/{zone_id}/apply", post(apply_profile_handler))
        .route("/ir/upload/{zone_id}", post(upload_ir_handler))
        .route("/ir/clear/{zone_id}", post(clear_ir_handler))
        .route("/ir/status/{zone_id}", get(ir_status_handler))
        .route("/ir/options/{zone_id}", post(ir_options_handler))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `GET /room-correction/profiles` — list all room correction profiles.
async fn list_profiles_handler(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let profiles = list_profiles(&state.backend);
    Ok(Json(json!({
        "profiles": profiles,
        "count": profiles.len(),
    }))
    .into_response())
}

/// `GET /room-correction/profiles/{zone_id}` — get a zone's profile.
async fn get_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    match load_profile(&state.backend, &zone_id) {
        Some(profile) => Ok(Json(json!(profile)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no profile for this zone"})),
        )
            .into_response()),
    }
}

/// Request body for saving a room correction profile.
#[derive(Deserialize)]
struct SaveProfileBody {
    name: String,
    #[serde(default)]
    filters: Vec<CorrectionFilterInput>,
    /// Raw measurement data (JSON-encoded) for storage / re-analysis.
    measurement_data: Option<String>,
}

#[derive(Deserialize)]
struct CorrectionFilterInput {
    frequency_hz: f64,
    gain_db: f64,
    #[serde(default = "default_q")]
    q_factor: f64,
    #[serde(default = "default_filter_type")]
    filter_type: FilterType,
}

fn default_q() -> f64 {
    1.0
}

fn default_filter_type() -> FilterType {
    FilterType::Peaking
}

/// `POST /room-correction/profiles/{zone_id}` — save a profile.
async fn save_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
    Json(body): Json<SaveProfileBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let filters: Vec<CorrectionFilter> = body
        .filters
        .into_iter()
        .map(|f| CorrectionFilter {
            frequency_hz: f.frequency_hz,
            gain_db: f.gain_db,
            q_factor: f.q_factor,
            filter_type: f.filter_type,
        })
        .collect();

    let now = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Simple ISO-8601 from epoch — good enough for a creation timestamp.
        let dt = time::OffsetDateTime::from_unix_timestamp(secs as i64)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        dt.format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| format!("{secs}"))
    };
    let profile = RoomProfile {
        name: body.name,
        zone_id: zone_id.clone(),
        filters,
        created_at: now,
        measurement_data: body.measurement_data,
    };

    save_profile(&state.backend, &profile).map_err(AppError::internal)?;

    Ok((StatusCode::CREATED, Json(json!(profile))).into_response())
}

/// `DELETE /room-correction/profiles/{zone_id}` — delete a zone's profile.
async fn delete_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let existed = delete_profile(&state.backend, &zone_id).map_err(AppError::internal)?;
    if existed {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no profile for this zone"})),
        )
            .into_response())
    }
}

/// Request body for the analyze endpoint.
#[derive(Deserialize)]
struct AnalyzeBody {
    measurements: Vec<FrequencyPoint>,
}

/// `POST /room-correction/analyze` — analyze measurement data and return
/// suggested correction filters without saving anything.
async fn analyze_handler(
    State(state): State<AppState>,
    Json(body): Json<AnalyzeBody>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    if body.measurements.is_empty() {
        return Err(AppError::bad_request("measurements array is empty"));
    }

    let filters = generate_correction_from_measurements(&body.measurements);

    Ok(Json(json!({
        "filters": filters,
        "filter_count": filters.len(),
        "measurement_points": body.measurements.len(),
    }))
    .into_response())
}

/// `POST /room-correction/profiles/{zone_id}/apply` — apply a zone's room
/// correction profile to the zone's EQ (writes to the existing parametric
/// EQ settings key so the playback pipeline picks it up).
async fn apply_profile_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return Ok(resp);
    }

    let profile = match load_profile(&state.backend, &zone_id) {
        Some(p) => p,
        None => {
            return Ok((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "no profile for this zone"})),
            )
                .into_response());
        }
    };

    if profile.filters.is_empty() {
        return Ok(Json(json!({
            "applied": false,
            "zone_id": zone_id,
            "reason": "profile has no correction filters",
        }))
        .into_response());
    }

    // #7642 — un `EqProfile` COMPLET, typé : l'ancien JSON fait main n'avait
    // pas les champs obligatoires et la chaîne audio ne le relisait jamais.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let eq_profile = profil_eq_de_correction(&profile.filters);

    settings
        .set(
            &format!("zone_{zone_id}_eq_profile"),
            &serde_json::to_string(&eq_profile).map_err(|e| AppError::internal(e.to_string()))?,
        )
        .map_err(AppError::internal)?;

    // Persister ne suffit pas : sans ceci la correction n'atteint le son qu'a
    // la piste SUIVANTE sur une zone locale, alors que la reponse annonce deja
    // `applied: true` (#1725). Une correction de piece se juge a l'oreille,
    // musique en cours — c'est le geste meme qu'on attend de l'utilisateur.
    // `zone_id` est textuel sur cette route ; l'orchestrateur indexe par i64.
    let portee = match zone_id.parse::<i64>() {
        Ok(id) => Some(state.orchestrator.apply_eq_change_portee(id).await),
        Err(_) => None,
    };
    let applique_a_chaud = portee == Some(tune_core::orchestrator::PorteeDuReglage::Immediate);

    Ok(Json(json!({
        "applied": true,
        "zone_id": zone_id,
        "profile_name": profile.name,
        "filter_count": profile.filters.len(),
        // `applied` dit « persiste » ; celui-ci dit « entendu maintenant ».
        // Faux ne signale pas un echec : rien ne joue, zone non locale, PURE.
        "applied_live": applique_a_chaud,
        // #4680 — quand il s'entend ; voir `POST /zones/{id}/eq`.
        "portee": portee.map(|p| p.code()),
    }))
    .into_response())
}

/// Le canal vise par un televersement de filtre.
#[derive(Deserialize, Default)]
struct CanalDuFiltre {
    /// `left` ou `right` pour deposer UN canal ; absent = le fichier porte deja
    /// la correction complete (mono duplique, ou stereo tel quel).
    channel: Option<String>,
    /// #8761 — `1`/`true` : AJOUTER ce fichier au jeu de la zone, pour SA
    /// cadence (lue dans l'en-tete WAV), au lieu de remplacer le fichier
    /// principal. Un fichier deja present a la meme cadence est remplace.
    per_rate: Option<String>,
}

fn vrai(v: Option<&str>) -> bool {
    matches!(v.map(str::trim), Some("1") | Some("true") | Some("yes") | Some("oui"))
}

fn dossier_ir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("TUNE_DATA_DIR").unwrap_or_else(|_| ".".into()))
        .join("ir")
}

/// #8761 — reinstaller sur la sortie LOCALE de la zone le jeu tel que les
/// reglages le decrivent (principal, fichiers par cadence, repli). Sans
/// fichier principal, la convolution est retiree. Rien a faire pour une
/// zone reseau : le transcodage relit le jeu a chaque piste.
#[cfg(feature = "local-audio")]
async fn installer_le_jeu_local(
    state: &AppState,
    zone_id: i64,
    device_id: &str,
) -> Result<(), String> {
    if !device_id.starts_with("local:") {
        return Ok(());
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let jeu = tune_core::audio::convolver::JeuDeZone::lire(&settings, zone_id);
    let outputs = state.outputs.lock().await;
    if let Some(output) = outputs.get(device_id) {
        let output = output.lock().await;
        if let Some(local) = output
            .as_any()
            .downcast_ref::<tune_core::outputs::local::LocalOutput>()
        {
            return match jeu {
                Some(jeu) => local.set_convolver_config(jeu.charger()?),
                None => {
                    local.clear_convolver();
                    Ok(())
                }
            };
        }
    }
    Ok(())
}

#[cfg(not(feature = "local-audio"))]
async fn installer_le_jeu_local(
    _state: &AppState,
    _zone_id: i64,
    _device_id: &str,
) -> Result<(), String> {
    Ok(())
}

/// #8761 — les fichiers du jeu de la zone, decrits (cadence, canaux, taps),
/// le principal en tete. Un fichier illisible est decrit avec son erreur.
fn decrire_le_jeu(settings: &SettingsRepo, zone_id: i64) -> Vec<serde_json::Value> {
    let Some(jeu) = tune_core::audio::convolver::JeuDeZone::lire(settings, zone_id) else {
        return Vec::new();
    };
    jeu.chemins()
        .into_iter()
        .enumerate()
        .map(|(i, chemin)| {
            match tune_core::audio::convolver::Convolver::read_ir_taps(&chemin) {
                Ok((taps, sr)) => json!({
                    "path": chemin,
                    "sample_rate": sr,
                    "channels": taps.len(),
                    "taps": taps.first().map(Vec::len).unwrap_or(0),
                    "main": i == 0,
                }),
                Err(e) => json!({"path": chemin, "main": i == 0, "error": e}),
            }
        })
        .collect()
}

/// Cadence d'un fichier de reponse impulsionnelle, `None` s'il est illisible.
fn cadence_du_fichier(chemin: &str) -> Option<u32> {
    tune_core::audio::convolver::Convolver::read_ir_taps(chemin)
        .ok()
        .map(|(_, sr)| sr)
}

fn ecrire_supplementaires(settings: &SettingsRepo, zone_id: i64, liste: &[String]) {
    let cle = format!("{}{zone_id}", tune_core::audio::convolver::par_cadence::CLE_JEU);
    if liste.is_empty() {
        settings.delete(&cle).ok();
    } else {
        settings
            .set(&cle, &serde_json::to_string(liste).unwrap_or_default())
            .ok();
    }
}

/// #8761 — `POST /room-correction/ir/upload/{zone}?per_rate=1` : le fichier
/// est range sous sa cadence et ajoute au jeu. Sans fichier principal, il le
/// devient (les consommateurs qui ne connaissent que `ir_path_{zone}` le
/// voient). A cadence egale, il remplace le fichier precedent du jeu.
async fn ajouter_au_jeu(
    state: &AppState,
    zone_id: i64,
    device_id: &str,
    is_local: bool,
    body: &[u8],
) -> axum::response::Response {
    let ir_dir = dossier_ir();
    std::fs::create_dir_all(&ir_dir).ok();
    let provisoire = ir_dir.join(format!("zone_{zone_id}_depot.wav"));
    if let Err(e) = std::fs::write(&provisoire, body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write IR: {e}")})),
        )
            .into_response();
    }
    let Some(cadence) = cadence_du_fichier(provisoire.to_str().unwrap_or("")) else {
        let e = tune_core::audio::convolver::Convolver::read_ir_taps(
            provisoire.to_str().unwrap_or(""),
        )
        .err()
        .unwrap_or_default();
        std::fs::remove_file(&provisoire).ok();
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    };
    let chemin = ir_dir.join(format!("zone_{zone_id}_sr{cadence}.wav"));
    if let Err(e) = std::fs::rename(&provisoire, &chemin) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write IR: {e}")})),
        )
            .into_response();
    }
    let chemin = chemin.to_str().unwrap_or("").to_string();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let principal = settings
        .get(&format!("ir_path_{zone_id}"))
        .ok()
        .flatten()
        .filter(|p| !p.is_empty() && std::path::Path::new(p).exists());
    match principal {
        None => {
            settings.set(&format!("ir_path_{zone_id}"), &chemin).ok();
        }
        Some(principal) if principal != chemin => {
            let mut liste =
                tune_core::audio::convolver::JeuDeZone::lire_supplementaires(&settings, zone_id);
            // Un seul fichier par cadence dans la liste : l'ancien de meme
            // cadence (meme nom, ou nom d'avant) cede la place.
            liste.retain(|p| p != &chemin && cadence_du_fichier(p) != Some(cadence));
            liste.push(chemin.clone());
            ecrire_supplementaires(&settings, zone_id, &liste);
        }
        Some(_) => {}
    }
    if let Err(e) = installer_le_jeu_local(state, zone_id, device_id).await {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    Json(json!({
        "ok": true,
        "zone_id": zone_id,
        "ir_path": chemin,
        "sample_rate": cadence,
        "size_bytes": body.len(),
        "active": true,
        "per_rate": true,
        "files": decrire_le_jeu(&settings, zone_id),
        "applies_to": if is_local { "local" } else { "network (transcode)" },
    }))
    .into_response()
}

/// `POST /room-correction/ir/upload/{zone_id}` — upload a WAV impulse response
///
/// `?channel=left` / `?channel=right` depose UN canal a la fois. Les outils de
/// correction de piece — REW, Acourate, Audiolense — exportent deux fichiers
/// mono, `filter_L.wav` et `filter_R.wav`, jamais un stereo : sans ce chemin,
/// l'utilisateur devait les fusionner lui-meme dans un editeur audio (Daniel,
/// 24/08/2026).
///
/// Les deux canaux deposes sont combines en un WAV stereo ecrit au chemin que
/// les consommateurs lisent DEJA — sortie locale, transcodage vers les
/// renderers reseau, visualisation `/convolver/response`. Aucun d'eux n'a a
/// connaitre ce nouveau mode.
///
/// Tant qu'il manque un canal, la correction n'est PAS activee : n'appliquer
/// qu'une oreille serait pire que ne rien appliquer. La reponse dit lequel
/// manque.
async fn upload_ir_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(canal): Query<CanalDuFiltre>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    };

    let device_id = zone.output_device_id.unwrap_or_default();
    let is_local = device_id.starts_with("local:");

    if vrai(canal.per_rate.as_deref()) && canal.channel.as_deref().is_none_or(str::is_empty) {
        return ajouter_au_jeu(&state, zone_id, &device_id, is_local, &body).await;
    }

    // Persist the IR to disk.
    let ir_dir = dossier_ir();
    std::fs::create_dir_all(&ir_dir).ok();
    let ir_path = ir_dir.join(format!("zone_{zone_id}.wav"));
    let cote = match canal.channel.as_deref() {
        None | Some("") => None,
        Some("left") | Some("l") | Some("gauche") => Some("L"),
        Some("right") | Some("r") | Some("droite") => Some("R"),
        Some(autre) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(crate::message_api::corps_refus(
                    "unknown_channel",
                    "api.roomCorrection.unknownChannel",
                    &[("channel", autre.to_string())],
                )),
            )
                .into_response();
        }
    };

    // Depot d'un seul canal : on garde le fichier brut de son cote, et on ne
    // combine que lorsque les DEUX sont la.
    if let Some(cote) = cote {
        let chemin_cote = ir_dir.join(format!("zone_{zone_id}_{cote}.wav"));
        if let Err(e) = std::fs::write(&chemin_cote, &body) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("write IR: {e}")})),
            )
                .into_response();
        }
        let gauche = ir_dir.join(format!("zone_{zone_id}_L.wav"));
        let droite = ir_dir.join(format!("zone_{zone_id}_R.wav"));
        if !gauche.exists() || !droite.exists() {
            let manquant = if gauche.exists() { "right" } else { "left" };
            let cle_attente = if cote == "L" {
                "api.roomCorrection.leftSavedAwaitingRight"
            } else {
                "api.roomCorrection.rightSavedAwaitingLeft"
            };
            return Json(json!({
                "ok": true,
                "zone_id": zone_id,
                "channel": if cote == "L" { "left" } else { "right" },
                "awaiting_channel": manquant,
                "active": false,
                "size_bytes": body.len(),
                "message": crate::message_api::texte(cle_attente, &[]),
                "message_key": cle_attente,
            }))
            .into_response();
        }
        if let Err(e) = tune_core::audio::convolver::Convolver::combiner_en_stereo(
            gauche.to_str().unwrap_or(""),
            droite.to_str().unwrap_or(""),
            ir_path.to_str().unwrap_or(""),
        ) {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    } else if let Err(e) = std::fs::write(&ir_path, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write IR: {e}")})),
        )
            .into_response();
    }
    let ir_path_str = ir_path.to_str().unwrap_or("").to_string();

    // Store the path so the TRANSCODE path picks it up too: the orchestrator's
    // convolver applies the FIR to the bytes served to a network renderer
    // (DLNA/UPnP/AirPlay). Room correction is no longer local-output only.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(&format!("ir_path_{zone_id}"), &ir_path_str)
        .ok();
    // #8761 — un fichier par cadence de MEME cadence que le nouveau principal
    // l'emporterait sur lui (le dernier du jeu gagne) : il est retire.
    if let Some(cadence) = cadence_du_fichier(&ir_path_str) {
        let mut liste =
            tune_core::audio::convolver::JeuDeZone::lire_supplementaires(&settings, zone_id);
        let avant = liste.len();
        liste.retain(|p| cadence_du_fichier(p) != Some(cadence));
        if liste.len() != avant {
            ecrire_supplementaires(&settings, zone_id, &liste);
        }
    }

    // A LOCAL output additionally gets the convolver installed immediately (it
    // runs its own real-time convolver rather than going through the transcode).
    // #8761 — le jeu entier : les fichiers par cadence deja deposes restent.
    if let Err(e) = installer_le_jeu_local(&state, zone_id, &device_id).await {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }

    Json(json!({
        "ok": true,
        "zone_id": zone_id,
        "ir_path": ir_path_str,
        "size_bytes": body.len(),
        "active": true,
        "per_channel": cote.is_some(),
        "applies_to": if is_local { "local" } else { "network (transcode)" },
    }))
    .into_response()
}

/// Le filtre a retirer : une cadence (#8761), ou tout le jeu.
#[derive(Deserialize, Default)]
struct CadenceARetirer {
    rate: Option<u32>,
}

/// `POST /room-correction/ir/clear/{zone_id}` — remove FIR convolution
///
/// #8761 — `?rate=96000` ne retire que le fichier de cette cadence. Si c'est
/// le principal et qu'il reste d'autres fichiers, le premier d'entre eux
/// devient le principal.
async fn clear_ir_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(cible): Query<CadenceARetirer>,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    };

    let device_id = zone.output_device_id.unwrap_or_default();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let ir_dir = dossier_ir();
    let principal_par_defaut = ir_dir.join(format!("zone_{zone_id}.wav"));
    // On n'efface que ce que Tune a lui-meme range dans son dossier `ir/`.
    let effacer = |chemin: &str| {
        if std::path::Path::new(chemin).starts_with(&ir_dir) {
            std::fs::remove_file(chemin).ok();
        }
    };
    let mut supplementaires =
        tune_core::audio::convolver::JeuDeZone::lire_supplementaires(&settings, zone_id);

    if let Some(rate) = cible.rate {
        let principal = settings
            .get(&format!("ir_path_{zone_id}"))
            .ok()
            .flatten()
            .filter(|p| !p.is_empty());
        supplementaires.retain(|p| {
            let garde = cadence_du_fichier(p) != Some(rate);
            if !garde {
                effacer(p);
            }
            garde
        });
        let principal_retire = principal
            .as_deref()
            .is_some_and(|p| cadence_du_fichier(p) == Some(rate));
        if principal_retire {
            if let Some(p) = principal.as_deref() {
                effacer(p);
            }
            if supplementaires.is_empty() {
                settings.delete(&format!("ir_path_{zone_id}")).ok();
            } else {
                let nouveau = supplementaires.remove(0);
                settings.set(&format!("ir_path_{zone_id}"), &nouveau).ok();
            }
        }
        ecrire_supplementaires(&settings, zone_id, &supplementaires);
        if let Err(e) = installer_le_jeu_local(&state, zone_id, &device_id).await {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
        return Json(json!({
            "ok": true,
            "zone_id": zone_id,
            "rate": rate,
            "files": decrire_le_jeu(&settings, zone_id),
        }))
        .into_response();
    }

    // Drop the stored path (network/transcode) + the file for any zone.
    settings.delete(&format!("ir_path_{zone_id}")).ok();
    std::fs::remove_file(&principal_par_defaut).ok();
    // Et les deux depots par canal, sinon un « effacer » suivi d'un depot d'un
    // seul cote ressusciterait l'ancien filtre de l'autre.
    for cote in ["L", "R"] {
        std::fs::remove_file(ir_dir.join(format!("zone_{zone_id}_{cote}.wav"))).ok();
    }
    // #8761 — et tout le jeu par cadence.
    for p in &supplementaires {
        effacer(p);
    }
    for sr in tune_core::audio::convolver::par_cadence::CADENCES_USUELLES {
        std::fs::remove_file(ir_dir.join(format!("zone_{zone_id}_sr{sr}.wav"))).ok();
    }
    ecrire_supplementaires(&settings, zone_id, &[]);

    // A local output also drops its live convolver.
    let _ = installer_le_jeu_local(&state, zone_id, &device_id).await;
    Json(json!({"ok": true, "zone_id": zone_id})).into_response()
}

/// Les options de la convolution d'une zone (#8761).
#[derive(Deserialize)]
struct OptionsDeConvolution {
    /// Rééchantillonner le fichier le plus proche quand aucun fichier du jeu
    /// n'est a la cadence du flux (defaut : oui).
    resample: Option<bool>,
}

/// `POST /room-correction/ir/options/{zone_id}` — `{"resample": false}`
/// refuse le reechantillonnage en repli (la piste joue alors sans
/// correction, comme avant #8761) ; `true` le retablit.
async fn ir_options_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(options): Json<OptionsDeConvolution>,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let Ok(Some(zone)) = zone_repo.get(zone_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "zone not found"})),
        )
            .into_response();
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cle = format!(
        "{}{zone_id}",
        tune_core::audio::convolver::par_cadence::CLE_REECHANTILLONNAGE
    );
    match options.resample {
        Some(false) => {
            settings.set(&cle, "false").ok();
        }
        Some(true) => {
            settings.delete(&cle).ok();
        }
        None => {}
    }
    let device_id = zone.output_device_id.unwrap_or_default();
    let installe = installer_le_jeu_local(&state, zone_id, &device_id).await;
    let resample = settings.get(&cle).ok().flatten().as_deref() != Some("false");
    Json(json!({
        "ok": true,
        "zone_id": zone_id,
        "resample": resample,
        "warning": installe.err(),
    }))
    .into_response()
}

/// `GET /room-correction/ir/status/{zone_id}` — check if FIR is active
async fn ir_status_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone = match zone_repo.get(zone_id) {
        Ok(Some(z)) => z,
        _ => return Json(json!({"active": false, "zone_id": zone_id})).into_response(),
    };

    let device_id = zone.output_device_id.unwrap_or_default();
    let ir_path = SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("ir_path_{zone_id}"))
        .ok()
        .flatten();
    let has_setting = ir_path.as_deref().map(|p| !p.is_empty()).unwrap_or(false);
    // #8761 — le jeu par cadence, le repli, et le fichier REELLEMENT employe
    // pour le dernier flux (sortie locale ou transcodage reseau).
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let files = decrire_le_jeu(&settings, zone_id);
    let resample = tune_core::audio::convolver::JeuDeZone::lire(&settings, zone_id)
        .is_none_or(|j| j.reechantillonner);
    let in_use = ir_path
        .as_deref()
        .filter(|p| !p.is_empty())
        .and_then(tune_core::audio::convolver::par_cadence::dernier_choix)
        .map(|c| {
            json!({
                "stream_sample_rate": c.cadence_flux,
                "file_sample_rate": c.cadence_fichier,
                "path": c.chemin,
                "resampled": c.reechantillonne,
                "reason": c.raison,
                "description": c.decrire(),
            })
        });

    // Local outputs run a live convolver; a network zone applies the IR on the
    // transcode path, so the stored path is the source of truth there.
    #[cfg(feature = "local-audio")]
    {
        if device_id.starts_with("local:") {
            let outputs = state.outputs.lock().await;
            if let Some(output) = outputs.get(&device_id) {
                let output = output.lock().await;
                if let Some(local) = output
                    .as_any()
                    .downcast_ref::<tune_core::outputs::local::LocalOutput>()
                {
                    return Json(json!({
                        "active": local.has_convolver() || has_setting,
                        "zone_id": zone_id,
                        "ir_path": ir_path,
                        "files": files,
                        "resample": resample,
                        "in_use": in_use,
                    }))
                    .into_response();
                }
            }
        }
    }
    let _ = &device_id;
    Json(json!({
        "active": has_setting,
        "zone_id": zone_id,
        "ir_path": ir_path,
        "files": files,
        "resample": resample,
        "in_use": in_use,
    }))
    .into_response()
}
