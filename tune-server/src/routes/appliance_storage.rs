//! Appliance mode: relocate Tune data (SQLite DB + artwork cache) to an
//! external volume — see docs/DATA-RELOCATION.md.
//!
//! Gated like the rest of /appliance (marker file / TUNE_APPLIANCE). The
//! target volume is mounted by UUID through a systemd mount unit on a stable
//! path (default /srv/tune-data) so device renames (sda→sdb) never break the
//! config. The job ends in "restart_required": the client then calls the
//! existing POST /system/restart.
//!
//! External tools are overridable for tests (same pattern as TUNE_NMCLI_BIN):
//! TUNE_BLKID_BIN, TUNE_DF_BIN, TUNE_SYSTEMCTL_BIN, TUNE_PROC_MOUNTS,
//! TUNE_MOUNT_UNIT_DIR, TUNE_DATA_MOUNT_POINT, TUNE_CONFIG_PATH.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::process::Command;

use crate::error::AppError;
use crate::state::AppState;

const CMD_TIMEOUT: Duration = Duration::from_secs(20);
const DATA_SUBDIR: &str = "TuneData";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/storage", get(list_storage))
        .route("/data/status", get(data_status))
        .route("/data/relocate", post(relocate))
}

fn require_appliance() -> Result<(), AppError> {
    if super::appliance::is_appliance() {
        Ok(())
    } else {
        Err(AppError::not_found("appliance mode not active"))
    }
}

fn env_or(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.into())
}

fn data_mount_point() -> String {
    env_or("TUNE_DATA_MOUNT_POINT", "/srv/tune-data")
}

/// systemd unit name for a mount path: `/` → `-`, other specials hex-escaped.
/// We only need the subset systemd-escape applies to our fixed default
/// (`/srv/tune-data` → `srv-tune\x2ddata.mount`).
fn mount_unit_name(mount_point: &str) -> String {
    let trimmed = mount_point.trim_matches('/');
    let mut out = String::new();
    for c in trimmed.chars() {
        match c {
            '/' => out.push('-'),
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' => out.push(c),
            other => {
                for b in other.to_string().as_bytes() {
                    out.push_str(&format!("\\x{b:02x}"));
                }
            }
        }
    }
    format!("{out}.mount")
}

async fn run_tool(bin_env: &str, default: &str, args: &[&str]) -> Result<String, String> {
    let bin = env_or(bin_env, default);
    match tokio::time::timeout(CMD_TIMEOUT, Command::new(&bin).args(args).output()).await {
        Ok(Ok(out)) if out.status.success() => Ok(String::from_utf8_lossy(&out.stdout).to_string()),
        Ok(Ok(out)) => Err(format!(
            "{bin} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Err(e)) => Err(format!("{bin}: {e}")),
        Err(_) => Err(format!("{bin}: timeout")),
    }
}

// ---------- mounts / df / blkid parsing (pure, unit-tested) ----------

#[derive(Debug, Clone)]
struct MountEntry {
    device: String,
    mount_path: String,
    fs: String,
}

fn parse_mounts(raw: &str) -> Vec<MountEntry> {
    const FS_OK: &[&str] = &[
        "ext2", "ext3", "ext4", "xfs", "btrfs", "vfat", "exfat", "ntfs", "ntfs3", "fuseblk",
    ];
    raw.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 3 {
                return None;
            }
            let (device, mount_path, fs) = (f[0], f[1], f[2]);
            if !device.starts_with("/dev/")
                || !FS_OK.contains(&fs)
                || mount_path == "/"
                || mount_path.starts_with("/boot")
            {
                return None;
            }
            Some(MountEntry {
                device: device.into(),
                // /proc/mounts octal-escapes spaces as \040
                mount_path: mount_path.replace("\\040", " "),
                fs: fs.into(),
            })
        })
        .collect()
}

/// Parse `df -Pk <path>` output → (size_bytes, free_bytes).
fn parse_df(raw: &str) -> Option<(u64, u64)> {
    let line = raw.lines().nth(1)?;
    let f: Vec<&str> = line.split_whitespace().collect();
    let size_k: u64 = f.get(1)?.parse().ok()?;
    let avail_k: u64 = f.get(3)?.parse().ok()?;
    Some((size_k * 1024, avail_k * 1024))
}

/// Parse `blkid -o export <device>` → (uuid, label).
fn parse_blkid_export(raw: &str) -> (Option<String>, Option<String>) {
    let mut uuid = None;
    let mut label = None;
    for line in raw.lines() {
        if let Some(v) = line.strip_prefix("UUID=") {
            uuid = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("LABEL=") {
            label = Some(v.trim().to_string());
        }
    }
    (uuid, label)
}

async fn candidate_volumes() -> Result<Vec<Value>, AppError> {
    let mounts_path = env_or("TUNE_PROC_MOUNTS", "/proc/mounts");
    let raw = std::fs::read_to_string(&mounts_path)
        .map_err(|e| AppError::internal(format!("read {mounts_path}: {e}")))?;
    let mut seen_devices = std::collections::HashSet::new();
    let mut out = Vec::new();
    for m in parse_mounts(&raw) {
        if !seen_devices.insert(m.device.clone()) {
            continue;
        }
        let (size, free) = run_tool("TUNE_DF_BIN", "df", &["-Pk", &m.mount_path])
            .await
            .ok()
            .and_then(|o| parse_df(&o))
            .unwrap_or((0, 0));
        let (uuid, label) = run_tool("TUNE_BLKID_BIN", "blkid", &["-o", "export", &m.device])
            .await
            .map(|o| parse_blkid_export(&o))
            .unwrap_or((None, None));
        out.push(json!({
            "device": m.device,
            "mount_path": m.mount_path,
            "fs": m.fs,
            "size_bytes": size,
            "free_bytes": free,
            "uuid": uuid,
            "label": label,
        }));
    }
    Ok(out)
}

// ---------- relocation job state ----------

#[derive(Debug, Clone, Default)]
struct RelocJob {
    phase: String, // "", preparing, mounting, copying, verifying, switching, done, failed
    copied_bytes: u64,
    total_bytes: u64,
    error: Option<String>,
    target: String,
}

fn job() -> &'static Mutex<RelocJob> {
    static JOB: OnceLock<Mutex<RelocJob>> = OnceLock::new();
    JOB.get_or_init(|| Mutex::new(RelocJob::default()))
}

fn set_phase(phase: &str) {
    let mut j = job().lock().unwrap();
    j.phase = phase.into();
}

fn fail_job(err: String) {
    tracing::warn!(error = %err, "data_relocation_failed");
    let mut j = job().lock().unwrap();
    j.phase = "failed".into();
    j.error = Some(err);
}

// ---------- helpers ----------

fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(path) {
        for e in rd.flatten() {
            if let Ok(md) = e.metadata() {
                if md.is_dir() {
                    total += dir_size(&e.path());
                } else {
                    total += md.len();
                }
            }
        }
    }
    total
}

fn copy_dir_with_progress(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    let rd = std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    for e in rd.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        let md = e.metadata().map_err(|e| e.to_string())?;
        if md.is_dir() {
            copy_dir_with_progress(&from, &to)?;
        } else {
            let n =
                std::fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
            let mut j = job().lock().unwrap();
            j.copied_bytes += n;
        }
    }
    Ok(())
}

/// Locate the loaded config file, mirroring config.rs search order.
fn config_file_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("TUNE_CONFIG_PATH") {
        return Some(PathBuf::from(p));
    }
    for p in ["tune.toml", "/etc/tune/tune.toml"] {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Some(pb);
        }
    }
    None
}

/// Line-based rewrite that preserves comments/unknown keys: replaces (or
/// appends) top-level `db_path` and `artwork_dir` assignments.
fn rewrite_config(contents: &str, db_path: &str, artwork_dir: &str) -> String {
    let mut out = Vec::new();
    let mut seen_db = false;
    let mut seen_art = false;
    for line in contents.lines() {
        let t = line.trim_start();
        if t.starts_with("db_path") && t[7..].trim_start().starts_with('=') {
            out.push(format!("db_path = \"{db_path}\""));
            seen_db = true;
        } else if t.starts_with("artwork_dir") && t[11..].trim_start().starts_with('=') {
            out.push(format!("artwork_dir = \"{artwork_dir}\""));
            seen_art = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !seen_db {
        out.push(format!("db_path = \"{db_path}\""));
    }
    if !seen_art {
        out.push(format!("artwork_dir = \"{artwork_dir}\""));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn mount_unit_contents(uuid: &str, mount_point: &str) -> String {
    format!(
        "# Tune OS — volume de données (généré par la relocalisation)\n\
         [Unit]\n\
         Description=Tune data volume\n\n\
         [Mount]\n\
         What=/dev/disk/by-uuid/{uuid}\n\
         Where={mount_point}\n\
         Options=nofail\n\
         TimeoutSec=15\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

/// Snapshot cohérent d'une base vivante via `VACUUM INTO` (SQLite ≥ 3.27) —
/// pas besoin de fermer le pool, et la copie est compactée au passage.
fn sqlite_backup(src: &Path, dst: &Path) -> Result<(), String> {
    if dst.exists() {
        std::fs::remove_file(dst).map_err(|e| format!("rm stale {}: {e}", dst.display()))?;
    }
    let src_conn = rusqlite::Connection::open(src).map_err(|e| format!("open src: {e}"))?;
    src_conn
        .execute("VACUUM INTO ?1", [dst.to_string_lossy().as_ref()])
        .map_err(|e| format!("vacuum into: {e}"))?;
    Ok(())
}

fn sqlite_integrity_ok(path: &Path) -> Result<bool, String> {
    let conn = rusqlite::Connection::open(path).map_err(|e| e.to_string())?;
    let res: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    Ok(res == "ok")
}

// ---------- handlers ----------

async fn list_storage(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    let db_path = state.config.db_path.clone();
    let mut vols = candidate_volumes().await?;
    for v in &mut vols {
        let mp = v["mount_path"].as_str().unwrap_or("").to_string();
        v["is_data_target"] = json!(!mp.is_empty() && db_path.starts_with(&mp));
    }
    Ok(Json(json!({ "volumes": vols })))
}

async fn data_status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    let db_path = PathBuf::from(&state.config.db_path);
    let artwork_dir = PathBuf::from(&state.config.artwork_dir);
    let mount_point = data_mount_point();
    let on_external = state.config.db_path.starts_with(&mount_point);
    let volume_present = !on_external || db_path.exists();
    let data_size = db_path.metadata().map(|m| m.len()).unwrap_or(0) + dir_size(&artwork_dir);
    let j = job().lock().unwrap().clone();
    Ok(Json(json!({
        "db_path": state.config.db_path,
        "artwork_dir": state.config.artwork_dir,
        "on_external": on_external,
        "volume_present": volume_present,
        "data_size_bytes": data_size,
        "job": if j.phase.is_empty() { Value::Null } else { json!({
            "phase": j.phase,
            "copied_bytes": j.copied_bytes,
            "total_bytes": j.total_bytes,
            "error": j.error,
            "target": j.target,
        }) },
    })))
}

#[derive(Deserialize)]
struct RelocateBody {
    uuid: String,
}

async fn relocate(
    State(state): State<AppState>,
    Json(body): Json<RelocateBody>,
) -> Result<Json<Value>, AppError> {
    require_appliance()?;
    if state.backend.engine() != tune_core::db::engine::Engine::Sqlite {
        return Err(AppError::bad_request(
            "data relocation requires the SQLite engine",
        ));
    }
    {
        let j = job().lock().unwrap();
        if matches!(
            j.phase.as_str(),
            "preparing" | "mounting" | "copying" | "verifying" | "switching"
        ) {
            return Err(AppError::conflict("a relocation job is already running"));
        }
    }
    // Resolve the target volume by UUID.
    let vols = candidate_volumes().await?;
    let vol = vols
        .iter()
        .find(|v| v["uuid"].as_str() == Some(body.uuid.as_str()))
        .cloned()
        .ok_or_else(|| AppError::bad_request("unknown volume uuid"))?;

    let uuid = body.uuid.clone();
    let src_db = PathBuf::from(state.config.db_path.clone());
    let src_art = PathBuf::from(state.config.artwork_dir.clone());
    let mount_point = data_mount_point();
    let free = vol["free_bytes"].as_u64().unwrap_or(0);
    let total = src_db.metadata().map(|m| m.len()).unwrap_or(0) + dir_size(&src_art);
    if free > 0 && (free as f64) < (total as f64) * 1.2 {
        return Err(AppError::bad_request(format!(
            "not enough free space on target ({free} B free, {total} B needed +20%)"
        )));
    }

    {
        let mut j = job().lock().unwrap();
        *j = RelocJob {
            phase: "preparing".into(),
            total_bytes: total,
            target: mount_point.clone(),
            ..Default::default()
        };
    }

    tokio::spawn(async move {
        // 1) systemd mount unit by UUID → stable path, then mount now.
        set_phase("mounting");
        let unit_dir = env_or("TUNE_MOUNT_UNIT_DIR", "/etc/systemd/system");
        let unit_name = mount_unit_name(&mount_point);
        let unit_path = Path::new(&unit_dir).join(&unit_name);
        if let Err(e) = std::fs::write(&unit_path, mount_unit_contents(&uuid, &mount_point)) {
            return fail_job(format!("write {}: {e}", unit_path.display()));
        }
        for args in [
            vec!["daemon-reload"],
            vec!["enable", "--now", unit_name.as_str()],
        ] {
            if let Err(e) = run_tool("TUNE_SYSTEMCTL_BIN", "systemctl", &args).await {
                return fail_job(e);
            }
        }

        let target_root = Path::new(&mount_point).join(DATA_SUBDIR);
        let target_db = target_root.join("tune.db");
        let target_art = target_root.join("artwork_cache");

        // 2) copy artwork with progress, then a consistent DB snapshot.
        set_phase("copying");
        if src_art.exists() {
            if let Err(e) = copy_dir_with_progress(&src_art, &target_art) {
                let _ = std::fs::remove_dir_all(&target_root);
                return fail_job(e);
            }
        } else if let Err(e) = std::fs::create_dir_all(&target_art) {
            return fail_job(format!("mkdir {}: {e}", target_art.display()));
        }
        if let Err(e) = std::fs::create_dir_all(&target_root) {
            return fail_job(format!("mkdir {}: {e}", target_root.display()));
        }
        // rusqlite is blocking — run the snapshot off the async runtime.
        let (src_db2, target_db2) = (src_db.clone(), target_db.clone());
        let res = tokio::task::spawn_blocking(move || sqlite_backup(&src_db2, &target_db2)).await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&target_root);
                return fail_job(e);
            }
            Err(e) => return fail_job(format!("join: {e}")),
        }

        // 3) verify the copied DB before switching anything.
        set_phase("verifying");
        let target_db3 = target_db.clone();
        match tokio::task::spawn_blocking(move || sqlite_integrity_ok(&target_db3)).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => {
                let _ = std::fs::remove_dir_all(&target_root);
                return fail_job("integrity_check failed on copied database".into());
            }
            Ok(Err(e)) => {
                let _ = std::fs::remove_dir_all(&target_root);
                return fail_job(format!("integrity_check error: {e}"));
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&target_root);
                return fail_job(format!("join: {e}"));
            }
        }

        // 4) switch the config — nothing changed until this point.
        set_phase("switching");
        let Some(cfg_path) = config_file_path() else {
            return fail_job("tune.toml not found".into());
        };
        let contents = match std::fs::read_to_string(&cfg_path) {
            Ok(c) => c,
            Err(e) => return fail_job(format!("read {}: {e}", cfg_path.display())),
        };
        let new_contents = rewrite_config(
            &contents,
            &target_db.to_string_lossy(),
            &target_art.to_string_lossy(),
        );
        if let Err(e) = std::fs::write(&cfg_path, new_contents) {
            return fail_job(format!("write {}: {e}", cfg_path.display()));
        }

        tracing::info!(target = %target_root.display(), "data_relocation_done");
        set_phase("done");
    });

    Ok(Json(json!({ "started": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_unit_name_escapes_dashes() {
        assert_eq!(mount_unit_name("/srv/tune-data"), "srv-tune\\x2ddata.mount");
        assert_eq!(mount_unit_name("/mnt/data"), "mnt-data.mount");
    }

    #[test]
    fn parse_mounts_filters_system_paths() {
        let raw = "\
/dev/sda2 / ext4 rw 0 0
/dev/sda1 /boot/efi vfat rw 0 0
/dev/sdb1 /media/sdb1 exfat rw 0 0
tmpfs /run tmpfs rw 0 0
//nas/music /mnt/music cifs rw 0 0
/dev/sdc1 /media/My\\040Disk ntfs3 rw 0 0
";
        let m = parse_mounts(raw);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].mount_path, "/media/sdb1");
        assert_eq!(m[1].mount_path, "/media/My Disk");
    }

    #[test]
    fn parse_df_extracts_sizes() {
        let raw = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/sdb1 1953480700 1863480700 90000000 96% /media/sdb1\n";
        let (size, free) = parse_df(raw).unwrap();
        assert_eq!(size, 1953480700 * 1024);
        assert_eq!(free, 90000000 * 1024);
    }

    #[test]
    fn parse_blkid_reads_uuid_and_label() {
        let raw = "DEVNAME=/dev/sdb1\nUUID=A1B2-C3D4\nLABEL=DSD2TO\nTYPE=exfat\n";
        let (uuid, label) = parse_blkid_export(raw);
        assert_eq!(uuid.as_deref(), Some("A1B2-C3D4"));
        assert_eq!(label.as_deref(), Some("DSD2TO"));
    }

    #[test]
    fn rewrite_config_replaces_and_appends() {
        let src = "# conf\nport = 8888\ndb_path = \"old.db\"\nmusic_dirs = [\"/m\"]\n";
        let out = rewrite_config(
            src,
            "/srv/tune-data/TuneData/tune.db",
            "/srv/tune-data/TuneData/artwork_cache",
        );
        assert!(out.contains("db_path = \"/srv/tune-data/TuneData/tune.db\""));
        assert!(out.contains("artwork_dir = \"/srv/tune-data/TuneData/artwork_cache\""));
        assert!(out.contains("# conf"));
        assert!(out.contains("port = 8888"));
        assert!(!out.contains("old.db"));
    }

    #[test]
    fn mount_unit_contents_uses_uuid() {
        let u = mount_unit_contents("A1B2-C3D4", "/srv/tune-data");
        assert!(u.contains("What=/dev/disk/by-uuid/A1B2-C3D4"));
        assert!(u.contains("Where=/srv/tune-data"));
        assert!(u.contains("Options=nofail"));
    }
}

/// Boot guard (docs/DATA-RELOCATION.md) : si la config pointe sous le volume
/// de données et que celui-ci est absent, on attend — jamais de démarrage
/// silencieux sur une base vide. Tente un `systemctl start` de l'unité à
/// chaque itération (disque branché après coup).
pub async fn wait_for_data_volume(db_path: &str) {
    let mount_point = data_mount_point();
    if !super::appliance::is_appliance() || !db_path.starts_with(mount_point.as_str()) {
        return;
    }
    let unit = mount_unit_name(&mount_point);
    let mut attempt = 0u32;
    while !Path::new(db_path).exists() {
        attempt += 1;
        tracing::warn!(
            attempt,
            mount_point = %mount_point,
            "volume de données absent — en attente (branchez le disque Tune)"
        );
        let _ = run_tool("TUNE_SYSTEMCTL_BIN", "systemctl", &["start", &unit]).await;
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
    if attempt > 0 {
        tracing::info!("volume de données présent — démarrage");
    }
}
