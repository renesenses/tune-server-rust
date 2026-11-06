//! #7858 — titres de démonstration proposés au premier démarrage.
//!
//! Un serveur neuf, sans bibliothèque, peut ajouter un petit jeu de titres
//! (CD, Hi-Res, DSD) pour essayer Tune et vérifier sa chaîne sans attendre un
//! scan. AUCUN fichier audio n'est dans le dépôt ni dans le binaire : la liste
//! est un manifeste JSON publié sur mozaiklabs.fr, chaque titre est téléchargé
//! à la demande et n'est gardé que si sa somme SHA-256 est celle du manifeste.
//! Le format du manifeste est décrit dans `docs/titres-de-demonstration.md`.
//!
//! Les titres vont dans un dossier dédié, `<données>/demo-tracks`, ajouté aux
//! dossiers de musique comme le ferait `POST /system/music-dirs`. Le retrait
//! passe par le même chemin que `POST /system/music-dirs/remove`
//! ([`crate::routes::system::retirer_un_dossier`]) : seules les pistes de ce
//! dossier partent, les autres racines et leurs pistes ne sont pas touchées.
//!
//! Routes, sous `/api/v1/onboarding` :
//! - `GET  /demo-tracks`        — état : proposés, installés, titres et licences ;
//! - `POST /demo-tracks/add`    — télécharge, vérifie, ajoute le dossier, scanne ;
//! - `POST /demo-tracks/remove` — retire le dossier et ses pistes, efface le dossier.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use tune_core::db::settings_repo::SettingsRepo;

use crate::message_api::refus;
use crate::state::AppState;

/// Chemin du manifeste sous la racine du site (`mozaik_base_url`).
pub(crate) const CHEMIN_DU_MANIFESTE: &str = "/tune/demo-tracks/manifest.json";
const BASE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";
/// La seule version de manifeste que ce serveur sait lire.
pub(crate) const VERSION_DU_MANIFESTE: u64 = 1;
const TAILLE_MAX_DU_MANIFESTE: usize = 256 * 1024;
const TITRES_MAX: usize = 40;
/// Plafond par fichier : un extrait DSD de quelques minutes tient largement.
const TAILLE_MAX_D_UN_TITRE: u64 = 2 * 1024 * 1024 * 1024;
const EXTENSIONS_AUDIO: &[&str] = &[
    "flac", "wav", "aif", "aiff", "dsf", "dff", "wv", "m4a", "ogg", "mp3",
];
const SOUS_DOSSIER: &str = "demo-tracks";
/// Copie du manifeste posée dans le dossier : la liste des titres installés
/// et de leurs licences, relue par `GET /demo-tracks`.
const COPIE_DU_MANIFESTE: &str = "tune-demo-tracks.json";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/demo-tracks", get(etat))
        .route("/demo-tracks/add", post(ajouter))
        .route("/demo-tracks/remove", post(retirer))
}

/// `<données>/demo-tracks`, le dossier de données étant résolu comme pour la
/// base. Aucun pour une base en mémoire (essais) : rien n'est écrit dans le
/// répertoire courant.
pub(crate) fn dossier_par_defaut(db_path: &str) -> Option<PathBuf> {
    if db_path.is_empty() || db_path == ":memory:" {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let donnees = crate::gel_executeur::dossier_de_donnees(
        std::env::var("TUNE_DATA_DIR").ok().as_deref(),
        db_path,
        &cwd,
    );
    Some(donnees.join(SOUS_DOSSIER))
}

/// L'adresse du manifeste : `mozaik_base_url` (réglage existant, déjà lu par
/// le rapport de bogue et le support) ou mozaiklabs.fr.
pub(crate) fn url_du_manifeste(state: &AppState) -> String {
    let base = SettingsRepo::with_backend(state.backend.clone())
        .get("mozaik_base_url")
        .ok()
        .flatten()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| BASE_PAR_DEFAUT.to_string());
    format!("{base}{CHEMIN_DU_MANIFESTE}")
}

// ───────────────────────────────────────────────────────────────────────────
// Le manifeste
// ───────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct Manifeste {
    pub version: u64,
    pub tracks: Vec<TitreDemo>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct TitreDemo {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// Libellé libre du format (`cd_16_44`, `hires_24_96`, `dsd64`…).
    pub format: String,
    /// La licence, citée telle quelle (`CC BY 4.0`, « accord écrit 2L »…).
    pub license: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    pub url: String,
    pub sha256: String,
    /// Nom du fichier dans le dossier des titres de démonstration.
    pub file_name: String,
    /// Taille en octets ; si présente, elle est exigée exactement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Lit et valide un manifeste. L'erreur dit ce qui ne va pas, en anglais
/// (journal et champ `cause` de la réponse).
pub(crate) fn lire_le_manifeste(octets: &[u8]) -> Result<Manifeste, String> {
    let m: Manifeste = serde_json::from_slice(octets).map_err(|e| format!("json: {e}"))?;
    if m.version != VERSION_DU_MANIFESTE {
        return Err(format!("unsupported version {}", m.version));
    }
    if m.tracks.is_empty() {
        return Err("no tracks".into());
    }
    if m.tracks.len() > TITRES_MAX {
        return Err(format!(
            "too many tracks ({} > {TITRES_MAX})",
            m.tracks.len()
        ));
    }
    let mut noms = std::collections::HashSet::new();
    for (i, t) in m.tracks.iter().enumerate() {
        let ou = |quoi: &str| format!("track {i}: {quoi}");
        for (champ, valeur) in [
            ("title", &t.title),
            ("format", &t.format),
            ("license", &t.license),
        ] {
            if valeur.trim().is_empty() {
                return Err(ou(&format!("{champ} is empty")));
            }
        }
        if !somme_valide(&t.sha256) {
            return Err(ou("sha256 must be 64 hexadecimal characters"));
        }
        if !nom_de_fichier_sur(&t.file_name) {
            return Err(ou("file_name is not a plain audio file name"));
        }
        if !noms.insert(t.file_name.to_lowercase()) {
            return Err(ou("file_name is duplicated"));
        }
        if !url_permise(&t.url) {
            return Err(ou("url must be https"));
        }
        if t.size.is_some_and(|s| s == 0 || s > TAILLE_MAX_D_UN_TITRE) {
            return Err(ou("size is out of range"));
        }
    }
    Ok(m)
}

fn somme_valide(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Un nom simple, sans chemin, qui finit par une extension audio connue :
/// jamais `..`, `/`, `\`, `:` ni nom caché.
fn nom_de_fichier_sur(nom: &str) -> bool {
    if nom.is_empty()
        || nom.len() > 200
        || nom.starts_with('.')
        || nom.contains("..")
        || nom
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
    {
        return false;
    }
    Path::new(nom)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS_AUDIO.contains(&e.to_ascii_lowercase().as_str()))
}

/// `https` seulement ; `http` n'est admis que vers la machine elle-même
/// (bancs d'essai). La somme SHA-256 garde de toute façon le contenu.
fn url_permise(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else {
        return false;
    };
    match u.scheme() {
        "https" => u.host_str().is_some(),
        "http" => matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Téléchargement vérifié
// ───────────────────────────────────────────────────────────────────────────

/// Pourquoi un ajout est refusé en entier.
#[derive(Debug)]
pub(crate) enum Refus {
    /// Pas de dossier de données (base en mémoire).
    DossierIndisponible,
    /// Manifeste injoignable, ou réponse HTTP autre que 2xx.
    ManifesteIndisponible(String),
    /// Manifeste lu mais illisible ou incomplet.
    ManifesteInvalide(String),
    /// Un ajout ou un retrait est déjà en cours.
    Occupe,
    /// Aucun titre n'a pu être téléchargé et vérifié.
    AucunTitre(Vec<Value>),
    /// Écriture impossible (disque, réglages).
    Interne(String),
}

impl IntoResponse for Refus {
    fn into_response(self) -> Response {
        use crate::message_api::refus_avec_cause;
        match self {
            Refus::DossierIndisponible => refus(
                StatusCode::SERVICE_UNAVAILABLE,
                "demo_tracks_unavailable",
                "api.demoTracks.unavailable",
                &[],
            ),
            Refus::ManifesteIndisponible(cause) => refus_avec_cause(
                StatusCode::BAD_GATEWAY,
                "demo_manifest_unavailable",
                "api.demoTracks.manifestUnavailable",
                &[],
                cause,
            ),
            Refus::ManifesteInvalide(cause) => refus_avec_cause(
                StatusCode::BAD_GATEWAY,
                "demo_manifest_invalid",
                "api.demoTracks.manifestInvalid",
                &[],
                cause,
            ),
            Refus::Occupe => refus(
                StatusCode::CONFLICT,
                "demo_tracks_busy",
                "api.demoTracks.busy",
                &[],
            ),
            Refus::AucunTitre(titres) => {
                let mut corps = crate::message_api::corps_refus(
                    "demo_downloads_failed",
                    "api.demoTracks.downloadsFailed",
                    &[],
                );
                corps["tracks"] = json!(titres);
                (StatusCode::BAD_GATEWAY, Json(corps)).into_response()
            }
            Refus::Interne(cause) => refus_avec_cause(
                StatusCode::INTERNAL_SERVER_ERROR,
                "demo_tracks_write_failed",
                "api.demoTracks.writeFailed",
                &[],
                cause,
            ),
        }
    }
}

/// Ce que l'ajout a fait.
#[derive(Debug)]
pub(crate) struct Rapport {
    pub dossier: String,
    pub titres: Vec<Value>,
    pub installes: usize,
    pub echecs: usize,
    /// Le dossier vient d'entrer dans `music_dirs` : un scan est à lancer.
    pub dossier_ajoute: bool,
}

fn client() -> Result<reqwest::Client, Refus> {
    tune_core::http::client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| Refus::Interne(format!("http client: {e}")))
}

async fn lire_le_manifeste_en_ligne(
    client: &reqwest::Client,
    url: &str,
) -> Result<Manifeste, Refus> {
    let mut reponse = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| Refus::ManifesteIndisponible(format!("{url}: {e}")))?;
    if !reponse.status().is_success() {
        return Err(Refus::ManifesteIndisponible(format!(
            "{url}: HTTP {}",
            reponse.status().as_u16()
        )));
    }
    let mut octets = Vec::new();
    while let Some(morceau) = reponse
        .chunk()
        .await
        .map_err(|e| Refus::ManifesteIndisponible(format!("{url}: {e}")))?
    {
        octets.extend_from_slice(&morceau);
        if octets.len() > TAILLE_MAX_DU_MANIFESTE {
            return Err(Refus::ManifesteInvalide(format!(
                "manifest larger than {TAILLE_MAX_DU_MANIFESTE} bytes"
            )));
        }
    }
    lire_le_manifeste(&octets).map_err(Refus::ManifesteInvalide)
}

async fn somme_du_fichier(chemin: &Path) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(chemin).await?;
    let mut h = Sha256::new();
    let mut tampon = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut tampon).await?;
        if n == 0 {
            break;
        }
        h.update(&tampon[..n]);
    }
    Ok(format!("{:x}", h.finalize()))
}

/// Télécharge un titre dans `<nom>.part`, puis ne le renomme que si sa somme
/// et sa taille sont celles du manifeste. En cas d'échec, rien ne reste.
/// Rend `Ok(true)` si le fichier était déjà là et intact.
async fn telecharger_un_titre(
    client: &reqwest::Client,
    titre: &TitreDemo,
    dossier: &Path,
) -> Result<bool, String> {
    let attendue = titre.sha256.to_ascii_lowercase();
    let cible = dossier.join(&titre.file_name);
    if cible.is_file() && somme_du_fichier(&cible).await.ok().as_deref() == Some(&attendue) {
        return Ok(true);
    }
    let partiel = dossier.join(format!("{}.part", titre.file_name));
    let resultat = telecharger_vers(client, titre, &partiel, &attendue).await;
    match resultat {
        Ok(()) => tokio::fs::rename(&partiel, &cible)
            .await
            .map(|()| false)
            .map_err(|e| format!("rename: {e}")),
        Err(e) => {
            let _ = tokio::fs::remove_file(&partiel).await;
            Err(e)
        }
    }
}

async fn telecharger_vers(
    client: &reqwest::Client,
    titre: &TitreDemo,
    partiel: &Path,
    attendue: &str,
) -> Result<(), String> {
    let plafond = titre.size.unwrap_or(TAILLE_MAX_D_UN_TITRE);
    let mut reponse = client
        .get(&titre.url)
        .send()
        .await
        .map_err(|e| format!("download: {e}"))?;
    if !reponse.status().is_success() {
        return Err(format!("http_status_{}", reponse.status().as_u16()));
    }
    let mut f = tokio::fs::File::create(partiel)
        .await
        .map_err(|e| format!("create: {e}"))?;
    let mut h = Sha256::new();
    let mut lus: u64 = 0;
    while let Some(morceau) = reponse
        .chunk()
        .await
        .map_err(|e| format!("download: {e}"))?
    {
        lus += morceau.len() as u64;
        if lus > plafond {
            return Err("size_exceeded".into());
        }
        h.update(&morceau);
        f.write_all(&morceau)
            .await
            .map_err(|e| format!("write: {e}"))?;
    }
    f.flush().await.map_err(|e| format!("write: {e}"))?;
    drop(f);
    if titre.size.is_some_and(|s| s != lus) {
        return Err("size_mismatch".into());
    }
    if format!("{:x}", h.finalize()) != attendue {
        return Err("checksum_mismatch".into());
    }
    Ok(())
}

fn chemin_normalise(dossier: &Path) -> String {
    tune_core::scanner::walker::normalize_path(&dossier.to_string_lossy())
}

/// `music_dirs` lu strictement : absent = liste vide, illisible = refus.
/// Une valeur illisible ne doit jamais devenir une liste vide qu'on
/// réécrirait en perdant les autres racines (#6175).
fn lire_les_racines(settings: &SettingsRepo) -> Result<Vec<String>, Refus> {
    match settings
        .get("music_dirs")
        .map_err(|e| Refus::Interne(format!("music_dirs: {e}")))?
    {
        None => Ok(Vec::new()),
        Some(v) => {
            serde_json::from_str(&v).map_err(|_| Refus::Interne("music_dirs is unreadable".into()))
        }
    }
}

fn est_installe(state: &AppState, dossier: &Path) -> bool {
    let n = chemin_normalise(dossier);
    super::system::get_music_dirs_list(&state.backend)
        .iter()
        .any(|d| tune_core::scanner::walker::normalize_path(d) == n)
}

/// L'ajout, sans le scan : lit le manifeste, télécharge et vérifie chaque
/// titre dans `dossier`, puis ajoute `dossier` aux dossiers de musique en
/// gardant les autres. Relançable : un titre déjà présent et intact n'est
/// pas retéléchargé, et le dossier n'est jamais ajouté deux fois.
pub(crate) async fn installer(
    state: &AppState,
    dossier: &Path,
    url: &str,
) -> Result<Rapport, Refus> {
    let client = client()?;
    let manifeste = lire_le_manifeste_en_ligne(&client, url).await?;
    tokio::fs::create_dir_all(dossier)
        .await
        .map_err(|e| Refus::Interne(format!("create {}: {e}", dossier.display())))?;

    let mut titres = Vec::new();
    let (mut installes, mut echecs) = (0, 0);
    for t in &manifeste.tracks {
        let (statut, erreur) = match telecharger_un_titre(&client, t, dossier).await {
            Ok(true) => ("already_present", None),
            Ok(false) => ("downloaded", None),
            Err(e) => ("failed", Some(e)),
        };
        if erreur.is_some() {
            echecs += 1;
            tracing::warn!(fichier = %t.file_name, url = %t.url, erreur = ?erreur, "demo_track_rejected");
        } else {
            installes += 1;
        }
        titres.push(json!({
            "file_name": t.file_name,
            "title": t.title,
            "format": t.format,
            "license": t.license,
            "status": statut,
            "error": erreur,
        }));
    }

    if installes == 0 {
        // Rien d'utilisable : on ne laisse pas un dossier vide derrière soi.
        let _ = tokio::fs::remove_dir(dossier).await;
        return Err(Refus::AucunTitre(titres));
    }

    // Seuls les titres présents et vérifiés sont cités dans la copie locale.
    let gardes: Vec<TitreDemo> = manifeste
        .tracks
        .iter()
        .zip(&titres)
        .filter(|(_, r)| r["status"] != "failed")
        .map(|(t, _)| t.clone())
        .collect();
    let copie = Manifeste {
        version: manifeste.version,
        tracks: gardes,
    };
    let octets = serde_json::to_vec_pretty(&copie).unwrap_or_default();
    tokio::fs::write(dossier.join(COPIE_DU_MANIFESTE), octets)
        .await
        .map_err(|e| Refus::Interne(format!("write manifest copy: {e}")))?;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut racines = lire_les_racines(&settings)?;
    let n = chemin_normalise(dossier);
    let dossier_ajoute = !racines
        .iter()
        .any(|d| tune_core::scanner::walker::normalize_path(d) == n);
    if dossier_ajoute {
        racines.push(n.clone());
        let json = serde_json::to_string(&racines).unwrap_or_else(|_| "[]".into());
        settings
            .set("music_dirs", &json)
            .map_err(|e| Refus::Interne(format!("music_dirs: {e}")))?;
    }

    Ok(Rapport {
        dossier: n,
        titres,
        installes,
        echecs,
        dossier_ajoute,
    })
}

/// Le retrait : le dossier sort des dossiers de musique avec ses seules
/// pistes, puis il est effacé du disque. Rien d'autre n'est touché : le
/// compte des pistes qui partent est calculé contre les racines qui restent
/// ([`crate::routes::system::pistes_qui_partiraient`]).
pub(crate) fn desinstaller(state: &AppState, dossier: &Path) -> Result<Value, Response> {
    let mut corps = json!({ "dir": chemin_normalise(dossier), "purged": 0 });
    if est_installe(state, dossier) {
        let n = chemin_normalise(dossier);
        let compte = super::system::pistes_qui_partiraient(state, std::slice::from_ref(&n));
        let r = super::system::retirer_un_dossier(state, &n, Some(compte))
            .map_err(IntoResponse::into_response)?;
        if r["purge_refused"] == json!(true) {
            // Le plafond de #1943 a refusé : les fichiers restent, pour que
            // les pistes encore en base ne pointent pas dans le vide.
            let mut c = crate::message_api::corps_refus(
                "demo_tracks_purge_refused",
                "api.demoTracks.purgeRefused",
                &[],
            );
            c["detail"] = r;
            return Err((StatusCode::CONFLICT, Json(c)).into_response());
        }
        corps["purged"] = r["purged"].clone();
        corps["dirs"] = r["dirs"].clone();
    }
    if dossier.exists() {
        std::fs::remove_dir_all(dossier).map_err(|e| {
            Refus::Interne(format!("remove {}: {e}", dossier.display())).into_response()
        })?;
    }
    corps["removed"] = json!(true);
    Ok(corps)
}

/// Faut-il proposer les titres de démonstration ? Bibliothèque vide, dossier
/// disponible et titres pas encore installés.
pub(crate) fn proposables(state: &AppState, pistes: i64) -> bool {
    pistes == 0
        && state
            .dossier_titres_demo
            .as_deref()
            .is_some_and(|d| !est_installe(state, d))
}

fn titres_installes(dossier: &Path) -> Vec<TitreDemo> {
    std::fs::read(dossier.join(COPIE_DU_MANIFESTE))
        .ok()
        .and_then(|o| serde_json::from_slice::<Manifeste>(&o).ok())
        .map(|m| m.tracks)
        .unwrap_or_default()
}

// ───────────────────────────────────────────────────────────────────────────
// Routes
// ───────────────────────────────────────────────────────────────────────────

/// `GET /onboarding/demo-tracks`
async fn etat(State(state): State<AppState>) -> Json<Value> {
    let Some(dossier) = state.dossier_titres_demo.clone() else {
        return Json(json!({
            "available": false,
            "offered": false,
            "installed": false,
            "tracks": [],
        }));
    };
    let pistes = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    let installe = est_installe(&state, &dossier);
    let titres: Vec<Value> = if installe {
        titres_installes(&dossier)
            .into_iter()
            .map(|t| {
                json!({
                    "title": t.title,
                    "artist": t.artist,
                    "format": t.format,
                    "license": t.license,
                    "license_url": t.license_url,
                    "file_name": t.file_name,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Json(json!({
        "available": true,
        "offered": pistes == 0 && !installe,
        "installed": installe,
        "dir": chemin_normalise(&dossier),
        "manifest_url": url_du_manifeste(&state),
        "tracks": titres,
    }))
}

/// `POST /onboarding/demo-tracks/add`
async fn ajouter(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Result<Json<Value>, Refus> {
    let dossier = state
        .dossier_titres_demo
        .clone()
        .ok_or(Refus::DossierIndisponible)?;
    // Un seul ajout ou retrait à la fois, par serveur (`AppState`).
    let verrou = state.verrou_titres_demo.clone();
    let _verrou = verrou.try_lock().map_err(|_| Refus::Occupe)?;
    let url = url_du_manifeste(&state);
    let r = installer(&state, &dossier, &url).await?;
    tracing::info!(
        dossier = %r.dossier,
        installes = r.installes,
        echecs = r.echecs,
        "demo_tracks_installed"
    );
    // Même geste que l'ajout d'un dossier de musique : le scan part tout de
    // suite pour que les titres apparaissent sans redémarrage.
    let scan_lance = if r.dossier_ajoute {
        super::system::scan::spawn_library_scan_par(
            state.clone(),
            false,
            None,
            super::system::scan::ORIGINE_AJOUT_DE_DOSSIER,
        )
        .await
    } else {
        false
    };
    Ok(Json(json!({
        "dir": r.dossier,
        "installed": r.installes,
        "failed": r.echecs,
        "tracks": r.titres,
        "scan_started": scan_lance,
    })))
}

/// `POST /onboarding/demo-tracks/remove`
async fn retirer(_admin: crate::auth::RequireAdmin, State(state): State<AppState>) -> Response {
    let Some(dossier) = state.dossier_titres_demo.clone() else {
        return Refus::DossierIndisponible.into_response();
    };
    let verrou = state.verrou_titres_demo.clone();
    let Ok(_verrou) = verrou.try_lock() else {
        return Refus::Occupe.into_response();
    };
    match desinstaller(&state, &dossier) {
        Ok(corps) => {
            tracing::info!(dossier = %dossier.display(), purgees = %corps["purged"], "demo_tracks_removed");
            Json(corps).into_response()
        }
        Err(r) => r,
    }
}

#[cfg(test)]
#[path = "titres_demo_tests_7858.rs"]
mod tests;
