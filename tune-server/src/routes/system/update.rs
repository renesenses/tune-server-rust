use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tracing::{error, info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::updater::{ReleaseAsset, ReleaseInfo, UpdateChannel, UpdateChecker};

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
///
/// L'horodatage est EXIGÉ. Sans lui la fenêtre d'ancienneté n'a rien à
/// mesurer, et le report que ce garde-fou pose n'a plus aucune sortie — il
/// n'existe même pas de `force` pour le contourner ici, contrairement au
/// garde-fou de la lecture (#2976).
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
        // Aucun horodatage lisible : ce n'est PAS un scan que ce binaire a
        // annoncé. Les deux seuls chemins de production qui posent
        // `scan_status = "scanning"` — le manuel/planifié et celui du
        // démarrage — passent désormais par `scan::marquer_scan_en_cours`,
        // qui écrit la date AVANT le statut. Un scan vivant est donc toujours
        // daté ; « scanning » sans date ne peut plus venir que d'une base
        // laissée par une version antérieure, c'est-à-dire du cas même que la
        // fenêtre d'ancienneté existe pour dénouer : un scan mort.
        //
        // Le traiter comme frais, ce qu'on faisait, rendait le report
        // ÉTERNEL : `POST /system/update/install` rendait 409
        // `scan_in_progress` à chaque tentative, en promettant une reprise
        // « une fois le scan terminé » que rien ne pouvait déclencher. Une
        // mise à jour reportée se rattrape à la tentative suivante ; un scan
        // coupé se relance. L'asymétrie tranche dans ce sens (#2976).
        None => false,
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
    !playing_zone_ids(playback).await.is_empty()
}

/// Depuis combien de temps la position OBSERVÉE d'une zone doit-elle être
/// immobile avant que le chemin de mise à jour cesse de la croire ?
///
/// **C'est la sortie qui manquait à #3581.** Une zone `Playing` en mémoire
/// n'était contredite par rien : la sortie disparue du registre du sondeur, la
/// boucle fait `continue` (`poller/tick.rs`, `outputs.get(&device_id) → None`)
/// et plus personne n'observe la zone ; le seul détecteur de zone figée est
/// DLNA-only (#3155) ; `startup.rs` ne remet à `stopped` que la COLONNE, pas la
/// mémoire. Tades ne pouvait donc plus mettre à jour, et rien — ni l'écran, ni
/// le corps du 409 — ne lui offrait de recours.
///
/// **Justification de la constante.** Elle doit être plus grande que le plus
/// long silence LÉGITIME d'une lecture réelle, et franchement plus petite que
/// le plafond de deux heures qu'elle remplace :
/// - la position observée est réécrite toutes les **1 s**
///   (`POLL_INTERVAL_MS = 1000`, `tune-core/src/poller.rs`) ;
/// - la plus longue tolérance que le sondeur s'accorde AVANT de déclarer une
///   zone en panne est `TRACK_LOAD_GRACE_SECS = 45` puis
///   `STOPPED_FAILURE_THRESHOLD = 30` ticks, soit **75 s** ;
/// - le plus long gel d'appareil mesuré sur ce dépôt est le réveil d'un ampli
///   DLNA sorti de veille réseau, `BUDGET_REVEIL_STANDBY` ≈ **32 s**
///   (`outputs/dlna.rs`) ;
/// - la branche compressée de `outputs/local.rs` télécharge et décode la piste
///   ENTIÈRE avant le premier échantillon (#3618) : quelques dizaines de
///   secondes sur un mono-cœur — et pendant ce temps aucune avance n'a jamais
///   été observée, donc le champ vaut `None` et la zone est tenue pour
///   vivante de toute façon.
///
/// **600 s = 8 × le verdict de panne du sondeur, ≈ 19 × le plus long gel
/// mesuré, et 12 × moins que `RESTART_DEFERRAL_MAX`.** Aucune lecture que le
/// reste du serveur considère encore vivante ne peut franchir ce seuil ; une
/// zone qui le franchit a cessé d'être observée depuis dix minutes.
///
/// Le pire cas d'un faux positif reste borné : la relance coupe un son qui,
/// par construction, n'avance plus depuis dix minutes. Le pire cas d'un faux
/// négatif était de deux heures d'attente muette. L'asymétrie tranche.
const SILENCE_DE_POSITION_AVANT_ZONE_FIGEE: Duration = Duration::from_secs(600);

/// Les zones qui jouent, par identifiant. Sert uniquement à nommer dans le
/// journal ce qui retient la relance : le 30 août, `update_restarting` est
/// tombé 24 s après le début d'un morceau et rien, dans le journal, ne disait
/// ce que le chemin de mise à jour avait regardé (#2954).
/// Une zone FIGÉE n'en fait pas partie : elle annonce `Playing` mais sa
/// position observée n'a plus bougé depuis
/// [`SILENCE_DE_POSITION_AVANT_ZONE_FIGEE`]. C'est le fantôme de #3581, et
/// c'est ici qu'il cesse de retenir la mise à jour — au garde-fou d'entrée
/// comme au report de la relance, puisque les deux passent par cette
/// fonction. Le journal la NOMME, avec son âge : sans cela le correctif
/// serait invisible dans un `diagnostic.md`.
async fn playing_zone_ids(playback: &tune_core::playback::PlaybackManager) -> Vec<i64> {
    zones_en_lecture_vivante(playback, SILENCE_DE_POSITION_AVANT_ZONE_FIGEE).await
}

/// Le corps de [`playing_zone_ids`], seuil paramétré.
///
/// Le seuil est un argument pour que les épreuves puissent le tenir des deux
/// côtés — un seuil nul doit écarter une zone déjà observée, un seuil de
/// production doit garder celle qui vient de l'être — sans faire dormir dix
/// minutes un test qu'on finirait par désarmer.
async fn zones_en_lecture_vivante(
    playback: &tune_core::playback::PlaybackManager,
    silence_max: Duration,
) -> Vec<i64> {
    let mut vivantes = Vec::new();
    for z in playback.all_states().await {
        if z.state != tune_core::playback::PlayState::Playing {
            continue;
        }
        if tune_core::playback::zone_figee(&z, silence_max) {
            warn!(
                zone_id = z.zone_id,
                immobile_secs = z
                    .derniere_avance_de_position
                    .map(|t| t.elapsed().as_secs())
                    .unwrap_or_default(),
                seuil_secs = silence_max.as_secs(),
                "update_zone_figee_ignoree"
            );
            continue;
        }
        vivantes.push(z.zone_id);
    }
    vivantes
}

/// Les zones qui retiennent la relance, NOMMÉES, et sous la forme que
/// l'interface peut afficher telle quelle.
///
/// Le journal les nomme depuis #2954 (`update_deferred_playback_in_progress
/// zones=[…]`). La réponse rendue à l'appelant, elle, ne portait qu'un motif
/// `playback_in_progress` et une phrase générique — l'utilisateur voyait un
/// refus sans sujet.
///
/// Tades (#3581) ne pouvait donc ni voir QUELLE zone prétendait jouer — sa
/// Serenade était à l'arrêt — ni savoir qu'une sortie existait : `?force=true`
/// est dans la route depuis #2976, et rien, dans la réponse, ne l'annonçait.
/// #3155 a établi qu'aucun détecteur ne rattrape une zone locale figée ; tant
/// que c'est vrai, le seul recours possible est de nommer la zone et de dire
/// qu'on peut passer outre. La borne haute du report reste, elle, à deux
/// heures.
///
/// Un nom introuvable en base ne fait pas échouer le refus : l'identifiant est
/// rendu seul, ce qui vaut toujours mieux que rien.
fn zones_qui_retiennent(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    ids: &[i64],
) -> Vec<Value> {
    let repo = tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    ids.iter()
        .map(|id| {
            let nom = repo.get(*id).ok().flatten().map(|z| z.name);
            json!({ "id": id, "name": nom })
        })
        .collect()
}

/// Plafond du report de la relance. Passé ce délai on relance MALGRÉ une zone
/// annoncée en lecture.
///
/// Il faut une sortie, sans quoi une zone oubliée bloque les mises à jour pour
/// toujours — et #3155 a établi qu'aucun détecteur ne rattrape une zone locale
/// figée : une zone peut rester `Playing` en mémoire indéfiniment sans qu'un
/// seul échantillon sorte. Deux heures couvrent un album ou une œuvre longue
/// d'un bout à l'autre ; au-delà, une zone qui « joue » encore est plus
/// probablement une zone figée sans auditeur qu'une session réelle, et la mise
/// à jour reprend la main.
const RESTART_DEFERRAL_MAX: Duration = Duration::from_secs(2 * 3600);

/// Cadence de relecture de l'état de lecture pendant le report. Une lecture de
/// l'état en mémoire (un verrou, une `HashMap`) toutes les 5 s : le coût est
/// nul pour la lecture en cours, et la relance suit la fin du morceau à 5 s
/// près.
const RESTART_DEFERRAL_POLL: Duration = Duration::from_secs(5);

/// Comment le report de la relance s'est terminé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartRelease {
    /// Rien ne jouait : la relance part sans attendre.
    Idle,
    /// Une lecture était en cours et s'est arrêtée d'elle-même.
    PlaybackEnded(Duration),
    /// Le plafond a expiré, la zone annonce toujours une lecture, on relance.
    WindowExpired(Duration),
}

/// Retient la relance tant qu'une zone joue — bornée par
/// [`RESTART_DEFERRAL_MAX`].
///
/// **C'est ici que se joue le défaut de #2954, pas au garde-fou d'entrée.** Le
/// garde-fou de `update_install` consulte l'état de lecture UNE fois, à la
/// réception de la requête, avant un téléchargement de 38 Mo — et un appelant
/// qui passe `?force=true` le saute entièrement. Or ce que la requête autorise,
/// c'est de télécharger et d'installer : deux actes inaudibles. Ce qui coupe le
/// son, c'est l'échange d'image (`execv`), plusieurs secondes ou plusieurs
/// minutes plus tard, et il ne consultait rien du tout. Le 30 août la lecture a
/// démarré à 15:42:00 et `update_reexec` est tombé à 15:42:24 sans qu'une seule
/// ligne dise ce qui avait été regardé.
///
/// Le report est donc posé au dernier instant utile, et il ne dépend PAS de
/// `force` : l'utilisateur averti que « la musique va s'arrêter » a été averti
/// d'un état de lecture qui datait de sa requête, pas de celui de la relance.
/// Le binaire est déjà remplacé sur le disque quand on arrive ici — la mise à
/// jour est acquise, même si le processus meurt pendant l'attente, la prochaine
/// ouverture démarre la nouvelle version. Seul l'échange d'image attend.
///
/// Une zone en PAUSE ne retient rien : `playback_in_progress` ne compte que
/// `PlayState::Playing`, exactement comme le frein de repos du poller depuis
/// #3120. Les deux chemins disent la même chose de « actif ».
async fn defer_restart_until_quiet(
    playback: &tune_core::playback::PlaybackManager,
    max: Duration,
    poll: Duration,
) -> RestartRelease {
    if !playback_in_progress(playback).await {
        return RestartRelease::Idle;
    }
    let zones = playing_zone_ids(playback).await;
    warn!(
        zones = ?zones,
        max_secs = max.as_secs(),
        "update_restart_deferred_playback"
    );
    let started = tokio::time::Instant::now();
    loop {
        let waited = started.elapsed();
        if waited >= max {
            return RestartRelease::WindowExpired(waited);
        }
        // Le dernier pas est rogné pour atterrir exactement sur le plafond.
        tokio::time::sleep(poll.min(max - waited)).await;
        if !playback_in_progress(playback).await {
            return RestartRelease::PlaybackEnded(started.elapsed());
        }
    }
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

/// Préfixe Homebrew qui POSSÈDE cette installation, déduit du chemin du Cellar
/// et non de `PATH`.
///
/// C'est le point qui rend la mise à jour en place possible. Un serveur lancé
/// par `brew services` reçoit le `PATH` du plist — `std_service_path_env`,
/// c'est-à-dire `<prefix>/bin:<prefix>/sbin:/usr/bin:/bin:/usr/sbin:/sbin`
/// (Homebrew, `Library/Homebrew/service.rb`) — mais un serveur lancé à la main
/// depuis un terminal, ou par un automate, n'a aucune garantie de ce genre.
/// Chercher `brew` dans `PATH` était donc le piège classique : il marche sur la
/// machine du développeur et échoue chez le testeur.
///
/// Le Cellar, lui, dit tout : `/opt/homebrew/Cellar/tune-server/0.9.143/bin/tune-server`
/// nomme son propre préfixe, `/opt/homebrew`. Apple Silicon, Intel
/// (`/usr/local`) et Linuxbrew (`/home/linuxbrew/.linuxbrew`) sont couverts
/// sans qu'aucun chemin ne soit écrit en dur.
fn homebrew_prefix(executable: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut prefix = std::path::PathBuf::new();
    for component in executable.components() {
        if component.as_os_str() == std::ffi::OsStr::new("Cellar") {
            return (!prefix.as_os_str().is_empty()).then_some(prefix);
        }
        prefix.push(component);
    }
    None
}

/// Tune tourne-t-il en root ? `brew` s'y refuse, et root passe pourtant tous
/// les tests d'écriture — c'est le seul empêchement que le disque ne dit pas.
#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: `geteuid` ne fait que lire un identifiant du processus.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(not(unix))]
fn running_as_root() -> bool {
    false
}

/// Un fichier est-il exécutable par quelqu'un ?
#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &std::path::Path) -> bool {
    path.is_file()
}

/// Ce qui empêche Tune de conduire lui-même `brew upgrade`.
///
/// Chaque variante nomme une chose vérifiable sur la machine, pas une
/// supposition : le refus rendu à l'écran doit dire QUOI manque.
#[derive(Debug, Clone, PartialEq, Eq)]
enum HomebrewUpgradeBlock {
    /// Le chemin du Cellar ne laisse aucun préfixe où accrocher `bin/brew`.
    PrefixUnknown,
    /// Aucun `brew` exécutable sous le préfixe qui possède cette installation.
    BrewMissing(std::path::PathBuf),
    /// Le Cellar n'appartient pas au compte qui fait tourner Tune : `brew
    /// upgrade` échouerait à mi-chemin, après le téléchargement.
    NotWritable {
        path: std::path::PathBuf,
        error: String,
    },
    /// Nulle part où poser le fichier de progression. Il DOIT survivre au
    /// redémarrage — c'est le seul canal qui traverse l'échange de binaire —
    /// donc sans lui l'écran ne peut plus rien suivre et on ne lance rien.
    NoStateDir {
        path: std::path::PathBuf,
        error: String,
    },
    /// Tune tourne en root. `brew` REFUSE de s'exécuter en root — ce n'est pas
    /// une préférence, c'est un abandon franc de sa part. Une installation
    /// posée en LaunchDaemon système est donc hors d'atteinte, et il faut le
    /// dire plutôt que de lancer un script qui échouera à la première ligne.
    ///
    /// La seule vérification qui ne peut PAS être remplacée par le test
    /// d'écriture : root écrit partout, donc `probe_dir_writable` réussit
    /// justement dans le cas où `brew` va refuser.
    RunningAsRoot,
}

impl HomebrewUpgradeBlock {
    fn reason(&self) -> &'static str {
        match self {
            Self::PrefixUnknown => "homebrew_prefix_unknown",
            Self::BrewMissing(_) => "homebrew_brew_missing",
            Self::NotWritable { .. } => "homebrew_cellar_not_writable",
            Self::NoStateDir { .. } => "homebrew_state_dir_not_writable",
            Self::RunningAsRoot => "homebrew_running_as_root",
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::PrefixUnknown => {
                "Homebrew prefix could not be derived from the Cellar path.".into()
            }
            Self::BrewMissing(path) => format!("No executable brew at {}.", path.display()),
            Self::NotWritable { path, error } => format!(
                "{} is not writable by the account running Tune ({error}).",
                path.display()
            ),
            Self::NoStateDir { path, error } => format!(
                "Progress file directory {} is not writable ({error}).",
                path.display()
            ),
            Self::RunningAsRoot => "Tune runs as root, and Homebrew refuses to run as root.".into(),
        }
    }
}

/// Tout ce qu'il faut pour conduire la mise à jour, une fois mesuré.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HomebrewUpgradePlan {
    /// `<prefix>/bin/brew`, mesuré exécutable.
    brew: std::path::PathBuf,
    /// `<prefix>/opt/tune-server/bin/tune-server-launcher` — le lien stable qui
    /// pointe TOUJOURS sur le keg courant, donc sur le nouveau après l'échange.
    launcher: std::path::PathBuf,
    /// Fichier d'état JSON, hors du Cellar : `brew` remplace le Cellar.
    state_file: std::path::PathBuf,
    /// Journal complet de `brew`, à côté.
    log_file: std::path::PathBuf,
    /// Le script lui-même.
    script_file: std::path::PathBuf,
}

const HOMEBREW_STATE_FILE: &str = "tune-homebrew-upgrade.json";
const HOMEBREW_LOG_FILE: &str = "tune-homebrew-upgrade.log";
const HOMEBREW_SCRIPT_FILE: &str = "tune-homebrew-upgrade.sh";

/// Mesure, sur CETTE machine, si Tune peut conduire `brew upgrade` lui-même.
///
/// Aucune de ces vérifications n'est un raisonnement : chacune touche le disque.
fn homebrew_upgrade_plan(
    installation: &HomebrewInstallation,
    state_dir: &std::path::Path,
) -> Result<HomebrewUpgradePlan, HomebrewUpgradeBlock> {
    if running_as_root() {
        return Err(HomebrewUpgradeBlock::RunningAsRoot);
    }

    let prefix =
        homebrew_prefix(&installation.executable).ok_or(HomebrewUpgradeBlock::PrefixUnknown)?;

    let brew = prefix.join("bin").join("brew");
    if !is_executable(&brew) {
        return Err(HomebrewUpgradeBlock::BrewMissing(brew));
    }

    // Le keg neuf atterrit dans `<prefix>/Cellar/tune-server`. Si ce répertoire
    // n'accepte pas une écriture du compte qui fait tourner Tune, `brew upgrade`
    // ira au bout du téléchargement pour échouer ensuite — exactement le défaut
    // que le garde-fou d'écriture du chemin autonome existe déjà pour éviter.
    let cellar = prefix.join("Cellar").join("tune-server");
    if let Err(error) = probe_dir_writable(&cellar) {
        return Err(HomebrewUpgradeBlock::NotWritable {
            path: cellar,
            error,
        });
    }

    if let Err(error) = probe_dir_writable(state_dir) {
        return Err(HomebrewUpgradeBlock::NoStateDir {
            path: state_dir.to_path_buf(),
            error,
        });
    }

    Ok(HomebrewUpgradePlan {
        brew,
        launcher: prefix
            .join("opt")
            .join("tune-server")
            .join("bin")
            .join("tune-server-launcher"),
        state_file: state_dir.join(HOMEBREW_STATE_FILE),
        log_file: state_dir.join(HOMEBREW_LOG_FILE),
        script_file: state_dir.join(HOMEBREW_SCRIPT_FILE),
    })
}

/// Le script qui fait le travail.
///
/// **Pourquoi un script détaché et non un appel synchrone.** `brew upgrade`
/// remplace le binaire EN COURS D'EXÉCUTION et les ressources web du Cellar,
/// puis il faut redémarrer — et `brew upgrade` ne redémarre AUCUN service
/// (mesuré : `Library/Homebrew/upgrade.rb` ne mentionne pas les services ; c'est
/// pourquoi la formule elle-même écrit « Après une mise à jour, redémarrez le
/// serveur »). Le redémarrage tue donc l'appelant. C'est le même problème que
/// Windows, et c'est la même réponse : un script hors du processus, comme
/// `tune-update.bat`.
///
/// **Aucune entrée utilisateur n'entre ici.** Les seuls chemins interpolés sont
/// déduits de `current_exe` et de `db_path` ; le nom de la formule est une
/// constante. Rien de ce que l'appelant HTTP envoie n'atteint le script.
///
/// **La progression traverse le redémarrage** parce qu'elle est sur le disque,
/// hors du Cellar. Le serveur neuf relit le même fichier et l'écran retrouve
/// l'opération là où elle en était.
fn homebrew_upgrade_script(plan: &HomebrewUpgradePlan, server_pid: u32) -> String {
    let brew = plan.brew.display();
    let launcher = plan.launcher.display();
    let state = plan.state_file.display();
    let log = plan.log_file.display();
    format!(
        r#"#!/bin/sh
# Écrit par Tune. Conduit la mise à jour Homebrew de tune-server puis relance le
# serveur. Détaché de son parent : le redémarrage tue le serveur, pas ce script.
set -u

BREW='{brew}'
LAUNCHER='{launcher}'
STATE='{state}'
LOG='{log}'
SRV_PID={server_pid}

export HOMEBREW_NO_AUTO_UPDATE=1
export HOMEBREW_NO_ENV_HINTS=1
export HOMEBREW_NO_COLOR=1
export HOMEBREW_NO_EMOJI=1
export NONINTERACTIVE=1

etape() {{
  printf '{{"phase":"%s","exit_code":%s,"pid":%s,"updated_at":%s}}\n' \
    "$1" "${{2:-null}}" "$SRV_PID" "$(date +%s)" > "$STATE.tmp" 2>/dev/null \
    && mv "$STATE.tmp" "$STATE" 2>/dev/null
}}

etape brew_update
"$BREW" update >>"$LOG" 2>&1 || {{ etape failed_brew_update $?; exit 1; }}

etape brew_upgrade
"$BREW" upgrade tune-server >>"$LOG" 2>&1 || {{ etape failed_brew_upgrade $?; exit 1; }}

etape restarting
# Le service brew n'est redémarré QUE s'il est réellement démarré. Sinon
# `brew services restart` en démarrerait un second à côté du serveur lancé à la
# main, et les deux se disputeraient le port.
if "$BREW" services list 2>/dev/null | grep -q '^tune-server[[:space:]][[:space:]]*started'; then
  "$BREW" services restart tune-server >>"$LOG" 2>&1 || {{ etape failed_restart $?; exit 1; }}
  etape done 0
  exit 0
fi

# On ne coupe RIEN sans savoir qu'on peut relancer. Sans cette mesure, un
# lanceur absent laissait Tune eteint — strictement pire que le refus qu'on
# remplace. La mise a jour est acquise sur le disque ; il ne manque que la
# relance, et l'ecran le dit avec la commande.
if [ ! -x "$LAUNCHER" ]; then
  etape failed_no_launcher 0
  exit 1
fi

kill "$SRV_PID" 2>/dev/null
i=0
while kill -0 "$SRV_PID" 2>/dev/null && [ "$i" -lt 30 ]; do
  sleep 1
  i=$((i+1))
done
"$LAUNCHER" >>"$LOG" 2>&1 &
etape done 0
exit 0
"#
    )
}

/// Écrit le script et le lance DÉTACHÉ.
///
/// `setsid` est ce qui rend l'ensemble possible sur macOS : `brew services
/// restart` passe par `launchctl`, qui abat le job — donc le serveur et tout ce
/// qui reste dans sa session. Un script resté dans la session mourrait avec le
/// serveur qu'il vient de faire redémarrer, à mi-chemin, sans jamais écrire
/// `done`. C'est l'équivalent du `start /min` détaché du chemin Windows.
fn spawn_homebrew_upgrade(plan: &HomebrewUpgradePlan, server_pid: u32) -> Result<(), String> {
    let script = homebrew_upgrade_script(plan, server_pid);
    std::fs::write(&plan.script_file, script)
        .map_err(|e| format!("write {}: {e}", plan.script_file.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&plan.script_file, std::fs::Permissions::from_mode(0o700));
    }
    // Un journal neuf par tentative : sinon le rapport d'échec du testeur
    // mélange trois essais et ne dit plus lequel a échoué.
    let _ = std::fs::remove_file(&plan.log_file);

    let mut command = std::process::Command::new("/bin/sh");
    command
        .arg(&plan.script_file)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` est async-signal-safe et ne touche à aucun état de
        // l'allocateur ni à un verrou du processus parent.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("spawn {}: {e}", plan.script_file.display()))
}

/// Où poser le fichier de progression : à côté de la base, jamais dans le
/// Cellar que `brew` remplace.
fn homebrew_state_dir(db_path: &str) -> std::path::PathBuf {
    std::path::Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

/// Ce que le script a écrit, s'il a écrit quelque chose. Relu par
/// `GET /system/update/status`, y compris par le serveur NEUF.
fn homebrew_upgrade_state(state_dir: &std::path::Path) -> Option<Value> {
    let raw = std::fs::read_to_string(state_dir.join(HOMEBREW_STATE_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

fn homebrew_update_refusal(
    installation: &HomebrewInstallation,
    current: &str,
    blocked: Option<&HomebrewUpgradeBlock>,
) -> Value {
    json!({
        "status": "managed_installation",
        "reason": "homebrew_managed_installation",
        "manager": "homebrew",
        // `message` reste en anglais et reste dans la charge utile: c'est le
        // repli des clients qui ne connaissent pas `reason`. La phrase que
        // l'utilisateur LIT est rendue par le client, dans SA langue, à partir
        // de `reason` et de `command` — le serveur n'a pas à connaître la
        // langue de l'écran, et cette charge-ci est de surcroît recopiée telle
        // quelle dans `last_update_result`, où une phrase traduite se figerait
        // dans la langue du jour de l'écriture.
        "message": HOMEBREW_UPDATE_HINT,
        "detail": HOMEBREW_UPDATE_HINT,
        "command": HOMEBREW_UPDATE_COMMAND,
        "installation_version": installation.cellar_version,
        "current_version": current,
        "installation_version_mismatch": !homebrew_version_matches(
            &installation.cellar_version,
            current,
        ),
        // POURQUOI Tune ne conduit pas la mise à jour lui-même sur cette
        // machine — un motif de machine, et le détail qui nomme le chemin
        // cherché. Un refus n'existe QUE dans ce cas : quand le plan tient, la
        // route lance le travail et rend 202. Il n'y a donc pas de drapeau
        // « c'est possible » à porter ici, et un champ constamment faux aurait
        // eu l'air de dire quelque chose.
        "upgrade_in_place_blocked_reason": blocked.map(HomebrewUpgradeBlock::reason),
        "upgrade_in_place_detail": blocked.map(HomebrewUpgradeBlock::detail),
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
/// Le dernier maillon du canal (#2266) : le réglage relu dans la base ARME
/// bien le vérificateur que les deux routes utilisent.
///
/// Ce test ne passe pas par le réseau — il n'interroge pas l'API des releases,
/// il vérifie que `checker_for` porte le canal enregistré. C'est exactement le
/// maillon qu'un « écrit mais pas branché » casserait sans qu'aucun test de
/// filtrage ne bronche : `select_release` pourrait être parfait pendant que les
/// routes construiraient encore un vérificateur en `Auto`.
#[cfg(test)]
mod canal_branche_sur_le_verificateur {
    use super::{checker_for, update_channel};
    use tune_core::updater::UpdateChannel;

    fn etat() -> crate::state::AppState {
        crate::state::AppState::new(":memory:", 0, Default::default()).unwrap()
    }

    /// LE TÉMOIN, côté serveur : base neuve, réglage jamais écrit → `Auto`,
    /// c'est-à-dire le vérificateur d'avant #2266.
    #[test]
    fn sans_reglage_le_verificateur_reste_en_auto() {
        let state = etat();
        assert_eq!(update_channel(&state.backend), UpdateChannel::Auto);
        assert_eq!(checker_for(&state.backend).channel(), UpdateChannel::Auto);
    }

    #[test]
    fn le_reglage_enregistre_arme_le_verificateur() {
        let state = etat();
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        for canal in [
            UpdateChannel::Stable,
            UpdateChannel::Beta,
            UpdateChannel::Auto,
        ] {
            settings
                .set(UpdateChannel::SETTING_KEY, canal.as_str())
                .unwrap();
            assert_eq!(update_channel(&state.backend), canal);
            assert_eq!(
                checker_for(&state.backend).channel(),
                canal,
                "le vérificateur des routes doit porter le canal {}",
                canal.as_str()
            );
        }
    }

    /// Une valeur illisible ne doit jamais OUVRIR le canal bêta : le repli est
    /// le comportement historique.
    #[test]
    fn valeur_illisible_retombe_sur_auto() {
        let state = etat();
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(UpdateChannel::SETTING_KEY, "nightly")
            .unwrap();
        assert_eq!(update_channel(&state.backend), UpdateChannel::Auto);
    }
}

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

/// Le canal de mise à jour effectivement en vigueur, relu dans la base.
///
/// **C'est le seul lecteur du réglage**, et les DEUX routes qui interrogent
/// l'API des releases passent par lui — `update_check` et `update_install`.
/// Un canal que seule la route de lecture consulterait laisserait
/// `POST /update/install` installer la préversion que `GET /update/check`
/// venait de refuser d'annoncer : le réglage serait décoratif.
///
/// Une valeur illisible (base écrite par une version postérieure, réglage
/// bricolé à la main) retombe sur [`UpdateChannel::Auto`] : le comportement
/// historique est le repli sûr, jamais une ouverture du canal bêta.
fn update_channel(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> UpdateChannel {
    SettingsRepo::with_backend(backend.clone())
        .get(UpdateChannel::SETTING_KEY)
        .ok()
        .flatten()
        .and_then(|raw| UpdateChannel::parse(&raw))
        .unwrap_or_default()
}

/// Le vérificateur de releases ARMÉ du canal enregistré.
///
/// Les deux routes qui interrogent l'API des releases passent par ici : c'est
/// le point unique où le réglage rejoint le cœur, et donc le seul endroit à
/// éprouver pour savoir que le réglage est BRANCHÉ.
fn checker_for(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> UpdateChecker {
    UpdateChecker::with_channel(update_channel(backend))
}

/// La clé où le vérificateur périodique dépose ce qu'il a TROUVÉ — jamais ce
/// qu'il a fait, puisqu'il n'installe rien.
///
/// Distincte de `last_update_result`, qui porte le résultat de la DERNIÈRE
/// installation appliquée : confondre les deux ferait passer une simple
/// disponibilité pour une mise à jour effectuée.
pub(crate) const CLE_MISE_A_JOUR_DISPONIBLE: &str = "update_available_release";

/// Délai avant le premier contrôle après le démarrage.
///
/// La boucle ne part pas au tour zéro : le démarrage a déjà de quoi faire, et
/// surtout une machine qui redémarre en boucle (unité systemd `Restart=always`
/// devant un défaut de configuration) taperait l'API des releases à chaque
/// relance. Deux minutes suffisent à sortir de cette fenêtre-là.
const DELAI_PREMIER_CONTROLE: Duration = Duration::from_secs(120);

/// L'annonce déposée en base quand une version plus récente existe.
///
/// Fonction pure, pour que ce que l'écran lira soit éprouvable sans réseau.
fn annonce_de_release(current: &str, release: &ReleaseInfo, channel: UpdateChannel) -> Value {
    let (setting, effective) = channel_fields(channel, current);
    json!({
        "current": current,
        "latest": release.version,
        "tag_name": release.tag_name,
        "name": release.name,
        "published_at": release.published_at,
        "html_url": release.html_url,
        "channel": setting,
        "effective_channel": effective,
        "checked_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    })
}

/// Un tour du vérificateur périodique : interroger, consigner, et RIEN d'autre.
///
/// Le canal est relu À CHAQUE TOUR, par `checker_for` — le même point unique
/// que les deux routes. Le lire une fois au lancement rendrait le réglage
/// `update_channel` inopérant jusqu'au prochain redémarrage : quelqu'un qui
/// passe de `beta` à `stable` doit être entendu au tour suivant, pas au
/// prochain démarrage.
///
/// Une erreur réseau NE TOUCHE PAS l'annonce déjà déposée : une coupure de
/// liaison n'est pas la preuve qu'une version a disparu.
async fn tour_de_verification(state: &AppState) {
    let current = tune_core::version();
    let channel = update_channel(&state.backend);
    let checker = checker_for(&state.backend);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match checker.check().await {
        Ok(Some(release)) => {
            let annonce = annonce_de_release(current, &release, channel);
            if let Err(e) = settings.set(CLE_MISE_A_JOUR_DISPONIBLE, &annonce.to_string()) {
                warn!(error = %e, "update_available_write_failed");
            }
            info!(
                version = %release.version,
                current,
                channel = channel.as_str(),
                "update_available"
            );
        }
        // Plus rien à annoncer : la version installée est à jour, ou l'annonce
        // précédente portait une version que le canal ne propose plus. Effacer
        // évite qu'un écran garde éternellement un point rouge périmé.
        Ok(None) => {
            let _ = settings.delete(CLE_MISE_A_JOUR_DISPONIBLE);
        }
        Err(e) => {
            warn!(error = %e, "update_check_failed");
        }
    }
}

/// #3217 — le vérificateur périodique de mises à jour, enfin LANCÉ.
///
/// ## Ce qui était en place, et ce qui ne l'était pas
///
/// `TUNE_AUTO_UPDATE` était déclaré (`config.rs:130`), par défaut à `false`
/// (`config.rs:220`) et réglable par l'environnement (`config.rs:299`) — et lu
/// NULLE PART. En face, `UpdateChecker::spawn_periodic` avait une seule
/// occurrence dans tout le dépôt : sa propre définition. Poser
/// `TUNE_AUTO_UPDATE=true` dans une unité systemd ou un `docker-compose`
/// n'obtenait rien, sans un mot au journal.
///
/// ## Pourquoi il NOTIFIE et n'installe pas
///
/// La garde anti-coupure de la route d'installation (#2954) repose sur une
/// prémisse écrite noir sur blanc en tête de ce fichier : « toute installation est
/// aujourd'hui un geste délibéré ». Celui qui ne passe pas `?force=true` n'a
/// pas été prévenu de ce qu'il s'apprête à interrompre — l'écran, lui, prévient
/// puis force. Un vérificateur qui installerait tout seul n'est prévenu par
/// personne : il n'a pas d'interface pour avertir, et il ne peut pas dire
/// `force` de bonne foi. Le souvenir du 10/08/2026 sur le .18 est dans le même
/// commentaire : six mises à jour en une journée, deux qui ont ré-exécuté
/// pendant que la zone 12 diffusait.
///
/// Ce lanceur ne touche donc à aucun chemin d'installation. Il interroge,
/// journalise `update_available` et dépose l'annonce sous
/// [`CLE_MISE_A_JOUR_DISPONIBLE`], que `GET /system/update/status` rend. Le
/// geste d'installation reste entier, délibéré, et la garde de #2954 garde
/// exactement ce qu'elle gardait. Une garde de site le tient
/// (`le_verificateur_periodique_n_installe_rien`).
///
/// ## Ce que le réglage veut dire désormais
///
/// `TUNE_AUTO_UPDATE=true` = « préviens-moi quand une version paraît ». C'est
/// moins que ce que le nom promet, et c'est délibéré : passer à l'installation
/// automatique demanderait de rouvrir la garde anti-coupure, ce qui est un
/// arbitrage de Bertrand et non une décision d'implémentation.
pub(crate) fn spawn_verificateur_de_mise_a_jour(state: AppState, auto_update: bool) {
    if !auto_update {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(DELAI_PREMIER_CONTROLE).await;
        let cadence = Duration::from_secs(tune_core::updater::CHECK_INTERVAL_SECS);
        loop {
            tour_de_verification(&state).await;
            tokio::time::sleep(cadence).await;
        }
    });
}

/// Câblage et portée du vérificateur périodique (#3217).
///
/// Ce qui a été perdu pendant des mois, c'est un APPEL — pas une logique :
/// `spawn_periodic` était écrit, complet, et personne ne le lançait. Aucun test
/// de comportement ne pouvait le voir : ils passaient tous sans que la boucle
/// tourne jamais. Même procédé que `scan_scheduler_cablage_tests`, pour la
/// même raison.
#[cfg(test)]
mod verificateur_periodique_cablage {
    use super::*;

    /// Le seul endroit qui lance les passes de fond doit porter l'appel, et lui
    /// passer `config.auto_update` — sans quoi `TUNE_AUTO_UPDATE` redevient un
    /// réglage accepté et sans effet.
    #[test]
    fn le_verificateur_periodique_est_lance_au_demarrage() {
        let background = include_str!("../../background.rs");
        // Témoin : si `include_str!` pointait sur un fichier vide ou faux,
        // l'assertion suivante échouerait pour la mauvaise raison.
        assert!(
            background.contains("pub async fn spawn_background_tasks"),
            "témoin : le fichier lu doit être celui qui câble les passes de fond"
        );
        // Espaces normalisés : l'appel dépasse la largeur de `rustfmt`, qui le
        // replie sur trois lignes. Un garde qui exigerait la ligne d'un seul
        // tenant tomberait au premier `cargo fmt`, pour rien.
        let serre: String = background.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            serre.contains(
                "update::spawn_verificateur_de_mise_a_jour( state.clone(), config.auto_update, );"
            ) || serre.contains(
                "update::spawn_verificateur_de_mise_a_jour(state.clone(), config.auto_update);"
            ),
            "spawn_verificateur_de_mise_a_jour doit être appelé depuis \
             background.rs, en lui passant `config.auto_update` — sans cet \
             appel, `TUNE_AUTO_UPDATE` est de nouveau sans effet (#3217)"
        );
    }

    /// 🔴 Le réglage doit être lu par la configuration que le serveur CHARGE.
    ///
    /// Il y a deux `TuneConfig` dans ce dépôt. `TUNE_AUTO_UPDATE` n'était lu
    /// que par celle de `tune-core`, dont `from_env()` n'a aucun appelant : le
    /// drapeau n'était donc pas seulement ignoré, il était déclaré dans une
    /// configuration que rien ne construit. Celle qui atteint
    /// `spawn_background_tasks` est `tune_server::config::TuneConfig`, et c'est
    /// elle que ce test lit.
    #[test]
    fn le_reglage_est_lu_par_la_configuration_du_serveur() {
        let config = include_str!("../../config.rs");
        assert!(
            config.contains("pub fn load() -> Self"),
            "témoin : le fichier lu doit être celui que le serveur charge"
        );
        assert!(
            config.contains("pub auto_update: bool"),
            "`auto_update` doit être un champ de la TuneConfig du serveur (#3217)"
        );
        let serre: String = config.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            serre.contains(
                "std::env::var(\"TUNE_AUTO_UPDATE\") { config.auto_update = v == \"true\";"
            ),
            "TUNE_AUTO_UPDATE doit être lu par `TuneConfig::load` — sans cela le \
             réglage reste accepté et sans effet (#3217)"
        );
    }

    /// 🔴 Garde de site : le vérificateur périodique n'installe RIEN.
    ///
    /// La garde anti-coupure de #2954 se justifie par « toute installation est
    /// un geste délibéré ». Un tour de vérification qui appellerait un chemin
    /// d'installation ferait tomber cette prémisse, et rouvrirait les
    /// micro-coupures du 10/08/2026 — cette fois sans personne devant l'écran.
    /// Brancher l'installation automatique est un arbitrage, pas une retouche :
    /// il doit rougir ici avant d'être livré.
    #[test]
    fn le_verificateur_periodique_n_installe_rien() {
        let source = include_str!("update.rs");
        let debut = source
            .find("async fn tour_de_verification")
            .expect("témoin : `tour_de_verification` doit exister dans ce fichier");
        let fin = source[debut..]
            .find("\n/// Câblage et portée du vérificateur périodique")
            .map(|f| debut + f)
            .expect("témoin : la borne de fin du bloc doit exister");
        let bloc = &source[debut..fin];
        assert!(
            bloc.contains("spawn_verificateur_de_mise_a_jour"),
            "témoin : le bloc lu doit contenir le lanceur — {} octets",
            bloc.len()
        );
        for interdit in [
            "update_install",
            "install_unix",
            "install_windows",
            "update_apply",
            "defer_restart_until_quiet",
        ] {
            assert!(
                !bloc.contains(interdit),
                "le vérificateur périodique appelle `{interdit}` : il installerait \
                 sans que personne ait été prévenu, et la garde anti-coupure de \
                 #2954 repose sur le contraire (#3217)"
            );
        }
    }

    /// 🔴 L'annonce doit être LUE quelque part.
    ///
    /// Une notification déposée en base qu'aucune route ne rend serait le
    /// défaut « écrit mais pas branché » à l'autre bout : le vérificateur
    /// tournerait, l'écran ne verrait rien, et `TUNE_AUTO_UPDATE` resterait
    /// aussi muet qu'avant. `GET /system/update/status` est le point de lecture.
    #[test]
    fn l_annonce_est_rendue_par_la_route_de_statut() {
        let source = include_str!("update.rs");
        let debut = source
            .find("pub(super) async fn update_status")
            .expect("témoin : `update_status` doit exister dans ce fichier");
        let fin = source[debut..]
            .find("\n/// Compare the version an in-progress update")
            .map(|f| debut + f)
            .expect("témoin : la borne de fin de la route doit exister");
        let bloc = &source[debut..fin];
        assert!(
            bloc.contains("CLE_MISE_A_JOUR_DISPONIBLE"),
            "`update_status` doit relire la clé du vérificateur périodique (#3217)"
        );
        assert!(
            bloc.contains("\"available_update\": available_update"),
            "`update_status` doit RENDRE l'annonce, pas seulement la lire (#3217)"
        );
    }

    /// L'annonce déposée porte de quoi décider : la version, le canal qui l'a
    /// choisie, et la date du contrôle.
    #[test]
    fn l_annonce_dit_la_version_le_canal_et_la_date() {
        let release = ReleaseInfo {
            tag_name: "v0.9.141".into(),
            version: "0.9.141".into(),
            name: "Tune 0.9.141".into(),
            body: String::new(),
            published_at: "2026-09-06T10:00:00Z".into(),
            html_url: "https://example.invalid/releases/v0.9.141".into(),
            assets: Vec::new(),
        };
        let annonce = annonce_de_release("0.9.140", &release, UpdateChannel::Stable);
        assert_eq!(annonce["current"], "0.9.140");
        assert_eq!(annonce["latest"], "0.9.141");
        assert_eq!(annonce["tag_name"], "v0.9.141");
        assert_eq!(annonce["channel"], "stable");
        assert_eq!(annonce["effective_channel"], "stable");
        assert!(
            annonce["checked_at"].as_u64().unwrap_or(0) > 1_700_000_000,
            "la date du contrôle doit être un horodatage réel : {annonce}"
        );
        // Le canal `auto` doit sortir RÉSOLU, sans quoi un écran affichant
        // « auto » ne dit pas à l'utilisateur ce qu'il va recevoir.
        let auto = annonce_de_release("0.9.140-rc2", &release, UpdateChannel::Auto);
        assert_eq!(auto["channel"], "auto");
        assert_eq!(auto["effective_channel"], "beta");
    }
}

/// Les deux champs que toute réponse de `/update/check` porte désormais :
/// le réglage tel qu'il est enregistré, et le canal EFFECTIF une fois `auto`
/// résolu contre le binaire en cours. Sans le second, un écran affichant
/// « auto » ne dit pas à l'utilisateur ce qu'il va recevoir.
fn channel_fields(channel: UpdateChannel, current: &str) -> (&'static str, &'static str) {
    (channel.as_str(), channel.effective(current))
}

/// GET /system/update/channel — le réglage, et ce qu'il donne concrètement.
pub(super) async fn update_channel_get(State(state): State<AppState>) -> Json<Value> {
    let current = tune_core::version();
    let channel = update_channel(&state.backend);
    let (setting, effective) = channel_fields(channel, current);
    Json(json!({
        "channel": setting,
        "effective_channel": effective,
        "current": current,
        "choices": ["auto", "stable", "beta"],
    }))
}

#[derive(serde::Deserialize)]
pub(super) struct UpdateChannelBody {
    channel: String,
}

/// PUT /system/update/channel — choisir stable, bêta, ou revenir à `auto`.
///
/// Réservé à l'administrateur, comme `update_install` : ce réglage décide de ce
/// que la machine acceptera d'installer.
///
/// Une valeur inconnue est REFUSÉE (400) plutôt qu'ignorée. Un `200` pour rien
/// laisserait l'écran croire que « nightly » a été retenu alors que le serveur
/// serait resté sur `auto`.
pub(super) async fn update_channel_set(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<UpdateChannelBody>,
) -> impl IntoResponse {
    let Some(channel) = UpdateChannel::parse(&body.channel) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "unknown_channel",
                "message": format!("Unknown update channel '{}'", body.channel),
                "choices": ["auto", "stable", "beta"],
            })),
        )
            .into_response();
    };

    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set(UpdateChannel::SETTING_KEY, channel.as_str()) {
        error!(error = %e, "update_channel_write_failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "settings_write_failed", "message": e})),
        )
            .into_response();
    }

    let current = tune_core::version();
    let (setting, effective) = channel_fields(channel, current);
    info!(channel = setting, effective, "update_channel_set");
    (
        StatusCode::OK,
        Json(json!({
            "channel": setting,
            "effective_channel": effective,
            "current": current,
            "choices": ["auto", "stable", "beta"],
        })),
    )
        .into_response()
}

/// GET /system/update/check
///
/// Fetches the latest release from GitHub, compares versions, and returns update info.
pub(super) async fn update_check(State(state): State<AppState>) -> Json<Value> {
    let current = tune_core::version();
    let channel = update_channel(&state.backend);
    let (setting, effective) = channel_fields(channel, current);
    let checker = checker_for(&state.backend);
    let homebrew = current_homebrew_installation();
    let installation_version_mismatch = homebrew
        .as_ref()
        .is_some_and(|install| !homebrew_version_matches(&install.cellar_version, current));

    match checker.check().await {
        Ok(Some(release)) => {
            let mut payload = update_release_payload(current, &release, homebrew.as_ref());
            payload["channel"] = json!(setting);
            payload["effective_channel"] = json!(effective);
            Json(payload)
        }
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
            "channel": setting,
            "effective_channel": effective,
        })),
        Err(e) => {
            warn!(error = %e, "update_check_failed");
            Json(json!({
                "current": current,
                "latest": null,
                "update_available": false,
                "error": e,
                "channel": setting,
                "effective_channel": effective,
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
/// `?force=true` overrides the *request-time* deferral guard that protects work
/// in progress (currently: playback). The UI sets it on the install button,
/// which sits directly under the warning that playback will stop. It does NOT
/// override the restart deferral — see [`defer_restart_until_quiet`].
pub(super) async fn update_install(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<UpdateInstallParams>,
) -> impl IntoResponse {
    let force = params.force.unwrap_or(false);

    // Journaliser l'entrée AVANT tout garde-fou. Sur l'incident du 30 août
    // (#2954), le journal montrait `update_download_starting` puis
    // `update_restarting` 4 s plus tard, et rien ne permettait de départager
    // « le garde-fou a été court-circuité par `force` » de « le garde-fou a
    // regardé un état de lecture qui disait autre chose ». Les deux pistes
    // étaient strictement indiscernables sur le journal du testeur. Ces trois
    // champs — l'intention de l'appelant, son identité, et ce que le serveur
    // voyait de la lecture au même instant — rendent le prochain signalement
    // imputable sans témoin.
    let origin = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    let playing = playing_zone_ids(&state.playback).await;
    info!(
        force,
        origin = %origin,
        playing_zones = ?playing,
        "update_install_requested"
    );
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
        let state_dir = homebrew_state_dir(&state.config.db_path);
        let blocage = match homebrew_upgrade_plan(&installation, &state_dir) {
            Ok(plan) => {
                // Le chemin Homebrew redémarre le serveur, tout comme le chemin
                // autonome : les mêmes reports s'appliquent, mot pour mot. Ils
                // sont répétés ici parce que cette branche rend avant eux.
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
                if !force && !playing.is_empty() {
                    let zones = zones_qui_retiennent(&state.backend, &playing);
                    warn!(zones = ?playing, "update_deferred_playback_in_progress");
                    return (
                        StatusCode::CONFLICT,
                        Json(json!({
                            "status": "blocked",
                            "reason": "playback_in_progress",
                            "zones": zones,
                            "force_available": true,
                            "force_hint": "POST /system/update/install?force=true",
                            "message": "Update deferred: music is playing and installing it would stop playback. It will be applied automatically once playback stops."
                        })),
                    )
                        .into_response();
                }
                match spawn_homebrew_upgrade(&plan, std::process::id()) {
                    Ok(()) => {
                        info!(
                            brew = %plan.brew.display(),
                            cellar_version = %installation.cellar_version,
                            binary_version = tune_core::version(),
                            log = %plan.log_file.display(),
                            "update_homebrew_upgrade_started"
                        );
                        *state.update_phase.lock().unwrap() = Some("homebrew_brew_update".into());
                        return (
                            StatusCode::ACCEPTED,
                            Json(json!({
                                "status": "homebrew_upgrade_started",
                                "reason": "homebrew_upgrade_started",
                                "manager": "homebrew",
                                "command": HOMEBREW_UPDATE_COMMAND,
                                "phase": "brew_update",
                                "log": plan.log_file.to_string_lossy(),
                                "installation_version": installation.cellar_version,
                                "current_version": tune_core::version(),
                            })),
                        )
                            .into_response();
                    }
                    // Le script n'a même pas pu être posé ou lancé. On retombe
                    // sur le refus — jamais sur un silence.
                    Err(error) => HomebrewUpgradeBlock::NoStateDir {
                        path: state_dir.clone(),
                        error,
                    },
                }
            }
            Err(block) => block,
        };

        let refusal = homebrew_update_refusal(&installation, tune_core::version(), Some(&blocage));
        info!(
            executable = %installation.executable.display(),
            cellar_version = %installation.cellar_version,
            binary_version = tune_core::version(),
            mismatch = refusal["installation_version_mismatch"].as_bool().unwrap_or(false),
            blocked = refusal["upgrade_in_place_blocked_reason"].as_str().unwrap_or("none"),
            "update_skipped_homebrew"
        );
        let _ = SettingsRepo::with_backend(state.backend.clone())
            .set("last_update_result", &refusal.to_string());
        // 409 et non 200. Le refus n'est pas une erreur du serveur, mais rendu
        // en 200 il était INDISCERNABLE d'un succès pour son unique
        // consommateur — le client web, qui ne teste que `res.ok` — et laissait
        // le bouton « Installation… » tourner trois minutes dans le vide avant
        // de revenir sans un mot (Yves, Homebrew macOS). Les quatre autres
        // refus de cette même route rendent déjà 409 ; celui-ci rejoint la
        // famille, et les clients qui la traitent déjà l'affichent sans rien
        // apprendre de neuf.
        return (StatusCode::CONFLICT, Json(refusal)).into_response();
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

    // Guard: don't even DOWNLOAD while music is playing. The restart re-execs
    // the process, which kills every output mid-stream — and says so nowhere,
    // so the listener just hears the music cut out (#1462). An update that
    // lands after the album is worth more than one that interrupts it.
    //
    // Ce garde-fou-ci ne protège plus le son à lui seul : il consulte l'état de
    // lecture à la RÉCEPTION de la requête, et `force` le saute. C'est
    // `defer_restart_until_quiet` qui tient l'échange d'image (#2954). Ce qu'il
    // évite encore, et qui vaut d'être gardé : 38 Mo tirés du réseau pendant
    // qu'une zone joue — la contention est exactement le terrain des coupures
    // signalées dans le même fil (#2952).
    if !force && !playing.is_empty() {
        let zones = zones_qui_retiennent(&state.backend, &playing);
        warn!(zones = ?playing, "update_deferred_playback_in_progress");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "status": "blocked",
                "reason": "playback_in_progress",
                // Ce que le journal savait déjà et que l'appelant n'avait pas :
                // QUI retient, et qu'il existe une sortie (#3581).
                "zones": zones,
                "force_available": true,
                "force_hint": "POST /system/update/install?force=true",
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
    //
    // MÊME canal que `/update/check`. C'est ici que le réglage cesse d'être
    // décoratif : sans lui, l'installation retomberait sur le canal déduit du
    // binaire et poserait la préversion que la vérification venait de refuser
    // d'annoncer (#2266).
    let checker = checker_for(&state.backend);
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
        force,
        playing_zones = ?playing,
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
        //
        // Le nouveau binaire est en place sur le disque : la mise à jour est
        // acquise. Il ne reste que l'échange d'image, et c'est LUI, et lui
        // seul, qui coupe le son. On ne le fait pas au milieu d'un morceau
        // (#2954) — pas même quand la requête portait `force`, qui décrivait
        // l'état de lecture d'il y a un téléchargement. Borné par
        // `RESTART_DEFERRAL_MAX` pour qu'une zone oubliée en lecture ne bloque
        // pas les mises à jour à vie.
        if playback_in_progress(&state.playback).await {
            set_phase("restart_pending_playback");
        }
        match defer_restart_until_quiet(
            &state.playback,
            RESTART_DEFERRAL_MAX,
            RESTART_DEFERRAL_POLL,
        )
        .await
        {
            RestartRelease::Idle => {}
            RestartRelease::PlaybackEnded(waited) => {
                info!(
                    waited_secs = waited.as_secs(),
                    "update_restart_window_clear"
                );
            }
            RestartRelease::WindowExpired(waited) => {
                let zones = playing_zone_ids(&state.playback).await;
                warn!(
                    waited_secs = waited.as_secs(),
                    zones = ?zones,
                    "update_restart_deferral_expired"
                );
            }
        }

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
    // La progression d'une mise à jour Homebrew est sur le DISQUE, hors du
    // Cellar, parce qu'elle doit traverser le redémarrage : `update_phase` est
    // en mémoire et le processus qui l'a posée n'existe plus quand l'écran
    // revient interroger. C'est le serveur NEUF qui relit ce fichier.
    let homebrew_upgrade = homebrew_upgrade_state(&homebrew_state_dir(&state.config.db_path));
    let is_failed = phase
        .as_deref()
        .map(|p| p.starts_with("failed"))
        .unwrap_or(false);

    // Result of the LAST applied update, recorded at startup (see
    // record_post_update_result). Lets the UI surface a silent swap failure —
    // e.g. Windows came back on the old binary — instead of the update just
    // looking like it did nothing.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let last_update_result = settings
        .get("last_update_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    // #3217 — ce que le vérificateur périodique a TROUVÉ, s'il tourne. C'est
    // l'autre moitié de `TUNE_AUTO_UPDATE` : sans un endroit où la lire,
    // l'annonce déposée en base serait « écrite mais pas branchée ». `null`
    // quand le réglage est à `false`, quand aucun tour n'a encore eu lieu, ou
    // quand la version installée est déjà la dernière du canal.
    let available_update = settings
        .get(CLE_MISE_A_JOUR_DISPONIBLE)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    // #3581 — la phase `restart_pending_playback` était un mot sans sujet. Le
    // binaire est DÉJÀ posé sur le disque, la mise à jour est acquise, et il ne
    // manque plus que l'échange d'image : seule une zone retient. Le client
    // sondait 180 s puis abandonnait sans un mot, et l'utilisateur concluait
    // « la mise à jour ne fonctionne pas ».
    //
    // On rend donc, dans CETTE phase et elle seule, les trois choses qui
    // manquaient : ce qui est déjà acquis, QUI retient (nommé), et le geste qui
    // libère. `POST /zones/{id}/stop` remet inconditionnellement l'état mémoire
    // à `Stopped` — sans garde, sans admin, sans vérifier que la zone joue
    // vraiment (`routes/playback.rs`, `orchestrator/transport.rs` →
    // `playback.stop`) — et la relance repart au tour de sonde suivant, cinq
    // secondes plus tard. C'était déjà le recours ; il n'était annoncé nulle
    // part. `force_hint` orientait vers le forçage, qui ne porte QUE sur le
    // garde-fou d'entrée et ne touche pas ce report.
    let (restart_pending_zones, recovery_hint) =
        if phase.as_deref() == Some("restart_pending_playback") {
            let ids = playing_zone_ids(&state.playback).await;
            (
                Some(zones_qui_retiennent(&state.backend, &ids)),
                Some("POST /zones/{id}/stop"),
            )
        } else {
            (None, None)
        };
    Json(json!({
        "current_version": tune_core::version(),
        "phase": phase,
        "update_in_progress": phase.is_some() && !is_failed,
        "last_update_result": last_update_result,
        "available_update": available_update,
        // `null` hors de la phase de report : rien à dire, rien à afficher.
        "restart_pending_zones": restart_pending_zones,
        "recovery_hint": recovery_hint,
        // Le binaire est en place : ce qui reste n'est plus une installation,
        // c'est une relance en attente. Le dire évite qu'un client conclue
        // « échec » quand la mise à jour est en fait acquise.
        "binary_installed": phase.as_deref() == Some("restart_pending_playback"),
        // Ce que le script Homebrew détaché a écrit sur le disque, s'il tourne
        // ou s'il vient de finir. `null` partout ailleurs.
        "homebrew_upgrade": homebrew_upgrade,
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

/// Paramètres de `GET /system/changelog`. Le client envoie aussi `limit`, que
/// la route n'a jamais lu ; serde l'ignore, comme avant.
#[derive(Debug, Default, serde::Deserialize)]
pub(super) struct ChangelogQuery {
    /// Langue demandée explicitement (`?lang=en`). Sinon `Accept-Language`.
    pub lang: Option<String>,
}

/// GET /system/changelog — notes de version depuis les releases GitHub, dans
/// la langue demandée, cache 1 heure.
///
/// La langue vient de [`crate::i18n::lang_from_request`] : `?lang=` explicite,
/// sinon `Accept-Language`, sinon `fr` (#3089). Les notes sont traduites À LA
/// PUBLICATION — un bloc par langue dans le corps de la release, cf.
/// [`blocs_par_langue`] — et la route sert le bloc de la langue demandée, ou
/// le français en repli, en le DISANT : `lang` est la langue effectivement
/// servie, `fallback` vaut `true` dès qu'au moins une entrée n'a pas pu être
/// servie dans la langue demandée. Chaque entrée porte aussi ses propres
/// `lang`/`fallback`, car une release ancienne (français seul) peut côtoyer
/// une release traduite dans la même liste.
///
/// Le cache mémorise les releases BRUTES, pas une réponse rendue : la
/// dérivation par langue est un découpage de texte, sans réseau, refait à
/// chaque appel. Un cache de réponses aurait dû être indexé par langue, sans
/// quoi le premier appelant fixait la langue de tous les autres pendant une
/// heure (piège nommé dans l'arbitrage de #3089).
pub(super) async fn changelog(
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<ChangelogQuery>,
) -> Json<Value> {
    use std::sync::OnceLock;
    use tokio::sync::Mutex;

    let lang = crate::i18n::base_tag(&crate::i18n::lang_from_request(q.lang.as_deref(), &headers));

    static CACHE: OnceLock<Mutex<(std::time::Instant, Vec<Value>)>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        Mutex::new((
            std::time::Instant::now() - std::time::Duration::from_secs(7200),
            Vec::new(),
        ))
    });
    let mut guard = cache.lock().await;

    let releases =
        if guard.0.elapsed() < std::time::Duration::from_secs(3600) && !guard.1.is_empty() {
            guard.1.clone()
        } else {
            match fetch_github_releases().await {
                Ok(r) => {
                    *guard = (std::time::Instant::now(), r.clone());
                    r
                }
                Err(_) => guard.1.clone(),
            }
        };
    drop(guard);

    // Le cache démarre vide. Sur un serveur fraîchement lancé et sans réseau,
    // les deux branches ci-dessus rendent donc une liste VIDE, et le panneau
    // « Quoi de neuf » s'affiche désert — ce qui se lit non pas comme « je
    // n'ai pas pu joindre la source » mais comme « cette version n'apporte
    // rien ». Le repli en dur existait depuis toujours pour ce cas ; il
    // n'était simplement jamais appelé.
    if releases.is_empty() {
        return changelog_hardcoded(&lang);
    }

    let NotesServies {
        entries,
        lang: servie,
        fallback,
    } = entrees_pour_langue(&releases, &lang);
    Json(json!({
        "version": tune_core::version(),
        "lang": servie,
        "fallback": fallback,
        "entries": entries,
    }))
}

/// Marqueur ouvrant un bloc de langue dans un corps de release :
/// `<!-- lang:en -->`, seul sur sa ligne. Rend la base de l'étiquette, ou
/// `None` si la ligne n'est pas un marqueur.
fn marqueur_de_langue(line: &str) -> Option<String> {
    let inner = line
        .trim()
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim()
        .strip_prefix("lang:")?;
    let tag = crate::i18n::base_tag(inner);
    (!tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphabetic())).then_some(tag)
}

/// Découpe un corps de release en blocs `(langue, texte)`, dans l'ordre.
///
/// Format de publication multilingue (docs/RELEASE-WORKFLOW.md, « Notes de
/// version multilingues ») : le français d'abord, tel qu'il a toujours été
/// écrit, puis un bloc par traduction, chacun ouvert par un commentaire HTML
/// `<!-- lang:xx -->` seul sur sa ligne. Le commentaire est invisible sur la
/// page GitHub et traverse le proxy `mozaiklabs.fr` comme n'importe quel
/// texte : rien à télécharger de plus, rien à parser de plus qu'un corps.
///
/// Compatibilité : une release ANCIENNE n'a aucun marqueur — tout son corps
/// est le bloc `fr`. Un préambule sans marqueur est de même le bloc `fr`, et
/// un `<!-- lang:fr -->` explicite est accepté. Un bloc vide (marqueur laissé
/// sans texte) est ignoré : il ne « couvre » pas la langue, elle repliera.
fn blocs_par_langue(body: &str) -> Vec<(String, String)> {
    let mut blocs: Vec<(String, String)> = Vec::new();
    let mut courant = String::from("fr");
    let mut texte = String::new();
    let clore = |lang: &str, texte: &mut String, blocs: &mut Vec<(String, String)>| {
        if texte.trim().is_empty() {
            texte.clear();
        } else {
            blocs.push((lang.to_string(), std::mem::take(texte)));
        }
    };
    for line in body.lines() {
        if let Some(lang) = marqueur_de_langue(line) {
            clore(&courant, &mut texte, &mut blocs);
            courant = lang;
            continue;
        }
        texte.push_str(line);
        texte.push('\n');
    }
    clore(&courant, &mut texte, &mut blocs);
    blocs
}

/// Le texte des notes à servir pour `lang`, avec la langue effectivement
/// servie et le drapeau de repli. Cherche d'abord le bloc de la langue
/// demandée, puis le bloc `fr` ; sans aucun des deux (corps vide, ou notes
/// publiées sans français — cas non prévu par le format), rend le corps
/// entier, étiqueté `fr` et en repli.
fn notes_dans_la_langue(body: &str, lang: &str) -> (String, String, bool) {
    let blocs = blocs_par_langue(body);
    if let Some((_, texte)) = blocs.iter().find(|(l, _)| l == lang) {
        return (texte.clone(), lang.to_string(), false);
    }
    if let Some((_, texte)) = blocs.iter().find(|(l, _)| l == "fr") {
        return (texte.clone(), "fr".to_string(), lang != "fr");
    }
    (body.to_string(), "fr".to_string(), lang != "fr")
}

/// Ce que la route rend pour une langue : les entrées, la langue servie et le
/// drapeau de repli agrégé.
struct NotesServies {
    entries: Vec<Value>,
    /// La langue demandée si au moins une entrée est servie dedans, sinon
    /// `fr` : c'est ce que le panneau affiche majoritairement.
    lang: String,
    /// `true` dès qu'une entrée n'a pas pu être servie dans la langue
    /// demandée — le client peut alors dire « notes en français ».
    fallback: bool,
}

/// Dérive les entrées du panneau depuis les releases brutes, pour `lang`.
/// Sans réseau : c'est la partie testable de la route.
fn entrees_pour_langue(releases: &[Value], lang: &str) -> NotesServies {
    let mut fallback = false;
    let mut une_dans_la_langue = false;
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
            let (texte, servie, repli) = notes_dans_la_langue(body, lang);
            fallback |= repli;
            une_dans_la_langue |= !repli;
            let ParsedBody {
                mut features,
                fixes,
                improvements,
            } = parse_release_body(&texte);
            if features.is_empty() && fixes.is_empty() && improvements.is_empty() {
                features.push(format!("Release {version}"));
            }
            Some(json!({
                "version": version,
                "date": date,
                "lang": servie,
                "fallback": repli,
                "features": features,
                "fixes": fixes,
                "improvements": improvements,
            }))
        })
        .collect();
    NotesServies {
        entries,
        lang: if une_dans_la_langue {
            lang.to_string()
        } else {
            "fr".to_string()
        },
        fallback,
    }
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

/// Mots-clés de titre, par rubrique, dans les dix langues de l'interface
/// (`crate::i18n::SUPPORTED`). Le format de publication multilingue
/// (docs/RELEASE-WORKFLOW.md, « Notes de version multilingues ») impose aux
/// blocs traduits les titres que cette table reconnaît : rédacteur et lecteur
/// partagent la même liste, sinon les puces d'un bloc allemand tomberaient en
/// `Other` et le panneau afficherait « Release x.y.z » à la place des notes.
/// Comparaison en minuscules, par sous-chaîne, dans l'ordre : corrections,
/// puis améliorations, puis nouveautés.
const TITRES_CORRECTIONS: &[&str] = &[
    "correct",
    "fix",
    "bug", // fr, en (et « Buggfixar » sv)
    "korrektur",
    "fehler",
    "behoben", // de
    "correc",
    "correz",
    "corect",
    "remed", // es, it, ro
    "rätt",
    "ratt", // sv
    "修复",
    "修正",
    "수정", // zh, ja, ko
];
const TITRES_AMELIORATIONS: &[&str] = &[
    "amélio",
    "ameli",
    "improv", // fr, en
    "verbesser",
    "mejor",
    "miglior", // de, es, it
    "îmbunăt",
    "imbunat",
    "förbättr",
    "forbattr", // ro, sv
    "改进",
    "优化",
    "改善",
    "개선", // zh, ja, ko
];
const TITRES_NOUVEAUTES: &[&str] = &[
    "nouveaut",
    "feature",
    "ajout", // fr, en
    "neuheit",
    "neuerung",
    "neue funktion", // de
    "noved",
    "nuevas func",
    "novit",
    "nuove", // es, it
    "noutăț",
    "noutat",
    "nyhet",
    "nya funktion", // ro, sv
    "新功能",
    "新增",
    "新機能",
    "새로운 기능",
    "신규", // zh, ja, ko
];

/// Classe un intitulé (titre de section) par mots-clés, dans les dix langues
/// de l'interface.
fn section_from_title(title: &str) -> Section {
    let l = title.to_lowercase();
    let contient = |mots: &[&str]| mots.iter().any(|m| l.contains(m));
    if contient(TITRES_CORRECTIONS) {
        Section::Fixes
    } else if contient(TITRES_AMELIORATIONS) {
        Section::Improvements
    } else if contient(TITRES_NOUVEAUTES) {
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

/// Les releases GitHub BRUTES (JSON de l'API, 20 dernières), via le proxy
/// `mozaiklabs.fr` puis GitHub. La dérivation en entrées du panneau, par
/// langue, est faite par [`entrees_pour_langue`] — hors réseau, donc testable.
async fn fetch_github_releases() -> Result<Vec<Value>, String> {
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
    Ok(releases)
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
///
/// Ces notes n'existent qu'en français : `lang` le dit, et `fallback` vaut
/// `true` dès que la langue demandée n'est pas `fr` (#3089).
fn changelog_hardcoded(lang: &str) -> Json<Value> {
    Json(json!({
        "version": tune_core::version(),
        "lang": "fr",
        "fallback": lang != "fr",
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

    /// Remplace `scanning_without_start_time_blocks_update`, qui épinglait
    /// l'inverse (« pas de date ⇒ on protège le scan »). La règle a changé,
    /// délibérément : tant que le scan de démarrage n'horodatait pas, cette
    /// branche était le SEUL comportement possible pour lui, donc un report
    /// perpétuel déguisé en prudence. Maintenant que les deux chemins de
    /// production datent leur annonce (`scan::marquer_scan_en_cours`), un scan
    /// vivant ne passe plus jamais par ici : ne reste que la base héritée
    /// d'une version antérieure tuée en plein scan — exactement ce que la
    /// fenêtre d'ancienneté doit dénouer (#2976).
    #[test]
    fn scanning_without_start_time_no_longer_blocks_update() {
        let b = backend();
        SettingsRepo::with_backend(b.clone())
            .set("scan_status", "scanning")
            .unwrap();
        assert!(
            !scan_in_progress(&b),
            "« scanning » sans horodatage ne peut venir que d'un scan mort : \
             le bloquer indéfiniment n'a aucune sortie"
        );
    }

    /// Même chose pour un horodatage illisible : `parse::<u64>()` échoue, et
    /// le garde-fou ne doit pas retomber sur un blocage sans issue.
    #[test]
    fn scanning_with_unparsable_start_time_does_not_block_update() {
        let b = backend();
        let s = SettingsRepo::with_backend(b.clone());
        s.set("scan_status", "scanning").unwrap();
        s.set("scan_started_at", "2026-08-30T15:42:00Z").unwrap();
        assert!(!scan_in_progress(&b));
    }

    /// TÉMOIN. Un scan RÉELLEMENT en cours doit continuer de différer la mise
    /// à jour. L'annonce est faite par la FONCTION DE PRODUCTION que les deux
    /// scans appellent, jamais par une transcription : si elle cesse
    /// d'horodater, ce test tombe.
    #[test]
    fn scan_annonce_par_la_production_bloque_la_mise_a_jour() {
        let b = backend();
        crate::routes::system::scan::marquer_scan_en_cours(&b);
        assert!(
            scan_in_progress(&b),
            "un scan vivant, annoncé par le chemin de production, doit différer la mise à jour"
        );
    }

    /// L'autre sens, sur la MÊME annonce de production : passé la fenêtre
    /// d'ancienneté, le scan ne bloque plus. Avant #2976 le scan de démarrage
    /// n'était pas daté et ne pouvait donc JAMAIS franchir cette fenêtre.
    #[test]
    fn scan_annonce_par_la_production_puis_perime_laisse_passer() {
        let b = backend();
        crate::routes::system::scan::marquer_scan_en_cours(&b);
        let s = SettingsRepo::with_backend(b.clone());
        let pose: u64 = s
            .get("scan_started_at")
            .unwrap()
            .expect("le chemin de production doit horodater son annonce")
            .trim()
            .parse()
            .expect("l'horodatage doit être un epoch en secondes");
        assert!(
            now_secs().saturating_sub(pose) < 60,
            "l'horodatage posé doit être celui de maintenant"
        );
        s.set(
            "scan_started_at",
            &(pose - (super::SCAN_GUARD_STALE_SECS + 3600)).to_string(),
        )
        .unwrap();
        assert!(!scan_in_progress(&b));
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

/// Le scan de DÉMARRAGE, éprouvé de bout en bout — c'est lui, et lui seul,
/// que #2976 laissait indatable.
///
/// Le vrai `spawn_auto_scan` est exécuté sur un dossier de musique temporaire.
/// `ScanStatusGuard` remet `scan_status` à `idle` en fin de tâche, mais il
/// n'efface PAS `scan_started_at` : la trace que le scan de démarrage a bien
/// daté son annonce survit à sa terminaison, et se lit donc sans course.
#[cfg(test)]
mod scan_de_demarrage_tests {
    use super::scan_in_progress;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::sqlite::SqliteDb;

    #[tokio::test]
    async fn le_scan_de_demarrage_horodate_son_annonce_et_devient_perimable() {
        // Le dossier doit exister pendant TOUT le scan : le handle est gardé
        // en vie jusqu'à la fin du test, et il n'est pas dans /tmp partagé.
        let dossier = tempfile::tempdir().unwrap();

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let settings = SettingsRepo::with_backend(backend.clone());
        settings
            .set(
                "music_dirs",
                &serde_json::to_string(&[dossier.path().to_string_lossy()]).unwrap(),
            )
            .unwrap();

        let bus = Arc::new(tune_core::event_bus::EventBus::new());
        let fini = crate::auto_scan::spawn_auto_scan(backend.clone(), bus);
        for _ in 0..600 {
            if fini.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            fini.load(Ordering::Acquire),
            "le scan de démarrage n'a pas terminé"
        );

        // 1. Le scan de démarrage a DATÉ son annonce. C'est l'écriture qui
        //    manquait : sans elle tout ce qui suit est indécidable.
        let pose: u64 = settings
            .get("scan_started_at")
            .unwrap()
            .expect("le scan de démarrage doit poser `scan_started_at` (#2976)")
            .trim()
            .parse()
            .expect("`scan_started_at` doit être un epoch en secondes parseable");
        let maintenant = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            maintenant.saturating_sub(pose) < 300,
            "l'horodatage doit être celui de ce scan-ci"
        );

        // 2. Le processus meurt pendant le scan : `ScanStatusGuard` ne
        //    s'exécute pas, la base garde « scanning ». C'est l'état exact que
        //    laisse une coupure de courant ou un `kill -9`.
        settings.set("scan_status", "scanning").unwrap();

        // TÉMOIN : tant que la fenêtre n'est pas franchie, la mise à jour
        //    reste différée — la correction ne tue pas un scan vivant.
        assert!(
            scan_in_progress(&backend),
            "un scan de démarrage récent doit continuer de différer la mise à jour"
        );

        // 3. Treize heures plus tard, le garde-fou le déclare périmé et la
        //    mise à jour passe. C'est ce que le scan de démarrage ne pouvait
        //    PAS faire avant #2976, faute de date.
        settings
            .set(
                "scan_started_at",
                &(pose - (super::SCAN_GUARD_STALE_SECS + 3600)).to_string(),
            )
            .unwrap();
        assert!(
            !scan_in_progress(&backend),
            "passé la fenêtre d'ancienneté, un scan de démarrage mort ne doit plus rien bloquer"
        );

        drop(dossier);
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

/// #3581 — la SORTIE d'un état bloquant : une zone qui annonce `Playing` mais
/// que plus personne n'observe avancer ne retient plus rien.
///
/// Tades ne pouvait pas mettre à jour son serveur, et n'avait aucun recours :
/// sa Serenade était restée `Playing` en mémoire, le seul détecteur de zone
/// figée est DLNA-only (#3155), `startup.rs` ne remet à `stopped` que la
/// colonne, et la seule sortie automatique était le plafond de DEUX HEURES du
/// report de relance.
///
/// Ces épreuves passent par [`super::zones_en_lecture_vivante`] — la fonction
/// que le garde-fou d'entrée ET le report de relance appellent tous deux, via
/// [`playing_zone_ids`]. Ce n'est pas une réplique du mécanisme.
#[cfg(test)]
mod zone_figee_tests {
    use super::{SILENCE_DE_POSITION_AVANT_ZONE_FIGEE, zones_en_lecture_vivante};
    use std::time::Duration;
    use tune_core::playback::{NowPlaying, PlaybackManager};

    /// Seuil nul : toute zone DÉJÀ observée est immobile « depuis plus
    /// longtemps que le seuil ». C'est le seul moyen d'atteindre la branche
    /// figée sans faire dormir dix minutes.
    const TOUT_DE_SUITE: Duration = Duration::ZERO;

    /// LE défaut : la zone a été observée, l'observation s'est arrêtée, et
    /// elle retenait la mise à jour pour toujours.
    #[tokio::test]
    async fn une_zone_dont_la_position_ne_bouge_plus_ne_retient_plus_la_mise_a_jour() {
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        // Ce que le sondeur écrit chaque seconde sur une zone observée.
        pm.update_position(12, 1_000).await;
        assert!(
            zones_en_lecture_vivante(&pm, TOUT_DE_SUITE)
                .await
                .is_empty(),
            "une zone observée dont la position n'avance plus au-delà du seuil \
             doit cesser de retenir la mise à jour : c'est la sortie qui \
             manquait à #3581"
        );
    }

    /// La contre-épreuve du remède : une lecture RÉELLE ne doit jamais être
    /// coupée. La zone vient d'être observée, le seuil est celui de
    /// production — elle retient.
    #[tokio::test]
    async fn une_lecture_reelle_qui_avance_retient_toujours_la_mise_a_jour() {
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        pm.update_position(12, 1_000).await;
        assert_eq!(
            zones_en_lecture_vivante(&pm, SILENCE_DE_POSITION_AVANT_ZONE_FIGEE).await,
            vec![12],
            "au seuil de production, une zone observée il y a un instant est \
             VIVANTE : la mise à jour ne doit pas lui couper le son"
        );
    }

    /// « Jamais observée » n'est pas « immobile ». Une zone navigateur — aucun
    /// périphérique, donc `poller/tick.rs` fait `continue` avant son unique
    /// `update_position` — ne doit pas être déclarée figée par son immobilité.
    ///
    /// `NowPlaying::default()` n'annonce AUCUNE durée : c'est le cas où le
    /// serveur n'a rien à comparer, et il ne conclut rien. La borne qui
    /// s'applique quand la durée EST connue est tenue par les deux tests
    /// suivants.
    #[tokio::test]
    async fn une_zone_jamais_observee_reste_traitee_comme_jouant() {
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        assert_eq!(
            zones_en_lecture_vivante(&pm, TOUT_DE_SUITE).await,
            vec![12],
            "sans une seule avance observée, le serveur ne SAIT pas : il doit \
             conclure « ça joue », jamais « c'est figé »"
        );
    }

    /// LE reste de #3581, après #3723 : la zone n'a JAMAIS été observée — donc
    /// le prédicat d'immobilité ne mord pas — mais sa piste est finie. Elle
    /// retenait la mise à jour sans aucune borne.
    ///
    /// La durée d'une milliseconde et la marge nulle ne sont qu'une échelle :
    /// le fait tenu est « la fin annoncée est dépassée », et il est le même à
    /// quatre minutes et dix.
    #[tokio::test]
    async fn une_piste_finie_sans_la_moindre_observation_ne_retient_plus_la_mise_a_jour() {
        let pm = PlaybackManager::new();
        pm.play(
            12,
            NowPlaying {
                duration_ms: 1,
                ..Default::default()
            },
        )
        .await;
        // Aucun `update_position` : c'est tout le sujet — personne n'observe
        // cette zone, et personne ne l'observera jamais.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            zones_en_lecture_vivante(&pm, TOUT_DE_SUITE)
                .await
                .is_empty(),
            "une zone jamais observée dont la piste est FINIE doit cesser de \
             retenir la mise à jour : c'est la moitié de #3581 que #3723 \
             laissait ouverte"
        );
    }
    /// La contre-épreuve du remède, et elle est sévère : marge NULLE, et la
    /// zone n'a jamais été observée. Une piste d'une heure qui vient de
    /// démarrer n'est pas finie — la mise à jour ne doit pas lui couper le son.
    #[tokio::test]
    async fn une_piste_encore_en_cours_sans_observation_retient_toujours_la_mise_a_jour() {
        let pm = PlaybackManager::new();
        pm.play(
            12,
            NowPlaying {
                duration_ms: 3_600_000,
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            zones_en_lecture_vivante(&pm, TOUT_DE_SUITE).await,
            vec![12],
            "sans la moindre observation, une piste dont la fin annoncée n'est \
             PAS atteinte reste une lecture : c'est la zone navigateur qui \
             joue vraiment, et on ne la coupe pas"
        );
    }
    /// Une RADIO est exclue du verdict : un flux live n'a pas de durée, et
    /// plusieurs renderers en annoncent la position par à-coups. La couper
    /// serait exactement le défaut grave que ce correctif doit éviter.
    #[tokio::test]
    async fn une_radio_immobile_retient_toujours_la_mise_a_jour() {
        let pm = PlaybackManager::new();
        pm.play(
            12,
            NowPlaying {
                source: "radio".into(),
                ..Default::default()
            },
        )
        .await;
        pm.update_position(12, 1_000).await;
        assert_eq!(
            zones_en_lecture_vivante(&pm, TOUT_DE_SUITE).await,
            vec![12],
            "une radio ne doit JAMAIS être déclarée figée sur l'immobilité de \
             sa position"
        );
    }

    /// Une pause reste une pause : le fantôme ne doit pas ressusciter des
    /// zones que le garde-fou laissait déjà passer.
    #[tokio::test]
    async fn une_zone_en_pause_reste_hors_du_compte() {
        let pm = PlaybackManager::new();
        pm.play(12, NowPlaying::default()).await;
        pm.update_position(12, 1_000).await;
        pm.pause(12).await;
        assert!(
            zones_en_lecture_vivante(&pm, SILENCE_DE_POSITION_AVANT_ZONE_FIGEE)
                .await
                .is_empty()
        );
    }

    /// La constante de PRODUCTION, tenue par les deux bouts.
    ///
    /// Sans ce test, la façon la plus simple de rétablir le défaut — ramener
    /// le seuil à l'infini — passerait au vert ; et la façon la plus simple de
    /// créer un défaut GRAVE — le ramener à quelques secondes — aussi.
    #[test]
    fn le_seuil_de_production_est_borne_des_deux_cotes() {
        assert_eq!(
            SILENCE_DE_POSITION_AVANT_ZONE_FIGEE,
            Duration::from_secs(600)
        );
        // Borne BASSE : le sondeur s'accorde lui-même jusqu'à 45 s de
        // chargement de piste puis 30 ticks avant de déclarer une panne, soit
        // 75 s. Un seuil sous cette barre couperait une lecture que le reste
        // du serveur considère encore vivante.
        assert!(
            SILENCE_DE_POSITION_AVANT_ZONE_FIGEE > Duration::from_secs(75),
            "sous le propre verdict de panne du sondeur (45 + 30 s), ce seuil \
             couperait du son réel"
        );
        // Borne HAUTE : il doit être franchement meilleur que le plafond de
        // deux heures qu'il remplace, sinon il n'offre aucune sortie.
        assert!(
            SILENCE_DE_POSITION_AVANT_ZONE_FIGEE * 10 < super::RESTART_DEFERRAL_MAX,
            "le seuil doit libérer la mise à jour bien avant le plafond de \
             deux heures, sans quoi il ne sert à rien"
        );
    }
}

/// Le report de la relance — la moitié qui manquait à #2954.
///
/// L'horloge est celle de tokio, mise en pause : `start_paused = true` fait
/// avancer le temps VIRTUEL dès que toutes les tâches dorment. Ces tests
/// traversent deux heures de plafond sans qu'une seule seconde réelle passe.
/// Aucun `sleep` réel : un test qui attendrait vraiment 24 s finirait désarmé.
///
/// Ils portent sur `defer_restart_until_quiet` — la fonction que la tâche
/// d'installation appelle juste avant `set_phase("restarting")`, pas une
/// réplique de son mécanisme. Dégrader son corps fait tomber ces tests.
#[cfg(test)]
mod restart_deferral_tests {
    use super::{RestartRelease, defer_restart_until_quiet};
    use std::sync::Arc;
    use std::time::Duration;
    use tune_core::playback::{NowPlaying, PlaybackManager};

    const MAX: Duration = Duration::from_secs(2 * 3600);
    const POLL: Duration = Duration::from_secs(5);

    /// Les valeurs de PRODUCTION, pas celles du test.
    ///
    /// Les cas ci-dessous passent leurs propres bornes pour rester lisibles ;
    /// ce test-ci est le seul qui tienne les constantes réelles. Sans lui, un
    /// plafond porté à l'infini — la façon la plus simple de rétablir le défaut
    /// « une zone oubliée bloque les mises à jour à vie » — passerait au vert.
    #[test]
    fn the_production_ceiling_is_finite_and_the_poll_is_short() {
        assert_eq!(
            super::RESTART_DEFERRAL_MAX,
            MAX,
            "le plafond du report doit rester borné : sans sortie, une zone \
             laissée en lecture — ou figée, ce qu'aucun détecteur ne rattrape \
             (#3155) — bloque les mises à jour pour toujours"
        );
        assert_eq!(
            super::RESTART_DEFERRAL_POLL,
            POLL,
            "la relance doit suivre la fin du morceau de près, sinon le report \
             devient une attente en soi"
        );
        assert!(super::RESTART_DEFERRAL_POLL < super::RESTART_DEFERRAL_MAX);
    }

    /// LA moitié qui décrit l'incident : la lecture démarre, la relance se
    /// présente 24 s plus tard, et elle N'A PAS LIEU. Le journal du 30 août
    /// montre `local_audio_playing_after_prefill` à 15:42:00 et
    /// `update_reexec` à 15:42:24.
    #[tokio::test(start_paused = true)]
    async fn restart_waits_while_a_zone_plays() {
        let pm = PlaybackManager::new();
        pm.play(20, NowPlaying::default()).await;

        // 24 s de plafond : la fenêtre exacte de l'incident. Rien ne s'arrête,
        // donc la seule sortie est le plafond — et on doit l'avoir ATTENDU.
        let short = Duration::from_secs(24);
        let started = tokio::time::Instant::now();
        let release = defer_restart_until_quiet(&pm, short, POLL).await;

        assert_eq!(release, RestartRelease::WindowExpired(short));
        assert_eq!(
            started.elapsed(),
            short,
            "la relance est partie avant la fin de la fenêtre"
        );
    }

    /// L'autre moitié, qui compte autant : quand plus rien ne joue, la relance
    /// part — et sans rien attendre du tout.
    #[tokio::test(start_paused = true)]
    async fn restart_goes_ahead_when_nothing_plays() {
        let pm = PlaybackManager::new();
        let started = tokio::time::Instant::now();

        let release = defer_restart_until_quiet(&pm, MAX, POLL).await;

        assert_eq!(release, RestartRelease::Idle);
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "une mise à jour hors lecture ne doit rien payer"
        );
    }

    /// Une zone arrêtée entre-temps libère la relance au tour de scrutation
    /// suivant : c'est le cas nominal — la mise à jour prend la fin du morceau,
    /// pas son milieu.
    #[tokio::test(start_paused = true)]
    async fn restart_fires_as_soon_as_playback_stops() {
        let pm = Arc::new(PlaybackManager::new());
        pm.play(20, NowPlaying::default()).await;

        let stopper = Arc::clone(&pm);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(90)).await;
            stopper.stop(20).await;
        });

        let release = defer_restart_until_quiet(&pm, MAX, POLL).await;

        match release {
            RestartRelease::PlaybackEnded(waited) => {
                assert!(
                    waited >= Duration::from_secs(90) && waited <= Duration::from_secs(90) + POLL,
                    "libérée à {waited:?}, attendu entre 90 s et 95 s"
                );
            }
            other => panic!("attendu PlaybackEnded, obtenu {other:?}"),
        }
    }

    /// Le piège symétrique : une zone oubliée EN LECTURE ne bloque pas les
    /// mises à jour à vie. #3155 a établi qu'aucun détecteur ne rattrape une
    /// zone locale figée — elle peut rester `Playing` indéfiniment sans qu'un
    /// échantillon sorte. Le plafond est la sortie, et il est atteint.
    #[tokio::test(start_paused = true)]
    async fn a_zone_left_playing_forever_does_not_block_updates_forever() {
        let pm = PlaybackManager::new();
        pm.play(20, NowPlaying::default()).await;

        let release = defer_restart_until_quiet(&pm, MAX, POLL).await;

        assert_eq!(release, RestartRelease::WindowExpired(MAX));
    }

    /// Une zone en PAUSE ne retient rien. Sans quoi une zone laissée en pause
    /// des jours durant bloquerait toutes les mises à jour jusqu'au plafond,
    /// pour un son que personne n'écoute. C'est aussi ce que dit le frein de
    /// repos du poller depuis #3120 : la pause n'est pas une lecture.
    #[tokio::test(start_paused = true)]
    async fn a_paused_zone_never_holds_the_restart() {
        let pm = PlaybackManager::new();
        pm.play(20, NowPlaying::default()).await;
        pm.pause(20).await;

        let started = tokio::time::Instant::now();
        let release = defer_restart_until_quiet(&pm, MAX, POLL).await;

        assert_eq!(release, RestartRelease::Idle);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// Le corps du handler `update_install`, isolé du fichier source.
    ///
    /// `include_str!` rend le fichier ENTIER, ce module de test compris — où le
    /// nom de la fonction surveillée apparaît en toutes lettres, et où le
    /// fichier compte huit modules `#[cfg(test)]` intercalés dans la
    /// production. Sans cette découpe, le garde-fou ci-dessous se prouverait
    /// lui-même et resterait vert quel que soit le code d'installation.
    fn corps_de_update_install(source: &str) -> &str {
        let debut = source
            .find("pub(super) async fn update_install(")
            .expect("handler `update_install` introuvable dans le source");
        let reste = &source[debut..];
        // Jusqu'au prochain handler de premier niveau, ou au prochain module de
        // test — le premier des deux.
        let fin = reste[1..]
            .find("\npub(super) async fn ")
            .into_iter()
            .chain(reste[1..].find("\n#[cfg(test)]"))
            .min()
            .map(|i| i + 1)
            .unwrap_or(reste.len());
        &reste[..fin]
    }

    /// Le report est-il POSÉ SUR LE CHEMIN DE LA RELANCE ?
    ///
    /// Les tests ci-dessus éprouvent le mécanisme ; celui-ci éprouve son
    /// branchement. C'est exactement la faille de #2954 : le garde-fou de
    /// lecture existait, il était juste consulté au mauvais endroit. Un
    /// correctif juste qui n'est appelé nulle part ne coupe rien.
    #[test]
    fn the_install_task_defers_the_restart_before_re_execing() {
        let corps = corps_de_update_install(include_str!("update.rs"));
        let defer = corps
            .find("defer_restart_until_quiet(")
            .expect("la tâche d'installation n'appelle plus le report de relance (#2954)");
        let restart = corps
            .find("set_phase(\"restarting\")")
            .expect("phase `restarting` introuvable dans `update_install`");
        assert!(
            defer < restart,
            "le report doit être consulté AVANT la phase `restarting` : \
             c'est l'échange d'image qui coupe le son, pas le téléchargement"
        );
    }

    /// Contre-épreuve du garde-fou statique : sur un source d'où l'appel a
    /// disparu, il doit tomber. Un détecteur qui trouve son motif partout ne
    /// détecte rien.
    #[test]
    fn the_call_site_guard_falls_on_a_source_without_the_call() {
        // Un handler nu : rien à trouver.
        let nu = "pub(super) async fn update_install(s: S) {\n    set_phase(\"restarting\");\n}\n";
        assert!(
            !corps_de_update_install(nu).contains("defer_restart_until_quiet("),
            "le détecteur trouve l'appel dans un handler qui ne l'a pas"
        );
        // Le report posé dans un AUTRE handler ne compte pas : la découpe doit
        // s'arrêter au handler suivant.
        let ailleurs = "pub(super) async fn update_install(s: S) {\n    set_phase(\"restarting\");\n}\n\
             \npub(super) async fn update_status(s: S) {\n    defer_restart_until_quiet(&s.playback);\n}\n";
        assert!(
            !corps_de_update_install(ailleurs).contains("defer_restart_until_quiet("),
            "la découpe déborde sur le handler suivant"
        );
        // Ni un module de test intercalé.
        let en_test = "pub(super) async fn update_install(s: S) {\n    set_phase(\"restarting\");\n}\n\
             \n#[cfg(test)]\nmod t {\n    defer_restart_until_quiet(&s.playback);\n}\n";
        assert!(
            !corps_de_update_install(en_test).contains("defer_restart_until_quiet("),
            "la coupe à `#[cfg(test)]` ne tient pas"
        );
    }

    /// Treize zones sur .18 : le report regarde toutes les zones, pas la
    /// première venue. Une seule qui joue suffit à retenir.
    #[tokio::test(start_paused = true)]
    async fn one_playing_zone_among_idle_ones_holds_the_restart() {
        let pm = PlaybackManager::new();
        pm.play(4, NowPlaying::default()).await;
        pm.stop(4).await;
        pm.play(8, NowPlaying::default()).await;
        pm.pause(8).await;
        pm.play(20, NowPlaying::default()).await;

        let short = Duration::from_secs(60);
        assert_eq!(
            defer_restart_until_quiet(&pm, short, POLL).await,
            RestartRelease::WindowExpired(short)
        );
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
        let response = homebrew_update_refusal(&installation, "0.9.110", None);

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
        let body = changelog_hardcoded("fr").0;
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
        let body = changelog_hardcoded("fr").0;
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
        let body = changelog_hardcoded("fr").0;
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

#[cfg(test)]
mod changelog_lang_tests {
    //! #3089 — les notes de version sont traduites À LA PUBLICATION, un bloc
    //! par langue dans le corps de la release ; la route sert la langue
    //! demandée, ou le français en le disant. Aucun réseau : on part des
    //! releases brutes telles que l'API les rend.
    use super::{
        Section, blocs_par_langue, changelog_hardcoded, entrees_pour_langue, section_from_title,
    };
    use serde_json::{Value, json};

    /// Corps publié selon le format multilingue : français d'abord, sans
    /// marqueur, puis un bloc anglais.
    const CORPS_TRADUIT: &str = "\
## Nouveautés
- Recherche dans un serveur UPnP
## Corrections
- Pochette erronée dans les compilations

<!-- lang:en -->
## Features
- Search inside a UPnP server
## Bug fixes
- Wrong cover art in compilations
";

    /// Corps d'une release ANCIENNE : français seul, aucun marqueur.
    const CORPS_ANCIEN: &str = "\
## Corrections
- Lecture qui s'arrêtait au premier morceau
";

    fn release(tag: &str, body: &str) -> Value {
        json!({ "tag_name": tag, "published_at": "2026-09-06T19:06:57Z", "body": body })
    }

    #[test]
    fn langue_demandee_presente_elle_est_servie() {
        let r = [release("v0.9.141", CORPS_TRADUIT)];
        let n = entrees_pour_langue(&r, "en");
        assert_eq!(n.lang, "en");
        assert!(
            !n.fallback,
            "la langue demandée existe : aucun repli à déclarer"
        );
        let e = &n.entries[0];
        assert_eq!(e["lang"], json!("en"));
        assert_eq!(e["fallback"], json!(false));
        assert_eq!(e["features"], json!(["Search inside a UPnP server"]));
        assert_eq!(e["fixes"], json!(["Wrong cover art in compilations"]));
        // Le bloc français ne fuit pas dans la réponse anglaise.
        assert!(
            !e.to_string().contains("Pochette"),
            "le bloc français a fui dans la réponse anglaise : {e}"
        );
    }

    #[test]
    fn langue_absente_repli_francais_declare() {
        let r = [release("v0.9.141", CORPS_TRADUIT)];
        let n = entrees_pour_langue(&r, "de");
        assert_eq!(
            n.lang, "fr",
            "sans bloc allemand, c'est le français qui est servi"
        );
        assert!(n.fallback, "le repli doit être DIT, pas silencieux");
        let e = &n.entries[0];
        assert_eq!(e["lang"], json!("fr"));
        assert_eq!(e["fallback"], json!(true));
        assert_eq!(e["features"], json!(["Recherche dans un serveur UPnP"]));
        assert!(
            !e.to_string().contains("Search inside"),
            "le bloc anglais a été servi à un appel allemand : {e}"
        );
    }

    #[test]
    fn release_ancienne_sans_marqueur_reste_francaise() {
        let r = [release("v0.9.129", CORPS_ANCIEN)];
        // Demandée en français : servie telle quelle, sans repli.
        let n = entrees_pour_langue(&r, "fr");
        assert_eq!(n.lang, "fr");
        assert!(!n.fallback);
        assert_eq!(
            n.entries[0]["fixes"],
            json!(["Lecture qui s'arrêtait au premier morceau"])
        );
        // Demandée en anglais : même contenu, repli déclaré.
        let n = entrees_pour_langue(&r, "en");
        assert_eq!(n.lang, "fr");
        assert!(n.fallback);
        assert_eq!(n.entries[0]["fallback"], json!(true));
        assert_eq!(
            n.entries[0]["fixes"],
            json!(["Lecture qui s'arrêtait au premier morceau"])
        );
    }

    #[test]
    fn releases_traduites_et_anciennes_cohabitent() {
        // Une liste réelle mêle des releases publiées avant et après le
        // format : chaque entrée dit sa langue, l'agrégat dit le repli.
        let r = [
            release("v0.9.141", CORPS_TRADUIT),
            release("v0.9.129", CORPS_ANCIEN),
        ];
        let n = entrees_pour_langue(&r, "en");
        assert_eq!(n.lang, "en", "au moins une entrée est en anglais");
        assert!(
            n.fallback,
            "une entrée n'a pas pu l'être : le repli est déclaré"
        );
        assert_eq!(n.entries[0]["lang"], json!("en"));
        assert_eq!(n.entries[0]["fallback"], json!(false));
        assert_eq!(n.entries[1]["lang"], json!("fr"));
        assert_eq!(n.entries[1]["fallback"], json!(true));
    }

    #[test]
    fn le_marqueur_tolere_espaces_casse_et_region() {
        let blocs =
            blocs_par_langue("Préambule\n<!--lang:EN-GB-->\nBody\n<!--  lang: de  -->\nText\n");
        let langues: Vec<&str> = blocs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(langues, ["fr", "en", "de"]);
        assert_eq!(blocs[1].1.trim(), "Body");
    }

    #[test]
    fn un_bloc_vide_ne_couvre_pas_sa_langue() {
        // Un marqueur laissé sans texte (traduction oubliée) ne doit pas
        // produire un panneau vide : la langue replie sur le français.
        let r = [release(
            "v0.9.141",
            "## Corrections\n- Un correctif\n<!-- lang:en -->\n\n",
        )];
        let n = entrees_pour_langue(&r, "en");
        assert_eq!(n.lang, "fr");
        assert!(n.fallback);
        assert_eq!(n.entries[0]["fixes"], json!(["Un correctif"]));
    }

    #[test]
    fn un_commentaire_html_quelconque_nest_pas_un_marqueur() {
        // `<!-- generated by git-cliff -->` (pied de cliff.toml) et autres
        // commentaires ne découpent rien.
        let blocs = blocs_par_langue("## Corrections\n- x\n<!-- generated by git-cliff -->\n");
        assert_eq!(blocs.len(), 1);
        assert_eq!(blocs[0].0, "fr");
    }

    #[test]
    fn les_titres_traduits_se_classent() {
        for (titre, attendu) in [
            ("Fehlerbehebungen", Section::Fixes),
            ("Correcciones", Section::Fixes),
            ("Correzioni", Section::Fixes),
            ("Rättningar", Section::Fixes),
            ("修复", Section::Fixes),
            ("バグ修正", Section::Fixes),
            ("버그 수정", Section::Fixes),
            ("Verbesserungen", Section::Improvements),
            ("Mejoras", Section::Improvements),
            ("Miglioramenti", Section::Improvements),
            ("Îmbunătățiri", Section::Improvements),
            ("Förbättringar", Section::Improvements),
            ("改进", Section::Improvements),
            ("改善", Section::Improvements),
            ("개선", Section::Improvements),
            ("Neuheiten", Section::Features),
            ("Novedades", Section::Features),
            ("Novità", Section::Features),
            ("Noutăți", Section::Features),
            ("Nyheter", Section::Features),
            ("新功能", Section::Features),
            ("新機能", Section::Features),
            ("새로운 기능", Section::Features),
            // Inchangé : ce qui n'est pas une rubrique du panneau reste dehors.
            ("Mise à jour", Section::Other),
            ("Downloads", Section::Other),
            ("Lecture", Section::Other),
        ] {
            assert!(
                section_from_title(titre) == attendu,
                "« {titre} » mal classé"
            );
        }
    }

    #[test]
    fn le_repli_en_dur_dit_sa_langue() {
        let fr = changelog_hardcoded("fr").0;
        assert_eq!(fr["lang"], json!("fr"));
        assert_eq!(fr["fallback"], json!(false));
        let en = changelog_hardcoded("en").0;
        assert_eq!(
            en["lang"],
            json!("fr"),
            "le secours n'existe qu'en français"
        );
        assert_eq!(en["fallback"], json!(true));
    }
}

/// #3581 — ce que le refus de mise à jour rend à l'appelant.
#[cfg(test)]
mod tests_zones_qui_retiennent {
    use super::zones_qui_retiennent;
    use std::sync::Arc;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::db::zone_repo::ZoneRepo;

    fn backend() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().expect("base en mémoire");
        db.init_schema().expect("schéma");
        tune_core::db::migrations::run_migrations(&db).expect("migrations");
        Arc::new(db)
    }

    /// Le refus doit NOMMER la zone. Tades voyait « playback_in_progress » et
    /// rien d'autre : il ne pouvait pas savoir que c'était sa Serenade que le
    /// serveur croyait en lecture.
    #[test]
    fn le_refus_nomme_la_zone_qui_retient() {
        let b = backend();
        let repo = ZoneRepo::with_backend(b.clone());
        let id = repo
            .create("Serenade", Some("dlna"), None)
            .expect("création de zone");

        let rendu = zones_qui_retiennent(&b, &[id]);

        assert_eq!(rendu.len(), 1);
        assert_eq!(rendu[0]["id"].as_i64(), Some(id));
        assert_eq!(rendu[0]["name"].as_str(), Some("Serenade"));
    }

    /// Contre-épreuve : un identifiant sans zone en base ne fait pas échouer le
    /// refus et ne fabrique pas de nom. L'identifiant seul vaut mieux que rien
    /// — c'est exactement le cas d'une zone figée en mémoire dont la ligne a
    /// disparu (#3155).
    #[test]
    fn un_identifiant_sans_zone_rend_un_nom_vide_sans_echouer() {
        let b = backend();
        let rendu = zones_qui_retiennent(&b, &[4242]);

        assert_eq!(rendu.len(), 1);
        assert_eq!(rendu[0]["id"].as_i64(), Some(4242));
        assert!(rendu[0]["name"].is_null());
    }

    /// Aucune zone en lecture : rien à nommer. Une garde qui rendrait toujours
    /// une entrée se lirait comme un refus permanent.
    #[test]
    fn sans_zone_en_lecture_il_n_y_a_rien_a_nommer() {
        let b = backend();
        assert!(zones_qui_retiennent(&b, &[]).is_empty());
    }
}

/// Ce que la mise à jour Homebrew EN PLACE doit tenir.
///
/// Aucun de ces tests ne relit le source : ils construisent un faux Cellar sur
/// le disque, et le dernier exécute réellement le script produit contre un
/// `brew` factice. Un test qui vérifierait que la fonction a été appelée ne
/// prouverait pas que `brew` est trouvé ni que le fichier d'état bouge.
#[cfg(test)]
mod homebrew_upgrade_tests {
    use std::path::{Path, PathBuf};

    use super::{
        HomebrewInstallation, HomebrewUpgradeBlock, HomebrewUpgradePlan, homebrew_prefix,
        homebrew_state_dir, homebrew_update_refusal, homebrew_upgrade_plan,
        homebrew_upgrade_script, homebrew_upgrade_state,
    };

    /// Répertoire temporaire propre à un test, nettoyé à la fin.
    ///
    /// On passe par la sortie AUTORISÉE du dépôt, pas par un chemin composé à
    /// la main : `tune-core/tests/aucune_fuite_de_temporaires.rs` bannit le
    /// second geste, et `update.rs` appelle déjà `scratch_dir` quelques
    /// dizaines de lignes plus haut.
    type Bac = tune_core::test_scratch::ScratchDir;

    fn bac(nom: &str) -> Bac {
        tune_core::test_scratch::scratch_dir(&format!("tune-hb-{nom}"))
    }

    #[cfg(unix)]
    fn ecrire_executable(chemin: &Path, contenu: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
        std::fs::write(chemin, contenu).unwrap();
        std::fs::set_permissions(chemin, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Faux préfixe Homebrew complet : `bin/brew` exécutable, Cellar peuplé.
    #[cfg(unix)]
    fn faux_prefixe(bac: &Bac, brew: &str) -> (PathBuf, HomebrewInstallation) {
        let prefix = bac.path().join("opt/homebrew");
        ecrire_executable(&prefix.join("bin/brew"), brew);
        let exe = prefix.join("Cellar/tune-server/0.9.143/bin/tune-server");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();
        (
            prefix,
            HomebrewInstallation {
                executable: exe,
                cellar_version: "0.9.143".into(),
            },
        )
    }

    #[test]
    fn le_prefixe_se_deduit_du_cellar_pas_du_path() {
        for (chemin, attendu) in [
            (
                "/opt/homebrew/Cellar/tune-server/0.9.143/bin/tune-server",
                "/opt/homebrew",
            ),
            (
                "/usr/local/Cellar/tune-server/0.9.71/bin/tune-server",
                "/usr/local",
            ),
            (
                "/home/linuxbrew/.linuxbrew/Cellar/tune-server/0.9.113_1/bin/tune-server",
                "/home/linuxbrew/.linuxbrew",
            ),
        ] {
            assert_eq!(
                homebrew_prefix(Path::new(chemin)),
                Some(PathBuf::from(attendu)),
                "préfixe non déduit pour {chemin}"
            );
        }
        // Une installation autonome n'a pas de préfixe Homebrew : on ne doit
        // surtout pas inventer `/Applications/bin/brew`.
        assert_eq!(
            homebrew_prefix(Path::new("/Applications/Tune/tune-server")),
            None
        );
    }

    #[test]
    fn l_etat_se_pose_a_cote_de_la_base_jamais_dans_le_cellar() {
        assert_eq!(
            homebrew_state_dir("/Users/yves/Library/Application Support/Tune/tune.db"),
            PathBuf::from("/Users/yves/Library/Application Support/Tune")
        );
        // `db_path` sans répertoire : le répertoire courant, jamais une chaîne
        // vide qui donnerait un chemin absolu inattendu.
        assert_eq!(homebrew_state_dir("tune.db"), PathBuf::from("."));
    }

    #[cfg(unix)]
    #[test]
    fn le_plan_nomme_le_brew_du_prefixe_qui_possede_l_installation() {
        let bac = bac("plan");
        let (prefix, installation) = faux_prefixe(&bac, "#!/bin/sh\nexit 0\n");
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();

        let plan = homebrew_upgrade_plan(&installation, &etat).expect("plan attendu");
        assert_eq!(plan.brew, prefix.join("bin/brew"));
        assert_eq!(
            plan.launcher,
            prefix.join("opt/tune-server/bin/tune-server-launcher")
        );
        assert_eq!(plan.state_file.parent().unwrap(), etat);
    }

    #[cfg(unix)]
    #[test]
    fn sans_brew_le_plan_refuse_et_nomme_le_chemin_cherche() {
        let bac = bac("sansbrew");
        let (prefix, installation) = faux_prefixe(&bac, "#!/bin/sh\nexit 0\n");
        std::fs::remove_file(prefix.join("bin/brew")).unwrap();
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();

        match homebrew_upgrade_plan(&installation, &etat) {
            Err(HomebrewUpgradeBlock::BrewMissing(chemin)) => {
                assert_eq!(chemin, prefix.join("bin/brew"));
            }
            autre => panic!("attendu BrewMissing, obtenu {autre:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn un_brew_non_executable_ne_compte_pas() {
        let bac = bac("nonexec");
        let (prefix, installation) = faux_prefixe(&bac, "#!/bin/sh\nexit 0\n");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            prefix.join("bin/brew"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();

        assert!(matches!(
            homebrew_upgrade_plan(&installation, &etat),
            Err(HomebrewUpgradeBlock::BrewMissing(_))
        ));
    }

    /// Root passe tous les tests d'écriture — c'est précisément pourquoi il
    /// lui faut un garde à lui. Le test ne s'exécute que SI la suite tourne en
    /// root ; sinon il vérifie l'autre moitié : hors root, ce n'est jamais ce
    /// motif-là qui est rendu.
    #[cfg(unix)]
    #[test]
    fn root_est_refuse_parce_que_brew_refuse_root() {
        let bac = bac("root");
        let (_prefix, installation) = faux_prefixe(&bac, "#!/bin/sh\nexit 0\n");
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();
        let verdict = homebrew_upgrade_plan(&installation, &etat);
        // SAFETY: lecture d'un identifiant du processus.
        if unsafe { libc::geteuid() } == 0 {
            assert!(matches!(verdict, Err(HomebrewUpgradeBlock::RunningAsRoot)));
        } else {
            assert!(!matches!(verdict, Err(HomebrewUpgradeBlock::RunningAsRoot)));
        }
    }

    #[test]
    fn le_refus_dit_a_l_ecran_s_il_peut_offrir_le_bouton() {
        let installation = HomebrewInstallation {
            executable: "/opt/homebrew/Cellar/tune-server/0.9.71/bin/tune-server".into(),
            cellar_version: "0.9.71".into(),
        };

        let sans_motif = homebrew_update_refusal(&installation, "0.9.110", None);
        assert!(sans_motif["upgrade_in_place_blocked_reason"].is_null());
        assert!(sans_motif["upgrade_in_place_detail"].is_null());

        let bloque = homebrew_update_refusal(
            &installation,
            "0.9.110",
            Some(&HomebrewUpgradeBlock::BrewMissing(
                "/opt/homebrew/bin/brew".into(),
            )),
        );
        assert_eq!(
            bloque["upgrade_in_place_blocked_reason"],
            "homebrew_brew_missing"
        );
        assert!(
            bloque["upgrade_in_place_detail"]
                .as_str()
                .unwrap()
                .contains("/opt/homebrew/bin/brew"),
            "le détail doit nommer le chemin cherché : {bloque}"
        );
        // La commande manuelle reste rendue dans les DEUX cas : c'est la sortie
        // de secours de l'utilisateur.
        assert_eq!(sans_motif["command"], super::HOMEBREW_UPDATE_COMMAND);
        assert_eq!(bloque["command"], super::HOMEBREW_UPDATE_COMMAND);
    }

    /// Attend qu'une condition devienne vraie, au plus `limite` dixièmes de
    /// seconde. Un script détaché n'est pas synchrone avec le test.
    /// Un processus jetable, dont ce témoin est seul propriétaire.
    ///
    /// Le script termine le PID qu'on lui nomme dès qu'il emprunte le repli
    /// « hors service ». Un témoin qui lui passerait `std::process::id()` se
    /// ferait donc abattre par le chemin même qu'il mesure — vécu en
    /// contre-épreuve : la suite s'est arrêtée au milieu, sans ligne de
    /// résultat.
    #[cfg(unix)]
    fn processus_sacrificiel() -> std::process::Child {
        std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 60"])
            .spawn()
            .expect("processus témoin")
    }

    #[cfg(unix)]
    fn patienter(limite: u32, mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..limite {
            if condition() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        condition()
    }

    /// **La preuve du chemin nominal.** Le script produit est réellement
    /// exécuté contre un `brew` factice qui journalise ses arguments et déclare
    /// le service démarré. On vérifie ce que `brew` a REÇU et où le fichier
    /// d'état a fini.
    #[cfg(unix)]
    #[test]
    fn le_script_conduit_brew_puis_redemarre_le_service() {
        let bac = bac("script");
        let trace = bac.path().join("brew-args.txt");
        let brew = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nif [ \"$1\" = services ] && [ \"$2\" = list ]; then\n  echo 'tune-server  started  yves  /x/y.plist'\nfi\nexit 0\n",
            trace.display()
        );
        let (_prefix, installation) = faux_prefixe(&bac, &brew);
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();
        let plan = homebrew_upgrade_plan(&installation, &etat).expect("plan attendu");

        let mut jetable = processus_sacrificiel();
        let script = homebrew_upgrade_script(&plan, jetable.id());
        std::fs::write(&plan.script_file, &script).unwrap();
        let sortie = std::process::Command::new("/bin/sh")
            .arg(&plan.script_file)
            .output()
            .expect("script exécutable");
        assert!(
            sortie.status.success(),
            "le script a échoué : {}",
            String::from_utf8_lossy(&sortie.stderr)
        );

        let recu = std::fs::read_to_string(&trace).unwrap();
        assert!(recu.contains("update\n"), "brew update non appelé : {recu}");
        assert!(
            recu.contains("upgrade tune-server"),
            "brew upgrade tune-server non appelé : {recu}"
        );
        assert!(
            recu.contains("services restart tune-server"),
            "le service démarré doit être redémarré : {recu}"
        );

        let fin = homebrew_upgrade_state(&etat).expect("fichier d'état attendu");
        assert_eq!(fin["phase"], "done");
        assert_eq!(fin["exit_code"], 0);
        let _ = jetable.kill();
        let _ = jetable.wait();
    }

    /// **La preuve du chemin d'échec.** `brew upgrade` sort en erreur : le
    /// fichier d'état doit NOMMER l'étape et porter le code, et le service ne
    /// doit surtout pas être redémarré sur une mise à jour qui n'a pas eu lieu.
    #[cfg(unix)]
    #[test]
    fn un_upgrade_en_echec_s_arrete_et_se_nomme() {
        let bac = bac("echec");
        let trace = bac.path().join("brew-args.txt");
        let brew = format!(
            "#!/bin/sh\necho \"$@\" >> '{}'\nif [ \"$1\" = upgrade ]; then exit 7; fi\nexit 0\n",
            trace.display()
        );
        let (_prefix, installation) = faux_prefixe(&bac, &brew);
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();
        let plan = homebrew_upgrade_plan(&installation, &etat).expect("plan attendu");

        let mut jetable = processus_sacrificiel();
        std::fs::write(
            &plan.script_file,
            homebrew_upgrade_script(&plan, jetable.id()),
        )
        .unwrap();
        let sortie = std::process::Command::new("/bin/sh")
            .arg(&plan.script_file)
            .output()
            .unwrap();
        assert!(
            !sortie.status.success(),
            "un échec de brew doit sortir non nul"
        );

        let fin = homebrew_upgrade_state(&etat).expect("fichier d'état attendu");
        assert_eq!(fin["phase"], "failed_brew_upgrade");
        assert_eq!(fin["exit_code"], 7);
        let recu = std::fs::read_to_string(&trace).unwrap();
        assert!(
            !recu.contains("services restart"),
            "rien ne doit être redémarré après un upgrade en échec : {recu}"
        );
        // Le script s'est arrêté AVANT le repli, donc le processus nommé vit
        // encore. C'est la seconde moitié de « il s'arrête » : sans elle, un
        // script qui poursuivrait jusqu'au `kill` passerait pour correct.
        assert!(
            jetable.try_wait().unwrap().is_none(),
            "un upgrade en échec ne doit arrêter AUCUN processus"
        );
        let _ = jetable.kill();
        let _ = jetable.wait();
    }

    /// **La preuve du repli hors `brew services`.** Quand le serveur n'a pas
    /// été lancé comme service, `brew services restart` en démarrerait un
    /// SECOND à côté. Le script doit alors arrêter le processus nommé et
    /// relancer le lanceur du keg.
    #[cfg(unix)]
    #[test]
    fn hors_service_le_script_arrete_le_serveur_et_relance_le_lanceur() {
        let bac = bac("repli");
        let trace = bac.path().join("brew-args.txt");
        // `services list` ne montre RIEN : le serveur n'est pas un service.
        let brew = format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", trace.display());
        let (prefix, installation) = faux_prefixe(&bac, &brew);
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();
        let plan = homebrew_upgrade_plan(&installation, &etat).expect("plan attendu");

        let temoin = bac.path().join("lanceur-appele.txt");
        ecrire_executable(
            &prefix.join("opt/tune-server/bin/tune-server-launcher"),
            &format!("#!/bin/sh\ntouch '{}'\n", temoin.display()),
        );

        // Un vrai processus à arrêter, pour ne pas mesurer un `kill` dans le vide.
        let mut victime = std::process::Command::new("/bin/sh")
            .args(["-c", "sleep 60"])
            .spawn()
            .unwrap();
        let pid = victime.id();

        std::fs::write(&plan.script_file, homebrew_upgrade_script(&plan, pid)).unwrap();
        let sortie = std::process::Command::new("/bin/sh")
            .arg(&plan.script_file)
            .output()
            .unwrap();
        assert!(sortie.status.success());

        assert!(
            patienter(50, || temoin.exists()),
            "le lanceur du keg n'a pas été relancé"
        );
        let recu = std::fs::read_to_string(&trace).unwrap();
        assert!(
            !recu.contains("services restart"),
            "hors service, aucun `brew services restart` ne doit partir : {recu}"
        );
        assert_eq!(homebrew_upgrade_state(&etat).unwrap()["phase"], "done");
        // Le processus visé a bien été arrêté.
        let _ = victime.wait();
    }

    /// **La preuve qu'on ne coupe rien sans pouvoir relancer.** Le lanceur du
    /// keg est absent : le script doit s'arrêter AVANT le `kill`, laisser le
    /// serveur en vie, et nommer ce qui manque. Un lanceur manquant qui
    /// laisserait Tune éteint serait strictement pire que le refus remplacé.
    #[cfg(unix)]
    #[test]
    fn sans_lanceur_le_script_n_arrete_pas_le_serveur() {
        let bac = bac("sanslanceur");
        let trace = bac.path().join("brew-args.txt");
        // `services list` ne montre rien : on ira vers le repli.
        let brew = format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 0\n", trace.display());
        let (_prefix, installation) = faux_prefixe(&bac, &brew);
        let etat = bac.path().join("data");
        std::fs::create_dir_all(&etat).unwrap();
        let plan = homebrew_upgrade_plan(&installation, &etat).expect("plan attendu");
        // Le lanceur n'est volontairement PAS créé.
        assert!(!plan.launcher.exists());

        let mut jetable = processus_sacrificiel();
        std::fs::write(
            &plan.script_file,
            homebrew_upgrade_script(&plan, jetable.id()),
        )
        .unwrap();
        let sortie = std::process::Command::new("/bin/sh")
            .arg(&plan.script_file)
            .output()
            .unwrap();
        assert!(
            !sortie.status.success(),
            "il manque quelque chose : sortie non nulle"
        );

        assert_eq!(
            homebrew_upgrade_state(&etat).unwrap()["phase"],
            "failed_no_launcher"
        );
        assert!(
            jetable.try_wait().unwrap().is_none(),
            "le serveur ne doit PAS être arrêté quand rien ne peut le relancer"
        );
        let _ = jetable.kill();
        let _ = jetable.wait();
    }

    /// Le plan et le script ne portent AUCUNE entrée de l'appelant HTTP : tout
    /// ce qui est interpolé vient de `current_exe` et de `db_path`.
    #[cfg(unix)]
    #[test]
    fn le_script_n_interpole_que_des_chemins_mesures() {
        let plan = HomebrewUpgradePlan {
            brew: "/opt/homebrew/bin/brew".into(),
            launcher: "/opt/homebrew/opt/tune-server/bin/tune-server-launcher".into(),
            state_file: "/data/tune-homebrew-upgrade.json".into(),
            log_file: "/data/tune-homebrew-upgrade.log".into(),
            script_file: "/data/tune-homebrew-upgrade.sh".into(),
        };
        let script = homebrew_upgrade_script(&plan, 4242);
        assert!(script.contains("BREW='/opt/homebrew/bin/brew'"));
        assert!(script.contains("SRV_PID=4242"));
        // Le nom de la formule est une constante, jamais une variable de shell
        // qui pourrait porter autre chose.
        assert!(script.contains("\"$BREW\" upgrade tune-server"));
        assert!(script.contains("NONINTERACTIVE=1"));
    }
}
