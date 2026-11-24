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
    // Au démarrage : les dépôts provisoires d'un import interrompu (arrêt
    // brutal) ne restent pas dans `ir/`.
    balayer_les_depots_provisoires(&dossier_ir());
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
        // D1 (TESS) : un profil posé à la main par l'API est traité comme
        // une mesure (Premium) ; le corps ne peut pas se dire « estimation ».
        origin: Some(tune_core::room_correction::ORIGINE_MESUREE.to_owned()),
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
    /// TESS T09a — `1`/`true` : le corps est une ARCHIVE `.zip` (de `.wav`,
    /// ou de `.cfg` Convolver avec leurs réponses) qui remplace le jeu
    /// entier. Un corps qui commence par la signature zip est traité de même.
    archive: Option<String>,
    /// Le nom du fichier déposé (affiché « Filter set: … »).
    name: Option<String>,
}

/// Réglage : le nom du dernier dépôt d'un jeu (`.zip` ou `.wav`) de la zone.
const CLE_NOM_DU_JEU: &str = "ir_set_name_";

/// Un nom de dépôt affichable : sans chemin, sans caractère de contrôle,
/// 128 caractères au plus.
fn nom_de_depot(nom: Option<&str>) -> Option<String> {
    let nom = nom?.replace('\\', "/");
    let base = nom.rsplit('/').next()?.trim();
    let propre: String = base.chars().filter(|c| !c.is_control()).take(128).collect();
    (!propre.is_empty()).then_some(propre)
}

/// Un seul import d'archive à la fois sur le serveur : la mémoire d'un import
/// reste bornée par les plafonds de `import_ri`, pas multipliée. Pris DANS la
/// tâche bloquante : un client qui abandonne sa requête ne libère rien avant
/// la fin de l'import.
static IMPORT_UNIQUE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Un verrou par zone pour toute écriture de son jeu de réponses (dépôt,
/// import, retrait, options) : deux requêtes sur la même zone ne
/// s'entrelacent pas.
fn verrou_de_zone(zone_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static VERROUS: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    > = std::sync::LazyLock::new(Default::default);
    VERROUS
        .lock()
        .map(|mut m| m.entry(zone_id).or_default().clone())
        .unwrap_or_default()
}

/// Retire les dépôts provisoires (`depot_zone_*.tmp`) laissés dans `ir/`.
fn balayer_les_depots_provisoires(ir_dir: &std::path::Path) {
    let Ok(entrees) = std::fs::read_dir(ir_dir) else {
        return;
    };
    for e in entrees.flatten() {
        let nom = e.file_name();
        let nom = nom.to_string_lossy();
        if nom.starts_with("depot_zone_") && nom.ends_with(".tmp") {
            std::fs::remove_file(e.path()).ok();
        }
    }
}

/// Ce qu'un import réussi rend.
struct JeuPose {
    jeu: tune_core::audio::import_ri::JeuImporte,
    chemin_principal: String,
}

/// Refus d'un import, au format de l'API.
fn refus_d_import(raison: &str, detail: &str) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": detail,
            "code": "archive_refused",
            "reason": raison,
            "message_key": format!("api.roomCorrection.archive.{raison}"),
        })),
    )
        .into_response()
}

fn erreur_d_ecriture(e: impl std::fmt::Display) -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": format!("write IR: {e}")})),
    )
        .into_response()
}

/// Les réglages que lit et écrit l'import : le dépôt des réglages, ou un faux
/// qui refuse l'écriture dans les témoins.
trait ReglagesImport {
    fn lire(&self, cle: &str) -> Result<Option<String>, String>;
    fn ecrire(&self, cle: &str, valeur: &str) -> Result<(), String>;
    fn effacer(&self, cle: &str) -> Result<(), String>;
}

impl ReglagesImport for SettingsRepo {
    fn lire(&self, cle: &str) -> Result<Option<String>, String> {
        self.get(cle)
    }
    fn ecrire(&self, cle: &str, valeur: &str) -> Result<(), String> {
        self.set(cle, valeur)
    }
    fn effacer(&self, cle: &str) -> Result<(), String> {
        self.delete(cle)
    }
}

/// Le nom d'une COPIE DE CÔTÉ (un fichier en place, écarté le temps d'un
/// import) : distinct des dépôts provisoires `depot_zone_*.tmp`, que le
/// balayage du démarrage efface. Une copie qui n'a pas pu être remise reste
/// dans `ir/`, intacte, pour une reprise à la main.
fn chemin_de_cote(ir_dir: &std::path::Path, zone_id: i64) -> std::path::PathBuf {
    let provisoire = chemin_provisoire(ir_dir, zone_id);
    let nom = provisoire
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .replacen("depot_zone_", "cote_zone_", 1)
        .replace(".tmp", ".bak");
    ir_dir.join(nom)
}

/// Retour arrière d'un import qui échoue après la mise en place : les
/// nouveaux fichiers sont retirés, les fichiers mis de côté remis, les
/// provisoires effacés. Tout ce qui n'a pas pu être remis (fichier ou
/// réglage) est SIGNALÉ (journal, et `restore_incomplete` dans la réponse) ;
/// une copie de côté non remise n'est jamais effacée.
fn remettre_en_place(
    zone_id: i64,
    en_place: &[std::path::PathBuf],
    de_cote: &[(std::path::PathBuf, std::path::PathBuf)],
    poses: &[(std::path::PathBuf, std::path::PathBuf)],
    mut non_remis: Vec<String>,
    cause: impl std::fmt::Display,
) -> axum::response::Response {
    for f in en_place {
        std::fs::remove_file(f).ok();
    }
    for (cote, final_) in de_cote {
        if let Err(e) = std::fs::rename(cote, final_) {
            tracing::error!(
                zone_id,
                copie = %cote.display(),
                fichier = %final_.display(),
                error = %e,
                "convolver_import_remise_en_place_echouee"
            );
            non_remis.push(cote.display().to_string());
        }
    }
    for (p, _) in poses {
        std::fs::remove_file(p).ok();
    }
    let mut corps = json!({"error": format!("write IR: {cause}")});
    if !non_remis.is_empty() {
        corps["restore_incomplete"] = json!(non_remis);
    }
    (StatusCode::INTERNAL_SERVER_ERROR, Json(corps)).into_response()
}

/// L'import entier, BLOQUANT (décompression, décodage, fichiers, réglages),
/// sous le verrou global d'import et celui de la zone.
///
/// Ordre : (0) les réglages d'avant lus (une lecture qui ÉCHOUE arrête tout
/// avant le moindre fichier) ; (1) nouveaux fichiers écrits en provisoire ;
/// (2) les fichiers en place de même nom mis de côté (`cote_zone_*.bak`),
/// puis les nouveaux renommés à leur place — au moindre échec, tout est
/// remis comme avant ; (3) les RÉGLAGES écrits — au moindre échec, les
/// réglages d'avant et les fichiers sont remis ; (4) seulement alors,
/// l'ancien jeu qui ne sert plus et les copies de côté retirés. Un arrêt
/// brutal ne laisse que des `depot_zone_*.tmp`, balayés au démarrage, et
/// d'éventuelles copies de côté, jamais balayées.
fn importer_bloquant(
    corps: &[u8],
    nom: Option<String>,
    ir_dir: &std::path::Path,
    zone_id: i64,
    settings: &dyn ReglagesImport,
) -> Result<JeuPose, axum::response::Response> {
    use tune_core::audio::import_ri;
    let nom_lu = nom.clone().unwrap_or_else(|| "filtre.wav".into());
    let jeu = import_ri::importer(corps, &nom_lu).map_err(|refus| {
        tracing::warn!(zone_id, raison = refus.raison, detail = %refus.detail, "convolver_import_refuse");
        refus_d_import(refus.raison, &refus.detail)
    })?;
    let principal = jeu
        .indice_principal()
        .ok_or_else(|| refus_d_import("no_filter", "no impulse response in the upload"))?;
    let cle_principal = format!("ir_path_{zone_id}");
    let cle_jeu = format!(
        "{}{zone_id}",
        tune_core::audio::convolver::par_cadence::CLE_JEU
    );
    let cle_nom = format!("{CLE_NOM_DU_JEU}{zone_id}");
    // (0) Les réglages d'avant. Une lecture qui échoue n'est pas une clé
    // absente : rien n'est touché.
    let mut reglages_avant: Vec<(String, Option<String>)> = Vec::new();
    for cle in [&cle_principal, &cle_jeu, &cle_nom] {
        let valeur = settings.lire(cle).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("settings read: {e}")})),
            )
                .into_response()
        })?;
        reglages_avant.push((cle.clone(), valeur));
    }
    let mut anciens: Vec<String> = reglages_avant[1]
        .1
        .as_deref()
        .and_then(|v| serde_json::from_str::<Vec<String>>(v).ok())
        .unwrap_or_default();
    if let Some(p) = reglages_avant[0].1.clone().filter(|p| !p.is_empty()) {
        anciens.push(p);
    }
    std::fs::create_dir_all(ir_dir).ok();
    // (1) Provisoires.
    let mut poses: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    for (i, r) in jeu.reponses.iter().enumerate() {
        let provisoire = chemin_provisoire(ir_dir, zone_id);
        let final_ = if i == principal {
            ir_dir.join(format!("zone_{zone_id}.wav"))
        } else {
            ir_dir.join(format!("zone_{zone_id}_sr{}.wav", r.sample_rate))
        };
        if let Err(e) = tune_core::audio::convolver::ecrire_ri_wav(
            provisoire.to_str().unwrap_or(""),
            &r.taps,
            r.sample_rate,
        ) {
            std::fs::remove_file(&provisoire).ok();
            for (p, _) in &poses {
                std::fs::remove_file(p).ok();
            }
            return Err(erreur_d_ecriture(e));
        }
        poses.push((provisoire, final_));
    }
    // (2) Mise de côté puis mise en place, avec retour arrière.
    let mut de_cote: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let mut en_place: Vec<std::path::PathBuf> = Vec::new();
    let mut echec = None;
    for (provisoire, final_) in &poses {
        if final_.exists() {
            let cote = chemin_de_cote(ir_dir, zone_id);
            if let Err(e) = std::fs::rename(final_, &cote) {
                echec = Some(e);
                break;
            }
            de_cote.push((cote, final_.clone()));
        }
        if let Err(e) = std::fs::rename(provisoire, final_) {
            echec = Some(e);
            break;
        }
        en_place.push(final_.clone());
    }
    if let Some(e) = echec {
        return Err(remettre_en_place(
            zone_id,
            &en_place,
            &de_cote,
            &poses,
            Vec::new(),
            e,
        ));
    }
    let chemins: Vec<String> = poses
        .iter()
        .map(|(_, f)| f.to_str().unwrap_or("").to_string())
        .collect();
    let chemin_principal = chemins[principal].clone();
    let supplementaires: Vec<String> = chemins
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != principal)
        .map(|(_, c)| c.clone())
        .collect();
    // (3) Réglages : un échec fait échouer l'import et remet l'ancien jeu.
    let ecrits = settings
        .ecrire(&cle_principal, &chemin_principal)
        .and_then(|_| {
            if supplementaires.is_empty() {
                settings.effacer(&cle_jeu)
            } else {
                settings.ecrire(
                    &cle_jeu,
                    &serde_json::to_string(&supplementaires).unwrap_or_default(),
                )
            }
        })
        .and_then(|_| match &nom {
            Some(n) => settings.ecrire(&cle_nom, n),
            None => settings.effacer(&cle_nom),
        });
    if let Err(e) = ecrits {
        let mut non_remis = Vec::new();
        for (cle, valeur) in &reglages_avant {
            let remis = match valeur {
                Some(v) => settings.ecrire(cle, v),
                None => settings.effacer(cle),
            };
            if let Err(e2) = remis {
                tracing::error!(zone_id, cle, error = %e2, "convolver_import_reglage_non_remis");
                non_remis.push(format!("setting {cle}"));
            }
        }
        return Err(remettre_en_place(
            zone_id,
            &en_place,
            &de_cote,
            &poses,
            non_remis,
            format!("settings: {e}"),
        ));
    }
    // (4) L'ancien jeu qui ne sert plus, et les copies de côté.
    for (cote, _) in &de_cote {
        std::fs::remove_file(cote).ok();
    }
    for p in anciens {
        if chemins.contains(&p) {
            continue;
        }
        let chemin = std::path::Path::new(&p);
        if tune_core::audio::convolver::par_cadence::chemin_du_jeu_effacable(
            chemin, ir_dir, zone_id,
        ) {
            std::fs::remove_file(chemin).ok();
        }
    }
    for cote in ["L", "R"] {
        std::fs::remove_file(ir_dir.join(format!("zone_{zone_id}_{cote}.wav"))).ok();
    }
    Ok(JeuPose {
        jeu,
        chemin_principal,
    })
}

/// TESS T09a — `POST /room-correction/ir/upload/{zone}?archive=1&name=…` :
/// importer un jeu (`.zip` de `.wav`, `.zip` de `.cfg` Convolver, ou un
/// `.wav`) qui REMPLACE le jeu de la zone. Chaque réponse est réécrite par
/// Tune sous `zone_{id}.wav` (la principale) ou `zone_{id}_sr{cadence}.wav`.
/// Un refus laisse la zone telle qu'elle était.
async fn importer_le_jeu(
    state: &AppState,
    zone_id: i64,
    device_id: &str,
    is_local: bool,
    body: axum::body::Bytes,
    nom: Option<&str>,
) -> axum::response::Response {
    let nom = nom_de_depot(nom);
    let ir_dir = dossier_ir();
    let backend = state.backend.clone();
    let verrou = verrou_de_zone(zone_id);
    let nom_tache = nom.clone();
    // Tout l'import, verrous compris, dans la tâche bloquante : abandonner la
    // requête n'interrompt ni ne déverrouille rien en cours de route.
    let pose = tokio::task::spawn_blocking(move || {
        let _import = IMPORT_UNIQUE.lock().unwrap_or_else(|e| e.into_inner());
        let _zone = verrou.blocking_lock_owned();
        let settings = SettingsRepo::with_backend(backend);
        importer_bloquant(&body, nom_tache, &ir_dir, zone_id, &settings)
    })
    .await;
    let JeuPose {
        jeu,
        chemin_principal,
    } = match pose {
        Ok(Ok(p)) => p,
        Ok(Err(reponse)) => return reponse,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("import: {e}")})),
            )
                .into_response();
        }
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let avertissement = installer_le_jeu_local(state, zone_id, device_id)
        .await
        .err();
    tracing::info!(
        zone_id,
        set_name = nom.as_deref().unwrap_or(""),
        reponses = jeu.reponses.len(),
        ecartes = jeu.ecartes.len(),
        "convolver_jeu_importe"
    );
    Json(json!({
        "ok": true,
        "zone_id": zone_id,
        "set_name": nom,
        "ir_path": chemin_principal,
        "files": decrire_le_jeu(&settings, zone_id),
        "imported": jeu.reponses.iter().map(|r| json!({
            "name": r.nom,
            "sample_rate": r.sample_rate,
            "channels": r.taps.len(),
            "taps": r.taps.first().map(Vec::len).unwrap_or(0),
            "source": r.origine,
        })).collect::<Vec<_>>(),
        "ignored": jeu.ecartes.iter().map(|(n, r)| json!({"name": n, "reason": r})).collect::<Vec<_>>(),
        "active": true,
        "warning": avertissement,
        "applies_to": if is_local { "local" } else { "network (transcode)" },
    }))
    .into_response()
}

fn vrai(v: Option<&str>) -> bool {
    matches!(
        v.map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("oui")
    )
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
        .map(
            |(i, chemin)| match tune_core::audio::convolver::Convolver::read_ir_taps(&chemin) {
                Ok((taps, sr)) => json!({
                    "path": chemin,
                    "sample_rate": sr,
                    "channels": taps.len(),
                    "taps": taps.first().map(Vec::len).unwrap_or(0),
                    "main": i == 0,
                }),
                Err(e) => json!({"path": chemin, "main": i == 0, "error": e}),
            },
        )
        .collect()
}

/// Cadence d'un fichier de reponse impulsionnelle, `None` s'il est illisible.
fn cadence_du_fichier(chemin: &str) -> Option<u32> {
    tune_core::audio::convolver::Convolver::read_ir_taps(chemin)
        .ok()
        .map(|(_, sr)| sr)
}

/// #8761 — valider un fichier depose AVANT de l'enregistrer : lisible, et
/// dans les bornes (frequence de 8 a 768 kHz, canaux, prises). Rend sa
/// frequence.
fn valider_le_depot(chemin: &std::path::Path) -> Result<u32, String> {
    tune_core::audio::convolver::ConvolverConfig::from_wav(chemin.to_str().unwrap_or(""))
        .map(|c| c.sample_rate())
}

/// #8761 — un nom provisoire UNIQUE pour un depot : deux depots simultanes
/// sur la meme zone ne s'ecrasent pas.
fn chemin_provisoire(ir_dir: &std::path::Path, zone_id: i64) -> std::path::PathBuf {
    static NUMERO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NUMERO.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    ir_dir.join(format!(
        "depot_zone_{zone_id}_{}_{t}_{n}.tmp",
        std::process::id()
    ))
}

fn ecrire_supplementaires(settings: &SettingsRepo, zone_id: i64, liste: &[String]) {
    let cle = format!(
        "{}{zone_id}",
        tune_core::audio::convolver::par_cadence::CLE_JEU
    );
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
    let provisoire = chemin_provisoire(&ir_dir, zone_id);
    if let Err(e) = std::fs::write(&provisoire, body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write IR: {e}")})),
        )
            .into_response();
    }
    let cadence = match valider_le_depot(&provisoire) {
        Ok(c) => c,
        Err(e) => {
            std::fs::remove_file(&provisoire).ok();
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
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
    _admin: crate::auth::RequireAdmin,
    Path(zone_id): Path<i64>,
    Query(canal): Query<CanalDuFiltre>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Mêmes gardes que les profils de correction : administrateur, puis
    // Premium (`RoomCorrection`).
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return resp;
    }
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

    // TESS T09a — une archive (demandée, ou reconnue à sa signature) remplace
    // le jeu entier.
    if canal.channel.as_deref().is_none_or(str::is_empty)
        && (vrai(canal.archive.as_deref())
            || tune_core::audio::import_ri::est_une_archive_zip(&body))
    {
        return importer_le_jeu(
            &state,
            zone_id,
            &device_id,
            is_local,
            body,
            canal.name.as_deref(),
        )
        .await;
    }
    // Les autres dépôts : verrou de la zone (l'import le prend lui-même, dans
    // sa tâche bloquante).
    let verrou = verrou_de_zone(zone_id);
    let _zone_verrouillee = verrou.lock().await;

    if vrai(canal.per_rate.as_deref()) && canal.channel.as_deref().is_none_or(str::is_empty) {
        return ajouter_au_jeu(&state, zone_id, &device_id, is_local, &body).await;
    }

    // Persist the IR to disk.
    let ir_dir = dossier_ir();
    std::fs::create_dir_all(&ir_dir).ok();
    let ir_path = ir_dir.join(format!("zone_{zone_id}.wav"));
    let provisoire = chemin_provisoire(&ir_dir, zone_id);
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
            provisoire.to_str().unwrap_or(""),
        ) {
            std::fs::remove_file(&provisoire).ok();
            return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
        }
    } else if let Err(e) = std::fs::write(&provisoire, &body) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("write IR: {e}")})),
        )
            .into_response();
    }
    // #8761 — valider AVANT de remplacer le fichier et d'ecrire le reglage :
    // un fichier refuse ne laisse ni reglage ni ancien filtre ecrase.
    if let Err(e) = valider_le_depot(&provisoire) {
        std::fs::remove_file(&provisoire).ok();
        return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
    }
    if let Err(e) = std::fs::rename(&provisoire, &ir_path) {
        std::fs::remove_file(&provisoire).ok();
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
    // TESS T09a — un nouveau principal : le nom affiché suit le dépôt.
    match nom_de_depot(canal.name.as_deref()) {
        Some(n) => settings.set(&format!("{CLE_NOM_DU_JEU}{zone_id}"), &n).ok(),
        None => settings.delete(&format!("{CLE_NOM_DU_JEU}{zone_id}")).ok(),
    };
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
    _admin: crate::auth::RequireAdmin,
    Path(zone_id): Path<i64>,
    Query(cible): Query<CadenceARetirer>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return resp;
    }
    let verrou = verrou_de_zone(zone_id);
    let _zone_verrouillee = verrou.lock().await;
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
    // Validation stricte des chemins : on n'efface que ce que Tune a lui-meme
    // range dans son dossier `ir/` pour CETTE zone (dossier parent compare
    // apres resolution, nom exact `zone_{id}.wav` ou `zone_{id}_sr{N}.wav`).
    let effacer = |chemin: &str| {
        let p = std::path::Path::new(chemin);
        if tune_core::audio::convolver::par_cadence::chemin_du_jeu_effacable(p, &ir_dir, zone_id) {
            std::fs::remove_file(p).ok();
        } else {
            tracing::warn!(
                zone_id,
                chemin,
                "convolver_ir_suppression_refusee_hors_dossier"
            );
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
    settings.delete(&format!("{CLE_NOM_DU_JEU}{zone_id}")).ok();
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
    // Tous les fichiers par cadence de la zone, quelle que soit leur cadence.
    if let Ok(entrees) = std::fs::read_dir(&ir_dir) {
        for e in entrees.flatten() {
            let nom = e.file_name();
            let nom = nom.to_string_lossy();
            if nom.contains("_sr")
                && tune_core::audio::convolver::par_cadence::nom_du_jeu_valide(&nom, Some(zone_id))
            {
                std::fs::remove_file(e.path()).ok();
            }
        }
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
    _admin: crate::auth::RequireAdmin,
    Path(zone_id): Path<i64>,
    Json(options): Json<OptionsDeConvolution>,
) -> impl IntoResponse {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, Feature::RoomCorrection).await
    {
        return resp;
    }
    let verrou = verrou_de_zone(zone_id);
    let _zone_verrouillee = verrou.lock().await;
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
    // TESS T09a — le nom du dernier dépôt (« Filter set: … »).
    let set_name = settings
        .get(&format!("{CLE_NOM_DU_JEU}{zone_id}"))
        .ok()
        .flatten()
        .filter(|_| has_setting);
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
                        "set_name": set_name,
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
        "set_name": set_name,
    }))
    .into_response()
}

#[cfg(test)]
mod import_t09a_tests {
    //! TESS T09a — retour arrière de l'import : réglages refusés, remise en
    //! place incomplète, lecture des réglages en échec, copies de côté
    //! épargnées par le balayage du démarrage.
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Des réglages en mémoire qui peuvent refuser la lecture ou l'écriture.
    #[derive(Default)]
    struct FauxReglages {
        valeurs: Mutex<HashMap<String, String>>,
        refuser_lecture: bool,
        refuser_ecriture: bool,
    }

    impl ReglagesImport for FauxReglages {
        fn lire(&self, cle: &str) -> Result<Option<String>, String> {
            if self.refuser_lecture {
                return Err("base indisponible".into());
            }
            Ok(self.valeurs.lock().unwrap().get(cle).cloned())
        }
        fn ecrire(&self, cle: &str, valeur: &str) -> Result<(), String> {
            if self.refuser_ecriture {
                return Err("écriture refusée".into());
            }
            self.valeurs
                .lock()
                .unwrap()
                .insert(cle.into(), valeur.into());
            Ok(())
        }
        fn effacer(&self, cle: &str) -> Result<(), String> {
            if self.refuser_ecriture {
                return Err("écriture refusée".into());
            }
            self.valeurs.lock().unwrap().remove(cle);
            Ok(())
        }
    }

    /// Un `.wav` mono flottant 32 bits à 48 kHz.
    fn wav() -> Vec<u8> {
        let data: Vec<u8> = [0.5f32, 0.25, 0.125]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&3u16.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&48_000u32.to_le_bytes());
        w.extend_from_slice(&(48_000u32 * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&32u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        w
    }

    async fn corps(r: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = r.status();
        let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&octets).unwrap_or(json!(null)),
        )
    }

    fn fichiers(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn le_balayage_du_demarrage_epargne_les_copies_de_cote() {
        let d = tune_core::test_scratch::scratch_dir("t09a-balayage");
        let depot = chemin_provisoire(d.path(), 7);
        let cote = chemin_de_cote(d.path(), 7);
        std::fs::write(&depot, b"provisoire").unwrap();
        std::fs::write(&cote, b"ancien filtre").unwrap();
        balayer_les_depots_provisoires(d.path());
        assert!(!depot.exists(), "le dépôt provisoire est balayé");
        assert!(cote.exists(), "la copie de côté survit au balayage");
        assert_eq!(std::fs::read(&cote).unwrap(), b"ancien filtre");
    }

    #[tokio::test]
    async fn un_echec_d_ecriture_des_reglages_remet_l_ancien_jeu() {
        let d = tune_core::test_scratch::scratch_dir("t09a-reglages-refuses");
        let ancien = d.path().join("zone_7.wav");
        std::fs::write(&ancien, b"ANCIEN").unwrap();
        let reglages = FauxReglages {
            refuser_ecriture: true,
            ..Default::default()
        };
        reglages
            .valeurs
            .lock()
            .unwrap()
            .insert("ir_path_7".into(), ancien.to_str().unwrap().into());
        let r = importer_bloquant(&wav(), Some("f.wav".into()), d.path(), 7, &reglages)
            .err()
            .expect("l'import échoue");
        let (status, v) = corps(r).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{v}");
        // L'ancien fichier est remis, rien d'autre ne traîne.
        assert_eq!(std::fs::read(&ancien).unwrap(), b"ANCIEN");
        assert_eq!(fichiers(d.path()), ["zone_7.wav"]);
        // Les réglages n'ont pas bougé ; la remise d'un réglage refusée est
        // SIGNALÉE (`restore_incomplete`), pas avalée.
        assert_eq!(
            reglages
                .valeurs
                .lock()
                .unwrap()
                .get("ir_path_7")
                .map(String::as_str),
            ancien.to_str()
        );
        let non_remis = v["restore_incomplete"]
            .as_array()
            .expect("restore_incomplete");
        assert!(!non_remis.is_empty(), "{v}");
        // Contre-épreuve : les mêmes réglages acceptés, l'import passe.
        let reglages = FauxReglages::default();
        assert!(importer_bloquant(&wav(), Some("f.wav".into()), d.path(), 7, &reglages).is_ok());
        assert_ne!(std::fs::read(&ancien).unwrap(), b"ANCIEN");
    }

    #[tokio::test]
    async fn une_remise_en_place_ratee_est_signalee_et_la_copie_gardee() {
        let d = tune_core::test_scratch::scratch_dir("t09a-remise-ratee");
        let cote = chemin_de_cote(d.path(), 7);
        std::fs::write(&cote, b"ancien filtre").unwrap();
        // La destination est dans un dossier qui n'existe pas : le renommage
        // de remise échoue.
        let introuvable = d.path().join("absent").join("zone_7.wav");
        let r = remettre_en_place(
            7,
            &[],
            &[(cote.clone(), introuvable)],
            &[],
            Vec::new(),
            "essai",
        );
        let (status, v) = corps(r).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let non_remis = v["restore_incomplete"]
            .as_array()
            .expect("restore_incomplete");
        assert_eq!(non_remis.len(), 1, "{v}");
        assert!(cote.exists(), "la copie de côté n'est pas effacée");
    }

    #[tokio::test]
    async fn une_lecture_des_reglages_en_echec_ne_touche_a_rien() {
        let d = tune_core::test_scratch::scratch_dir("t09a-lecture-refusee");
        let reglages = FauxReglages {
            refuser_lecture: true,
            ..Default::default()
        };
        let r = importer_bloquant(&wav(), None, d.path(), 7, &reglages)
            .err()
            .expect("l'import échoue");
        let (status, _) = corps(r).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            !d.path().exists() || fichiers(d.path()).is_empty(),
            "aucun fichier écrit"
        );
    }
}
