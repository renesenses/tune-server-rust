//! One-shot migration of the historical Tune OS `tune` / `tune` SSH account.
//!
//! Updating an appliance replaces only the server binary and web assets.  A
//! change confined to the image builders would therefore leave every existing
//! machine exposed.  The Linux binary embeds the same audited script installed
//! by new images and runs its conservative migration once: only a shadow hash
//! that still verifies against the historical public password is rotated.
//!
//! `tune-server --tune-os-premier-acces` (#5617) runs the same policy as a
//! one-shot command and exits before any server state exists.  An image calls
//! it as root from a unit of its own, outside the `ProtectSystem=strict`
//! sandbox of `tune.service`, where `/etc` and `/run` are read-only: rotation,
//! acknowledgement of a changed password, and publication of a still-due
//! temporary password in `/run/tune/premier-mot-de-passe` for the console and
//! Cockpit screens.  The password itself never transits through this process.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const PASSWORD_SCRIPT: &str = include_str!("../../image/tune-os-password.sh");
const APPLIANCE_MARKER: &str = "/etc/tune-appliance";
const MOTD: &str = "/etc/motd";

/// Command-line flag of the one-shot first-access mode.
pub(crate) const PREMIER_ACCES_FLAG: &str = "--tune-os-premier-acces";

fn looks_like_tune_os(appliance_marker: &Path, motd: &Path) -> bool {
    appliance_marker.is_file()
        || std::fs::read_to_string(motd)
            .map(|text| text.contains("Tune OS v"))
            .unwrap_or(false)
}

fn policy_command(euid: u32, mode: &str) -> Command {
    if euid == 0 {
        let mut command = Command::new("/bin/bash");
        command.args(["-s", "--", mode]);
        command
    } else {
        // The historical RPi image runs tune-server as `tune`; that image also
        // grants this account NOPASSWD sudo. `-n` is essential: startup must
        // fail visibly, never hang on an impossible password prompt.
        let mut command = Command::new("/usr/bin/sudo");
        command.args(["-n", "/bin/bash", "-s", "--", mode]);
        command
    }
}

fn migration_command(euid: u32) -> Command {
    policy_command(euid, "--migrate-legacy")
}

/// Starts `command`, feeds it the embedded policy on stdin and waits.
fn run_policy(mut command: Command, stderr: Stdio) -> std::io::Result<std::process::Output> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()?;
    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("policy stdin unavailable"))
        .and_then(|mut stdin| stdin.write_all(PASSWORD_SCRIPT.as_bytes()));
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    child.wait_with_output()
}

/// Does the command line ask for the one-shot first-access mode?  Exact
/// match, never a prefix, like `--version`.
pub(crate) fn premier_acces_requested<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == PREMIER_ACCES_FLAG)
}

/// One-shot first-access mode; returns the process exit code.
///
/// Root only: unlike the in-process migration, nothing here may fall back to
/// sudo, since the caller is a root unit of the image.  The script's stderr
/// is inherited (it never contains the password); stdout is discarded.
pub(crate) fn run_premier_acces() -> i32 {
    if !looks_like_tune_os(Path::new(APPLIANCE_MARKER), Path::new(MOTD)) {
        eprintln!("tune-server {PREMIER_ACCES_FLAG}: not a Tune OS appliance, nothing to do");
        return 0;
    }
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("tune-server {PREMIER_ACCES_FLAG}: must run as root");
        return 1;
    }
    match run_policy(policy_command(0, "--premier-acces"), Stdio::inherit()) {
        Ok(output) => output.status.code().unwrap_or(1),
        Err(error) => {
            eprintln!("tune-server {PREMIER_ACCES_FLAG}: {error}");
            1
        }
    }
}

pub(crate) fn migrate_legacy_password() {
    if !looks_like_tune_os(Path::new(APPLIANCE_MARKER), Path::new(MOTD)) {
        return;
    }

    let euid = unsafe { libc::geteuid() };
    match run_policy(migration_command(euid), Stdio::piped()) {
        Ok(output) if output.status.success() => {
            tracing::info!("tune_os_ssh_password_policy_checked");
        }
        Ok(output)
            if euid != 0
                && crate::privilege::est_un_refus_d_elevation(&String::from_utf8_lossy(
                    &output.stderr,
                )) =>
        {
            // Tune OS sous `tune` (#3206) : sudoers n'ouvre plus que
            // l'assistant de montage. La politique tourne en root, hors du
            // service, par tune-os-premier-acces : ce refus est attendu.
            tracing::info!("tune_os_ssh_password_policy_left_to_image");
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                status = ?output.status.code(),
                error = %stderr.trim(),
                "tune_os_ssh_password_migration_failed"
            );
        }
        Err(error) => {
            tracing::warn!(%error, "tune_os_ssh_password_migration_not_run");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tune_os_is_identified_by_marker_or_historical_motd() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("tune-appliance");
        let motd = temp.path().join("motd");
        assert!(!looks_like_tune_os(&marker, &motd));

        std::fs::write(&motd, "Tune OS v0.9.12 (Raspberry Pi)\n").unwrap();
        assert!(looks_like_tune_os(&marker, &motd));

        std::fs::write(&motd, "Debian GNU/Linux\n").unwrap();
        std::fs::write(&marker, "Tune OS appliance image\n").unwrap();
        assert!(looks_like_tune_os(&marker, &motd));
    }

    #[test]
    fn migration_uses_root_directly_and_non_root_through_noninteractive_sudo() {
        let root = migration_command(0);
        assert_eq!(root.get_program(), "/bin/bash");
        assert_eq!(
            root.get_args().collect::<Vec<_>>(),
            ["-s", "--", "--migrate-legacy"]
        );

        let user = migration_command(1000);
        assert_eq!(user.get_program(), "/usr/bin/sudo");
        assert_eq!(
            user.get_args().collect::<Vec<_>>(),
            ["-n", "/bin/bash", "-s", "--", "--migrate-legacy"]
        );
    }

    #[test]
    fn premier_acces_flag_is_an_exact_match() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(premier_acces_requested(args(&["--tune-os-premier-acces"])));
        assert!(!premier_acces_requested(args(&[])));
        assert!(!premier_acces_requested(args(&[
            "--tune-os-premier-acces-x"
        ])));
        assert!(!premier_acces_requested(args(&["--tune-os"])));

        let root = policy_command(0, "--premier-acces");
        assert_eq!(root.get_program(), "/bin/bash");
        assert_eq!(
            root.get_args().collect::<Vec<_>>(),
            ["-s", "--", "--premier-acces"]
        );
    }

    #[test]
    fn embedded_policy_publishes_the_runtime_copy_for_root_only() {
        assert!(PASSWORD_SCRIPT.contains("--premier-acces) premier_acces"));
        assert!(
            PASSWORD_SCRIPT.contains(r#"RUNTIME_SECRET="${RUNTIME_DIR}/premier-mot-de-passe""#)
        );
        assert!(PASSWORD_SCRIPT.contains(r#"readonly RUNTIME_DIR="/run/tune""#));
        assert!(PASSWORD_SCRIPT.contains(r#"install -m 0600 "$INITIAL_SECRET""#));
    }

    /// #5617 — the embedded policy, fed on stdin exactly as in production,
    /// must reach `main`. A probe mode stops at its root check or its usage
    /// line, without touching any account.
    #[test]
    fn embedded_policy_reaches_main_when_fed_on_stdin() {
        let mut command = Command::new("/bin/bash");
        command.args(["-s", "--", "--sonde"]);
        let output = run_policy(command, Stdio::piped()).expect("bash lancé");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("unbound variable"),
            "politique arrêtée avant main : {stderr}"
        );
        assert!(
            stderr.contains("exécutée par root") || stderr.contains("usage:"),
            "main jamais atteint : {stderr}"
        );
        assert!(!output.status.success());
    }

    #[test]
    fn embedded_policy_never_reintroduces_the_public_password() {
        assert!(PASSWORD_SCRIPT.contains("password_matches_legacy"));
        assert!(PASSWORD_SCRIPT.contains("chage -d 0"));
        assert!(PASSWORD_SCRIPT.contains("openssl rand -hex 12"));
        assert!(!PASSWORD_SCRIPT.contains("tune:tune"));
    }
}
