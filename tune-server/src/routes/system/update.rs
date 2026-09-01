use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tracing::{error, info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::updater::{ReleaseAsset, ReleaseInfo, UpdateChecker};

use crate::state::AppState;

/// An in-progress library scan older than this is treated as stale (a scan
/// killed by a crash/restart leaves `scan_status = "scanning"` persisted), so
/// it can never block updates forever. A full cold scan of a large catalogue on
/// modest hardware (Synology ARM, ~49k files — Yacine) can legitimately run for
/// hours, so the window is generous.
const SCAN_GUARD_STALE_SECS: u64 = 12 * 3600;

/// Whether a library scan is genuinely in progress right now: `scan_status` is
/// "scanning" AND it started within [`SCAN_GUARD_STALE_SECS`]. Used to defer an
/// update restart that would otherwise kill a long scan mid-import (the batches
/// never persist, so the library stays empty and the scan looks "stuck" — the
/// user re-triggers it and the next auto-update kills it again).
fn scan_in_progress(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> bool {
    let settings = SettingsRepo::with_backend(backend.clone());
    let scanning = settings.get("scan_status").ok().flatten().as_deref() == Some("scanning");
    if !scanning {
        return false;
    }
    let started = settings
        .get("scan_started_at")
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<u64>().ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match started {
        Some(t) => now.saturating_sub(t) < SCAN_GUARD_STALE_SECS,
        // No/invalid start time recorded: treat as fresh so we err on the side
        // of protecting the scan rather than killing it.
        None => true,
    }
}

/// Query string of `POST /system/update/install`.
#[derive(serde::Deserialize, Default)]
pub(super) struct UpdateInstallParams {
    /// Install even if a deferral guard would otherwise hold the update back.
    force: Option<bool>,
}

/// Is any zone actually playing right now?
///
/// The update restarts the server — on UNIX by re-exec'ing in place, which
/// replaces the process image and tears every output down with it. Playback
/// does not survive: the OAAT endpoint sees its socket close and reconnects,
/// DLNA renderers stop, and the listener hears the music cut out.
///
/// It is silent, which is what makes it expensive. On .18 (2026-08-10) six
/// updates landed in a single day; two of them re-exec'd while zone 12 was
/// streaming to the OAAT endpoint — the journal shows `update_reexec` followed
/// immediately by `mdns_zone_reconnected` and `auto_resume_device_reconnected`.
/// Bertrand reported it as "micro-coupures du son" and there was nothing to
/// connect the sound to its cause: no message, no restart in the UI, same PID
/// in the journal (re-exec keeps it).
///
/// So defer, as we already do for a running scan. Every install is a deliberate
/// call today — `config.auto_update` is declared but read nowhere, so nothing
/// retries on its own — which is exactly why the UI carries the other half of
/// this fix: it warns that the music will stop and then passes `?force=true`.
/// A caller that does NOT say `force` has not been told what it is about to
/// interrupt, and that is the one we protect against.
async fn playback_in_progress(playback: &tune_core::playback::PlaybackManager) -> bool {
    playback
        .all_states()
        .await
        .iter()
        .any(|z| z.state == tune_core::playback::PlayState::Playing)
}

/// Can we actually create a file in `dir`? Permission *bits* are not the
/// answer: a read-only mount, an ACL, or a SELinux label all deny the write
/// while the mode still reads 0755. The only honest test is to create a file
/// and delete it — cheap, and it runs once per update request.
fn probe_dir_writable(dir: &std::path::Path) -> Result<(), String> {
    let probe = dir.join(".tune-update-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Best-effort detection of running inside a Docker/OCI container. In a
/// container the binary lives in a read-only image layer, so the in-app update
/// can never swap it (`copy new binary: Permission denied` — Yacine); the
/// correct update path is `docker compose pull && docker compose up -d`. Any one
/// of these signals is conclusive: the `/.dockerenv` marker file, a
/// `docker`/`containerd`/`kubepods` entry in the process cgroup, or the
/// `container` env var some runtimes set. Non-Linux hosts are never
/// containerised this way, so they always return false.
#[cfg(target_os = "linux")]
fn running_in_docker() -> bool {
    if std::path::Path::new("/.dockerenv").exists() {
        return true;
    }
    if std::env::var_os("container").is_some() {
        return true;
    }
    for cgroup in ["/proc/1/cgroup", "/proc/self/cgroup"] {
        if let Ok(contents) = std::fs::read_to_string(cgroup) {
            if contents.contains("docker")
                || contents.contains("containerd")
                || contents.contains("kubepods")
            {
                return true;
            }
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn running_in_docker() -> bool {
    false
}

const HOMEBREW_UPDATE_COMMAND: &str = "brew update && brew upgrade tune-server";
const HOMEBREW_UPDATE_HINT: &str = "This Tune installation is managed by Homebrew. Update it with `brew update && brew upgrade tune-server`. If the renesenses tap stays stale, run `brew untap renesenses/tap && brew tap renesenses/tap` first.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct HomebrewInstallation {
    executable: std::path::PathBuf,
    cellar_version: String,
}

/// Extract the formula version only from Tune's own Homebrew Cellar layout.
/// Merely seeing a `Cellar` component is not enough: another formula could
/// contain or invoke a binary named `tune-server` without owning this install.
fn homebrew_cellar_version(executable: &std::path::Path) -> Option<String> {
    let components: Vec<_> = executable.components().map(|c| c.as_os_str()).collect();
    components.windows(3).find_map(|parts| {
        if parts[0] != std::ffi::OsStr::new("Cellar")
            || parts[1] != std::ffi::OsStr::new("tune-server")
        {
            return None;
        }
        parts[2]
            .to_str()
            .filter(|version| !version.is_empty())
            .map(str::to_owned)
    })
}

/// Homebrew appends `_N` for formula revisions without changing the upstream
/// binary version. Treat `0.9.113_1` and binary `v0.9.113` as coherent.
fn homebrew_version_matches(cellar_version: &str, binary_version: &str) -> bool {
    fn normalize_binary(version: &str) -> &str {
        version.trim().trim_start_matches('v')
    }

    let cellar = normalize_binary(cellar_version);
    let cellar_upstream = cellar
        .split_once('_')
        .map_or(cellar, |(upstream, _)| upstream);
    cellar_upstream == normalize_binary(binary_version)
}

fn homebrew_installation(executable: &std::path::Path) -> Option<HomebrewInstallation> {
    // `current_exe` is usually already resolved, but Homebrew launches through
    // `opt/tune-server`. Canonicalising here makes the guard independent of the
    // platform's current_exe symlink semantics. A missing/unresolvable path is
    // still parsed as given so diagnostics never turn an I/O hiccup into a
    // silent permission to self-update.
    let resolved = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    let cellar_version = homebrew_cellar_version(&resolved)?;
    Some(HomebrewInstallation {
        executable: resolved,
        cellar_version,
    })
}

fn current_homebrew_installation() -> Option<HomebrewInstallation> {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(homebrew_installation)
}

fn homebrew_update_refusal(installation: &HomebrewInstallation, current: &str) -> Value {
    json!({
        "status": "managed_installation",
        "reason": "homebrew_managed_installation",
        "manager": "homebrew",
        "message": HOMEBREW_UPDATE_HINT,
        "detail": HOMEBREW_UPDATE_HINT,
        "command": HOMEBREW_UPDATE_COMMAND,
        "installation_version": installation.cellar_version,
        "current_version": current,
        "installation_version_mismatch": !homebrew_version_matches(
            &installation.cellar_version,
            current,
        ),
    })
}

fn homebrew_mismatch_result(installation: &HomebrewInstallation, current: &str) -> Option<Value> {
    if homebrew_version_matches(&installation.cellar_version, current) {
        return None;
    }
    Some(json!({
        "status": "warning",
        "reason": "homebrew_version_mismatch",
        "detail": format!(
            "Tune binary {current} is running from Homebrew Cellar {}, so the binary and web assets may come from different releases. {HOMEBREW_UPDATE_HINT}",
            installation.cellar_version
        ),
        "command": HOMEBREW_UPDATE_COMMAND,
        "current_version": current,
        "installation_version": installation.cellar_version,
    }))
}

/// Find the extractable archive asset (tar.gz or zip) for the current platform.
/// Excludes .dmg and .exe installers — we want the raw archive containing the binary + web/.
fn find_archive_asset(release: &ReleaseInfo) -> Option<&ReleaseAsset> {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    release.assets.iter().find(|a| {
        let name = a.name.to_lowercase();

        // Must be an archive, not an installer
        let is_archive = name.ends_with(".tar.gz") || name.ends_with(".zip");
        if !is_archive {
            return false;
        }

        // Exclude installer-only files
        if name.contains("setup") || name.contains("installer") {
            return false;
        }

        let os_match = match os {
            "macos" => name.contains("macos"),
            "linux" => name.contains("linux"),
            "windows" => name.contains("windows"),
            _ => false,
        };
        let arch_match = match arch {
            "aarch64" => name.contains("aarch64") || name.contains("arm64"),
            "x86_64" => name.contains("x86_64") || name.contains("amd64"),
            _ => true,
        };
        os_match && arch_match
    })
}

/// Build the public update contract only after resolving an archive that this
/// exact OS/architecture can install. GitHub can expose a release before every
/// platform asset has finished uploading; such a release is newer, but it is
/// not an available update for this server yet (#1575).
fn update_release_payload(
    current: &str,
    release: &ReleaseInfo,
    homebrew: Option<&HomebrewInstallation>,
) -> Value {
    let asset = find_archive_asset(release);
    let installation_version_mismatch =
        homebrew.is_some_and(|install| !homebrew_version_matches(&install.cellar_version, current));
    json!({
        "current": current,
        "latest": &release.version,
        "update_available": asset.is_some(),
        "download_url": asset.map(|a| &a.browser_download_url),
        "asset_name": asset.map(|a| &a.name),
        "release_notes": &release.body,
        "size_bytes": asset.map(|a| a.size).unwrap_or(0),
        "html_url": &release.html_url,
        "published_at": &release.published_at,
        "unavailable_reason": asset.is_none().then_some("no_compatible_asset"),
        "installable": homebrew.is_none(),
        "install_hint": homebrew.map(|_| HOMEBREW_UPDATE_HINT),
        "installation_manager": homebrew.map(|_| "homebrew"),
        "installation_version": homebrew.map(|install| &install.cellar_version),
        "installation_version_mismatch": installation_version_mismatch,
    })
}

#[cfg(test)]
mod update_availability_tests {
    use super::{HomebrewInstallation, update_release_payload};
    use std::path::PathBuf;
    use tune_core::updater::{ReleaseAsset, ReleaseInfo};

    fn release_with(asset_name: &str) -> ReleaseInfo {
        ReleaseInfo {
            tag_name: "v9.9.9".into(),
            version: "9.9.9".into(),
            name: "fixture".into(),
            body: "notes".into(),
            published_at: "2026-08-26T00:00:00Z".into(),
            html_url: "https://example.invalid/release".into(),
            assets: vec![ReleaseAsset {
                name: asset_name.into(),
                browser_download_url: "https://example.invalid/archive".into(),
                size: 42,
                content_type: "application/octet-stream".into(),
            }],
        }
    }

    #[test]
    fn une_release_sans_archive_compatible_n_est_pas_proposee() {
        let payload = update_release_payload(
            "0.9.113",
            &release_with("tune-server-plan9-mips64.tar.gz"),
            None,
        );

        assert_eq!(payload["update_available"], false);
        assert_eq!(payload["download_url"], serde_json::Value::Null);
        assert_eq!(payload["asset_name"], serde_json::Value::Null);
        assert_eq!(payload["unavailable_reason"], "no_compatible_asset");
    }

    #[test]
    fn une_release_avec_l_archive_de_la_plateforme_est_proposee() {
        let extension = if std::env::consts::OS == "windows" {
            "zip"
        } else {
            "tar.gz"
        };
        let name = format!(
            "tune-server-{}-{}.{}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            extension
        );
        let payload = update_release_payload("0.9.113", &release_with(&name), None);

        assert_eq!(payload["update_available"], true);
        assert_eq!(payload["asset_name"], name);
        assert_eq!(payload["unavailable_reason"], serde_json::Value::Null);
    }

    #[test]
    fn le_filtre_d_asset_conserve_le_contrat_homebrew() {
        let installation = HomebrewInstallation {
            executable: PathBuf::from("/opt/homebrew/Cellar/tune-server/0.9.112/bin/tune-server"),
            cellar_version: "0.9.112".into(),
        };
        let payload = update_release_payload(
            "0.9.113",
            &release_with("tune-server-plan9-mips64.tar.gz"),
            Some(&installation),
        );

        assert_eq!(payload["update_available"], false);
        assert_eq!(payload["unavailable_reason"], "no_compatible_asset");
        assert_eq!(payload["installable"], false);
        assert_eq!(payload["installation_manager"], "homebrew");
        assert_eq!(payload["installation_version"], "0.9.112");
        assert_eq!(payload["installation_version_mismatch"], true);
        assert!(
            payload["install_hint"]
                .as_str()
                .is_some_and(|hint| { hint.contains("brew update && brew upgrade tune-server") })
        );
    }
}

/// Trusted **minisign** public key for release signatures (audit item 8). The
/// matching secret key lives only in the release CI (a GitHub Actions secret);
/// this is the verify-only half, safe to embed.
///
/// ROLLOUT: left empty on purpose. While empty, signature verification is
/// skipped and self-update behaves exactly as before — nothing breaks. Fill it
/// with the real public key (the base64 line of `minisign -G`'s `.pub` file)
/// once the CI signing step is live; verification then becomes mandatory.
const UPDATE_PUBLIC_KEY: &str = "RWRjeNGnrhiQYHaMp7e0Cmr6PCC4tEY7UwenBFrbDBoIPDB7T9aBRwUM";

/// À qui la faute quand une mise à jour ne peut pas être vérifiée.
///
/// La question n'est pas cosmétique. En v0.9.71 la release est restée douze
/// heures visible mais incomplète ; le client a échoué, et la première réponse
/// au fil forum a envoyé Jean Valjean vérifier SON réseau. Il n'y était pour
/// rien. Un message qui ne nomme pas la cause fait chercher au mauvais endroit
/// — et le seul qui puisse trancher, c'est le code qui a vu la réponse HTTP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateBlame {
    /// Rien n'a répondu : réseau, DNS, proxy, coupure. Chez l'utilisateur.
    Unreachable,
    /// Le serveur a répondu, mais le fichier n'est pas là. Chez nous.
    ReleaseIncomplete,
    /// Le serveur a répondu qu'il allait mal (5xx, quota). Ni l'un ni l'autre.
    ServerError,
    /// Signature ou empreinte qui ne concorde pas. On refuse d'installer.
    Untrusted,
}

impl UpdateBlame {
    /// Marqueur de journal — un par cause, pour qu'un `grep` les sépare.
    pub(crate) fn marker(self) -> &'static str {
        match self {
            Self::Unreachable => "update_server_unreachable",
            Self::ReleaseIncomplete => "update_release_incomplete",
            Self::ServerError => "update_server_error",
            Self::Untrusted => "update_untrusted_archive",
        }
    }

    /// Ce que lit l'utilisateur : la cause, puis la conduite à tenir.
    pub(crate) fn user_message(self) -> &'static str {
        match self {
            Self::Unreachable => {
                "Impossible de joindre le serveur de mises à jour. Vérifiez votre connexion, \
                 puis réessayez."
            }
            Self::ReleaseIncomplete => {
                "Le serveur a répondu, mais cette version n'est pas complètement publiée. \
                 Ce n'est pas un problème de votre côté : réessayez plus tard."
            }
            Self::ServerError => {
                "Le serveur de mises à jour est momentanément indisponible. \
                 Ce n'est pas un problème de votre côté : réessayez plus tard."
            }
            Self::Untrusted => {
                "L'archive téléchargée ne correspond pas à sa signature. Installation refusée."
            }
        }
    }
}

/// Une cause typée, plus le détail technique destiné aux journaux.
#[derive(Debug, Clone)]
pub(crate) struct UpdateError {
    pub(crate) blame: UpdateBlame,
    pub(crate) detail: String,
}

impl UpdateError {
    fn new(blame: UpdateBlame, detail: impl Into<String>) -> Self {
        Self {
            blame,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

/// Un statut HTTP dit-il « ce fichier n'existe pas » ou « le serveur va mal » ?
///
/// Dans les deux cas il y a EU une réponse : quel que soit le statut, on ne
/// renvoie jamais l'utilisateur vérifier son propre réseau. C'est toute la
/// différence avec l'échec de `send()`.
pub(crate) fn blame_for_status(status: u16) -> UpdateBlame {
    match status {
        // L'artefact n'est pas là — la release est incomplète (cas v0.9.71).
        404 | 410 => UpdateBlame::ReleaseIncomplete,
        // Le serveur dit qu'il va mal, ou nous limite. Rien à conclure sur la
        // complétude de la release.
        429 | 500..=599 => UpdateBlame::ServerError,
        // 401/403 : dépôt privé, jeton, quota anonyme épuisé. Là encore une
        // réponse, donc pas le réseau de l'utilisateur.
        _ => UpdateBlame::ServerError,
    }
}

/// Verify a downloaded update archive against a minisign-signed `SHA256SUMS`
/// before it is extracted/installed. The signature authenticates `SHA256SUMS`
/// with the embedded key; the authenticated `SHA256SUMS` authenticates the
/// archive by hash. Defeats a compromised release proxy / GitHub metadata
/// pushing a malicious binary (RCE).
async fn verify_update_signature(
    client: &reqwest::Client,
    archive_name: &str,
    archive_bytes: &[u8],
    sums_url: Option<&str>,
    sig_url: Option<&str>,
) -> Result<(), UpdateError> {
    if UPDATE_PUBLIC_KEY.is_empty() {
        warn!("update_signature_check_skipped_no_key");
        return Ok(());
    }

    // Le fichier n'est même pas annoncé par la release : elle est incomplète,
    // exactement l'état dans lequel la v0.9.71 est restée douze heures.
    let sums_url = sums_url.ok_or_else(|| {
        UpdateError::new(
            UpdateBlame::ReleaseIncomplete,
            "release has no SHA256SUMS — refusing unsigned update",
        )
    })?;
    let sig_url = sig_url.ok_or_else(|| {
        UpdateError::new(
            UpdateBlame::ReleaseIncomplete,
            "release has no SHA256SUMS.minisig signature — refusing unsigned update",
        )
    })?;

    let fetch = |url: String| async move {
        // `send()` qui échoue = rien n'a répondu. C'est la SEULE branche qui
        // autorise à parler du réseau de l'utilisateur.
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| {
                UpdateError::new(UpdateBlame::Unreachable, format!("fetch {url} failed: {e}"))
            })?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(UpdateError::new(
                blame_for_status(status.as_u16()),
                format!("fetch {url}: HTTP {status}"),
            ));
        }
        resp.text().await.map_err(|e| {
            UpdateError::new(UpdateBlame::Unreachable, format!("read {url} failed: {e}"))
        })
    };
    let sums = fetch(sums_url.to_string()).await?;
    let sig_str = fetch(sig_url.to_string()).await?;

    // 1. Signature over SHA256SUMS with the embedded trusted key.
    let pk = minisign_verify::PublicKey::from_base64(UPDATE_PUBLIC_KEY).map_err(|e| {
        UpdateError::new(
            UpdateBlame::Untrusted,
            format!("invalid embedded update public key: {e}"),
        )
    })?;
    let sig = minisign_verify::Signature::decode(&sig_str).map_err(|e| {
        UpdateError::new(
            UpdateBlame::Untrusted,
            format!("invalid update signature: {e}"),
        )
    })?;
    pk.verify(sums.as_bytes(), &sig, false).map_err(|_| {
        UpdateError::new(
            UpdateBlame::Untrusted,
            "update signature does not match — refusing to install",
        )
    })?;

    // 2. The now-authenticated SHA256SUMS must list our archive with a hash
    //    matching the bytes we downloaded.
    use sha2::{Digest, Sha256};
    let got = format!("{:x}", Sha256::digest(archive_bytes));
    let want = sums
        .lines()
        .find_map(|line| {
            let mut it = line.split_whitespace();
            let hash = it.next()?;
            // `sha256sum` may prefix the name with `*` (binary) and CI writes a
            // `./` path prefix — match on the trailing file name.
            let file = it.next()?.trim_start_matches('*');
            if file.ends_with(archive_name) {
                Some(hash.to_lowercase())
            } else {
                None
            }
        })
        // Absent de la liste SIGNÉE : la release est incomplète, pas
        // frauduleuse. C'est exactement l'état de la v0.9.71 — SHA256SUMS
        // publié en ne couvrant que 5 fichiers sur 13. Accuser la signature
        // ici ferait croire à une attaque là où il n'y a qu'une publication
        // inachevée.
        .ok_or_else(|| {
            UpdateError::new(
                UpdateBlame::ReleaseIncomplete,
                format!("{archive_name} not listed in signed SHA256SUMS"),
            )
        })?;
    if want != got {
        // Là en revanche, le fichier est listé et son empreinte ne correspond
        // pas : on refuse d'installer.
        return Err(UpdateError::new(
            UpdateBlame::Untrusted,
            format!("archive hash mismatch — signed {want}, downloaded {got}"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod update_blame_tests {
    use super::{UpdateBlame, blame_for_status};

    #[test]
    fn une_reponse_recue_n_accuse_jamais_le_reseau_de_l_utilisateur() {
        // Le coeur de #1588 : dès qu'un statut HTTP existe, c'est qu'on a
        // joint le serveur. Renvoyer l'utilisateur vers sa connexion serait
        // le faire chercher chez lui un défaut qui est chez nous.
        for status in [400, 401, 403, 404, 410, 429, 500, 502, 503] {
            assert_ne!(
                blame_for_status(status),
                UpdateBlame::Unreachable,
                "HTTP {status} ne doit pas accuser le reseau"
            );
        }
    }

    #[test]
    fn artefact_absent_est_une_release_incomplete() {
        // Le cas vécu : l'asset macOS n'existait pas sur la v0.9.71.
        assert_eq!(blame_for_status(404), UpdateBlame::ReleaseIncomplete);
        assert_eq!(blame_for_status(410), UpdateBlame::ReleaseIncomplete);
    }

    #[test]
    fn serveur_en_peine_n_est_pas_une_release_incomplete() {
        // Un 503 ne dit RIEN sur la complétude de la release : l'annoncer
        // comme telle serait inventer une cause.
        for status in [429, 500, 502, 503] {
            assert_eq!(blame_for_status(status), UpdateBlame::ServerError);
        }
    }

    #[test]
    fn chaque_cause_a_son_marqueur_et_son_message() {
        let toutes = [
            UpdateBlame::Unreachable,
            UpdateBlame::ReleaseIncomplete,
            UpdateBlame::ServerError,
            UpdateBlame::Untrusted,
        ];
        let mut marqueurs: Vec<&str> = toutes.iter().map(|b| b.marker()).collect();
        marqueurs.sort_unstable();
        let avant = marqueurs.len();
        marqueurs.dedup();
        assert_eq!(marqueurs.len(), avant, "deux causes partagent un marqueur");

        // Seule la cause « injoignable » a le droit de parler de la connexion
        // de l'utilisateur. C'est la règle que la v0.9.71 a enfreinte.
        for b in toutes {
            let msg = b.user_message();
            assert!(!msg.is_empty());
            if b != UpdateBlame::Unreachable {
                assert!(
                    !msg.contains("votre connexion"),
                    "{b:?} ne doit pas renvoyer l'utilisateur a son reseau : {msg}"
                );
            }
        }
    }
}

#[cfg(test)]
mod signed_update_tests {
    use super::UPDATE_PUBLIC_KEY;

    // A real signature produced by `minisign -S` with the production key pair,
    // over the message below. Locks in that (a) the embedded public key parses,
    // (b) it verifies genuine minisign CLI output (prehashed format), and
    // (c) tampering is detected — i.e. the whole signed-update chain agrees.
    const FIXTURE_MSG: &[u8] = b"hello-tune-update";
    const FIXTURE_SIG: &str = "untrusted comment: signature from minisign secret key\n\
RURjeNGnrhiQYLis6QuGtYZRL+wCW2VzRIUVBFXrOHJphbtvrnQXDKmV2aitwA1ZHqOAPuIJRSVYT1HWTfHrXzosPtLiwNtZSA4=\n\
trusted comment: timestamp:1785772095\tfile:sigtest.txt\thashed\n\
kwD8rrpp1dpGuBsy+q0AByW/UZ9CjNSAOJH5bivNcpTQDNkE1aB073ruWxcwOeuJXwpWeh/XVMnkDIoV0BU3Aw==\n";

    #[test]
    fn embedded_key_verifies_real_minisign_signature() {
        assert!(
            !UPDATE_PUBLIC_KEY.is_empty(),
            "production public key must be embedded"
        );
        let pk = minisign_verify::PublicKey::from_base64(UPDATE_PUBLIC_KEY)
            .expect("embedded public key parses");
        let sig = minisign_verify::Signature::decode(FIXTURE_SIG).expect("signature decodes");
        pk.verify(FIXTURE_MSG, &sig, false)
            .expect("genuine signature verifies");
        // Tampered payload must be rejected.
        assert!(pk.verify(b"tampered-payload", &sig, false).is_err());
    }
}

/// GET /system/update/check
///
/// Fetches the latest release from GitHub, compares versions, and returns update info.
pub(super) async fn update_check() -> Json<Value> {
    let checker = UpdateChecker::new();
    let current = tune_core::version();
    let homebrew = current_homebrew_installation();
    let installation_version_mismatch = homebrew
        .as_ref()
        .is_some_and(|install| !homebrew_version_matches(&install.cellar_version, current));

    match checker.check().await {
        Ok(Some(release)) => Json(update_release_payload(current, &release, homebrew.as_ref())),
        Ok(None) => Json(json!({
            "current": current,
            "latest": current,
            "update_available": false,
            "download_url": null,
            "release_notes": null,
            "size_bytes": 0,
            "installable": homebrew.is_none(),
            "install_hint": homebrew.as_ref().map(|_| HOMEBREW_UPDATE_HINT),
            "installation_manager": homebrew.as_ref().map(|_| "homebrew"),
            "installation_version": homebrew.as_ref().map(|install| &install.cellar_version),
            "installation_version_mismatch": installation_version_mismatch,
        })),
        Err(e) => {
            warn!(error = %e, "update_check_failed");
            Json(json!({
                "current": current,
                "latest": null,
                "update_available": false,
                "error": e,
            }))
        }
    }
}

/// POST /system/update/install
///
/// Validates that an update is available, then spawns the download/extract/install
/// cycle in the background and returns immediately.  Progress is exposed via
/// `GET /system/update/status` (`phase` field).
///
/// `?force=true` overrides the deferral guards that exist to protect work in
/// progress (currently: playback). The UI sets it on the install button, which
/// sits directly under the warning that playback will stop.
pub(super) async fn update_install(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<UpdateInstallParams>,
) -> impl IntoResponse {
    let force = params.force.unwrap_or(false);
    // Prevent concurrent updates
    {
        let phase = state.update_phase.lock().unwrap();
        if let Some(ref p) = *phase {
            if !p.starts_with("failed") {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                        "status": "already_in_progress",
                        "phase": p,
                    })),
                )
                    .into_response();
            }
        }
    }

    // Guard: in Docker the binary lives in a read-only image layer, so the
    // self-update can never swap it (`copy new binary: Permission denied` —
    // Yacine) and every retry fails the same way. Detect it up front, before
    // downloading anything, and steer the user to the image-pull update path.
    // This is not an error condition, so return 200 with a clear status the UI
    // can present as guidance rather than a failure.
    if running_in_docker() {
        info!("update_skipped_docker");
        return (
            StatusCode::OK,
            Json(json!({
                "status": "docker",
                "message": "You're running Tune in Docker. Update by pulling the new image: docker compose pull && docker compose up -d (your data in the mounted volumes is preserved)."
            })),
        )
            .into_response();
    }

    let current_exe = std::env::current_exe().ok();

    // A Cellar is one Homebrew-owned unit: binary, receipt and web assets.
    // Replacing only Tune's executable leaves Homebrew believing the old
    // formula is installed and can pair a new server with an old web client
    // (#2448). Never mutate any part of that unit behind the package manager's
    // back; tell both current and older clients how to take the supported path.
    if let Some(installation) = current_exe.as_deref().and_then(homebrew_installation) {
        let refusal = homebrew_update_refusal(&installation, tune_core::version());
        info!(
            executable = %installation.executable.display(),
            cellar_version = %installation.cellar_version,
            binary_version = tune_core::version(),
            mismatch = refusal["installation_version_mismatch"].as_bool().unwrap_or(false),
            "update_skipped_homebrew"
        );
        let _ = SettingsRepo::with_backend(state.backend.clone())
            .set("last_update_result", &refusal.to_string());
        return (StatusCode::OK, Json(refusal)).into_response();
    }

    // Guard: refuse update if .no-auto-update flag file exists
    let working_dir = current_exe.and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(ref dir) = working_dir {
        if dir.join(".no-auto-update").exists() {
            warn!("update_blocked_no_auto_update_flag");
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "blocked",
                    "message": "Update blocked: .no-auto-update flag file exists. Remove it to allow updates."
                })),
            )
                .into_response();
        }
    }

    // Guard: the install stages the new binary next to the running one, so a
    // directory we cannot write to dooms the update — but only after a 45 MB
    // download, an extraction, and a raw `copy new binary: Permission denied`
    // that tells the user nothing about what to do (Yacine: two identical
    // failures 55 minutes apart, still on 0.9.42). Probe it up front and hand
    // back the path and the account so the fix is a single chown away.
    if let Some(ref dir) = working_dir {
        if let Err(e) = probe_dir_writable(dir) {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("USERNAME"))
                .unwrap_or_else(|_| "the account running Tune".into());
            warn!(dir = %dir.display(), user = %user, error = %e, "update_blocked_dir_not_writable");
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "status": "not_writable",
                    "message": format!(
                        "Tune cannot install the update: the folder holding the binary ({}) is not writable by {user} ({e}). Fix the ownership of that folder — e.g. sudo chown -R {user} {} — or install the new version by hand, then retry.",
                        dir.display(),
                        dir.display()
                    )
                })),
            )
                .into_response();
        }
    }

    // Guard: don't restart while a library scan is running. A full cold scan of
    // a large catalogue on modest hardware (Synology ARM, ~49k files — Yacine)
    // takes hours; the update restart kills it mid-import before any batch
    // persists, so the library never fills and the scan looks permanently
    // "stuck" — the user re-triggers it and the next auto-update kills it again.
    // Defer instead: the client's periodic auto-update simply retries and lands
    // once the scan finishes. Manual updates get the same clear message. Bounded
    // by a staleness window so a crashed scan can never block updates forever.
    if scan_in_progress(&state.backend) {
        warn!("update_deferred_scan_in_progress");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "blocked",
                "reason": "scan_in_progress",
                "message": "Update deferred: a library scan is in progress. It will be applied automatically once the scan finishes."
            })),
        )
            .into_response();
    }

    // Guard: don't restart while music is playing. The restart re-execs the
    // process, which kills every output mid-stream — and says so nowhere, so
    // the listener just hears the music cut out (#1462). An update that lands
    // after the album is worth more than one that interrupts it.
    if !force && playback_in_progress(&state.playback).await {
        warn!("update_deferred_playback_in_progress");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "blocked",
                "reason": "playback_in_progress",
                "message": "Update deferred: music is playing and installing it would stop playback. It will be applied automatically once playback stops."
            })),
        )
            .into_response();
    }

    // Guard: refuse update if current binary has postgres but we might lose it
    if cfg!(feature = "postgres") {
        // This is a pre-flight warning; the actual binary check happens after download
    }

    // 1. Check for update (fast — just a GitHub API call)
    let checker = UpdateChecker::new();
    let release = match checker.check().await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return (
                StatusCode::OK,
                Json(json!({"status": "up_to_date", "message": "Already running the latest version"})),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(json!({"status": "error", "message": format!("Failed to check for updates: {e}")})),
            )
                .into_response();
        }
    };

    let asset = match find_archive_asset(&release) {
        Some(a) => a.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"status": "error", "message": "No compatible archive found for this platform"})),
            )
                .into_response();
        }
    };

    info!(
        version = %release.version,
        asset = %asset.name,
        size = asset.size,
        "update_download_starting"
    );

    // Signed-update material: the archive is verified against a minisign-signed
    // SHA256SUMS before install (audit item 8). Both are release assets.
    let sums_url = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .map(|a| a.browser_download_url.clone());
    let sig_url = release
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS.minisig")
        .map(|a| a.browser_download_url.clone());

    // 2. Mark phase = downloading and spawn the background task
    {
        let mut phase = state.update_phase.lock().unwrap();
        *phase = Some("downloading".into());
    }

    let version = release.version.clone();
    let response_version = version.clone();
    let http_client = state.http_client.clone();
    let update_phase = state.update_phase.clone();

    tokio::spawn(async move {
        let set_phase = |p: &str| {
            // Log every phase, and warn on failures — set_phase was previously
            // silent, so a failed install (e.g. permission denied when Tune is
            // installed under Program Files) left no trace in the logs and the
            // update just "didn't happen" (Dominique, Windows 11).
            if p.starts_with("failed") {
                warn!(phase = %p, "update_phase_failed");
            } else {
                info!(phase = %p, "update_phase");
            }
            *update_phase.lock().unwrap() = Some(p.to_string());
        };

        // --- Download ---
        let archive_bytes = match async {
            let resp = http_client
                .get(&asset.browser_download_url)
                .timeout(std::time::Duration::from_secs(600))
                .send()
                .await
                .map_err(|e| format!("Download failed: {e}"))?;

            if !resp.status().is_success() {
                return Err(format!("Download failed: HTTP {}", resp.status()));
            }

            resp.bytes()
                .await
                .map_err(|e| format!("Failed to read download: {e}"))
        }
        .await
        {
            Ok(b) => {
                info!(size = b.len(), "update_downloaded");
                b
            }
            Err(e) => {
                error!(error = %e, "update_download_failed");
                set_phase(&format!("failed: {e}"));
                return;
            }
        };

        // --- Verify signature (before extract/install) ---
        set_phase("verifying");
        if let Err(e) = verify_update_signature(
            &http_client,
            &asset.name,
            &archive_bytes,
            sums_url.as_deref(),
            sig_url.as_deref(),
        )
        .await
        {
            // Le journal garde le détail technique ET un marqueur par cause,
            // pour qu'on puisse compter les échecs de publication séparément
            // des coupures réseau. L'utilisateur, lui, lit une phrase qui
            // nomme le responsable : c'est ce qui manquait quand on a envoyé
            // Jean Valjean vérifier son réseau pour un défaut de chez nous.
            error!(error = %e.detail, blame = ?e.blame, "{}", e.blame.marker());
            set_phase(&format!("failed: {}", e.blame.user_message()));
            return;
        }

        // --- Extract ---
        set_phase("extracting");

        let tmp_dir = std::env::temp_dir().join(format!("tune-update-{}", version));
        // Sweep leftover tune-update-* dirs from earlier updates. The success
        // path used to never remove the extraction dir, so one accumulated per
        // version (Benjithom, Windows: a new folder on every update).
        if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with("tune-update-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        if tmp_dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
        if let Err(e) = std::fs::create_dir_all(&tmp_dir) {
            set_phase(&format!("failed: Failed to create temp dir: {e}"));
            return;
        }

        let is_zip = asset.name.to_lowercase().ends_with(".zip");
        if let Err(e) = extract_archive(&archive_bytes, &tmp_dir, is_zip) {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            set_phase(&format!("failed: Extraction failed: {e}"));
            return;
        }

        info!(dir = %tmp_dir.display(), "update_extracted");

        // --- Install ---
        set_phase("installing");

        // Belt-and-braces: the handler already steers Docker users to the
        // image-pull path before we ever download, but if the install path is
        // somehow reached in a container the binary swap is doomed (read-only
        // image layer). Fail with a clear, actionable phase instead of the raw
        // "copy new binary: Permission denied".
        if running_in_docker() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            set_phase(
                "failed: Running in Docker — update by pulling the new image (docker compose pull && docker compose up -d)",
            );
            return;
        }

        let binary_name = if cfg!(windows) {
            "tune-server.exe"
        } else {
            "tune-server"
        };
        let new_binary = tmp_dir.join(binary_name);
        if !new_binary.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            set_phase(&format!(
                "failed: Binary '{}' not found in archive",
                binary_name
            ));
            return;
        }

        // Guard: refuse update if current binary has postgres but new one doesn't
        if cfg!(feature = "postgres") {
            // Detect postgres support in the DOWNLOADED binary via a string that
            // only a `--features postgres` build compiles in: the
            // `info!("postgres_backend_ready")` log lives in the
            // `#[cfg(feature = "postgres")]` branch of state.rs. The previous
            // markers were inverted — "PostgreSQL engine requested" is emitted
            // ONLY by the `cfg(not(feature="postgres"))` fallback (so a PG binary
            // *lacked* it) and "postgresql://" is nowhere in the code — so every
            // update on a PG server (.15) was wrongly blocked while a non-PG
            // binary would have passed. Keep this marker a PG-ONLY literal.
            // Scan the downloaded binary for the PG-only marker WITHOUT loading
            // the whole ~53 MB into memory: `fs::read` + `from_utf8_lossy` used
            // to allocate a ~150 MB lossy String copy of a binary — needless
            // memory pressure on modest hardware right in the middle of an
            // update. Stream it in bounded chunks instead.
            let new_has_pg = file_contains_bytes(&new_binary, b"postgres_backend_ready");
            if !new_has_pg {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                warn!("update_blocked_missing_postgres_feature");
                set_phase(
                    "failed: Update blocked: current binary has PostgreSQL support but the downloaded release does not.",
                );
                return;
            }
        }

        let current_exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                set_phase(&format!("failed: Cannot determine current exe: {e}"));
                return;
            }
        };

        // Install (swap the binary + web/). This is synchronous, blocking
        // filesystem work. Wrap it in `catch_unwind` so a panic surfaces as a
        // `failed` phase instead of vanishing: the update runs in a spawned task,
        // so an uncaught panic silently ends it, leaving the phase stuck on
        // "installing" and the server running the OLD binary while the UI keeps
        // re-offering the update (JP Borderies, Windows: install never completed,
        // no `restarting`, no error). `install_windows` now logs each step too,
        // so a genuine hang is pinpointed by the last step logged.
        info!(exe = %current_exe.display(), "update_install_starting");
        let install_outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if cfg!(windows) {
                install_windows(&current_exe, &new_binary, &tmp_dir)
            } else {
                install_unix(&current_exe, &new_binary, &tmp_dir)
            }
        }));
        match install_outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&tmp_dir);
                set_phase(&format!("failed: Install failed: {e}"));
                return;
            }
            Err(panic) => {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".to_string());
                let _ = std::fs::remove_dir_all(&tmp_dir);
                error!(panic = %msg, "update_install_panicked");
                set_phase(&format!("failed: Install crashed: {msg}"));
                return;
            }
        }

        // Success: install_windows/install_unix have copied the binary + web/
        // into the install dir (the Windows .bat swap works entirely within
        // exe_dir), so the extraction dir is no longer needed. Removing it here
        // stops the per-version accumulation.
        let _ = std::fs::remove_dir_all(&tmp_dir);

        info!(
            from = %tune_core::version(),
            to = %version,
            "update_installed"
        );

        // --- Restart ---
        set_phase("restarting");

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        info!("update_restarting");

        // Restart into the freshly-installed binary.
        //
        // UNIX (macOS/Linux): re-exec in place with execv. This replaces the
        // current process image while keeping the SAME PID, so recovery does
        // NOT depend on an external supervisor. It works identically under the
        // macOS DMG (no launchd/LaunchAgent), inside Docker (PID 1 never dies →
        // the container stays up), when launched from a terminal, and under
        // systemd (no exit → no Restart cycle, no parasite child, no port race).
        //
        // The previous approach — spawn() a child, then exit(0) — only recovered
        // when a supervisor happened to restart on exit (systemd Restart=always):
        // the .18 journal proved the spawned child was itself killed by systemd's
        // KillMode=control-group and did nothing (the process that came back had
        // a different PID). Without a supervisor (Docker, the DMG) nothing
        // restarted, so the server never came back — the reported bug.
        //
        // The listening socket is CLOEXEC (socket2 + std default), so exec()
        // releases port 8888 and the new image rebinds cleanly (main.rs also
        // retries bind). exec() only returns on failure — then we fall back to
        // spawn()+exit(0) so a supervised deployment still recovers.
        //
        // WINDOWS: we must NOT spawn or exec here. The binary is swapped by
        // tune-update.bat, which first waits for THIS process to exit (matched by
        // PID, with a 60s timeout backstop). Starting another process from the
        // still-old binary here would race the swap and could re-lock the .exe, so
        // we just exit; the .bat swaps the binary and starts the new one.
        // (History: the wait used to match by image name, which hung forever when
        // any second tune-server.exe was alive — Christophe's log
        // `update_installed to=0.8.261` then a restart as `version=0.8.260`. The
        // PID filter fixes that.)
        #[cfg(windows)]
        {
            // Record the version we're swapping TO next to the binary. The next
            // startup compares it to the version that actually loaded: if the
            // bat-swap was blocked (antivirus, a locked/relaunched .exe) the
            // server comes back on the OLD binary with no error anywhere — this
            // marker is what lets startup surface that silent failure (#1220).
            if let Some(dir) = current_exe.parent() {
                let _ = std::fs::write(dir.join("tune-update-expected.txt"), version.trim());
            }
            info!(
                "update_windows_exiting_for_bat_swap — tune-update.bat will swap the binary and restart"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::process::exit(0);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let exe = current_exe.clone();
            let args: Vec<String> = std::env::args().skip(1).collect();
            // Let the final status-poll response flush before we swap the image.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // Le lanceur pose TUNE_OPEN_BROWSER=1 ; l'image relancée l'hérite et
            // ROUVRAIT un onglet alors que l'ancien se reconnecte déjà → deux
            // onglets Tune à chaque mise à jour (Jean, forum #1236).
            unsafe { std::env::remove_var("TUNE_OPEN_BROWSER") };
            // Replier le WAL AVANT l'exec. `exec()` remplace l'image sans
            // dérouler un seul destructeur : aucune connexion n'est fermée,
            // aucun verrou n'est rendu proprement. Le 10 août, deux re-exec ont
            // eu lieu pendant que la base était en écriture, et elle s'est
            // retrouvée corrompue sans qu'on ait pu établir le mécanisme
            // (#1462). Un checkpoint ici ne prouve rien sur cette cause — il
            // supprime la fenêtre où elle pouvait jouer.
            if let Some(db) = state.db.as_ref() {
                db.checkpoint();
            }
            info!(exe = %exe.display(), "update_reexec");
            // exec() replaces this process on success and never returns.
            let err = std::process::Command::new(&exe).args(&args).exec();
            warn!(error = %err, "update_reexec_failed — falling back to spawn+exit");
            match std::process::Command::new(&exe)
                .args(&args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
            {
                Ok(child) => {
                    info!(pid = child.id(), exe = %exe.display(), "update_new_process_spawned");
                }
                Err(e) => {
                    warn!(error = %e, "update_restart_spawn_failed — manual restart required");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::process::exit(0);
        }
    });

    // Return immediately — client polls /system/update/status
    Json(json!({
        "status": "downloading",
        "version": response_version,
    }))
    .into_response()
}

/// Extract a tar.gz or zip archive to the given directory.
fn extract_archive(data: &[u8], dest: &std::path::Path, is_zip: bool) -> Result<(), String> {
    if is_zip {
        extract_zip(data, dest)
    } else {
        extract_tar_gz(data, dest)
    }
}

fn extract_tar_gz(data: &[u8], dest: &std::path::Path) -> Result<(), String> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let decoder = GzDecoder::new(data);
    let mut archive = Archive::new(decoder);
    archive
        .unpack(dest)
        .map_err(|e| format!("tar extraction: {e}"))
}

fn extract_zip(data: &[u8], dest: &std::path::Path) -> Result<(), String> {
    let cursor = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("zip open: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("zip entry {i}: {e}"))?;

        let out_path = match file.enclosed_name() {
            Some(p) => dest.join(p),
            None => continue,
        };

        if file.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("mkdir {}: {e}", out_path.display()))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
            }
            let mut out_file = std::fs::File::create(&out_path)
                .map_err(|e| format!("create {}: {e}", out_path.display()))?;
            std::io::copy(&mut file, &mut out_file)
                .map_err(|e| format!("write {}: {e}", out_path.display()))?;
        }
    }

    #[cfg(unix)]
    {
        let binary = dest.join("tune-server");
        if binary.exists() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).ok();
        }
    }

    Ok(())
}

/// Unix install: rename current binary to .old, put new one in place, update web/.
fn install_unix(
    current_exe: &std::path::Path,
    new_binary: &std::path::Path,
    tmp_dir: &std::path::Path,
) -> Result<(), String> {
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| "Cannot determine binary directory".to_string())?;

    let old_exe = current_exe.with_extension("old");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(new_binary, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod: {e}"))?;
    }

    let staging = current_exe.with_extension("new");
    std::fs::copy(new_binary, &staging).map_err(|e| format!("copy new binary: {e}"))?;

    if old_exe.exists() {
        std::fs::remove_file(&old_exe).ok();
    }
    std::fs::rename(current_exe, &old_exe).map_err(|e| format!("rename current to .old: {e}"))?;

    if let Err(e) = std::fs::rename(&staging, current_exe) {
        error!(error = %e, "rename_new_to_current_failed, rolling back");
        std::fs::rename(&old_exe, current_exe).ok();
        return Err(format!("rename .new to current: {e}"));
    }

    update_web_dir(exe_dir, tmp_dir)?;

    Ok(())
}

/// Windows install: write a bat script that replaces the binary after exit.
/// Le script de bascule écrit à côté du binaire, isolé pour être testable.
///
/// Les deux invariants que verrouillent les tests : le chemin nominal se
/// termine par `exit /b 0` avant `:swap_failed`, et le script ne se supprime
/// jamais lui-même.
fn windows_update_bat(
    pid: u32,
    exe: &str,
    new: &str,
    err_file: &str,
    exe_name: &str,
    exe_name_new: &str,
) -> String {
    format!(
        "@echo off\r\n\
         setlocal enabledelayedexpansion\r\n\
         echo Waiting for Tune server (PID {pid}) to stop...\r\n\
         set /a TRIES=0\r\n\
         :wait_loop\r\n\
         tasklist /FI \"PID eq {pid}\" 2>nul | find /I \"{exe_name}\" >nul\r\n\
         if errorlevel 1 goto do_swap\r\n\
         set /a TRIES+=1\r\n\
         if !TRIES! GEQ 60 (\r\n\
           echo Timed out after 60s waiting for old process, proceeding anyway...\r\n\
           goto do_swap\r\n\
         )\r\n\
         timeout /t 1 /nobreak >nul\r\n\
         goto wait_loop\r\n\
         :do_swap\r\n\
         timeout /t 1 /nobreak >nul\r\n\
         echo Replacing binary...\r\n\
         del \"{exe}\"\r\n\
         if exist \"{exe}\" (\r\n\
           echo File still locked, retrying...\r\n\
           timeout /t 3 /nobreak >nul\r\n\
           del \"{exe}\"\r\n\
         )\r\n\
         if exist \"{exe}\" goto swap_failed\r\n\
         rename \"{new}\" \"{exe_name}\"\r\n\
         echo Starting updated server...\r\n\
         set \"TUNE_OPEN_BROWSER=0\"\r\n\
         start \"\" \"{exe}\"\r\n\
         exit /b 0\r\n\
         :swap_failed\r\n\
         echo Tune update failed: could not replace {exe_name}.> \"{err_file}\"\r\n\
         echo The old binary was still locked by a running process.>> \"{err_file}\"\r\n\
         echo The new version is staged next to it as {exe_name_new} — close Tune>> \"{err_file}\"\r\n\
         echo completely, delete {exe_name}, then rename {exe_name_new} to {exe_name}.>> \"{err_file}\"\r\n\
         echo Update failed - old binary locked. Details written to {err_file}\r\n\
         set \"TUNE_OPEN_BROWSER=0\"\r\n\
         start \"\" \"{exe}\"\r\n"
    )
}

fn install_windows(
    current_exe: &std::path::Path,
    new_binary: &std::path::Path,
    tmp_dir: &std::path::Path,
) -> Result<(), String> {
    let exe_dir = current_exe
        .parent()
        .ok_or_else(|| "Cannot determine binary directory".to_string())?;

    let new_staging = current_exe.with_extension("new.exe");
    std::fs::copy(new_binary, &new_staging).map_err(|e| format!("copy new binary: {e}"))?;
    info!(staging = %new_staging.display(), "update_win_binary_staged");

    update_web_dir(exe_dir, tmp_dir)?;
    info!("update_win_web_swapped");

    // Wait for OUR specific PID to exit, not any process named tune-server.exe.
    // Matching by image name hangs forever whenever a second tune-server.exe is
    // alive (a lingering child, a double launch): the wait_loop never completes,
    // the binary is never swapped, and the OLD version comes back — an
    // intermittent "update did nothing" that reproduces only sometimes
    // (Christophe/Bilou/Yves). A PID filter is immune to that. A 60s timeout is
    // the backstop so the swap is never blocked indefinitely.
    let pid = std::process::id();
    let err_file = exe_dir.join("tune-update-failed.txt");

    let bat_path = exe_dir.join("tune-update.bat");
    // Le script ne se supprime plus lui-même, et c'est délibéré.
    //
    // HISTORIQUE — cette décision ANNULE celle qui la précédait, il faut donc
    // savoir pourquoi avant de la re-inverser.
    //
    // cmd.exe relit un fichier batch DEPUIS LE DISQUE après chaque commande, en
    // gardant une position de lecture. L'effacer pendant son interprétation fait
    // donc échouer la lecture suivante : « Le fichier de commande est
    // introuvable » juste après « Starting updated server... » (capture de
    // Bilou, Windows 11 25H2).
    //
    // Le correctif précédent (#1377) a conservé la suppression mais l'a fait
    // précéder de `(goto) 2>nul`, censé quitter le contexte batch AVANT que
    // `del` ne s'exécute. Le raisonnement se tient — mais **le terrain l'a
    // démenti** : Bilou a confirmé le 13/08/2026 que le message persiste en
    // v0.9.71, version qui contient pourtant ce correctif (vérifié par
    // ascendance). L'astuce n'est pas fiable ici, vraisemblablement à cause du
    // `setlocal enabledelayedexpansion` actif dès la première ligne.
    //
    // Plutôt que de parier une troisième fois sur une subtilité de cmd.exe
    // qu'on ne peut pas tester depuis un Mac, on supprime la cause : le script
    // ne s'efface plus. Il reste un fichier d'environ 2 Ko dans le répertoire
    // d'installation, réécrit à chaque mise à jour — un bien meilleur marché
    // qu'un message d'erreur à chaque fois. Et contrairement à un `cmd /c del`
    // détaché, aucune subtilité de guillemets ne peut casser la mise à jour
    // elle-même.
    //
    // `exit /b 0` est ce qui rend l'ensemble sûr : sans lui, le chemin nominal
    // tombe droit dans `:swap_failed`, écrit un rapport d'échec pour une mise à
    // jour réussie et relance l'exécutable une seconde fois. Seule l'astuce
    // `(goto)` l'empêchait — un second défaut latent que ceci supprime.
    let bat_content = windows_update_bat(
        pid,
        &current_exe.display().to_string(),
        &new_staging.display().to_string(),
        &err_file.display().to_string(),
        &current_exe
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        &new_staging
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
    );

    // A stale failure marker from a previous attempt would be misleading — clear it.
    let _ = std::fs::remove_file(&err_file);
    std::fs::write(&bat_path, bat_content).map_err(|e| format!("write update.bat: {e}"))?;
    info!(bat = %bat_path.display(), "update_win_bat_written");

    std::process::Command::new("cmd")
        .args(["/C", "start", "/min", "", &bat_path.to_string_lossy()])
        .spawn()
        .map_err(|e| format!("launch update.bat: {e}"))?;
    info!("update_win_bat_launched — process will now exit for the swap");

    Ok(())
}

/// Search a file for a byte pattern without loading it all into memory.
///
/// Reads in 64 KiB chunks with a `needle.len()-1` overlap so a match that
/// straddles a chunk boundary is still found. Used by the update installer to
/// detect a feature marker in a ~53 MB binary without allocating a full copy.
fn file_contains_bytes(path: &std::path::Path, needle: &[u8]) -> bool {
    use std::io::Read;
    if needle.is_empty() {
        return true;
    }
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    const CHUNK: usize = 64 * 1024;
    let overlap = needle.len() - 1;
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK + overlap);
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => return false,
        };
        window.extend_from_slice(&buf[..n]);
        if window.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        // Keep only the trailing `overlap` bytes so a boundary-straddling match
        // is caught on the next iteration.
        if window.len() > overlap {
            let cut = window.len() - overlap;
            window.drain(..cut);
        }
    }
    false
}

/// Replace the web/ directory with the one from the archive.
/// Writes to both CWD/web (where the server reads) and exe_dir/web (fallback).
fn update_web_dir(exe_dir: &std::path::Path, tmp_dir: &std::path::Path) -> Result<(), String> {
    let new_web = tmp_dir.join("web");
    if !new_web.exists() {
        info!("no web/ directory in archive, skipping web update");
        return Ok(());
    }

    let target_web = if let Ok(custom) = std::env::var("TUNE_WEB_DIR") {
        let p = std::path::PathBuf::from(&custom);
        if p.is_absolute() {
            p
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| exe_dir.to_path_buf())
                .join(p)
        }
    } else {
        std::env::current_dir()
            .map(|d| d.join("web"))
            .unwrap_or_else(|_| exe_dir.join("web"))
    };

    // Atomic swap: the old remove-then-copy left a BROKEN web/ (missing
    // index.html → no UI at all) whenever the copy failed partway — e.g.
    // stale root-owned files from a manual deploy (Bertrand, .15, v0.9.2
    // update). Stage a FULL copy next to the target, then swap via two
    // renames: at every instant the target is either the complete old web
    // or the complete new one. Rollback restores the old on a failed swap.
    swap_dir_atomic(&new_web, &target_web)?;

    let exe_web = exe_dir.join("web");
    if exe_web != target_web {
        swap_dir_atomic(&new_web, &exe_web).ok();
    }

    info!(dir = %target_web.display(), "web_directory_updated");
    Ok(())
}

/// Replace `target` with a copy of `src` without ever leaving a partial
/// directory at `target`: stage the full copy as `target.new` (same
/// filesystem → rename is atomic), move the old dir to `target.old`, rename
/// the staged copy into place, then delete the backup. On a failed final
/// rename the old directory is restored.
fn swap_dir_atomic(src: &std::path::Path, target: &std::path::Path) -> Result<(), String> {
    let staged = target.with_extension("new");
    let backup = target.with_extension("old");
    // Clear leftovers from a previous interrupted attempt.
    if staged.exists() {
        std::fs::remove_dir_all(&staged).map_err(|e| format!("clear staged web: {e}"))?;
    }
    if backup.exists() {
        std::fs::remove_dir_all(&backup).map_err(|e| format!("clear web backup: {e}"))?;
    }
    copy_dir_all(src, &staged).map_err(|e| format!("stage new web/: {e}"))?;
    let had_old = target.exists();
    if had_old {
        std::fs::rename(target, &backup).map_err(|e| format!("park old web/: {e}"))?;
    }
    if let Err(e) = std::fs::rename(&staged, target) {
        // Roll the old directory back so the UI keeps serving.
        if had_old {
            std::fs::rename(&backup, target).ok();
        }
        std::fs::remove_dir_all(&staged).ok();
        return Err(format!("swap new web/ into place: {e}"));
    }
    if had_old {
        std::fs::remove_dir_all(&backup).ok();
    }
    Ok(())
}

/// Recursively copy a directory.
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod web_swap_tests {
    use super::swap_dir_atomic;

    #[test]
    fn swap_replaces_and_cleans() {
        let tmp = tune_core::test_scratch::scratch_dir("tune-swap");
        let src = tmp.join("src");
        let target = tmp.join("web");
        std::fs::create_dir_all(src.join("assets")).unwrap();
        std::fs::write(src.join("index.html"), b"new").unwrap();
        std::fs::write(src.join("assets/a.js"), b"x").unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("index.html"), b"old").unwrap();

        swap_dir_atomic(&src, &target).unwrap();

        assert_eq!(std::fs::read(target.join("index.html")).unwrap(), b"new");
        assert!(target.join("assets/a.js").exists());
        assert!(!tmp.join("web.old").exists(), "backup must be cleaned");
        assert!(!tmp.join("web.new").exists(), "staging must be cleaned");
    }

    #[test]
    fn swap_into_missing_target_works() {
        let tmp = tune_core::test_scratch::scratch_dir("tune-swap2");
        let src = tmp.join("src");
        let target = tmp.join("web");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("index.html"), b"new").unwrap();

        swap_dir_atomic(&src, &target).unwrap();
        assert!(target.join("index.html").exists());
    }
}

/// GET /system/update/status
pub(super) async fn update_status(State(state): State<AppState>) -> Json<Value> {
    let phase = state.update_phase.lock().unwrap().clone();
    let is_failed = phase
        .as_deref()
        .map(|p| p.starts_with("failed"))
        .unwrap_or(false);

    // Result of the LAST applied update, recorded at startup (see
    // record_post_update_result). Lets the UI surface a silent swap failure —
    // e.g. Windows came back on the old binary — instead of the update just
    // looking like it did nothing.
    let last_update_result = SettingsRepo::with_backend(state.backend.clone())
        .get("last_update_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    Json(json!({
        "current_version": tune_core::version(),
        "phase": phase,
        "update_in_progress": phase.is_some() && !is_failed,
        "last_update_result": last_update_result,
    }))
}

/// Compare the version an in-progress update was swapping TO against the version
/// that actually loaded this startup. `Some(true)` = the swap took, `Some(false)`
/// = it silently reverted to the old binary, `None` = nothing to compare (no
/// update was pending). Tolerant of a leading `v` and surrounding whitespace.
fn swap_took(expected: &str, actual: &str) -> Option<bool> {
    let norm = |s: &str| s.trim().trim_start_matches('v').to_string();
    let (e, a) = (norm(expected), norm(actual));
    if e.is_empty() {
        return None;
    }
    Some(e == a)
}

/// Called once at startup: turn the markers left by an in-progress update into a
/// persisted `last_update_result` the UI can show. Fixes the silent Windows
/// bat-swap failure (#1220): the binary swap could be blocked (antivirus, a
/// locked/relaunched .exe) and the server would come back on the OLD version
/// with no error anywhere — "the update did nothing". Consumes the markers.
pub fn record_post_update_result(state: &AppState) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let current = tune_core::version();

    // Marker written by tune-update.bat when it could NOT replace the locked
    // binary — the most explicit failure, with user-facing detail.
    let bat_failed = dir.join("tune-update-failed.txt");
    if let Ok(detail) = std::fs::read_to_string(&bat_failed) {
        let detail = detail.trim().to_string();
        warn!(detail = %detail, "update_swap_failed_bat — binary replacement was blocked");
        let _ = settings.set(
            "last_update_result",
            &json!({
                "status": "failed",
                "reason": "binary_locked",
                "detail": detail,
                "current_version": current,
            })
            .to_string(),
        );
        let _ = std::fs::remove_file(&bat_failed);
        let _ = std::fs::remove_file(dir.join("tune-update-expected.txt"));
        return;
    }

    // Marker written by the server just before it exited for the swap: the
    // version we EXPECTED to be running now.
    let expected_marker = dir.join("tune-update-expected.txt");
    if let Ok(expected) = std::fs::read_to_string(&expected_marker) {
        match swap_took(&expected, current) {
            Some(true) => {
                info!(version = current, "update_swap_verified");
                let _ = settings.set(
                    "last_update_result",
                    &json!({ "status": "success", "current_version": current }).to_string(),
                );
            }
            Some(false) => {
                let expected = expected.trim();
                warn!(
                    expected,
                    actual = current,
                    "update_swap_failed_version_mismatch — restarted on the old binary (swap blocked?)"
                );
                let _ = settings.set(
                    "last_update_result",
                    &json!({
                        "status": "failed",
                        "reason": "swap_did_not_take",
                        "expected_version": expected,
                        "current_version": current,
                    })
                    .to_string(),
                );
            }
            None => {}
        }
        let _ = std::fs::remove_file(&expected_marker);
    }

    match homebrew_installation(&exe)
        .as_ref()
        .and_then(|installation| homebrew_mismatch_result(installation, current))
    {
        Some(result) => {
            warn!(
                executable = %exe.display(),
                cellar_version = result["installation_version"].as_str().unwrap_or("unknown"),
                binary_version = current,
                "homebrew_installation_version_mismatch"
            );
            let _ = settings.set("last_update_result", &result.to_string());
        }
        _ => {
            // Do not leave the warning behind after `brew upgrade` has made
            // the Cellar coherent again (or after moving to a standalone
            // install). Preserve unrelated update results.
            let stale_homebrew_warning = settings
                .get("last_update_result")
                .ok()
                .flatten()
                .and_then(|value| serde_json::from_str::<Value>(&value).ok())
                .and_then(|value| value["reason"].as_str().map(str::to_owned))
                .as_deref()
                == Some("homebrew_version_mismatch");
            if stale_homebrew_warning {
                let _ = settings.delete("last_update_result");
            }
        }
    }
}

/// POST /system/update/apply — kept for backward compatibility.
pub(super) async fn update_apply() -> impl IntoResponse {
    Json(json!({
        "status": "deprecated",
        "message": "Use POST /system/update/install instead",
    }))
}

/// GET /system/changelog — fetch from GitHub releases, cache 1 hour.
pub(super) async fn changelog() -> Json<Value> {
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    static CACHE: OnceLock<Mutex<(std::time::Instant, Value)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new((
            std::time::Instant::now() - std::time::Duration::from_secs(7200),
            json!([]),
        ))
    });
    let mut guard = cache.lock().await;

    if guard.0.elapsed() < std::time::Duration::from_secs(3600)
        && guard.1.as_array().is_some_and(|a| !a.is_empty())
    {
        return Json(json!({ "version": tune_core::version(), "entries": guard.1 }));
    }

    let entries = match fetch_github_changelog().await {
        Ok(e) => {
            *guard = (std::time::Instant::now(), e.clone());
            e
        }
        Err(_) => guard.1.clone(),
    };
    drop(guard);

    // Le cache démarre à `json!([])`. Sur un serveur fraîchement lancé et sans
    // réseau, les deux branches ci-dessus rendent donc un tableau VIDE, et le
    // panneau « Quoi de neuf » s'affiche désert — ce qui se lit non pas comme
    // « je n'ai pas pu joindre la source » mais comme « cette version
    // n'apporte rien ». Le repli en dur existait depuis toujours pour ce cas ;
    // il n'était simplement jamais appelé.
    if entries.as_array().is_none_or(|a| a.is_empty()) {
        return changelog_hardcoded();
    }

    Json(json!({ "version": tune_core::version(), "entries": entries }))
}

/// Les trois listes du panneau « Quoi de neuf », telles qu'il les attend.
#[derive(Default)]
struct ParsedBody {
    features: Vec<String>,
    fixes: Vec<String>,
    improvements: Vec<String>,
}

/// À quelle rubrique un titre de section renvoie-t-il ?
#[derive(Clone, Copy, PartialEq)]
enum Section {
    Features,
    Fixes,
    Improvements,
    /// Rubrique reconnue mais sans destination (« Téléchargements », « Mise à
    /// jour »…) : ses puces ne sont pas des nouveautés et n'ont rien à faire
    /// dans le panneau.
    Other,
}

/// Classe un intitulé (titre de section) par mots-clés, FR et EN.
fn section_from_title(title: &str) -> Section {
    let l = title.to_lowercase();
    if l.contains("correct") || l.contains("fix") || l.contains("bug") {
        Section::Fixes
    } else if l.contains("amélio") || l.contains("ameli") || l.contains("improv") {
        Section::Improvements
    } else if l.contains("nouveaut") || l.contains("feature") || l.contains("ajout") {
        Section::Features
    } else {
        Section::Other
    }
}

/// Retire le balisage Markdown *en ligne* d'une puce : gras, italique,
/// `code`, et liens `[texte](url)` réduits à leur texte.
///
/// Le panneau affiche ces chaînes en TEXTE BRUT (`{item}` dans un `<li>`), donc
/// tout marqueur laissé ici s'affiche tel quel — c'est ce qui donnait
/// « \*\*Accueil — …\*\* » à l'écran (capture d'Alex Campbell, 09/08).
fn strip_inline_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // ** / __ : marqueurs de gras, avalés par paires ; un seul
            // caractère isolé (un souligné dans un identifiant) est conservé.
            '*' | '_' if chars.peek() == Some(&c) => {
                chars.next();
            }
            '*' => {}
            '`' => {}
            // [texte](url) → texte
            '[' => {
                let text: String = chars.by_ref().take_while(|&c| c != ']').collect();
                out.push_str(&text);
                if chars.peek() == Some(&'(') {
                    chars.next();
                    for c in chars.by_ref() {
                        if c == ')' {
                            break;
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// Découpe un corps de release GitHub en trois listes d'items.
///
/// **La structure prime sur les mots-clés.** L'ancienne version classait
/// *ligne à ligne* par mots-clés : `## Corrections` contient « correction »,
/// donc le TITRE lui-même atterrissait en puce sous « Corrections » ; et une
/// phrase de résumé contenant « nouveautés » devenait une nouveauté. Ici un
/// titre choisit la rubrique courante, et seules les **puces** deviennent des
/// items — la prose et les titres n'en sont jamais.
fn parse_release_body(body: &str) -> ParsedBody {
    let mut out = ParsedBody::default();
    // Sans aucun titre, on garde le comportement historique : les puces vont
    // aux nouveautés, et les mots-clés de la puce peuvent la rediriger.
    let mut current: Option<Section> = None;
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(title) = line.strip_prefix('#') {
            current = Some(section_from_title(title.trim_start_matches('#')));
            continue;
        }
        // Un titre en gras seul sur sa ligne (**Corrections**) tient lieu de
        // titre de section : c'est fréquent dans nos notes.
        let bold_title = line
            .strip_prefix("**")
            .and_then(|s| s.strip_suffix("**"))
            .filter(|s| !s.contains("**"));
        if let Some(title) = bold_title {
            current = Some(section_from_title(title));
            continue;
        }
        let Some(item) = line
            .strip_prefix("- ")
            .or_else(|| line.strip_prefix("* "))
            .or_else(|| line.strip_prefix("• "))
        else {
            continue; // prose, séparateur, image… : jamais un item.
        };
        let item = strip_inline_markdown(item);
        if item.is_empty() {
            continue;
        }
        let dest = match current {
            Some(Section::Other) => continue,
            Some(s) => s,
            // Hors de toute section : les mots-clés de la puce décident.
            None => match section_from_title(&item) {
                Section::Other => Section::Features,
                s => s,
            },
        };
        match dest {
            Section::Features => out.features.push(item),
            Section::Fixes => out.fixes.push(item),
            Section::Improvements => out.improvements.push(item),
            Section::Other => {}
        }
    }
    out
}

async fn fetch_github_changelog() -> Result<Value, String> {
    let client = tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("Tune/2.0")
        .build()
        .map_err(|e| e.to_string())?;

    // Try mozaiklabs.fr proxy first, fallback to GitHub
    let releases: Vec<Value> = match async {
        let resp = client
            .get("https://mozaiklabs.fr/api/tune/releases?per_page=20")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("proxy API {}", resp.status()));
        }
        resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())
    }
    .await
    {
        Ok(r) => r,
        Err(_) => {
            let mut req = client.get(
                "https://api.github.com/repos/renesenses/tune-server-rust/releases?per_page=20",
            );
            if let Ok(token) = std::env::var("GITHUB_TOKEN") {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("GitHub API {}", resp.status()));
            }
            resp.json::<Vec<Value>>().await.map_err(|e| e.to_string())?
        }
    };
    let entries: Vec<Value> = releases
        .iter()
        .filter_map(|r| {
            let tag = r["tag_name"].as_str()?;
            let version = tag.strip_prefix('v').unwrap_or(tag);
            let date = r["published_at"]
                .as_str()
                .unwrap_or("")
                .split('T')
                .next()
                .unwrap_or("");
            let body = r["body"].as_str().unwrap_or("");
            let ParsedBody {
                mut features,
                fixes,
                improvements,
            } = parse_release_body(body);
            if features.is_empty() && fixes.is_empty() && improvements.is_empty() {
                features.push(format!("Release {version}"));
            }
            Some(json!({
                "version": version,
                "date": date,
                "features": features,
                "fixes": fixes,
                "improvements": improvements,
            }))
        })
        .collect();
    Ok(json!(entries))
}

/// Dernier recours quand la source distante est injoignable ET que le cache
/// est vide (serveur qui vient de démarrer, machine hors ligne, panne de
/// GitHub — vécu le 17/08/2026). Le client sait lire cette forme `sections`
/// aussi bien que la forme `features/fixes/improvements` du chemin réseau :
/// `WhatsNew.svelte` convertit l'une vers l'autre.
///
/// Ces notes sont figées et ne suivent pas les releases : elles valent mieux
/// qu'un panneau vide, pas mieux que les vraies notes. Chaque entrée porte sa
/// version et sa date, donc rien n'est présenté comme récent à tort.
fn changelog_hardcoded() -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        // Dit au client que ces notes sont un secours, pas l'actualité du
        // produit. Sans ce drapeau, le panneau badge sa première entrée
        // « Récent » — soit « v0.8.15 » annoncée comme la version en cours sur
        // un serveur bien plus récent. Un panneau vide n'affirmait rien ; un
        // panneau mal étiqueté affirme quelque chose de faux, ce qui est pire.
        // Lu par `WhatsNew.svelte` (tune-web-client#501).
        "offline": true,
        "entries": [
            {
                "version": "0.8.15",
                "date": "2026-06-01",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Zones = 0 dans le dashboard",
                        "Gapless DLNA triple fix",
                        "WAV Content-Length fix",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Credits Now Playing",
                        "Windows crash log",
                        "MockOutput test infra",
                    ]},
                ]
            },
            {
                "version": "0.8.28",
                "date": "2026-06-03",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Zone creation race condition fix",
                        "PostgreSQL FTS accent search",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Release autonomy pipeline",
                        "PostgreSQL abstraction layer",
                    ]},
                ]
            },
            {
                "version": "0.8.35",
                "date": "2026-06-03",
                "sections": [
                    { "title": "Corrections", "items": [
                        "SSDP non-standard UPnP renderers",
                        "Artwork rescan coalesce bug",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "DLNA cover art profileID in DIDL-Lite",
                        "Cargo audit security check in CI",
                    ]},
                ]
            },
            {
                "version": "0.8.37",
                "date": "2026-06-04",
                "sections": [
                    { "title": "Corrections", "items": [
                        "OAAT streams FLAC directly (native pipeline)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Mood DJ — ambient mix generation",
                    ]},
                ]
            },
            {
                "version": "0.8.39",
                "date": "2026-06-04",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Nested transaction fix in artist_repo",
                        "Signal path shows actual renderer name",
                        "TCP poll before browser open (no sleep)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Output errors surfaced to clients",
                        "Radio favorites: playlist_name + limit params",
                    ]},
                ]
            },
            {
                "version": "0.8.50",
                "date": "2026-06-05",
                "sections": [
                    { "title": "Nouveautes", "items": [
                        "Auth JWT multi-utilisateurs",
                        "AI Assistant Claude (11 outils)",
                        "Plugin SDK + EventBus",
                        "PostgreSQL abstraction complète",
                        "Tune Bridge (WebSocket cloud-to-home)",
                        "Intégration cloud mozaiklabs.fr (SSO, télémétrie)",
                    ]},
                ]
            },
            {
                "version": "0.8.58",
                "date": "2026-06-06",
                "sections": [
                    { "title": "Corrections", "items": [
                        "ALAC 24-bit décodage (hiss fix)",
                        "WAL checkpoint stale reads",
                        "M4A scan fallback",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Docker officiel multi-arch",
                        "FFmpeg entièrement supprimé — pipeline 100% Rust",
                        "5 décodeurs natifs (ALAC, AAC, MP3, Vorbis, Opus)",
                    ]},
                ]
            },
            {
                "version": "0.8.65",
                "date": "2026-06-08",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Fix DLNA darTZeel coupure 2s",
                        "Volume buttons web client (PUT + int 0-100)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "HQPlayer output (v4/v5/v6)",
                        "OAAT protocol (9 crates, crates.io)",
                        "Community metadata (covers + artist images)",
                        "Forum 7 langues (350 traductions)",
                        "MusicBrainz batch MBID matching",
                    ]},
                ]
            },
            {
                "version": "0.8.70",
                "date": "2026-06-09",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Volume slider (debounce + DLNA normalisation)",
                        "Podcast Affaires Sensibles (feed URL corrigée)",
                        "Zones fantômes filtrées de En cours d'écoute",
                        "Télémétrie report après scan (5 min au lieu de 30s)",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Page Ambassadeurs (mozaiklabs.fr/ambassadors)",
                        "Page Fabricants OAAT (mozaiklabs.fr/oaat/manufacturers)",
                        "Admin Tune Cloud (instances, SSO, bridges)",
                        "Threads privés forum",
                        "Images artistes fallback MusicBrainz/Wikimedia",
                    ]},
                ]
            },
            {
                "version": "0.8.83",
                "date": "2026-06-11",
                "sections": [
                    { "title": "Corrections", "items": [
                        "Scrollbar plus large + visible sur Windows",
                        "Thème persisté après sync serveur",
                        "Quoi de neuf : parsing du format changelog API",
                    ]},
                    { "title": "Nouveautes", "items": [
                        "Next/Prev instantanés (DLNA async en background)",
                        "Tune Widget macOS (tray app Tauri v2)",
                    ]},
                ]
            },
        ]
    }))
}

#[cfg(test)]
mod scan_guard_tests {
    use super::scan_in_progress;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::sqlite::SqliteDb;

    fn backend() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn idle_allows_update() {
        let b = backend();
        SettingsRepo::with_backend(b.clone())
            .set("scan_status", "idle")
            .unwrap();
        assert!(!scan_in_progress(&b));
    }

    #[test]
    fn no_status_allows_update() {
        // Fresh DB, no scan_status key at all.
        assert!(!scan_in_progress(&backend()));
    }

    #[test]
    fn fresh_scan_blocks_update() {
        let b = backend();
        let s = SettingsRepo::with_backend(b.clone());
        s.set("scan_status", "scanning").unwrap();
        s.set("scan_started_at", &now_secs().to_string()).unwrap();
        assert!(scan_in_progress(&b));
    }

    #[test]
    fn scanning_without_start_time_blocks_update() {
        // Err on the side of protecting the scan when no start time is recorded.
        let b = backend();
        SettingsRepo::with_backend(b.clone())
            .set("scan_status", "scanning")
            .unwrap();
        assert!(scan_in_progress(&b));
    }

    #[test]
    fn stale_scan_does_not_block_update() {
        // A scan_status left "scanning" by a crash/restart must never block
        // updates forever — past the staleness window it is ignored.
        let b = backend();
        let s = SettingsRepo::with_backend(b.clone());
        s.set("scan_status", "scanning").unwrap();
        let stale = now_secs() - (super::SCAN_GUARD_STALE_SECS + 3600);
        s.set("scan_started_at", &stale.to_string()).unwrap();
        assert!(!scan_in_progress(&b));
    }
}

#[cfg(test)]
mod playback_guard_tests {
    use super::playback_in_progress;
    use tune_core::playback::{NowPlaying, PlaybackManager};

    #[tokio::test]
    async fn idle_server_allows_update() {
        assert!(!playback_in_progress(&PlaybackManager::new()).await);
    }

    #[tokio::test]
    async fn playing_zone_defers_update() {
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        assert!(playback_in_progress(&pm).await);
    }

    #[tokio::test]
    async fn paused_zone_allows_update() {
        // Paused means nothing is streaming, so the re-exec costs nothing
        // audible. Only Playing defers — otherwise a zone left paused for days
        // would block every update, which is the failure mode the scan guard's
        // staleness window exists to avoid.
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        pm.pause(12).await;
        assert!(!playback_in_progress(&pm).await);
    }

    #[tokio::test]
    async fn stopped_zone_allows_update() {
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        pm.stop(12).await;
        assert!(!playback_in_progress(&pm).await);
    }

    #[tokio::test]
    async fn one_playing_zone_among_idle_ones_defers_update() {
        // .18 runs 13-14 zones; the guard must look at all of them, not the
        // first one it finds.
        let pm = PlaybackManager::new();
        pm.play(4, NowPlaying::default()).await;
        pm.stop(4).await;
        pm.play(8, NowPlaying::default()).await;
        pm.pause(8).await;
        pm.play(12, NowPlaying::default()).await;
        assert!(playback_in_progress(&pm).await);
    }
}

#[cfg(test)]
mod changelog_parse_tests {
    use super::{Section, parse_release_body, section_from_title, strip_inline_markdown};

    /// Extrait réel d'une note de version (forme v0.9.60), avec ce qui cassait :
    /// un titre contenant « Corrections », une puce en gras contenant
    /// « Nouveautés », et une section « Mise à jour » sans rapport.
    const BODY: &str = "\
Une qualité Deezer annoncée à tort, et « Nouveautés » qui listait des morceaux.

## Nouveautés

- **Accueil — « Nouveautés » listait des morceaux au lieu d'albums.** Un même
- Qualité Deezer affichée depuis le `format` réel

## Corrections

- Pochette erronée dans les compilations maison
- Lecture qui s'arrêtait au premier morceau

## Mise à jour

- Depuis Tune : **Réglages → Système → Mettre à jour**.
- Sinon, les binaires de toutes les plateformes sont ci-dessous.
";

    #[test]
    fn heading_is_a_section_not_an_item() {
        // Le bug d'Alex : `## Corrections` contient « correction », donc
        // l'ancien classement ligne-à-ligne le poussait comme PUCE dans les
        // corrections. Un titre ne doit jamais devenir un item.
        let p = parse_release_body(BODY);
        assert!(
            !p.fixes.iter().any(|i| i.contains("Corrections")),
            "le titre de section a été rendu comme une puce : {:?}",
            p.fixes
        );
        assert_eq!(p.fixes.len(), 2);
        assert!(p.fixes[0].starts_with("Pochette erronée"));
    }

    #[test]
    fn inline_markdown_is_stripped() {
        // Le panneau affiche du texte brut : plus aucun `**` ne doit sortir.
        let p = parse_release_body(BODY);
        assert!(
            p.features
                .iter()
                .all(|i| !i.contains("**") && !i.contains('`')),
            "balisage laissé dans les items : {:?}",
            p.features
        );
        assert!(p.features[0].starts_with("Accueil — « Nouveautés »"));
    }

    #[test]
    fn prose_and_unrelated_sections_are_dropped() {
        let p = parse_release_body(BODY);
        // La phrase d'introduction contient « Nouveautés » : elle devenait une
        // nouveauté alors que ce n'est pas une puce.
        assert!(
            !p.features
                .iter()
                .any(|i| i.contains("Deezer annoncée à tort")),
            "la prose a été promue en item : {:?}",
            p.features
        );
        // « Mise à jour » n'est pas une rubrique du panneau : ses puces sont
        // des instructions, pas des nouveautés.
        assert!(
            !p.features.iter().any(|i| i.contains("Réglages")),
            "les instructions de mise à jour ont fui : {:?}",
            p.features
        );
        assert_eq!(p.features.len(), 2);
        assert!(p.improvements.is_empty());
    }

    #[test]
    fn bold_line_acts_as_a_section_title() {
        let p = parse_release_body("**Corrections**\n\n- Un correctif\n");
        assert_eq!(p.fixes, vec!["Un correctif"]);
        assert!(p.features.is_empty());
    }

    #[test]
    fn bullets_without_any_heading_fall_back_to_keywords() {
        // Notes plates (pas de titre) : on garde le classement historique par
        // mots-clés de la puce, défaut « nouveautés ».
        let p = parse_release_body("- fix: a crash\n- something else\n");
        assert_eq!(p.fixes, vec!["fix: a crash"]);
        assert_eq!(p.features, vec!["something else"]);
    }

    #[test]
    fn titles_classify_in_both_languages() {
        assert!(matches!(section_from_title("Bug fixes"), Section::Fixes));
        assert!(matches!(section_from_title("Corrections"), Section::Fixes));
        assert!(matches!(
            section_from_title("Améliorations"),
            Section::Improvements
        ));
        assert!(matches!(section_from_title("Downloads"), Section::Other));
    }

    #[test]
    fn links_keep_their_text_only() {
        assert_eq!(
            strip_inline_markdown("voir [le fil](https://exemple.fr/x) pour la suite"),
            "voir le fil pour la suite"
        );
        // Un souligné isolé (identifiant) n'est pas du balisage.
        assert_eq!(
            strip_inline_markdown("clé audio_embed_analyzed"),
            "clé audio_embed_analyzed"
        );
    }
}

#[cfg(test)]
mod swap_result_tests {
    use super::swap_took;

    #[test]
    fn swap_took_detects_match_mismatch_and_none() {
        // Same version → the swap took.
        assert_eq!(swap_took("0.9.49", "0.9.49"), Some(true));
        // Tolerant of a leading `v` and surrounding whitespace.
        assert_eq!(swap_took(" v0.9.49 ", "0.9.49"), Some(true));
        // Came back on the OLD binary → the swap did not take (the #1220 case).
        assert_eq!(swap_took("0.9.49", "0.9.48"), Some(false));
        // No pending update (empty marker) → nothing to compare.
        assert_eq!(swap_took("", "0.9.48"), None);
        assert_eq!(swap_took("   ", "0.9.48"), None);
    }
}

#[cfg(test)]
mod homebrew_guard_tests {
    use std::path::Path;

    use super::{
        HOMEBREW_UPDATE_COMMAND, HomebrewInstallation, homebrew_cellar_version,
        homebrew_installation, homebrew_mismatch_result, homebrew_update_refusal,
        homebrew_version_matches,
    };

    #[test]
    fn reconnait_les_cellars_apple_silicon_intel_et_linuxbrew() {
        for (path, version) in [
            (
                "/opt/homebrew/Cellar/tune-server/0.9.110/bin/tune-server",
                "0.9.110",
            ),
            (
                "/usr/local/Cellar/tune-server/0.9.71/bin/tune-server",
                "0.9.71",
            ),
            (
                "/home/linuxbrew/.linuxbrew/Cellar/tune-server/0.9.113_1/bin/tune-server",
                "0.9.113_1",
            ),
        ] {
            assert_eq!(
                homebrew_cellar_version(Path::new(path)).as_deref(),
                Some(version),
                "installation Homebrew non reconnue : {path}"
            );
        }
    }

    #[test]
    fn ne_confond_pas_une_installation_autonome_ou_une_autre_formule() {
        assert_eq!(
            homebrew_cellar_version(Path::new("/Applications/Tune/tune-server")),
            None
        );
        assert_eq!(
            homebrew_cellar_version(Path::new("/opt/homebrew/Cellar/ffmpeg/8.0/bin/tune-server")),
            None
        );
    }

    #[test]
    fn compare_la_version_du_cellar_au_binaire() {
        assert!(homebrew_version_matches("0.9.113", "0.9.113"));
        assert!(homebrew_version_matches("0.9.113_1", "v0.9.113"));
        assert!(!homebrew_version_matches("0.9.71", "0.9.110"));
    }

    #[test]
    fn le_refus_est_actionnable_et_nomme_la_divergence() {
        let installation = HomebrewInstallation {
            executable: "/opt/homebrew/Cellar/tune-server/0.9.71/bin/tune-server".into(),
            cellar_version: "0.9.71".into(),
        };
        let response = homebrew_update_refusal(&installation, "0.9.110");

        assert_eq!(response["status"], "managed_installation");
        assert_eq!(response["reason"], "homebrew_managed_installation");
        assert_eq!(response["command"], HOMEBREW_UPDATE_COMMAND);
        assert_eq!(response["installation_version"], "0.9.71");
        assert_eq!(response["current_version"], "0.9.110");
        assert_eq!(response["installation_version_mismatch"], true);
    }

    #[test]
    fn le_demarrage_ne_signale_que_les_cellars_incoherents() {
        let mut installation = HomebrewInstallation {
            executable: "/opt/homebrew/Cellar/tune-server/0.9.71/bin/tune-server".into(),
            cellar_version: "0.9.71".into(),
        };

        let warning = homebrew_mismatch_result(&installation, "0.9.110").unwrap();
        assert_eq!(warning["status"], "warning");
        assert_eq!(warning["reason"], "homebrew_version_mismatch");
        assert_eq!(warning["command"], HOMEBREW_UPDATE_COMMAND);

        installation.cellar_version = "0.9.110_1".into();
        assert!(homebrew_mismatch_result(&installation, "v0.9.110").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn resout_le_lien_opt_vers_le_vrai_cellar() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let real = tmp
            .path()
            .join("Cellar/tune-server/0.9.113/bin/tune-server");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"fixture").unwrap();

        let linked = tmp.path().join("opt/tune-server/bin/tune-server");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        symlink(&real, &linked).unwrap();

        let installation = homebrew_installation(&linked).expect("lien Homebrew non resolu");
        assert_eq!(
            installation.executable,
            std::fs::canonicalize(real).unwrap()
        );
        assert_eq!(installation.cellar_version, "0.9.113");
    }
}

#[cfg(test)]
mod windows_update_bat_tests {
    use super::windows_update_bat;

    fn script() -> String {
        windows_update_bat(
            4242,
            r"C:\Program Files\Tune\tune-server.exe",
            r"C:\Program Files\Tune\tune-server.new.exe",
            r"C:\Program Files\Tune\tune-update-failed.txt",
            "tune-server.exe",
            "tune-server.new.exe",
        )
    }

    /// Le script ne doit JAMAIS s'effacer lui-même.
    ///
    /// cmd.exe relit le fichier depuis le disque après chaque commande : le
    /// supprimer en cours d'interprétation affiche « Le fichier de commande est
    /// introuvable » à chaque mise à jour (Bilou, fil #1306). Le correctif
    /// précédent gardait la suppression derrière `(goto) 2>nul` ; le terrain a
    /// démenti cette parade en v0.9.71. Réintroduire l'une ou l'autre forme
    /// ramènerait le bug.
    #[test]
    fn never_deletes_itself() {
        let s = script();
        assert!(!s.contains("%~f0"), "le script se supprime lui-même");
        assert!(!s.contains("(goto)"), "l'astuce (goto) est de retour");
    }

    /// Le chemin nominal doit sortir avant l'étiquette d'échec.
    ///
    /// Sans `exit /b 0`, une mise à jour RÉUSSIE tombe dans `:swap_failed` :
    /// elle écrit un rapport d'échec mensonger et relance l'exécutable une
    /// seconde fois, `start` étant présent dans les deux branches.
    #[test]
    fn success_path_exits_before_the_failure_branch() {
        let s = script();
        let exit = s.find("exit /b 0").expect("pas de sortie explicite");
        let failed = s.find(":swap_failed").expect("pas d'étiquette d'échec");
        assert!(
            exit < failed,
            "le chemin nominal traverse :swap_failed au lieu de sortir"
        );
    }

    /// Garde-fou sur les points déjà corrigés ailleurs : l'attente porte sur
    /// NOTRE pid (et non sur le nom d'image, qui pendait indéfiniment quand un
    /// second tune-server.exe tournait), et le binaire de remplacement est bien
    /// celui préparé à côté.
    #[test]
    fn keeps_the_pid_wait_and_the_staged_binary() {
        let s = script();
        assert!(
            s.contains("PID eq 4242"),
            "l'attente ne filtre plus par PID"
        );
        assert!(s.contains("tune-server.new.exe"));
        assert!(s.contains("tune-update-failed.txt"));
    }
}

#[cfg(test)]
mod changelog_fallback_tests {
    use super::changelog_hardcoded;

    /// Le repli doit satisfaire le contrat que `changelog_has_entries` vérifie
    /// quand des données arrivent : au moins 5 versions, la plus récente
    /// nommée. Contrairement à ce test d'intégration, celui-ci ne touche PAS
    /// au réseau — il vaut donc aussi pendant une panne de GitHub, qui est
    /// précisément le moment où le repli sert.
    #[test]
    fn le_repli_satisfait_le_contrat_du_panneau() {
        let body = changelog_hardcoded().0;
        let entries = body["entries"]
            .as_array()
            .expect("le repli doit exposer un tableau `entries`");

        assert!(
            entries.len() >= 5,
            "le repli doit porter au moins 5 versions, il en a {}",
            entries.len()
        );
        assert!(
            body["version"].is_string(),
            "le repli doit annoncer la version du serveur"
        );
    }

    /// Le client distingue un secours d'une vraie réponse par ce seul drapeau.
    /// S'il disparaît, le panneau rebadge « Récent » sur une entrée de juin.
    #[test]
    fn le_repli_sannonce_comme_tel() {
        let body = changelog_hardcoded().0;
        assert_eq!(
            body["offline"],
            serde_json::json!(true),
            "sans ce drapeau, WhatsNew.svelte presente le secours comme l'actualite"
        );
    }

    /// Chaque entrée doit être exploitable par `WhatsNew.svelte` : une version
    /// non vide, une date, et des rubriques. Une entrée creuse produirait une
    /// ligne muette dans le panneau — le défaut même qu'on corrige.
    #[test]
    fn chaque_entree_du_repli_est_affichable() {
        let body = changelog_hardcoded().0;
        for e in body["entries"].as_array().unwrap() {
            let v = e["version"].as_str().unwrap_or("");
            assert!(!v.is_empty(), "entrée sans version : {e}");
            assert!(
                e["date"].as_str().is_some_and(|d| !d.is_empty()),
                "version {v} sans date"
            );
            let sections = e["sections"]
                .as_array()
                .unwrap_or_else(|| panic!("version {v} sans rubriques"));
            assert!(
                !sections.is_empty(),
                "version {v} : rubriques vides, la ligne serait muette"
            );
        }
    }
}
