//! Managed `yt-dlp` helper binary for YouTube playback.
//!
//! YouTube tightened its anti-bot gating (a `po_token` challenge) so Tune's
//! unauthenticated InnerTube clients now return `LOGIN_REQUIRED` even for public
//! videos. `yt-dlp` tracks that arms race, so Tune uses it as the YouTube stream
//! extraction backend. This module auto-provisions the `yt-dlp` binary (a single
//! self-contained executable — no Python required) into a tools directory and
//! resolves the path used by the extractor.
//!
//! Opt-in: nothing is downloaded until the user clicks "Enable YouTube playback"
//! (which calls [`download`]). Tune works fully without it.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::{info, warn};

/// Cached resolved path to the `yt-dlp` binary (set at startup / after download).
static YTDLP_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn cache() -> &'static Mutex<Option<PathBuf>> {
    YTDLP_PATH.get_or_init(|| Mutex::new(None))
}

/// Directory where Tune stores auto-provisioned helper binaries. Mirrors the
/// data-dir resolution used for the DB/artwork (config.rs): `%LOCALAPPDATA%`
/// on Windows, `~/Library/Application Support/Tune` on macOS, `~/.cache/tune`
/// (or `$XDG_CACHE_HOME/tune`) on Linux. Overridable with `TUNE_TOOLS_DIR`.
pub fn tools_dir() -> PathBuf {
    if let Ok(custom) = std::env::var("TUNE_TOOLS_DIR")
        && !custom.is_empty()
    {
        let p = PathBuf::from(custom);
        std::fs::create_dir_all(&p).ok();
        return p;
    }
    let lire = |nom: &str| std::env::var(nom).ok();
    let base = base_des_outils(
        &lire,
        // tmp-autorise: base seule : base_des_outils y joint l'UID du compte (#4770).
        &std::env::temp_dir(),
        crate::chemins_de_travail::uid_courant(),
    );
    let dir = base.join("tools");
    // #7420 : sur Tune OS (`ProtectHome=yes`) le dossier de l'utilisateur est
    // inaccessible et `yt-dlp: write: Permission denied (os error 13)` arrêtait
    // l'activation de YouTube. Repli : `TUNE_DATA_DIR/tools`, inscriptible.
    let repli = std::env::var("TUNE_DATA_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(|d| PathBuf::from(d).join("tools"));
    choisir_dossier_outils(dir, repli.as_deref())
}

/// Le dossier d'outils retenu : le dossier par défaut s'il est inscriptible
/// (ou s'il porte déjà `yt-dlp`, pour ne pas perdre un binaire installé), sinon
/// le repli quand lui l'est. Aucun des deux : le dossier par défaut, comme avant
/// (l'écriture dira alors son refus).
fn choisir_dossier_outils(defaut: PathBuf, repli: Option<&Path>) -> PathBuf {
    if defaut.join(binary_filename()).exists() || dossier_inscriptible(&defaut) {
        return defaut;
    }
    match repli {
        Some(r) if dossier_inscriptible(r) => {
            warn!(
                defaut = %defaut.display(),
                repli = %r.display(),
                "ytdlp_tools_dir_non_inscriptible_repli"
            );
            r.to_path_buf()
        }
        _ => defaut,
    }
}

/// Crée le dossier si besoin puis y écrit et retire un fichier sonde.
fn dossier_inscriptible(dir: &Path) -> bool {
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let sonde = dir.join(".sonde-ecriture");
    let ok = std::fs::write(&sonde, b"").is_ok();
    let _ = std::fs::remove_file(&sonde);
    ok
}

/// La base du dossier d'outils, variables d'environnement et repli **passés**.
///
/// Sans `HOME` (ni `XDG_CACHE_HOME` sous Linux), le repli était un nom FIXE
/// sous `temp_dir()` : `…/tune`, partagé par tous les comptes de la machine
/// (#4770). Le premier compte qui le créait en devenait propriétaire ; chez
/// les suivants, `create_dir_all(...).ok()` avalait le refus et le
/// téléchargement de `yt-dlp` échouait ensuite. Le repli passe désormais par
/// [`crate::chemins_de_travail`] : `temp_dir()/tune-<uid>`.
///
/// Tout est paramètre pour que le test simule deux comptes sans toucher à
/// l'environnement du processus, qui est partagé par les tests parallèles.
fn base_des_outils(lire: &dyn Fn(&str) -> Option<String>, temp: &Path, uid: u32) -> PathBuf {
    let repli = || crate::chemins_de_travail::racine_de_travail_sous(temp, "tune", uid);
    if cfg!(target_os = "windows") {
        lire("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("TuneServer"))
            .unwrap_or_else(|| PathBuf::from("TuneServer"))
    } else if cfg!(target_os = "macos") {
        lire("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support/Tune"))
            .unwrap_or_else(repli)
    } else {
        lire("XDG_CACHE_HOME")
            .map(|d| PathBuf::from(d).join("tune"))
            .or_else(|| lire("HOME").map(|h| PathBuf::from(h).join(".cache").join("tune")))
            .unwrap_or_else(repli)
    }
}

/// Local filename we store the binary under (platform-specific).
pub fn binary_filename() -> &'static str {
    if cfg!(target_os = "windows") {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

/// Path the auto-downloaded binary is stored at.
pub fn local_binary_path() -> PathBuf {
    tools_dir().join(binary_filename())
}

/// yt-dlp GitHub release asset name for the current platform, or `None` if
/// unsupported. yt-dlp ships single self-contained binaries (no archive).
fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => Some("yt-dlp_macos"),
        ("linux", "x86_64") => Some("yt-dlp_linux"),
        ("linux", "aarch64") => Some("yt-dlp_linux_aarch64"),
        ("windows", "x86_64") => Some("yt-dlp.exe"),
        ("windows", "x86") => Some("yt-dlp_x86.exe"),
        _ => None,
    }
}

/// Store the resolved binary path in the process-wide cache so the extractor
/// ([`binary`]) can find it without a DB handle.
pub fn set_binary(path: PathBuf) {
    *cache().lock().unwrap() = Some(path);
}

/// The currently-resolved `yt-dlp` binary path, if any. Read by the YouTube
/// extractor. `None` means YouTube playback is not enabled.
pub fn binary() -> Option<PathBuf> {
    cache().lock().unwrap().clone()
}

/// Resolve the `yt-dlp` binary and populate the cache. Order: an explicit
/// configured path (the `yt_dlp_path` setting), then the auto-download location,
/// then a `yt-dlp` on `PATH`. Returns the resolved path (also cached).
pub async fn resolve(configured_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = configured_path
        && !p.is_empty()
    {
        let pb = PathBuf::from(p);
        if pb.exists() {
            set_binary(pb.clone());
            return Some(pb);
        }
    }
    let local = local_binary_path();
    if local.exists() {
        set_binary(local.clone());
        return Some(local);
    }
    // Fall back to a `yt-dlp` already on PATH.
    let on_path = tokio::process::Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg("yt-dlp")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if on_path {
        let pb = PathBuf::from("yt-dlp");
        set_binary(pb.clone());
        return Some(pb);
    }
    None
}

/// Download the latest `yt-dlp` binary for this platform into [`tools_dir`],
/// make it executable, cache the path, and return `(path, version_tag)`.
pub async fn download() -> Result<(PathBuf, String), String> {
    let asset = asset_name().ok_or_else(|| {
        format!(
            "yt-dlp: unsupported platform {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;

    let client = crate::http::client::builder()
        .user_agent("tune-server")
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| format!("yt-dlp: http client: {e}"))?;

    // Resolve the release + asset URL from the GitHub API.
    let mut req = client
        .get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest")
        .header("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let release: serde_json::Value = req
        .send()
        .await
        .map_err(|e| format!("yt-dlp: fetch release: {e}"))?
        .error_for_status()
        .map_err(|e| format!("yt-dlp: release status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("yt-dlp: parse release: {e}"))?;

    let tag = release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let url = release
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|a| a.get("name").and_then(|n| n.as_str()) == Some(asset))
        })
        .and_then(|a| a.get("browser_download_url").and_then(|u| u.as_str()))
        .ok_or_else(|| format!("yt-dlp: asset '{asset}' not found in latest release"))?
        .to_string();

    info!(asset, url = %url, tag = %tag, "ytdlp_download_starting");
    let bytes = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("yt-dlp: download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("yt-dlp: download status: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("yt-dlp: download body: {e}"))?;

    let dest = local_binary_path();
    // Write to a temp file then rename, so a partial download never looks valid.
    let tmp = dest.with_extension("download");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("yt-dlp: write: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("yt-dlp: chmod: {e}"))?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| format!("yt-dlp: install: {e}"))?;

    set_binary(dest.clone());
    info!(path = %dest.display(), tag = %tag, size = bytes.len(), "ytdlp_download_complete");
    Ok((dest, tag))
}

/// Query `yt-dlp --version` for the given binary (best-effort).
pub async fn version_of(path: &Path) -> Option<String> {
    let out = tokio::process::Command::new(path)
        .arg("--version")
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        warn!("ytdlp_version_query_failed");
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

// ---------------------------------------------------------------------------
// #4366 — rafraîchir le binaire GÉRÉ
// ---------------------------------------------------------------------------
//
// Mesuré le 27/09/2026 : avec yt-dlp 2026.07.04 (la version des deux
// installations en échec), yt-dlp LUI-MÊME prend « HTTP Error 403 » au
// téléchargement, 6 essais sur 6 et sur trois vidéos ; avec 2026.08.19, 6
// réussites sur 6. Le rejeu des en-têtes (#4426) était en place. Le défaut est
// que le binaire n'était téléchargé qu'une fois, puis jamais rafraîchi.
//
// Deux déclencheurs (décision de Bertrand du 27/09) : un 403 amont sur une URL
// YouTube, au plus une fois toutes les 12 h ; et, au démarrage, un binaire de
// plus de 14 jours. Seul le binaire GÉRÉ est touché (celui de
// [`local_binary_path`]) : un `yt_dlp_path` choisi par l'utilisateur, ou un
// `yt-dlp` du PATH, ne l'est jamais. Un échec de téléchargement n'est jamais
// fatal : [`download`] écrit un fichier temporaire puis le renomme, l'ancien
// binaire reste donc en place tant que le nouveau n'est pas complet.

/// Réglage où la version installée est affichée par `/system/youtube/status`.
pub const CLE_VERSION: &str = "ytdlp_version";

/// Pas plus d'une tentative de rafraîchissement par fenêtre de 12 h.
pub const DELAI_ENTRE_RAFRAICHISSEMENTS: Duration = Duration::from_secs(12 * 3600);

/// Au démarrage, un binaire géré plus vieux que ça est rafraîchi.
pub const AGE_MAX_AU_DEMARRAGE: Duration = Duration::from_secs(14 * 24 * 3600);

/// Pourquoi on rafraîchit : écrit tel quel au journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaisonRafraichissement {
    /// `googlevideo` a refusé en 403 une URL rendue pour YouTube.
    Refus403,
    /// Le binaire géré a dépassé [`AGE_MAX_AU_DEMARRAGE`].
    Demarrage,
}

impl RaisonRafraichissement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refus403 => "refus_403",
            Self::Demarrage => "demarrage",
        }
    }
}

/// Ce qu'une demande de rafraîchissement a donné.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rafraichissement {
    /// Aucun yt-dlp résolu : YouTube n'est pas activé.
    Absent,
    /// Le binaire utilisé n'est pas le binaire géré : on n'y touche pas.
    BinaireUtilisateur,
    /// Dernière tentative trop récente, ou binaire trop jeune au démarrage.
    PasEncore,
    /// Nouveau binaire en place.
    Fait {
        ancienne: Option<String>,
        nouvelle: String,
    },
    /// Téléchargement raté : l'ancien binaire reste en place.
    Echec(String),
}

/// Le fichier qui horodate la dernière TENTATIVE, à côté du binaire.
fn fichier_horodatage(gere: &Path) -> PathBuf {
    gere.with_extension("rafraichi")
}

fn lire_horodatage(gere: &Path) -> Option<std::time::SystemTime> {
    let brut = std::fs::read_to_string(fichier_horodatage(gere)).ok()?;
    let secondes: u64 = brut.trim().parse().ok()?;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(secondes))
}

/// Écrit l'horodatage par fichier temporaire puis renommage : un arrêt au
/// milieu ne laisse jamais un fichier tronqué.
fn ecrire_horodatage(gere: &Path, quand: std::time::SystemTime) -> std::io::Result<()> {
    let secondes = quand
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cible = fichier_horodatage(gere);
    let tmp = cible.with_extension("rafraichi.tmp");
    std::fs::write(&tmp, secondes.to_string())?;
    std::fs::rename(&tmp, &cible)
}

fn date_du_binaire(gere: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(gere).and_then(|m| m.modified()).ok()
}

/// `reference` est-elle plus vieille que `seuil` ? Sans référence, oui. Une
/// référence dans le FUTUR (horloge reculée) compte aussi comme échue : sinon
/// le rafraîchissement serait bloqué jusqu'à ce que l'horloge la rattrape.
fn echu(
    reference: Option<std::time::SystemTime>,
    maintenant: std::time::SystemTime,
    seuil: Duration,
) -> bool {
    match reference {
        None => true,
        Some(r) => maintenant
            .duration_since(r)
            .map_or(true, |age| age >= seuil),
    }
}

/// La décision, sans réseau ni processus : `Ok(())` si on doit télécharger.
///
/// - `courant` : le binaire résolu ([`binary`]) ; `gere` : [`local_binary_path`].
/// - Toute raison : pas plus d'une tentative par [`DELAI_ENTRE_RAFRAICHISSEMENTS`]
///   (horodatage, sinon date du binaire).
/// - Au démarrage, en plus : le binaire doit avoir [`AGE_MAX_AU_DEMARRAGE`].
fn decider(
    raison: RaisonRafraichissement,
    courant: Option<&Path>,
    gere: &Path,
    maintenant: std::time::SystemTime,
) -> Result<(), Rafraichissement> {
    let courant = courant.ok_or(Rafraichissement::Absent)?;
    if courant != gere {
        return Err(Rafraichissement::BinaireUtilisateur);
    }
    let derniere = lire_horodatage(gere).or_else(|| date_du_binaire(gere));
    if !echu(derniere, maintenant, DELAI_ENTRE_RAFRAICHISSEMENTS) {
        return Err(Rafraichissement::PasEncore);
    }
    if raison == RaisonRafraichissement::Demarrage
        && !echu(date_du_binaire(gere), maintenant, AGE_MAX_AU_DEMARRAGE)
    {
        return Err(Rafraichissement::PasEncore);
    }
    Ok(())
}

/// Le rafraîchissement, téléchargement et lecture de version INJECTÉS (les
/// tests n'ont pas de réseau). `telecharger` rend la nouvelle version.
async fn rafraichir_avec<V, VF, T, TF>(
    raison: RaisonRafraichissement,
    courant: Option<&Path>,
    gere: &Path,
    maintenant: std::time::SystemTime,
    version: V,
    telecharger: T,
) -> Rafraichissement
where
    V: FnOnce(PathBuf) -> VF,
    VF: std::future::Future<Output = Option<String>>,
    T: FnOnce() -> TF,
    TF: std::future::Future<Output = Result<String, String>>,
{
    if let Err(refus) = decider(raison, courant, gere, maintenant) {
        return refus;
    }
    // L'horodatage d'abord : un téléchargement qui échoue compte aussi, sinon
    // chaque 403 relancerait un téléchargement quand GitHub est injoignable.
    if let Err(e) = ecrire_horodatage(gere, maintenant) {
        warn!(error = %e, "ytdlp_horodatage_non_ecrit");
    }
    let debut = std::time::Instant::now();
    let ancienne = version(gere.to_path_buf()).await;
    match telecharger().await {
        Ok(nouvelle) => {
            info!(
                raison = raison.as_str(),
                ancienne = ancienne.as_deref().unwrap_or("inconnue"),
                nouvelle = %nouvelle,
                duree_ms = debut.elapsed().as_millis() as u64,
                "ytdlp_rafraichi"
            );
            Rafraichissement::Fait { ancienne, nouvelle }
        }
        Err(e) => {
            // `download` renomme un fichier complet ou ne touche à rien.
            let _ = std::fs::remove_file(gere.with_extension("download"));
            warn!(
                raison = raison.as_str(),
                ancienne = ancienne.as_deref().unwrap_or("inconnue"),
                duree_ms = debut.elapsed().as_millis() as u64,
                error = %e,
                "ytdlp_rafraichissement_echoue_ancien_binaire_garde"
            );
            Rafraichissement::Echec(e)
        }
    }
}

/// Un seul rafraîchissement à la fois : le second appelant relit l'horodatage
/// écrit par le premier et s'arrête sur [`Rafraichissement::PasEncore`].
static VERROU_RAFRAICHISSEMENT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Rafraîchit le binaire géré si les règles de #4366 le permettent.
pub async fn rafraichir(raison: RaisonRafraichissement) -> Rafraichissement {
    let _garde = VERROU_RAFRAICHISSEMENT.lock().await;
    let courant = binary();
    rafraichir_avec(
        raison,
        courant.as_deref(),
        &local_binary_path(),
        std::time::SystemTime::now(),
        |chemin| async move { version_of(&chemin).await },
        || async {
            let (chemin, tag) = download().await?;
            Ok(version_of(&chemin).await.unwrap_or(tag))
        },
    )
    .await
}

// ---------------------------------------------------------------------------
// #6967 — la version installée, comparée à la dernière publiée
// ---------------------------------------------------------------------------
//
// Scorpio (VNAV, 24/10/2026) demandait à quelle fréquence yt-dlp est tenu à
// jour. `/system/youtube/status` disait la version installée, jamais si elle
// était la dernière. La dernière publiée est le `tag_name` de la publication
// GitHub, la même source que [`download`] ; elle est gardée six heures, pour
// qu'un écran de réglages ouvert souvent n'interroge pas GitHub à chaque fois.

/// La publication la plus récente de yt-dlp, côté GitHub.
pub const URL_DERNIERE_PUBLICATION: &str =
    "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

/// Durée pendant laquelle la dernière version lue reste valable.
pub const DUREE_DERNIERE_VERSION: Duration = Duration::from_secs(6 * 3600);

/// Après un échec de lecture, on ne réessaie pas avant ce délai.
const DUREE_ECHEC_DERNIERE_VERSION: Duration = Duration::from_secs(15 * 60);

/// Compare deux versions de yt-dlp (`2026.08.19`, nightly `2026.08.19.232611`)
/// partie par partie, en nombres. `None` si l'une n'est pas lisible : on ne
/// conclut pas alors qu'elle est à jour, ni qu'elle ne l'est pas.
pub fn comparer_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    fn parties(v: &str) -> Option<Vec<u64>> {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let parties: Option<Vec<u64>> = v.split('.').map(|p| p.parse().ok()).collect();
        parties.filter(|p| !p.is_empty())
    }
    let (a, b) = (parties(a)?, parties(b)?);
    // Une version nightly a une partie de plus : 2026.08.19.232611 vient
    // après 2026.08.19, ce que la comparaison de listes donne d'elle-même.
    Some(a.cmp(&b))
}

/// Ce que `/system/youtube/status` dit de la version (#6967).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EtatDeVersion {
    /// La version installée, `None` sans yt-dlp.
    pub installee: Option<String>,
    /// La dernière version publiée, `None` si GitHub n'a pas répondu.
    pub publiee: Option<String>,
    /// `Some(true)` : la version installée est la dernière (ou plus récente,
    /// une nightly). `None` : on ne sait pas — l'une des deux manque ou
    /// n'est pas lisible.
    pub a_jour: Option<bool>,
    /// Pourquoi la dernière version n'est pas connue.
    pub erreur: Option<String>,
}

/// La décision, sans réseau : à jour si l'installée n'est pas plus ancienne.
pub fn etat_de_version(installee: Option<&str>, publiee: Result<&str, &str>) -> EtatDeVersion {
    let a_jour = match (installee, publiee) {
        (Some(i), Ok(p)) => comparer_versions(i, p).map(|o| o != std::cmp::Ordering::Less),
        _ => None,
    };
    EtatDeVersion {
        installee: installee.map(str::to_string),
        publiee: publiee.ok().map(str::to_string),
        a_jour,
        erreur: publiee.err().map(str::to_string),
    }
}

/// Lit le `tag_name` de la publication à `url` (sans cache).
pub async fn lire_derniere_version(url: &str) -> Result<String, String> {
    let client = crate::http::client::builder()
        .user_agent("tune-server")
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|e| format!("yt-dlp: http client: {e}"))?;
    let mut req = client
        .get(url)
        .header("Accept", "application/vnd.github+json");
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let release: serde_json::Value = req
        .send()
        .await
        .map_err(|e| format!("yt-dlp: fetch release: {e}"))?
        .error_for_status()
        .map_err(|e| format!("yt-dlp: release status: {e}"))?
        .json()
        .await
        .map_err(|e| format!("yt-dlp: parse release: {e}"))?;
    release
        .get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "yt-dlp: release without tag_name".to_string())
}

type DerniereLue = (std::time::Instant, Result<String, String>);

static DERNIERE_VERSION: tokio::sync::Mutex<Option<DerniereLue>> =
    tokio::sync::Mutex::const_new(None);

/// La dernière version publiée de yt-dlp, gardée [`DUREE_DERNIERE_VERSION`]
/// (un échec, [`DUREE_ECHEC_DERNIERE_VERSION`]).
pub async fn derniere_version_publiee() -> Result<String, String> {
    let mut garde = DERNIERE_VERSION.lock().await;
    if let Some((quand, lue)) = garde.as_ref() {
        let validite = if lue.is_ok() {
            DUREE_DERNIERE_VERSION
        } else {
            DUREE_ECHEC_DERNIERE_VERSION
        };
        if quand.elapsed() < validite {
            return lue.clone();
        }
    }
    let lue = lire_derniere_version(URL_DERNIERE_PUBLICATION).await;
    if let Err(e) = &lue {
        warn!(error = %e, "ytdlp_derniere_version_inconnue");
    }
    *garde = Some((std::time::Instant::now(), lue.clone()));
    lue
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_matches_current_platform() {
        // On any supported dev/CI platform we must resolve an asset.
        let a = asset_name();
        match std::env::consts::OS {
            "macos" => assert_eq!(a, Some("yt-dlp_macos")),
            "linux" => assert!(a == Some("yt-dlp_linux") || a == Some("yt-dlp_linux_aarch64")),
            "windows" => assert!(a == Some("yt-dlp.exe") || a == Some("yt-dlp_x86.exe")),
            _ => {}
        }
    }

    #[test]
    fn binary_filename_has_exe_on_windows() {
        if cfg!(target_os = "windows") {
            assert_eq!(binary_filename(), "yt-dlp.exe");
        } else {
            assert_eq!(binary_filename(), "yt-dlp");
        }
    }

    /// #7420 : un dossier d'outils refusé (ici : un fichier à la place d'un
    /// dossier parent, refus garanti même en root) se replie sur le dossier
    /// inscriptible au lieu de laisser échouer l'écriture de `yt-dlp`.
    ///
    /// Contre-épreuve : faire rendre `defaut` à `choisir_dossier_outils` dans
    /// tous les cas — le premier assert rougit.
    #[test]
    fn dossier_d_outils_refuse_se_replie_sur_le_dossier_de_donnees() {
        let racine = crate::test_scratch::scratch_dir("tune-ytdlp-repli-7420");
        let fichier = racine.path().join("home");
        std::fs::write(&fichier, b"pas un dossier").unwrap();
        let defaut = fichier.join(".cache/tune/tools");
        let repli = racine.path().join("data/tools");

        let choisi = choisir_dossier_outils(defaut.clone(), Some(&repli));
        assert_eq!(choisi, repli, "le repli n'est pas pris");
        std::fs::write(choisi.join("yt-dlp"), b"#!").expect("le repli doit accepter l'écriture");

        // Sans repli, ou avec un repli refusé aussi : le dossier par défaut.
        assert_eq!(choisir_dossier_outils(defaut.clone(), None), defaut);
        let refuse = fichier.join("autre");
        assert_eq!(
            choisir_dossier_outils(defaut.clone(), Some(&refuse)),
            defaut
        );

        // Un dossier par défaut inscriptible reste prioritaire.
        let bon = racine.path().join("bon/tools");
        assert_eq!(choisir_dossier_outils(bon.clone(), Some(&repli)), bon);
    }

    #[test]
    fn local_binary_path_under_tools_dir() {
        let p = local_binary_path();
        assert!(p.ends_with(binary_filename()));
        assert!(p.parent().unwrap().ends_with("tools"));
    }

    /// Le témoin de #4770 pour les outils : sans `HOME`, deux comptes ne
    /// visent plus le même dossier, et le second peut y écrire même quand
    /// le premier a déjà créé le sien (ou l'ancien nom fixe) en `555`.
    ///
    /// Contre-épreuve : remettre `temp.join("tune")` comme repli dans
    /// `base_des_outils` — le test rougit sur l'écriture refusée.
    #[cfg(unix)]
    #[test]
    fn sans_home_deux_comptes_ont_chacun_leurs_outils() {
        use std::os::unix::fs::PermissionsExt;

        if crate::chemins_de_travail::uid_courant() == 0 {
            eprintln!("témoin ignoré : exécuté en root, les modes ne mordent pas");
            return;
        }
        let racine = crate::test_scratch::scratch_dir("tune-ytdlp-deux-comptes");
        let rien = |_: &str| None;
        let le_sien = base_des_outils(&rien, racine.path(), 1001);
        let le_mien = base_des_outils(&rien, racine.path(), 1000);
        assert_ne!(
            le_mien, le_sien,
            "deux comptes partagent le dossier d'outils"
        );

        let ancien = racine.path().join("tune");
        for d in [&ancien, &le_sien] {
            std::fs::create_dir_all(d.join("tools")).expect("outils « de l'autre compte »");
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).expect("mode 555");
        }
        let cree = std::fs::create_dir_all(le_mien.join("tools"))
            .and_then(|_| std::fs::write(le_mien.join("tools").join("yt-dlp"), b"#!"));
        for d in [&ancien, &le_sien] {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).ok();
        }
        cree.unwrap_or_else(|e| panic!("écriture refusée dans {le_mien:?} : {e}"));
        assert!(
            le_mien.starts_with(racine.path()),
            "repli hors du dossier temporaire"
        );
    }

    /// Quand `HOME` est là, rien ne change : le repli ne sert que sans lui.
    #[test]
    fn avec_home_le_dossier_d_outils_ne_change_pas() {
        let home = |nom: &str| (nom == "HOME").then(|| "/home/moi".to_string());
        let base = base_des_outils(&home, Path::new("/ailleurs"), 1000);
        if cfg!(target_os = "macos") {
            assert_eq!(
                base,
                Path::new("/home/moi/Library/Application Support/Tune")
            );
        } else if !cfg!(target_os = "windows") {
            assert_eq!(base, Path::new("/home/moi/.cache/tune"));
        }
    }

    // -- #4366 : rafraîchissement du binaire géré (sans réseau) --------------

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::SystemTime;

    const H: u64 = 3600;
    const J: u64 = 24 * H;

    /// Un dossier d'outils jetable, avec un « binaire géré » daté de `age`.
    fn outils(etiquette: &str, age: Duration) -> (crate::test_scratch::ScratchDir, PathBuf) {
        let dossier = crate::test_scratch::scratch_dir(etiquette);
        let gere = dossier.path().join(binary_filename());
        std::fs::write(&gere, b"ancien").unwrap();
        let f = std::fs::File::options().write(true).open(&gere).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
        (dossier, gere)
    }

    /// Lance `rafraichir_avec` avec un faux téléchargement qui écrit « neuf »
    /// dans le binaire géré, et compte ses appels.
    async fn essai(
        raison: RaisonRafraichissement,
        courant: Option<&Path>,
        gere: &Path,
        maintenant: SystemTime,
    ) -> (Rafraichissement, usize) {
        let appels = Arc::new(AtomicUsize::new(0));
        let compteur = appels.clone();
        let cible = gere.to_path_buf();
        let r = rafraichir_avec(
            raison,
            courant,
            gere,
            maintenant,
            |_| async { Some("2026.07.04".to_string()) },
            move || async move {
                compteur.fetch_add(1, Ordering::SeqCst);
                std::fs::write(&cible, b"neuf").map_err(|e| e.to_string())?;
                Ok("2026.08.19".to_string())
            },
        )
        .await;
        (r, appels.load(Ordering::SeqCst))
    }

    fn fait() -> Rafraichissement {
        Rafraichissement::Fait {
            ancienne: Some("2026.07.04".into()),
            nouvelle: "2026.08.19".into(),
        }
    }

    /// Après un 403, pas plus d'un téléchargement par fenêtre de 12 h.
    ///
    /// Contre-épreuve : `DELAI_ENTRE_RAFRAICHISSEMENTS` à 11 h, ou retirer le
    /// test `echu(derniere, ..)` de `decider` — le deuxième essai télécharge.
    #[tokio::test]
    async fn refus_403_un_seul_rafraichissement_par_12_heures() {
        let (_d, gere) = outils("ytdlp-4366-delai", Duration::from_secs(30 * J));
        let t0 = SystemTime::now();

        let (r, n) = essai(RaisonRafraichissement::Refus403, Some(&gere), &gere, t0).await;
        assert_eq!((r, n), (fait(), 1), "premier 403 : rafraîchir");
        assert_eq!(std::fs::read(&gere).unwrap(), b"neuf");

        // 11 h 59 plus tard : l'horodatage persistant bloque.
        let t1 = t0 + Duration::from_secs(12 * H - 60);
        let (r, n) = essai(RaisonRafraichissement::Refus403, Some(&gere), &gere, t1).await;
        assert_eq!((r, n), (Rafraichissement::PasEncore, 0), "moins de 12 h");

        // 12 h pile : de nouveau permis.
        let t2 = t0 + Duration::from_secs(12 * H);
        let (r, n) = essai(RaisonRafraichissement::Refus403, Some(&gere), &gere, t2).await;
        assert_eq!((r, n), (fait(), 1), "12 h écoulées");
    }

    /// Un téléchargement raté garde l'ancien binaire ET compte pour le délai.
    #[tokio::test]
    async fn echec_garde_l_ancien_binaire_et_compte_pour_le_delai() {
        let (_d, gere) = outils("ytdlp-4366-echec", Duration::from_secs(30 * J));
        let t0 = SystemTime::now();
        let r = rafraichir_avec(
            RaisonRafraichissement::Refus403,
            Some(&gere),
            &gere,
            t0,
            |_| async { None },
            || async { Err("github injoignable".to_string()) },
        )
        .await;
        assert_eq!(r, Rafraichissement::Echec("github injoignable".into()));
        assert_eq!(std::fs::read(&gere).unwrap(), b"ancien");
        let (r, n) = essai(
            RaisonRafraichissement::Refus403,
            Some(&gere),
            &gere,
            t0 + Duration::from_secs(H),
        )
        .await;
        assert_eq!((r, n), (Rafraichissement::PasEncore, 0));
    }

    /// Un `yt_dlp_path` de l'utilisateur (ou un yt-dlp du PATH) n'est jamais
    /// remplacé, même vieux et même après un 403.
    ///
    /// Contre-épreuve : retirer le test `courant != gere` de `decider` — le
    /// faux téléchargement est appelé.
    #[tokio::test]
    async fn le_binaire_de_l_utilisateur_n_est_jamais_touche() {
        let (d, gere) = outils("ytdlp-4366-utilisateur", Duration::from_secs(30 * J));
        let le_sien = d.path().join("mon-yt-dlp");
        std::fs::write(&le_sien, b"le sien").unwrap();
        for raison in [
            RaisonRafraichissement::Refus403,
            RaisonRafraichissement::Demarrage,
        ] {
            let (r, n) = essai(raison, Some(&le_sien), &gere, SystemTime::now()).await;
            assert_eq!(
                (r, n),
                (Rafraichissement::BinaireUtilisateur, 0),
                "{raison:?}"
            );
        }
        assert_eq!(std::fs::read(&le_sien).unwrap(), b"le sien");
        assert_eq!(std::fs::read(&gere).unwrap(), b"ancien");
        let (r, n) = essai(
            RaisonRafraichissement::Refus403,
            None,
            &gere,
            SystemTime::now(),
        )
        .await;
        assert_eq!((r, n), (Rafraichissement::Absent, 0));
    }

    /// Au démarrage : 13 jours, on garde ; 14 jours et plus, on rafraîchit.
    ///
    /// Contre-épreuve : `AGE_MAX_AU_DEMARRAGE` à 13 jours, ou retirer la
    /// branche `Demarrage` de `decider` — le binaire de 13 jours est remplacé.
    #[tokio::test]
    async fn demarrage_seuil_de_14_jours() {
        let (_d, gere) = outils("ytdlp-4366-jeune", Duration::from_secs(13 * J));
        let (r, n) = essai(
            RaisonRafraichissement::Demarrage,
            Some(&gere),
            &gere,
            SystemTime::now(),
        )
        .await;
        assert_eq!(
            (r, n),
            (Rafraichissement::PasEncore, 0),
            "13 jours : on garde"
        );
        assert_eq!(std::fs::read(&gere).unwrap(), b"ancien");

        let (_d, gere) = outils("ytdlp-4366-vieux", Duration::from_secs(14 * J + 60));
        let (r, n) = essai(
            RaisonRafraichissement::Demarrage,
            Some(&gere),
            &gere,
            SystemTime::now(),
        )
        .await;
        assert_eq!((r, n), (fait(), 1), "14 jours : rafraîchir");
        assert_eq!(std::fs::read(&gere).unwrap(), b"neuf");
    }

    // ── #6967 — version installée et dernière publiée ──────────────────

    #[test]
    fn versions_comparees_en_nombres_6967() {
        use std::cmp::Ordering::*;
        assert_eq!(comparer_versions("2026.08.19", "2026.08.19"), Some(Equal));
        assert_eq!(comparer_versions("2026.07.04", "2026.08.19"), Some(Less));
        // 2026.10.2 vient après 2026.9.30 : pas un ordre de texte.
        assert_eq!(comparer_versions("2026.10.2", "2026.9.30"), Some(Greater));
        // Une nightly du même jour est plus récente que la publication.
        assert_eq!(
            comparer_versions("2026.08.19.232611", "2026.08.19"),
            Some(Greater)
        );
        assert_eq!(comparer_versions("inconnue", "2026.08.19"), None);
    }

    #[test]
    fn etat_de_version_dit_a_jour_en_retard_ou_inconnu_6967() {
        let en_retard = etat_de_version(Some("2026.07.04"), Ok("2026.08.19"));
        assert_eq!(en_retard.a_jour, Some(false));
        assert_eq!(en_retard.publiee.as_deref(), Some("2026.08.19"));
        assert_eq!(
            etat_de_version(Some("2026.08.19"), Ok("2026.08.19")).a_jour,
            Some(true)
        );
        let sans_github = etat_de_version(Some("2026.08.19"), Err("timeout"));
        assert_eq!(sans_github.a_jour, None, "sans la dernière, on ne sait pas");
        assert_eq!(sans_github.erreur.as_deref(), Some("timeout"));
        assert_eq!(etat_de_version(None, Ok("2026.08.19")).a_jour, None);
    }

    /// La dernière version se lit dans `tag_name`, comme pour le
    /// téléchargement ; contre un serveur local, sans réseau.
    #[tokio::test]
    async fn derniere_version_lue_dans_tag_name_6967() {
        use axum::{Json, Router, routing::get};
        let app = Router::new().route(
            "/releases/latest",
            get(|| async { Json(serde_json::json!({"tag_name": "2026.10.22", "assets": []})) }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await });
        let lue = lire_derniere_version(&format!("http://{adresse}/releases/latest")).await;
        assert_eq!(lue.as_deref(), Ok("2026.10.22"));
        let absente = lire_derniere_version(&format!("http://{adresse}/absente")).await;
        assert!(
            absente.is_err(),
            "un 404 n'est pas une version : {absente:?}"
        );
        serveur.abort();
    }
}
