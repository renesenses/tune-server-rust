#![allow(dead_code)]
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::routes::playlists::{owned_or_404, owned_or_404_response};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/services", get(list_services))
        .route("/transfer", post(transfer_playlist))
        .route("/history", get(transfer_history))
        .route("/history/{id}", get(transfer_history_detail))
        .route("/links", get(list_links).post(create_link))
        .route("/links/{id}", axum::routing::delete(delete_link))
        .route("/links/{id}/sync", post(sync_link))
        .route("/backup", post(backup_playlists))
        .route("/backups", get(list_backups))
        .route("/backups/{id}", get(get_backup).delete(delete_backup))
        .route("/backups/{id}/restore", post(restore_backup))
        .route("/merge", post(merge_playlists))
        .route("/export", post(export_playlists))
        .route("/import", post(import_playlists))
        .route(
            "/playlists/{service}/{playlist_id}",
            axum::routing::delete(delete_service_playlist),
        )
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn load_json_setting(settings: &SettingsRepo, key: &str) -> Vec<Value> {
    settings
        .get(key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_json_setting(settings: &SettingsRepo, key: &str, data: &[Value]) {
    settings
        .set(
            key,
            &serde_json::to_string(data).unwrap_or_else(|_| "[]".into()),
        )
        .ok();
}

fn default_true() -> bool {
    true
}

fn next_id(items: &[Value]) -> i64 {
    items
        .iter()
        .filter_map(|v| v.get("id").and_then(|id| id.as_i64()))
        .max()
        .unwrap_or(0)
        + 1
}

fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Simple ISO-8601 UTC timestamp without chrono dependency
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Approximate date from days since epoch (good enough for timestamps)
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(days_since_epoch: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// List streaming services with their playlist capabilities.
async fn list_services(State(state): State<AppState>) -> Json<Value> {
    let registry = state.services.lock().await;
    let status_all = registry.status_all().await;

    let mut services = serde_json::Map::new();
    for svc_status in &status_all {
        let name = svc_status
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let authenticated = svc_status
            .get("authenticated")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let (write, delete) = if let Some(svc) = registry.get(name) {
            let svc = svc.read().await;
            (svc.supports_write(), svc.supports_playlist_delete())
        } else {
            (false, false)
        };
        services.insert(
            name.to_string(),
            json!({
                "authenticated": authenticated,
                "supports_write": write,
                "supports_delete": delete,
            }),
        );
    }
    drop(registry);
    services.insert(
        "local".to_string(),
        json!({ "authenticated": true, "supports_write": true, "supports_delete": true }),
    );
    Json(json!(services))
}

/// DELETE /playlist-manager/playlists/{service}/{playlist_id}
///
/// Supprime une playlist CHEZ le service de streaming. Les playlists locales
/// passent par `DELETE /playlists/{id}` ; ici on ne parle qu'aux services.
///
/// Le geste est définitif chez le service : on refuse tout de suite (501) si
/// le service ne sait pas le faire, plutôt que de laisser l'appel partir et
/// rendre une erreur opaque.
async fn delete_service_playlist(
    State(state): State<AppState>,
    Path((service, playlist_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if service == "local" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "use DELETE /playlists/{id} for local playlists" })),
        )
            .into_response();
    }

    let registry = state.services.lock().await;
    let Some(svc_arc) = registry.get(&service) else {
        drop(registry);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown service: {service}") })),
        )
            .into_response();
    };
    drop(registry);

    let svc = svc_arc.read().await;
    if !svc.supports_playlist_delete() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": format!("{service} does not support playlist deletion") })),
        )
            .into_response();
    }

    match svc.delete_playlist(&playlist_id).await {
        Ok(()) => {
            // 🔴 La liste des playlists du service est MÉMORISÉE 2 minutes.
            // Sans cet oubli, la carte supprimée resterait affichée, et un
            // rechargement de l'écran la ferait « revenir ».
            crate::routes::streaming::purge_contenu_utilisateur(&service);
            tracing::info!(%service, %playlist_id, "service_playlist_deleted");
            Json(json!({ "deleted": true, "service": service, "playlist_id": playlist_id }))
                .into_response()
        }
        Err(e) => {
            tracing::warn!(%service, %playlist_id, error = %e, "service_playlist_delete_failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Transfer — #4741 : UN seul moteur, celui du greffon « Playlists converter »
// ---------------------------------------------------------------------------
//
// Trois moteurs transféraient une playlist d'un service à l'autre : cette
// route (son propre appariement, sa propre création), `POST /playlist-transfer/*`
// (`tune_core::playlist_transfer`, sans aucun appelant) et
// `POST /playlist-manager/batch-transfer` (une coquille qui écrivait « started »
// dans l'historique et ne transférait rien). Les deux derniers sont retirés.
// Celle-ci GARDE son contrat — le web, l'appli iPad et l'appli Flutter
// l'appellent — mais n'a plus de moteur : elle passe la demande au greffon
// (`/apercu`, puis `/transfert` avec accord) et rend sa réponse sous la forme
// d'avant, enrichie du rapport par titre.
//
// Le geste de l'utilisateur sur cette route EST l'accord : elle écrivait déjà
// sans aperçu. L'aperçu sans écriture reste `dry_run: true`.

/// L'identifiant de manifeste du greffon — le seul moteur de transfert.
const GREFFON_CONVERTISSEUR: &str = "playlists-converter";

/// Le corps historique. Les champs que le moteur unique ne connaît plus —
/// `match_threshold`, `include_approximate`, `create_on_target` — sont
/// acceptés et ignorés (serde ignore les champs inconnus) : la règle
/// d'appariement est celle du greffon, ISRC puis titre + artiste + durée à
/// ±3 s, sans seuil réglable ni appariement approximatif.
#[derive(Deserialize)]
struct TransferRequest {
    source_service: String,
    source_playlist_id: String,
    target_service: String,
    /// `target_name` est ce que TOUS les clients envoient ; l'ancienne route
    /// ne lisait que `name` (`#[serde(rename)]`) et ignorait donc le nom choisi
    /// dans la fenêtre d'import. Les deux sont lus.
    #[serde(default, alias = "name")]
    target_name: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

async fn transfer_playlist(
    State(state): State<AppState>,
    profile: ActiveProfile,
    headers: axum::http::HeaderMap,
    Json(body): Json<TransferRequest>,
) -> axum::response::Response {
    if body.source_service == "local" {
        // La source locale se désigne par son id, et les ids de playlists sont
        // de petits entiers séquentiels : sans ce refus, n'importe quel profil
        // recopiait la playlist du voisin chez lui, ou la déversait sur son
        // propre compte de service (#2794, #3073). Le refus se joue ICI, avant
        // le greffon, et ne laisse aucune trace.
        let repo = PlaylistRepo::with_backend(state.backend.clone());
        let playlist_id: i64 = body.source_playlist_id.parse().unwrap_or(0);
        let source = match owned_or_404_response(&repo, playlist_id, profile.id()) {
            Ok(pl) => pl,
            Err(r) => return r,
        };
        if body.target_service == "local" {
            return copier_dans_la_bibliotheque(
                &repo,
                playlist_id,
                &source.name,
                body.target_name.as_deref(),
                body.dry_run,
                profile.id(),
            );
        }
    }
    // Bertrand, 07/10/2026 : le transfert ENTRE services (et l'import d'un
    // service dans la bibliothèque) est Premium, comme les routes du greffon.
    // La copie « bibliothèque → bibliothèque » ci-dessus reste gratuite : ce
    // n'est pas un transfert, aucun service n'est touché.
    if let Err(refus) = refus_premium_du_transfert(&state, &headers).await {
        return refus;
    }
    transferer_par_le_greffon(&state, &body, profile.id()).await
}

/// `402 premium_required` — le refus commun des routes payantes
/// (`premium_guard`), dans la langue de l'application, plus la `raison` qui
/// dit à l'utilisateur ce qui reste gratuit.
async fn refus_premium_du_transfert(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<(), axum::response::Response> {
    let droit = tune_core::license::Feature::PlaylistTransfer;
    if state.license.check_feature(droit).await {
        return Ok(());
    }
    tracing::info!("playlist_transfer_premium_refuse");
    let lang = crate::i18n::lang_from_header(headers);
    let mut corps = crate::premium_guard::corps_du_refus(droit, &lang);
    corps["raison"] = json!(
        "Le transfert d'une playlist entre services, ou d'un service vers la \
         bibliothèque, est réservé à Tune Premium. Dupliquer une playlist de la \
         bibliothèque reste gratuit."
    );
    Err((StatusCode::PAYMENT_REQUIRED, Json(corps)).into_response())
}

/// « Bibliothèque → bibliothèque » n'est pas un transfert : rien n'est à
/// apparier, c'est une copie (le « Dupliquer » de l'appli iPad passe par ici).
/// Elle reprend `POST /playlists/{id}/duplicate`, titres de service compris.
fn copier_dans_la_bibliotheque(
    repo: &PlaylistRepo,
    source_id: i64,
    source_nom: &str,
    nom: Option<&str>,
    dry_run: bool,
    profil: i64,
) -> axum::response::Response {
    let nom = nom
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("{source_nom} (copy)"));
    let total = repo.get_entries(source_id).map(|e| e.len()).unwrap_or(0);
    let (cible, copiees) = if dry_run {
        (None, total)
    } else {
        match crate::routes::playlists::copier_playlist_locale(repo, source_id, &nom, profil) {
            Ok((id, n)) => (Some(id), n),
            Err(e) => {
                tracing::warn!(source_playlist = source_id, error = %e, "playlist_copy_failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "detail": e, "error": "copie_impossible" })),
                )
                    .into_response();
            }
        }
    };
    Json(json!({
        "transfer_id": Value::Null,
        "source_service": "local",
        "source_playlist_name": source_nom,
        "target_service": "local",
        "target_playlist_name": nom,
        "target_playlist_id": cible,
        "local_playlist_id": cible,
        "remote_playlist_id": Value::Null,
        "total_tracks": total,
        "matched": copiees,
        "approximate": 0,
        "not_found": total.saturating_sub(copiees),
        "match_rate": if total > 0 { copiees as f64 / total as f64 } else { 0.0 },
        "dry_run": dry_run,
        "status": if dry_run { "dry_run" } else { "completed" },
        "tracks": [],
    }))
    .into_response()
}

#[cfg(not(feature = "plugins-wasm"))]
async fn transferer_par_le_greffon(
    _state: &AppState,
    _body: &TransferRequest,
    _profil: i64,
) -> axum::response::Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "greffon_requis",
            "detail": "Le transfert de playlists passe par le greffon « Playlists converter », \
                       et ce serveur est compilé sans greffons WASM (`plugins-wasm`).",
        })),
    )
        .into_response()
}

#[cfg(feature = "plugins-wasm")]
async fn transferer_par_le_greffon(
    state: &AppState,
    body: &TransferRequest,
    profil: i64,
) -> axum::response::Response {
    use crate::routes::plugins::appeler_greffon_wasm;

    let appeler = |chemin: &'static str, corps: Value| {
        appeler_greffon_wasm(
            state,
            GREFFON_CONVERTISSEUR,
            "POST",
            chemin,
            "",
            corps,
            Some(profil),
        )
    };

    let demande = json!({
        "source_service": body.source_service,
        "cible_service": body.target_service,
        "playlists": [body.source_playlist_id],
        "nom_cible": body.target_name,
    });
    let mut reponse = match appeler("/apercu", demande).await {
        Ok((st, corps)) if st.is_success() => corps,
        Ok((st, corps)) | Err((st, corps)) => return refus_du_greffon(st, corps),
    };

    if !body.dry_run {
        let lot_id = reponse["lot"]["lot_id"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        reponse = match appeler("/transfert", json!({ "lot_id": lot_id, "accord": true })).await {
            Ok((st, corps)) if st.is_success() => corps,
            Ok((st, corps)) | Err((st, corps)) => return refus_du_greffon(st, corps),
        };
    }

    Json(reponse_historique(&reponse["lot"], body.dry_run)).into_response()
}

/// Un refus du greffon, ou son absence, rendu avec `detail` en plus de
/// `error` : les clients d'avant lisaient `detail`.
#[cfg(feature = "plugins-wasm")]
fn refus_du_greffon(statut: StatusCode, corps: Value) -> axum::response::Response {
    if corps["error"] == "plugin not found" {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": "greffon_requis",
                "detail": "Le transfert de playlists passe par le greffon « Playlists converter », \
                           qui n'est pas chargé sur ce serveur (désactivé, ou absent du dossier \
                           des greffons).",
            })),
        )
            .into_response();
    }
    let detail = corps["error"]
        .as_str()
        .or_else(|| corps["message"].as_str())
        .unwrap_or("le greffon a refusé la demande")
        .to_string();
    let mut corps = if corps.is_object() { corps } else { json!({}) };
    corps["detail"] = json!(detail);
    (statut, Json(corps)).into_response()
}

/// Le lot d'UNE playlist, sous la forme que rendait l'ancienne route — plus
/// `lot_id`, `etat` et le rapport titre par titre (`tracks`), raison comprise.
fn reponse_historique(lot: &Value, dry_run: bool) -> Value {
    let pl = &lot["playlists"][0];
    let liste = |cle: &str| pl[cle].as_array().cloned().unwrap_or_default();
    let appariees = liste("appariees");
    let introuvables = liste("introuvables");
    let total = pl["total"].as_u64().unwrap_or(0);
    let cible = lot["cible_service"].as_str().unwrap_or_default();
    let cible_id = pl["cible_playlist_id"].as_str();
    let locale = cible == "local";
    let id_local = if locale {
        cible_id.and_then(|s| s.parse::<i64>().ok())
    } else {
        None
    };

    let mut pistes: Vec<Value> = appariees
        .iter()
        .map(|a| {
            json!({
                "title": a["source_titre"],
                "artist_name": a["source_artiste"],
                "status": "matched",
                "target_id": a["cible_id"],
                "target_title": a["cible_titre"],
                "target_artist": a["cible_artiste"],
                "score": a["score"],
                "match_method": "greffon",
            })
        })
        .collect();
    pistes.extend(introuvables.iter().map(|i| {
        json!({
            "title": i["source_titre"],
            "artist_name": i["source_artiste"],
            "status": "not_found",
            "raison": i["raison"],
        })
    }));

    let etat = lot["etat"].as_str().unwrap_or_default();
    json!({
        "transfer_id": lot["lot_id"],
        "lot_id": lot["lot_id"],
        "source_service": lot["source_service"],
        "source_playlist_name": pl["source_nom"],
        "target_service": cible,
        "target_playlist_name": pl["cible_nom"],
        "target_playlist_id": id_local.map(Value::from).unwrap_or_else(|| json!(cible_id)),
        "local_playlist_id": id_local,
        "remote_playlist_id": if locale { Value::Null } else { json!(cible_id) },
        "total_tracks": total,
        "matched": appariees.len(),
        "approximate": 0,
        "not_found": introuvables.len(),
        "match_rate": if total > 0 { appariees.len() as f64 / total as f64 } else { 0.0 },
        "dry_run": dry_run,
        "status": if dry_run { "dry_run" } else { statut_historique(etat) },
        "etat": etat,
        "snapshot_avant": pl["snapshot_avant"],
        "erreur": pl["erreur"],
        "tracks": pistes,
    })
}

/// L'état d'un lot du greffon, dans le vocabulaire de l'ancien historique.
fn statut_historique(etat: &str) -> &'static str {
    match etat {
        "termine" | "rien_a_transferer" => "completed",
        "interrompu" => "interrupted",
        "en_cours" => "running",
        "apercu" => "dry_run",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Transfer History — #4741 : l'historique ne dépend plus du chemin emprunté
// ---------------------------------------------------------------------------
//
// Les lots du greffon SONT l'historique des transferts, qu'ils viennent de
// l'onglet Transferts du web (`/plugins/playlists-converter/…`) ou de
// `POST /playlist-manager/transfer`. Les entrées écrites par l'ancien moteur
// dans le réglage `playlist_transfer_history` restent lisibles, après eux ;
// plus rien n'y est ajouté.
//
// Identifiants : un lot rend `id` = son numéro (`lot-12` → 12, un entier, ce
// que l'appli iPad décode) et `lot_id` = `"lot-12"` ; une entrée ancienne rend
// son `id` d'origine et `lot_id: null`. Le détail se demande par `lot-12` pour
// un lot, par l'entier pour une entrée ancienne.

const HISTORIQUE_ANCIEN: &str = "playlist_transfer_history";

#[derive(Deserialize)]
struct HistoryQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    operation: Option<String>,
}

/// Une entrée d'historique tirée d'un lot complet (`{resume, lot}`).
fn entree_de_lot(reponse: &Value) -> Value {
    let lot = &reponse["lot"];
    let resume = &reponse["resume"];
    let playlists = lot["playlists"].as_array().cloned().unwrap_or_default();
    let lot_id = lot["lot_id"].as_str().unwrap_or_default();
    let numero = lot_id
        .strip_prefix("lot-")
        .and_then(|n| n.parse::<i64>().ok());
    let (source_nom, cible_nom) = match playlists.as_slice() {
        [une] => (une["source_nom"].clone(), une["cible_nom"].clone()),
        _ => (json!(format!("{} playlists", playlists.len())), json!("")),
    };
    json!({
        "id": numero,
        "lot_id": lot_id,
        "operation": if playlists.len() > 1 { "batch_transfer" } else { "transfer" },
        "source_service": lot["source_service"],
        "source_playlist_name": source_nom,
        "target_service": lot["cible_service"],
        "target_playlist_name": cible_nom,
        "total_tracks": resume["titres"],
        "matched": resume["appariees"],
        "approximate": 0,
        "not_found": resume["introuvables"],
        "status": statut_historique(lot["etat"].as_str().unwrap_or_default()),
        "etat": lot["etat"],
        "started_at": Value::Null,
    })
}

/// Les en-têtes de lots du greffon, du plus récent au plus ancien. Vide si le
/// greffon n'est pas chargé : l'historique ancien reste alors seul.
#[cfg(feature = "plugins-wasm")]
async fn en_tetes_de_lots(state: &AppState) -> Vec<Value> {
    match crate::routes::plugins::appeler_greffon_wasm(
        state,
        GREFFON_CONVERTISSEUR,
        "GET",
        "/lots",
        "",
        Value::Null,
        None,
    )
    .await
    {
        Ok((st, corps)) if st.is_success() => corps["lots"].as_array().cloned().unwrap_or_default(),
        _ => Vec::new(),
    }
}

#[cfg(not(feature = "plugins-wasm"))]
async fn en_tetes_de_lots(_state: &AppState) -> Vec<Value> {
    Vec::new()
}

#[cfg(feature = "plugins-wasm")]
async fn lot_complet(state: &AppState, lot_id: &str) -> Option<Value> {
    match crate::routes::plugins::appeler_greffon_wasm(
        state,
        GREFFON_CONVERTISSEUR,
        "GET",
        "/lot",
        &format!("id={lot_id}"),
        Value::Null,
        None,
    )
    .await
    {
        Ok((st, corps)) if st.is_success() => Some(corps),
        _ => None,
    }
}

#[cfg(not(feature = "plugins-wasm"))]
async fn lot_complet(_state: &AppState, _lot_id: &str) -> Option<Value> {
    None
}

async fn transfer_history(
    State(state): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> Json<Value> {
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let garder = |operation: &str| q.operation.as_deref().is_none_or(|op| op == operation);

    // Les lots d'abord. L'opération se lit sur l'en-tête (`rangs` = une
    // playlist par rang) : seuls les lots de la fenêtre demandée sont relus en
    // entier.
    let lots: Vec<String> = en_tetes_de_lots(&state)
        .await
        .into_iter()
        .filter(|e| {
            let n = e["rangs"].as_array().map_or(0, Vec::len);
            garder(if n > 1 { "batch_transfer" } else { "transfer" })
        })
        .filter_map(|e| e["lot_id"].as_str().map(str::to_string))
        .collect();

    let mut sortie: Vec<Value> = Vec::new();
    for lot_id in lots.iter().skip(offset).take(limit) {
        if let Some(complet) = lot_complet(&state, lot_id).await {
            sortie.push(entree_de_lot(&complet));
        }
    }

    // Puis l'ancien historique, figé, sans ses détails.
    let reste = limit.saturating_sub(sortie.len());
    let saut = offset.saturating_sub(lots.len());
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let anciennes = load_json_setting(&settings, HISTORIQUE_ANCIEN);
    sortie.extend(
        anciennes
            .iter()
            .rev()
            .filter(|e| garder(e["operation"].as_str().unwrap_or_default()))
            .skip(saut)
            .take(reste)
            .map(|e| {
                let mut v = e.clone();
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("details");
                    obj.insert("lot_id".into(), Value::Null);
                }
                v
            }),
    );

    Json(json!(sortie))
}

async fn transfer_history_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if id.starts_with("lot-") {
        return match lot_complet(&state, &id).await {
            Some(complet) => {
                let mut entree = entree_de_lot(&complet);
                entree["details"] = reponse_historique(&complet["lot"], false)["tracks"].clone();
                if complet["lot"]["playlists"].as_array().map_or(0, Vec::len) > 1 {
                    entree["playlists"] = complet["lot"]["playlists"].clone();
                }
                Json(entree).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let Ok(numero) = id.parse::<i64>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let anciennes = load_json_setting(&settings, HISTORIQUE_ANCIEN);
    match anciennes
        .iter()
        .find(|e| e.get("id").and_then(Value::as_i64) == Some(numero))
    {
        Some(e) => Json(e.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Playlist Links
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateLinkRequest {
    local_playlist_id: i64,
    service: String,
    service_playlist_id: String,
    sync_direction: Option<String>,
    sync_interval_minutes: Option<i64>,
}

async fn list_links(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let links = load_json_setting(&settings, "playlist_links");
    Json(json!(links))
}

async fn create_link(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<CreateLinkRequest>,
) -> impl IntoResponse {
    // Le lien désigne une playlist locale par son id, et `sync_link` y écrit.
    // Refuser dès la création évite d'inscrire dans les réglages un lien qui
    // pointe sur la playlist d'un autre profil.
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = owned_or_404_response(&playlist_repo, body.local_playlist_id, profile.id()) {
        return r;
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");
    let id = next_id(&links);
    let link = json!({
        "id": id,
        "local_playlist_id": body.local_playlist_id,
        "service": body.service,
        "service_playlist_id": body.service_playlist_id,
        "sync_direction": body.sync_direction.as_deref().unwrap_or("pull"),
        "sync_interval_minutes": body.sync_interval_minutes.unwrap_or(0),
        "last_synced_at": null,
        "created_at": now_iso(),
    });
    links.push(link.clone());
    save_json_setting(&settings, "playlist_links", &links);
    (StatusCode::CREATED, Json(link)).into_response()
}

async fn delete_link(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");
    let before = links.len();
    links.retain(|l| l.get("id").and_then(|v| v.as_i64()) != Some(id));
    if links.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "playlist_links", &links);
    Json(json!({"ok": true})).into_response()
}

async fn sync_link(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut links = load_json_setting(&settings, "playlist_links");

    let link = links
        .iter_mut()
        .find(|l| l.get("id").and_then(|v| v.as_i64()) == Some(id));
    let link = match link {
        Some(l) => l,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let service = link
        .get("service")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let service_playlist_id = link
        .get("service_playlist_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let local_playlist_id = link
        .get("local_playlist_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let direction = link
        .get("sync_direction")
        .and_then(|v| v.as_str())
        .unwrap_or("pull")
        .to_string();

    // Les liens vivent dans un réglage commun au foyer et sont donc VISIBLES de
    // tous : sans ce refus, `POST /links/{id}/sync` faisait écrire des pistes
    // dans la playlist locale d'un autre profil, en énumérant simplement les
    // ids de liens. Le refus est posé AVANT l'appel au service distant : rien
    // n'est fetché, rien n'est écrit, et `last_synced_at` reste intact.
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    if let Err(r) = owned_or_404_response(&playlist_repo, local_playlist_id, profile.id()) {
        return r;
    }

    // Fetch remote playlist tracks
    let registry = state.services.lock().await;
    let svc_arc = match registry.get(&service) {
        Some(arc) => arc,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": format!("Service '{service}' not available")})),
            )
                .into_response();
        }
    };
    drop(registry);

    let svc = svc_arc.read().await;
    let remote_tracks = svc
        .get_playlist_tracks(&service_playlist_id)
        .await
        .unwrap_or_default();
    drop(svc);

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let local_track_ids = playlist_repo
        .get_track_ids(local_playlist_id)
        .unwrap_or_default();

    let mut added_to_local = 0i64;

    if direction == "pull" || direction == "bidirectional" {
        // Match remote tracks against local library and add missing ones
        for rt in &remote_tracks {
            let title = rt.title.as_str();
            let artist = rt.artist.as_str();
            let query = if artist.is_empty() {
                title.to_string()
            } else {
                format!("{title} {artist}")
            };
            if let Ok(results) = track_repo.search(&query, 1)
                && let Some(track) = results.first()
                && let Some(tid) = track.id
                && !local_track_ids.contains(&tid)
            {
                playlist_repo
                    .add_tracks(local_playlist_id, &[tid], None)
                    .ok();
                added_to_local += 1;
            }
        }
    }

    // Update last_synced_at
    link["last_synced_at"] = json!(now_iso());
    save_json_setting(&settings, "playlist_links", &links);

    Json(json!({
        "link_id": id,
        "direction": direction,
        "added_to_local": added_to_local,
        "added_to_remote": 0,
        "removed_from_local": 0,
        "removed_from_remote": 0,
        "conflicts": [],
        "snapshot_saved": false,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Backup / Snapshots
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct BackupRequest {
    services: Option<Vec<String>>,
    #[serde(default = "default_true")]
    include_tracks: bool,
}

async fn backup_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<BackupRequest>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let mut snapshots = load_json_setting(&settings, "playlist_snapshots");
    let mut total_playlists = 0usize;
    let mut total_tracks = 0usize;
    let mut service_counts = serde_json::Map::new();

    // Determine which services to back up
    let registry = state.services.lock().await;
    let status_all = registry.status_all().await;
    drop(registry);

    let service_names: Vec<String> = if let Some(ref svcs) = body.services {
        svcs.clone()
    } else {
        let mut names: Vec<String> = status_all
            .iter()
            .filter_map(|s| {
                s.get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        names.push("local".to_string());
        names
    };

    for svc_name in &service_names {
        let mut svc_count = 0usize;

        if svc_name == "local" {
            let playlists = playlist_repo
                .list(profile.id(), 99999, 0)
                .unwrap_or_default();
            for pl in &playlists {
                let pl_id = pl.id.unwrap_or(0);
                let mut tracks_data: Vec<Value> = Vec::new();
                if body.include_tracks {
                    let track_ids = playlist_repo.get_track_ids(pl_id).unwrap_or_default();
                    let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
                    tracks_data = tracks
                        .iter()
                        .map(|t| {
                            json!({
                                "title": t.title,
                                "artist_name": t.artist_name,
                                "album_title": t.album_title,
                                "duration_ms": t.duration_ms,
                            })
                        })
                        .collect();
                }
                let snap_id = next_id(&snapshots);
                snapshots.push(json!({
                    "id": snap_id,
                    "source_service": "local",
                    "source_playlist_id": pl_id.to_string(),
                    "playlist_name": pl.name,
                    "track_count": tracks_data.len(),
                    "created_at": now_iso(),
                    "snapshot_data": tracks_data,
                }));
                total_playlists += 1;
                total_tracks += tracks_data.len();
                svc_count += 1;
            }
        } else {
            // Streaming service backup
            let registry = state.services.lock().await;
            let svc_arc = match registry.get(svc_name) {
                Some(arc) => arc,
                None => continue,
            };
            drop(registry);

            let svc = svc_arc.read().await;
            let playlists = svc.get_user_playlists().await.unwrap_or_default();
            for pl in &playlists {
                let pl_name = &pl.name;
                let source_id = &pl.id;

                let mut tracks_data: Vec<Value> = Vec::new();
                if body.include_tracks
                    && let Ok(tracks) = svc.get_playlist_tracks(source_id).await
                {
                    tracks_data = tracks
                        .iter()
                        .map(|t| {
                            json!({
                                "title": t.title,
                                "artist_name": t.artist,
                                "album_title": t.album.as_deref().unwrap_or(""),
                                "duration_ms": t.duration_ms,
                                "source_id": t.id,
                            })
                        })
                        .collect();
                }
                let snap_id = next_id(&snapshots);
                snapshots.push(json!({
                    "id": snap_id,
                    "source_service": svc_name,
                    "source_playlist_id": source_id,
                    "playlist_name": pl_name,
                    "track_count": tracks_data.len(),
                    "created_at": now_iso(),
                    "snapshot_data": tracks_data,
                }));
                total_playlists += 1;
                total_tracks += tracks_data.len();
                svc_count += 1;
            }
        }
        service_counts.insert(svc_name.clone(), json!(svc_count));
    }

    save_json_setting(&settings, "playlist_snapshots", &snapshots);

    Json(json!({
        "backup_id": snapshots.len(),
        "playlists_backed_up": total_playlists,
        "total_tracks_snapshot": total_tracks,
        "services": service_counts,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct BackupsQuery {
    service: Option<String>,
    limit: Option<usize>,
}

async fn list_backups(State(state): State<AppState>, Query(q): Query<BackupsQuery>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let limit = q.limit.unwrap_or(500);

    let filtered: Vec<Value> = snapshots
        .iter()
        .rev()
        .filter(|s| {
            if let Some(ref svc) = q.service {
                s.get("source_service")
                    .and_then(|v| v.as_str())
                    .map(|n| n == svc)
                    .unwrap_or(false)
            } else {
                true
            }
        })
        .take(limit)
        .map(|s| {
            let mut v = s.clone();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("snapshot_data");
            }
            v
        })
        .collect();

    Json(json!(filtered))
}

async fn get_backup(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let snap = snapshots
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_i64()) == Some(id));
    match snap {
        Some(s) => Json(s.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn delete_backup(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut snapshots = load_json_setting(&settings, "playlist_snapshots");
    let before = snapshots.len();
    snapshots.retain(|s| s.get("id").and_then(|v| v.as_i64()) != Some(id));
    if snapshots.len() == before {
        return StatusCode::NOT_FOUND.into_response();
    }
    save_json_setting(&settings, "playlist_snapshots", &snapshots);
    Json(json!({"deleted": true, "id": id})).into_response()
}

#[derive(Deserialize)]
struct RestoreRequest {
    target_name: Option<String>,
    #[serde(default)]
    overwrite_existing: bool,
}

async fn restore_backup(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    body: Option<Json<RestoreRequest>>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let snapshots = load_json_setting(&settings, "playlist_snapshots");
    let snap = match snapshots
        .iter()
        .find(|s| s.get("id").and_then(|v| v.as_i64()) == Some(id))
    {
        Some(s) => s.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    let body = body.map(|b| b.0);
    let overwrite = body.as_ref().map(|b| b.overwrite_existing).unwrap_or(false);
    let playlist_name = snap
        .get("playlist_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Restored Playlist");
    let target_name = body
        .as_ref()
        .and_then(|b| b.target_name.as_deref())
        .unwrap_or(playlist_name);

    let snapshot_tracks: Vec<Value> = snap
        .get("snapshot_data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    // Check for existing playlist
    let existing_playlists = playlist_repo
        .list(profile.id(), 99999, 0)
        .unwrap_or_default();
    let existing = existing_playlists.iter().find(|p| p.name == target_name);
    if existing.is_some() && !overwrite {
        return (
            StatusCode::CONFLICT,
            Json(json!({"detail": format!("Local playlist '{target_name}' already exists. Use overwrite_existing=true to replace.")})),
        )
            .into_response();
    }

    let playlist_id = if let Some(ex) = existing {
        let pid = ex.id.unwrap_or(0);
        // Clear existing tracks — TOUTES les lignes. L'ancienne boucle
        // retirait les positions 0..n-1, n compté sur les seules pistes
        // LOCALES : depuis #4889 une playlist peut porter des titres de
        // service, qui décalent les positions, et « remplacer » aurait
        // laissé des lignes de l'ancienne version derrière la nouvelle.
        playlist_repo.set_tracks(pid, &[]).ok();
        pid
    } else {
        match playlist_repo.create(
            target_name,
            Some(&format!("Restored from snapshot #{id}")),
            profile.id(),
        ) {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": e})),
                )
                    .into_response();
            }
        }
    };

    // Match snapshot tracks against local library
    let mut matched = 0i64;
    let mut not_found = 0i64;
    let mut matched_ids: Vec<i64> = Vec::new();

    for track in &snapshot_tracks {
        let title = track.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let artist = track
            .get("artist_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if title.is_empty() {
            not_found += 1;
            continue;
        }
        let query = if artist.is_empty() {
            title.to_string()
        } else {
            format!("{title} {artist}")
        };
        if let Ok(results) = track_repo.search(&query, 1)
            && let Some(track) = results.first()
            && let Some(tid) = track.id
        {
            matched_ids.push(tid);
            matched += 1;
            continue;
        }
        not_found += 1;
    }

    if !matched_ids.is_empty() {
        playlist_repo
            .add_tracks(playlist_id, &matched_ids, None)
            .ok();
    }

    Json(json!({
        "local_playlist_id": playlist_id,
        "name": target_name,
        "tracks_restored": matched,
        "tracks_matched": matched,
        "tracks_not_found": not_found,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Merge
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MergeRequest {
    playlists: Vec<MergeSource>,
    target_name: String,
    #[serde(default = "default_true")]
    deduplicate: bool,
    /// Où atterrit la fusion. `None` ou `"local"` : la bibliothèque.
    ///
    /// 🔴 Ce champ N'EXISTAIT PAS. Le client l'envoyait déjà — serde jette en
    /// silence un champ non déclaré, et la fusion de huit playlists Qobuz
    /// créait une playlist LOCALE. Vide, de surcroît : la boucle des sources
    /// « sautait pour le moment » toute source de service. Bertrand :
    /// « Cela merge en local : erreur !! »
    target_service: Option<String>,
}

#[derive(Deserialize)]
struct MergeSource {
    service: Option<String>,
    playlist_id: String,
}

async fn merge_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<MergeRequest>,
) -> impl IntoResponse {
    let cible = body
        .target_service
        .as_deref()
        .unwrap_or("local")
        .to_string();

    // Les sources, lues chacune chez elle. Une seule erreur de lecture arrête
    // tout : fusionner la moitié d'une playlist sans le dire serait pire.
    let pistes = match rassembler_les_sources(&state, &profile, &body).await {
        Ok(p) => p,
        Err(reponse) => return reponse,
    };

    if cible == "local" {
        fusion_vers_le_local(&state, &profile, &body, pistes).await
    } else {
        fusion_vers_le_service(&state, &body, &cible, pistes).await
    }
}

/// Une piste vue depuis sa source, réduite à ce qui sert à l'apparier.
///
/// `id_source` n'est utilisable QUE chez `service` : un identifiant Qobuz ne
/// veut rien dire chez Tidal. C'est toute la raison de l'appariement.
struct PisteSource {
    titre: String,
    artiste: String,
    isrc: String,
    duree_ms: u64,
    id_source: String,
    service: String,
}

/// Lit les pistes de chaque playlist source, chacune chez son service.
async fn rassembler_les_sources(
    state: &AppState,
    profile: &ActiveProfile,
    body: &MergeRequest,
) -> Result<Vec<PisteSource>, axum::response::Response> {
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let mut pistes: Vec<PisteSource> = Vec::new();

    for source in &body.playlists {
        let service = source.service.as_deref().unwrap_or("local");
        if service == "local" {
            // Seule la CRÉATION était cloisonnée : les sources partaient d'un
            // `WHERE id = ?` nu, et la fusion recopiait donc chez l'appelant
            // le contenu de n'importe quelle playlist du foyer.
            let playlist_id: i64 = source.playlist_id.parse().unwrap_or(0);
            owned_or_404_response(&playlist_repo, playlist_id, profile.id())?;
            let ids = playlist_repo.get_track_ids(playlist_id).unwrap_or_default();
            for t in track_repo.get_multiple(&ids).unwrap_or_default() {
                pistes.push(PisteSource {
                    titre: t.title.clone(),
                    artiste: t.artist_name.clone().unwrap_or_default(),
                    isrc: String::new(),
                    duree_ms: t.duration_ms.max(0) as u64,
                    id_source: t.id.map(|i| i.to_string()).unwrap_or_default(),
                    service: "local".into(),
                });
            }
            continue;
        }

        let registry = state.services.lock().await;
        let Some(svc_arc) = registry.get(service) else {
            drop(registry);
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("unknown service: {service}") })),
            )
                .into_response());
        };
        drop(registry);
        let svc = svc_arc.read().await;
        match svc.get_playlist_tracks(&source.playlist_id).await {
            Ok(lot) => {
                for t in lot {
                    pistes.push(PisteSource {
                        titre: t.title,
                        artiste: t.artist,
                        isrc: t.isrc.unwrap_or_default(),
                        duree_ms: t.duration_ms,
                        id_source: t.id,
                        service: service.to_string(),
                    });
                }
            }
            Err(e) => {
                return Err((
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "error": e.to_string(), "service": service })),
                )
                    .into_response());
            }
        }
    }
    Ok(pistes)
}

/// Ce qu'une piste introuvable laisse dire à l'écran.
fn introuvable(p: &PisteSource) -> Value {
    json!({ "title": p.titre, "artist": p.artiste, "service": p.service })
}

async fn fusion_vers_le_local(
    state: &AppState,
    profile: &ActiveProfile,
    body: &MergeRequest,
    pistes: Vec<PisteSource>,
) -> axum::response::Response {
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let mut ids: Vec<i64> = Vec::new();
    let mut absentes: Vec<Value> = Vec::new();
    for p in &pistes {
        if p.service == "local" {
            if let Ok(id) = p.id_source.parse::<i64>() {
                ids.push(id);
                continue;
            }
            absentes.push(introuvable(p));
            continue;
        }
        // Une piste de service n'a pas d'identifiant local : on la CHERCHE
        // dans la bibliothèque. C'est le même rapprochement que le transfert.
        let requete = if p.artiste.is_empty() {
            p.titre.clone()
        } else {
            format!("{} {}", p.titre, p.artiste)
        };
        match track_repo.search(&requete, 5).unwrap_or_default().first() {
            Some(t) if t.id.is_some() => ids.push(t.id.unwrap()),
            _ => absentes.push(introuvable(p)),
        }
    }

    if body.deduplicate {
        let mut vues = std::collections::HashSet::new();
        ids.retain(|id| vues.insert(*id));
    }

    if ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no tracks to merge", "unmatched": absentes })),
        )
            .into_response();
    }

    let new_id =
        match playlist_repo.create(&body.target_name, Some("Merged playlist"), profile.id()) {
            Ok(id) => id,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"detail": e})),
                )
                    .into_response();
            }
        };
    playlist_repo.add_tracks(new_id, &ids, None).ok();

    Json(json!({
        "playlist_id": new_id,
        "service": "local",
        "name": body.target_name,
        "total_tracks": ids.len(),
        "deduplicated": body.deduplicate,
        "not_found": absentes.len(),
        "unmatched": absentes,
    }))
    .into_response()
}

/// Fusion CHEZ un service — « au même endroit », et depuis le 21/09 aussi
/// depuis AILLEURS.
///
/// Les sources déjà sur la cible donnent leur identifiant tel quel. Les
/// autres passent par une recherche dans le catalogue de la cible, appariée
/// par ISRC quand la source en porte un, puis titre / artiste / durée. C'est
/// le rapprochement du transfert, et il coûte un aller-retour réseau PAR
/// TITRE à apparier : une fusion croisée n'est pas instantanée.
async fn fusion_vers_le_service(
    state: &AppState,
    body: &MergeRequest,
    cible: &str,
    pistes: Vec<PisteSource>,
) -> axum::response::Response {
    let registry = state.services.lock().await;
    let Some(svc_arc) = registry.get(cible) else {
        drop(registry);
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown service: {cible}") })),
        )
            .into_response();
    };
    drop(registry);
    let svc = svc_arc.read().await;

    if !svc.supports_write() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({ "error": format!("{cible} cannot create playlists") })),
        )
            .into_response();
    }

    let mut ids: Vec<String> = Vec::new();
    let mut absentes: Vec<Value> = Vec::new();
    for p in &pistes {
        if p.service == cible {
            if p.id_source.is_empty() {
                absentes.push(introuvable(p));
            } else {
                ids.push(p.id_source.clone());
            }
            continue;
        }
        let requete = if p.artiste.is_empty() {
            p.titre.clone()
        } else {
            format!("{} {}", p.titre, p.artiste)
        };
        match svc.search(&requete, 10).await {
            Ok(res) => {
                // L'ISRC et la durée sont passés quand la source les porte :
                // le transfert, lui, envoie `""` et `0`, ce qui prive
                // l'appariement de son critère le plus sûr.
                match tune_core::streaming::matching::best_stream_match(
                    &p.titre,
                    &p.artiste,
                    &p.isrc,
                    p.duree_ms,
                    &res.tracks,
                ) {
                    Some(t) => ids.push(t.id.clone()),
                    None => absentes.push(introuvable(p)),
                }
            }
            Err(_) => absentes.push(introuvable(p)),
        }
    }

    if body.deduplicate {
        let mut vues = std::collections::HashSet::new();
        ids.retain(|id| vues.insert(id.clone()));
    }

    // 🔴 Ne JAMAIS créer une playlist vide : c'est exactement ce que la route
    // rendait avant, en annonçant un succès.
    if ids.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no tracks to merge", "unmatched": absentes })),
        )
            .into_response();
    }

    let nouvelle = match svc
        .create_playlist(&body.target_name, Some("Merged by Tune"))
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    match svc.add_tracks_to_playlist(&nouvelle, &ids).await {
        Ok(ajoutees) => {
            // La liste des playlists du service est MÉMORISÉE 2 minutes.
            // Sans cet oubli, la playlist créée n'apparaîtrait pas.
            tune_streaming_http::purge_contenu_utilisateur(cible);
            tracing::info!(service = %cible, playlist = %nouvelle, ajoutees, absentes = absentes.len(), "playlists_merged_on_service");
            Json(json!({
                "playlist_id": nouvelle,
                "service": cible,
                "name": body.target_name,
                "total_tracks": ajoutees,
                "deduplicated": body.deduplicate,
                "not_found": absentes.len(),
                "unmatched": absentes,
            }))
            .into_response()
        }
        Err(e) => {
            // 🔴 La playlist a été CRÉÉE avant que l'ajout échoue. Laissée
            // telle quelle, elle encombre le compte — Bertrand en a récolté
            // trois au fil des essais Tidal.
            //
            // On ne la retire QUE si elle est vraiment vide : un ajout peut
            // échouer au deuxième lot de 100, et la supprimer alors perdrait
            // les cent premiers titres.
            let vide = match svc.get_playlist_tracks(&nouvelle).await {
                Ok(p) => p.is_empty(),
                Err(_) => false,
            };
            let retiree = if vide {
                svc.delete_playlist(&nouvelle).await.is_ok()
            } else {
                false
            };
            if retiree {
                tune_streaming_http::purge_contenu_utilisateur(cible);
            }
            tracing::warn!(service = %cible, playlist = %nouvelle, %retiree, error = %e, "merge_add_tracks_failed");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": e.to_string(),
                    "playlist_id": nouvelle,
                    "service": cible,
                    "rolled_back": retiree,
                    "partial": !retiree,
                })),
            )
                .into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Export / Import
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ExportRequest {
    service: String,
    playlist_id: String,
    format: Option<String>,
}

async fn export_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<ExportRequest>,
) -> Result<impl IntoResponse, AppError> {
    let format = body.format.as_deref().unwrap_or("json");
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let (name, tracks) = if body.service == "local" {
        // Cette route n'avait aucune identité d'appelant : elle rendait en
        // clair — nom, titres, artistes, albums — la playlist désignée par
        // l'id, quel que soit son propriétaire. Un export est une lecture
        // complète : c'est la fuite la plus large des cinq.
        let playlist_id: i64 = body.playlist_id.parse().unwrap_or(0);
        let name = owned_or_404(&playlist_repo, playlist_id, profile.id())?.name;
        let track_ids = playlist_repo.get_track_ids(playlist_id).unwrap_or_default();
        let tracks = track_repo.get_multiple(&track_ids).unwrap_or_default();
        let tracks_json: Vec<Value> = tracks
            .iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist_name": t.artist_name,
                    "album_title": t.album_title,
                    "duration_ms": t.duration_ms,
                })
            })
            .collect();
        (name, tracks_json)
    } else {
        let registry = state.services.lock().await;
        let svc_arc = match registry.get(&body.service) {
            Some(arc) => arc,
            None => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"detail": format!("Service '{}' not found", body.service)})),
                )
                    .into_response());
            }
        };
        drop(registry);

        let svc = svc_arc.read().await;
        let name = svc
            .get_user_playlists()
            .await
            .unwrap_or_default()
            .iter()
            .find(|p| p.id == body.playlist_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "Playlist".into());
        let raw_tracks = svc
            .get_playlist_tracks(&body.playlist_id)
            .await
            .unwrap_or_default();
        let tracks: Vec<Value> = raw_tracks
            .iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist_name": t.artist,
                    "album_title": t.album.as_deref().unwrap_or(""),
                    "duration_ms": t.duration_ms,
                })
            })
            .collect();
        (name, tracks)
    };

    match format {
        "csv" => {
            let mut csv = String::from("title,artist,album,duration_ms\n");
            for t in &tracks {
                let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let artist = t.get("artist_name").and_then(|v| v.as_str()).unwrap_or("");
                let album = t.get("album_title").and_then(|v| v.as_str()).unwrap_or("");
                let dur = t.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                csv.push_str(&format!(
                    "\"{}\",\"{}\",\"{}\",{}\n",
                    title.replace('"', "\"\""),
                    artist.replace('"', "\"\""),
                    album.replace('"', "\"\""),
                    dur
                ));
            }
            let filename = format!("{}.csv", name.replace(' ', "_"));
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("text/csv; charset=utf-8"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, csv).into_response())
        }
        "xspf" => {
            let mut xspf = String::from(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                 <playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n\
                 <trackList>\n",
            );
            for t in &tracks {
                let title = t.get("title").and_then(|v| v.as_str()).unwrap_or("");
                let artist = t.get("artist_name").and_then(|v| v.as_str()).unwrap_or("");
                let dur = t.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0);
                xspf.push_str(&format!(
                    "  <track><title>{title}</title><creator>{artist}</creator><duration>{dur}</duration></track>\n"
                ));
            }
            xspf.push_str("</trackList>\n</playlist>\n");
            let filename = format!("{}.xspf", name.replace(' ', "_"));
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("application/xspf+xml; charset=utf-8"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, xspf).into_response())
        }
        _ => {
            let content = serde_json::to_string_pretty(&json!({
                "name": name,
                "tracks": tracks,
                "track_count": tracks.len(),
                "exported_at": now_iso(),
            }))
            .unwrap_or_default();
            let filename = format!("{}.json", name.replace(' ', "_"));
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "Content-Type",
                axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
            );
            headers.insert(
                "Content-Disposition",
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .map_err(|e| AppError::internal(format!("{e}")))?,
            );
            Ok((StatusCode::OK, headers, content).into_response())
        }
    }
}

#[derive(Deserialize)]
struct ImportRequest {
    name: Option<String>,
    format: Option<String>,
    tracks: Vec<ImportTrack>,
}

#[derive(Deserialize)]
struct ImportTrack {
    title: String,
    artist: Option<String>,
    album: Option<String>,
}

async fn import_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Json(body): Json<ImportRequest>,
) -> impl IntoResponse {
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    let track_repo = TrackRepo::with_backend(state.backend.clone());

    let name = body.name.unwrap_or_else(|| "Imported Playlist".into());

    let playlist_id = match playlist_repo.create(&name, Some("Imported playlist"), profile.id()) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": e})),
            )
                .into_response();
        }
    };

    let mut matched = 0i64;
    let mut not_found = 0i64;
    let mut matched_ids: Vec<i64> = Vec::new();

    for t in &body.tracks {
        let artist = t.artist.as_deref().unwrap_or("");
        let query = if artist.is_empty() {
            t.title.clone()
        } else {
            format!("{} {}", t.title, artist)
        };
        if let Ok(results) = track_repo.search(&query, 1)
            && let Some(track) = results.first()
            && let Some(tid) = track.id
        {
            matched_ids.push(tid);
            matched += 1;
            continue;
        }
        not_found += 1;
    }

    if !matched_ids.is_empty() {
        playlist_repo
            .add_tracks(playlist_id, &matched_ids, None)
            .ok();
    }

    Json(json!({
        "playlist_id": playlist_id,
        "playlist_name": name,
        "total_tracks": body.tracks.len(),
        "matched_to_library": matched,
        "unmatched": not_found,
    }))
    .into_response()
}
