//! Operator-controlled native audio packages. No default trust key and no
//! unsigned installation switch. Libraries activate only during startup.
use crate::{auth::RequireAdmin, state::AppState};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
pub fn root() -> PathBuf {
    std::env::var_os("TUNE_AUDIO_PLUGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("TUNE_PLUGINS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("plugins"))
                .join("audio")
        })
}
/// #5149 — clé publique minisign des greffons natifs de Mozaiklabs (id
/// `24A5D1CD444CB780`). Paire DÉDIÉE aux greffons, distincte de la clé des
/// mises à jour du serveur (`UPDATE_PUBLIC_KEY`) : un incident sur l'une ne
/// doit pas contraindre l'autre. Sans elle, aucun greffon natif signé par
/// Mozaiklabs ne s'installait chez un utilisateur qui n'avait pas réglé
/// `TUNE_AUDIO_PLUGIN_TRUST` lui-même.
pub(crate) const MOZAIKLABS_PLUGIN_PUBLIC_KEY: &str =
    "RWSAt0xEzdGlJGlttTKkGF4Q3M/tI3jyTY5kLh+PNWziX4QJ3FRQlrJT";

/// Les clés de confiance : celle de Mozaiklabs toujours, puis celles que
/// l'opérateur AJOUTE par `TUNE_AUDIO_PLUGIN_TRUST` (fichier JSON, liste de
/// clés) ou `TUNE_AUDIO_PLUGIN_PUBLIC_KEY`. Un fichier illisible reste une
/// erreur : une confiance mal réglée ne doit pas passer en silence.
pub fn trusted_keys() -> Result<Vec<String>, String> {
    let ajoutees: Vec<String> = if let Some(path) = std::env::var_os("TUNE_AUDIO_PLUGIN_TRUST") {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?
    } else {
        std::env::var("TUNE_AUDIO_PLUGIN_PUBLIC_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .into_iter()
            .collect()
    };
    Ok(avec_la_cle_de_mozaiklabs(ajoutees))
}

fn avec_la_cle_de_mozaiklabs(ajoutees: Vec<String>) -> Vec<String> {
    let mut cles = vec![MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string()];
    for cle in ajoutees {
        let cle = cle.trim().to_string();
        if !cle.is_empty() && !cles.contains(&cle) {
            cles.push(cle);
        }
    }
    cles
}
pub fn load_installed(settings: &tune_core::db::settings_repo::SettingsRepo) {
    let directory = root();
    let ids = match tune_plugin_native::package::installed_ids(&directory) {
        Ok(ids) => ids,
        Err(error) => {
            tracing::error!(%error,"native_audio_inventory_failed");
            for id in tune_core::audio::premium_plugins::IDS {
                tune_plugin_native::record_failure(id, error.clone());
            }
            return;
        }
    };
    let keys = trusted_keys();
    for id in ids {
        let tiers = !tune_core::audio::premium_plugins::contains(&id);
        if tiers && !tune_core::audio::natifs_tiers::identifiant_admissible(&id) {
            tracing::warn!(%id,"native_audio_unknown_slot");
            continue;
        }
        let demande = if tiers {
            tune_core::audio::natifs_tiers::demande(settings, &id)
        } else {
            tune_core::audio::premium_plugins::enabled(settings, &id)
        };
        if !demande {
            continue;
        }
        let loaded = keys
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|keys| tune_plugin_native::package::load(&directory, &id, keys))
            .and_then(|library| {
                // Un greffon natif tiers n'a de place que sur la chaîne DSP.
                if tiers && library.manifest.kind != tune_plugin_sdk::manifest::PluginKind::Dsp {
                    Err("third-party native plugins must be DSP plugins".to_string())
                } else {
                    Ok(library)
                }
            });
        match loaded {
            Ok(library) => {
                if let Err(error) = tune_plugin_native::register(library) {
                    tune_plugin_native::record_failure(&id, error.to_string());
                }
            }
            Err(error) => {
                tracing::error!(%id,%error,"native_audio_activation_refused");
                tune_plugin_native::record_failure(&id, error);
            }
        }
    }
}
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(status))
        .route("/{id}/install", post(install))
        .route(
            "/{id}/install-from-catalog",
            post(crate::catalogue_greffons_audio::install_from_catalog),
        )
        .route(
            "/{id}/catalog",
            get(crate::catalogue_greffons_audio::catalog_status),
        )
        .route(
            "/{id}/zones/{zone}",
            get(crate::routes::greffons_natifs_tiers::reglage_de_zone)
                .put(crate::routes::greffons_natifs_tiers::regler_la_zone),
        )
        .route(
            "/{id}/diagnostics",
            post(crate::routes::greffons_natifs_tiers::diagnostics_du_reglage),
        )
        .route(
            "/{id}/profiles",
            get(crate::routes::greffons_natifs_tiers::lister_les_profils)
                .post(crate::routes::greffons_natifs_tiers::enregistrer_un_profil),
        )
        .route(
            "/{id}/profiles/{profile}",
            axum::routing::delete(crate::routes::greffons_natifs_tiers::supprimer_un_profil),
        )
        .route("/{id}/assets/{*name}", get(asset))
        .route("/{id}/rollback", post(rollback))
        .route("/{id}/uninstall", post(uninstall))
        .layer(DefaultBodyLimit::max(
            tune_plugin_native::package::MAX_ARCHIVE as usize,
        ))
}
/// Les greffons natifs tiers présents sur le disque (au moins une version
/// retenue), triés. Un greffon désinstallé avec `remove_native` garde ses
/// versions : il reste listé, inactif.
pub fn third_party_ids() -> Vec<String> {
    let root = root();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|id| is_third_party(id))
        .collect();
    ids.sort();
    ids
}
/// Un greffon natif tiers présent sur le disque sous cet identifiant.
pub fn is_third_party(id: &str) -> bool {
    tune_core::audio::natifs_tiers::identifiant_admissible(id)
        && root().join(id).join("versions").is_dir()
}
/// Un emplacement que ces routes gèrent : l'un des quatre intégrés, ou un
/// greffon natif tiers présent sur le disque.
fn managed(id: &str) -> bool {
    tune_core::audio::premium_plugins::contains(id) || is_third_party(id)
}
async fn status(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let integres = tune_core::audio::premium_plugins::IDS
        .into_iter()
        .map(|id| (id.to_string(), false));
    let tiers = third_party_ids().into_iter().map(|id| (id, true));
    let mut plugins = Vec::new();
    for (id, third_party) in integres.chain(tiers) {
        // #9151 — le droit que les routes exigent.
        let licence = state.license.check_feature(feature(&id)).await;
        plugins.push(fiche_d_etat(&settings, &id, third_party, licence));
    }
    Json(json!({"abi":1,"target":tune_plugin_native::package::host_target(),"trust_configured":trusted_keys().is_ok_and(|v|!v.is_empty()),"plugins":plugins})).into_response()
}
/// Une ligne du panneau « État des greffons » (`GET /audio-plugins/`).
///
/// `native_loaded` dit seulement qu'un PAQUET natif a été chargé depuis le
/// disque. Un emplacement intégré sans paquet tourne par son moteur embarqué
/// (`Engine::Bundled`, `tune-core/src/audio/eq.rs`) : `native_loaded` y vaut
/// `false` chez tout le monde, et le panneau affichait « Not loaded » pour un
/// égaliseur ou un suréchantillonneur qui marchaient (#9151, fil 2945 ; #8740
/// point 3). D'où quatre champs :
/// - `engine` : `"native"` (paquet chargé), `"bundled"` (intégré, moteur
///   embarqué) ou `null` (greffon tiers sans bibliothèque chargée) ;
/// - `restart_required` : voulu actif mais pas encore en service. Seul un
///   tiers est dans ce cas : il ne charge sa bibliothèque qu'au démarrage.
///   Un intégré agit dès l'installation, ses portes lisant ses réglages à
///   chaque appel ([`crate::premium_audio_plugins::agit_sans_redemarrage`]) :
///   l'upsampler installé en cours de session marche aussitôt, et n'attend
///   aucun redémarrage (#9151) ;
/// - `licensed` : le droit que ses routes exigent (`require_premium`) ;
/// - `active` : voulu ([`premium_plugins::enabled`] pour un intégré,
///   [`natifs_tiers::actif`] — demandé ET chargé — pour un tiers) ET couvert
///   par la licence.
///
/// [`premium_plugins::enabled`]: tune_core::audio::premium_plugins::enabled
/// [`natifs_tiers::actif`]: tune_core::audio::natifs_tiers::actif
pub(crate) fn fiche_d_etat(
    settings: &tune_core::db::settings_repo::SettingsRepo,
    id: &str,
    third_party: bool,
    licence: bool,
) -> serde_json::Value {
    // `version` : celle du catalogue à l'installation, `null` pour un
    // paquet envoyé à la main (le manifeste du SDK n'en porte pas).
    let version = settings.get(&format!("plugin_{id}_version")).ok().flatten();
    let native_loaded = tune_plugin_native::provider(id).is_some();
    let error = tune_plugin_native::failure(id);
    let engine = match (native_loaded, third_party) {
        (true, _) => Some("native"),
        (false, false) => Some("bundled"),
        (false, true) => None,
    };
    let (voulu, monte) = if third_party {
        (
            tune_core::audio::natifs_tiers::demande(settings, id),
            tune_core::audio::natifs_tiers::actif(settings, id),
        )
    } else {
        let voulu = tune_core::audio::premium_plugins::enabled(settings, id);
        (voulu, voulu)
    };
    // Un échec de chargement n'attend pas un redémarrage : il le dit par `error`.
    let restart_required = voulu && !monte && error.is_none();
    json!({
        "id": id,
        "third_party": third_party,
        "native_loaded": native_loaded,
        "engine": engine,
        "active": voulu && monte && licence,
        "restart_required": restart_required,
        "licensed": licence,
        "error": error,
        "version": version,
    })
}
fn refusal(error: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"native_plugin_refused","detail":error})),
    )
        .into_response()
}
/// Binary ZIP body; the detached minisign signature is supplied as base64-free
/// UTF-8 in X-Tune-Plugin-Signature (newlines encoded as literal backslash-n).
async fn install(
    // Installe ou retire du code natif : administrateur, ou authentification
    // coupée, seulement depuis cette machine.
    _admin: crate::auth::AdminOuCetteMachine,
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let tiers = match check_slot(&state, &id).await {
        Ok(tiers) => tiers,
        Err(response) => return response,
    };
    let signature = match headers
        .get("x-tune-plugin-signature")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.replace("\\n", "\n"),
        None => return refusal("missing detached signature".into()),
    };
    match install_package(&state, &id, tiers, body.to_vec(), signature, None).await {
        Ok(package) => {
            Json(json!({"id":id,"installed":true,"restart_required":true,"target":package.target}))
                .into_response()
        }
        Err(e) => refusal(e),
    }
}

/// Les gardes d'un emplacement AVANT tout octet de paquet : identifiant
/// admissible, nom libre, Premium. Rend `true` pour un greffon natif tiers.
/// Partagé par l'envoi direct (`install`) et par l'installation depuis le
/// catalogue (`catalogue_greffons_audio`), qui doit refuser un compte Free
/// sans rien demander au réseau.
pub(crate) async fn check_slot(state: &AppState, id: &str) -> Result<bool, Response> {
    let tiers = !tune_core::audio::premium_plugins::contains(id);
    if tiers {
        if !tune_core::audio::natifs_tiers::identifiant_admissible(id) {
            return Err(refusal("invalid plugin id".into()));
        }
        // L'identifiant d'un autre greffon (compilé, WASM) partagerait ses
        // drapeaux `plugin_{id}_*` : refusé.
        if name_taken_by_another_plugin(state, id).await {
            return Err(refusal("plugin id already used by another plugin".into()));
        }
    }
    crate::premium_guard::require_premium(&state.license, feature(id)).await?;
    Ok(tiers)
}

/// Vérifie puis installe un paquet signé pour le PROCHAIN démarrage, et pose
/// les drapeaux `plugin_{id}_installed|enabled`. `version` est la version
/// annoncée par le catalogue (le manifeste du SDK n'en porte pas) : elle est
/// retenue sous `plugin_{id}_version` pour l'écran Extensions.
pub(crate) async fn install_package(
    state: &AppState,
    id: &str,
    tiers: bool,
    body: Vec<u8>,
    signature: String,
    version: Option<&str>,
) -> Result<tune_plugin_native::package::Package, String> {
    let keys = trusted_keys()?;
    let root = root();
    let expected = id.to_string();
    let package = tokio::task::spawn_blocking(move || {
        // Validate expected slot BEFORE activation, not after writing active.json.
        if tiers {
            let package = tune_plugin_native::package::inspect(&body, &signature, &keys)?;
            if package.manifest.kind != tune_plugin_sdk::manifest::PluginKind::Dsp {
                return Err("third-party native plugins must be DSP plugins".to_string());
            }
        }
        tune_plugin_native::package::install_for(&root, &body, &signature, &keys, &expected)
    })
    .await
    .map_err(|e| e.to_string())??;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    for suffix in ["installed", "enabled"] {
        settings.set(&format!("plugin_{id}_{suffix}"), "true")?;
    }
    let cle_version = format!("plugin_{id}_version");
    match version {
        Some(v) => settings.set(&cle_version, v)?,
        // Un paquet envoyé à la main ne dit pas sa version : ne pas laisser
        // croire que c'est encore celle du catalogue.
        None => settings.delete(&cle_version)?,
    }
    Ok(package)
}
/// Le droit qui ouvre un emplacement. Égaliseur, canaux et crossfeed simple
/// sont gratuits ; les autres identifiants, dont Crossfeed Pro et les
/// greffons natifs tiers, exigent le Premium.
pub(crate) fn feature(id: &str) -> tune_core::license::Feature {
    match id {
        "equalizer" | "channel-remap" => tune_core::license::Feature::DspEq,
        "crossfeed" => tune_core::license::Feature::Crossfeed,
        "converter" => tune_core::license::Feature::BatchConverter,
        "declick" => tune_core::license::Feature::Declick,
        "upsampler" => tune_core::license::Feature::Upsampling,
        _ => THIRD_PARTY_FEATURE,
    }
}
/// Le droit Premium des greffons natifs tiers.
pub(crate) const THIRD_PARTY_FEATURE: tune_core::license::Feature =
    tune_core::license::Feature::PluginMarketplace;
/// Le nom est-il déjà celui d'un greffon d'une autre famille : compilé dans
/// ce serveur (jeu registré) ou WASM posé sur le disque ?
async fn name_taken_by_another_plugin(state: &AppState, id: &str) -> bool {
    if state
        .plugin_names
        .get()
        .is_some_and(|names| names.iter().any(|n| n == id))
    {
        return true;
    }
    crate::plugins::scan_wasm_plugins()
        .await
        .is_ok_and(|infos| infos.iter().any(|i| i.manifest.id == id))
}
async fn rollback(
    // Installe ou retire du code natif : administrateur, ou authentification
    // coupée, seulement depuis cette machine.
    _admin: crate::auth::AdminOuCetteMachine,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    if !managed(&id) {
        return refusal("unknown plugin".into());
    }
    if let Err(response) = crate::premium_guard::require_premium(&state.license, feature(&id)).await
    {
        return response;
    }
    let keys = match trusted_keys() {
        Ok(keys) => keys,
        Err(e) => return refusal(e),
    };
    match tune_plugin_native::package::rollback(&root(), &id, &keys) {
        Ok(active) => {
            Json(json!({"id":id,"active":active,"restart_required":true})).into_response()
        }
        Err(e) => refusal(e),
    }
}
#[derive(Deserialize)]
struct UninstallOptions {
    #[serde(default)]
    remove_native: bool,
    /// #6256 — désinstallation COMPLÈTE d'un greffon natif tiers : déchargé,
    /// sorti des chaînes, toutes ses versions retirées du disque.
    #[serde(default)]
    purge: bool,
    /// #6256 — avec `purge` : garder les réglages de zone et les profils pour
    /// une réinstallation. Absent, ils sont GARDÉS : effacer ce que
    /// l'utilisateur a réglé doit être demandé, jamais supposé.
    #[serde(default = "garder_par_defaut")]
    keep_settings: bool,
}
fn garder_par_defaut() -> bool {
    true
}
async fn uninstall(
    // Installe ou retire du code natif : administrateur, ou authentification
    // coupée, seulement depuis cette machine.
    _admin: crate::auth::AdminOuCetteMachine,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(options): Json<UninstallOptions>,
) -> Response {
    if options.purge {
        return purger(&state, &id, options.keep_settings).await;
    }
    if !managed(&id) {
        return refusal("unknown plugin".into());
    }
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    // Choix explicite : le retour du Premium ne doit pas l'annuler (#4861).
    if let Err(e) = tune_core::audio::premium_plugins::forget_withheld(&settings, &id) {
        return refusal(e);
    }
    for suffix in ["installed", "enabled"] {
        if let Err(e) = settings.set(&format!("plugin_{id}_{suffix}"), "false") {
            return refusal(e);
        }
    }
    if options.remove_native {
        if let Err(e) = tune_plugin_native::package::deactivate(&root(), &id) {
            return refusal(e);
        }
    }
    Json(json!({"id":id,"installed":false,"configuration_preserved":true,"restart_required":true}))
        .into_response()
}

/// #6256 — ce que la désinstallation complète a fait.
#[derive(Debug)]
pub(crate) struct Desinstallation {
    /// Un fournisseur chargé a été retiré du registre.
    pub decharge: bool,
    /// Le dossier du greffon existait et a été retiré.
    pub fichiers_retires: bool,
    /// Les lignes de réglages effacées (vide quand elles sont gardées).
    pub reglages_effaces: Vec<String>,
}

/// #6256 — désinstalle complètement le greffon natif TIERS `id` installé sous
/// `root`. Les quatre emplacements intégrés n'y passent jamais : ils ont leur
/// propre cycle (`premium_audio_plugins`).
///
/// Dans l'ordre, pour qu'un échec laisse un état sûr :
///
/// 1. les drapeaux passent à `false` : plus aucune chaîne nouvelle ne demande
///    l'étage (`natifs_tiers::demande`) ;
/// 2. le fournisseur quitte le registre, et son échec de chargement est
///    oublié ; un étage vivant garde la bibliothèque jusqu'à sa destruction ;
/// 3. le dossier `root/<id>` est retiré, et lui seul. S'il ne peut pas l'être
///    (fichier encore ouvert sous Windows), l'erreur remonte et AUCUN réglage
///    n'est effacé : relancer la désinstallation après un redémarrage finit
///    le travail ;
/// 4. sans `garder_les_reglages`, les réglages de zone, les profils et les
///    drapeaux sont effacés. Sinon seule la version notée disparaît.
///
/// Rien n'est supprimé hors du dossier du greffon : en particulier, les
/// fichiers que la bibliothèque aurait écrits ailleurs ne sont ni cherchés ni
/// effacés — l'hôte ne sait pas où ils sont.
pub(crate) fn desinstaller_completement(
    root: &std::path::Path,
    settings: &tune_core::db::settings_repo::SettingsRepo,
    id: &str,
    garder_les_reglages: bool,
) -> Result<Desinstallation, String> {
    if !tune_core::audio::natifs_tiers::identifiant_admissible(id) {
        return Err("only third-party native plugins can be purged".into());
    }
    for suffix in ["installed", "enabled"] {
        settings.set(&format!("plugin_{id}_{suffix}"), "false")?;
    }
    let decharge = tune_plugin_native::unregister(id);
    tune_plugin_native::forget_failure(id);
    let fichiers_retires = tune_plugin_native::package::remove(root, id)?;
    let reglages_effaces = if garder_les_reglages {
        settings.delete(&format!("plugin_{id}_version"))?;
        Vec::new()
    } else {
        tune_core::audio::natifs_tiers::oublier_les_reglages(settings, id)?
    };
    Ok(Desinstallation {
        decharge,
        fichiers_retires,
        reglages_effaces,
    })
}

/// `POST /audio-plugins/{id}/uninstall` avec `purge: true` (#6256).
async fn purger(state: &AppState, id: &str, garder_les_reglages: bool) -> Response {
    let tiers = tune_core::audio::natifs_tiers::identifiant_admissible(id)
        && (is_third_party(id)
            || root().join(id).exists()
            || tune_plugin_native::provider(id).is_some());
    if !tiers {
        return refusal("unknown third-party native plugin".into());
    }
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    // Lues AVANT la désinstallation, qui baisse les drapeaux du greffon.
    let zones_du_greffon = state
        .orchestrator
        .zones_qui_utilisent_le_greffon_tiers(id)
        .await;
    let bilan = match desinstaller_completement(&root(), &settings, id, garder_les_reglages) {
        Ok(bilan) => bilan,
        Err(e) => {
            tracing::warn!(%id, error = %e, "native_audio_purge_failed");
            return refusal(e);
        }
    };
    // Sortir des chaînes SANS redémarrage : l'étage casque de chaque sortie
    // locale qui joue AVEC ce greffon est rebâti, sans lui. Les autres zones
    // ne sont pas touchées : les rebâtir ne leur retirerait rien et pourrait
    // s'entendre. Un flux réseau déjà préparé le porte jusqu'à la fin de sa
    // piste ; la suivante ne l'a plus.
    let mut zones_rebaties = Vec::new();
    for zone in zones_du_greffon {
        if state
            .orchestrator
            .rebatir_l_etage_casque_en_direct(zone)
            .await
        {
            zones_rebaties.push(zone);
        }
    }
    tracing::info!(
        %id,
        decharge = bilan.decharge,
        fichiers_retires = bilan.fichiers_retires,
        reglages_effaces = bilan.reglages_effaces.len(),
        "native_audio_purged"
    );
    Json(json!({
        "id": id,
        "installed": false,
        "unloaded": bilan.decharge,
        "files_removed": bilan.fichiers_retires,
        "configuration_preserved": garder_les_reglages,
        "settings_removed": bilan.reglages_effaces,
        "zones_refreshed_live": zones_rebaties,
        "restart_required": false,
    }))
    .into_response()
}

async fn asset(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    if !managed(&id) {
        return refusal("unknown plugin".into());
    }
    if let Err(response) = crate::premium_guard::require_premium(&state.license, feature(&id)).await
    {
        return response;
    }
    if let Err(response) = crate::premium_audio_plugins::require_installed(&state, &id, &headers) {
        return response;
    }
    let keys = match trusted_keys() {
        Ok(keys) => keys,
        Err(e) => return refusal(e),
    };
    let content_type = if name.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if name.ends_with(".mjs") || name.ends_with(".js") {
        "text/javascript; charset=utf-8"
    } else if name.ends_with(".css") {
        "text/css; charset=utf-8"
    } else {
        return refusal("unsupported UI asset type".into());
    };
    match tokio::task::spawn_blocking(move||tune_plugin_native::package::read_asset(&root(),&id,&name,&keys)).await {
        Ok(Ok(bytes)) => ([("content-type",content_type),("x-content-type-options","nosniff"),("cache-control","no-store"),("content-security-policy","sandbox allow-scripts; default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'none'; frame-ancestors 'self'"),("access-control-allow-origin","*")],bytes).into_response(),
        Ok(Err(e))=>refusal(e),Err(e)=>refusal(e.to_string())
    }
}

#[cfg(test)]
mod cle_de_mozaiklabs_5149 {
    use super::{MOZAIKLABS_PLUGIN_PUBLIC_KEY, avec_la_cle_de_mozaiklabs};

    #[test]
    fn la_cle_integree_est_une_cle_publique_minisign_valide() {
        assert!(
            minisign_verify::PublicKey::from_base64(MOZAIKLABS_PLUGIN_PUBLIC_KEY).is_ok(),
            "#5149 : la clé publique des greffons intégrée au serveur n'est pas lisible par minisign-verify"
        );
    }

    #[test]
    fn sans_reglage_la_cle_de_mozaiklabs_est_de_confiance() {
        assert_eq!(
            avec_la_cle_de_mozaiklabs(Vec::new()),
            vec![MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string()],
            "#5149 : sans réglage de l'opérateur, aucun greffon signé par Mozaiklabs ne s'installerait"
        );
    }

    #[test]
    fn les_cles_de_l_operateur_s_ajoutent_sans_doublon() {
        let cles = avec_la_cle_de_mozaiklabs(vec![
            "  CLE-OPERATEUR  ".to_string(),
            MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string(),
            String::new(),
            "CLE-OPERATEUR".to_string(),
        ]);
        assert_eq!(
            cles,
            vec![
                MOZAIKLABS_PLUGIN_PUBLIC_KEY.to_string(),
                "CLE-OPERATEUR".to_string()
            ],
            "#5149 : une clé de l'opérateur s'ajoute à celle de Mozaiklabs, sans la remplacer ni se dédoubler"
        );
    }
}

#[cfg(test)]
#[path = "native_audio_desinstallation_6256_tests.rs"]
mod desinstallation_6256_tests;

#[cfg(test)]
mod crossfeed_gratuit_9210_tests {
    #[tokio::test]
    async fn simple_autorise_pro_refuse_en_free() {
        use tune_core::license::Feature;
        crate::premium_audio_plugins::tests::dossier_de_donnees_jetable();
        let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        assert_eq!(super::feature("crossfeed-pro"), Feature::PluginMarketplace);
        let refusal =
            crate::premium_guard::require_premium(&state.license, super::feature("crossfeed-pro"))
                .await
                .unwrap_err();
        assert_eq!(refusal.status(), axum::http::StatusCode::PAYMENT_REQUIRED);
        assert!(
            crate::premium_guard::require_premium(&state.license, super::feature("crossfeed"))
                .await
                .is_ok()
        );
        state.license.set_account_premium(true, None).await;
        assert!(
            crate::premium_guard::require_premium(&state.license, super::feature("crossfeed-pro"))
                .await
                .is_ok()
        );
    }
}

#[cfg(test)]
#[path = "native_audio_etat_9151_tests.rs"]
mod etat_9151_tests;
