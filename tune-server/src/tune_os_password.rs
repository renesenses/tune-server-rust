//! One-shot migration of the historical Tune OS `tune` / `tune` SSH account.
//!
//! Updating an appliance replaces only the server binary and web assets.  A
//! change confined to the image builders would therefore leave every existing
//! machine exposed.  The Linux binary embeds the same audited script installed
//! by new images and runs its conservative migration once: only a shadow hash
//! that still verifies against the historical public password is rotated.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

const PASSWORD_SCRIPT: &str = include_str!("../../image/tune-os-password.sh");
const APPLIANCE_MARKER: &str = "/etc/tune-appliance";
const MOTD: &str = "/etc/motd";

fn looks_like_tune_os(appliance_marker: &Path, motd: &Path) -> bool {
    appliance_marker.is_file()
        || std::fs::read_to_string(motd)
            .map(|text| text.contains("Tune OS v"))
            .unwrap_or(false)
}

fn migration_command(euid: u32) -> Command {
    if euid == 0 {
        let mut command = Command::new("/bin/bash");
        command.args(["-s", "--", "--migrate-legacy"]);
        command
    } else {
        // The historical RPi image runs tune-server as `tune`; that image also
        // grants this account NOPASSWD sudo. `-n` is essential: startup must
        // fail visibly, never hang on an impossible password prompt.
        let mut command = Command::new("/usr/bin/sudo");
        command.args(["-n", "/bin/bash", "-s", "--", "--migrate-legacy"]);
        command
    }
}

pub(crate) fn migrate_legacy_password() {
    if !looks_like_tune_os(Path::new(APPLIANCE_MARKER), Path::new(MOTD)) {
        return;
    }

    let euid = unsafe { libc::geteuid() };
    let mut command = migration_command(euid);
    let mut child = match command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(%error, "tune_os_ssh_password_migration_not_started");
            return;
        }
    };

    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::other("migration stdin unavailable"))
        .and_then(|mut stdin| stdin.write_all(PASSWORD_SCRIPT.as_bytes()));
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        tracing::warn!(%error, "tune_os_ssh_password_migration_script_not_sent");
        return;
    }

    match child.wait_with_output() {
        Ok(output) if output.status.success() => {
            tracing::info!("tune_os_ssh_password_policy_checked");
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
            tracing::warn!(%error, "tune_os_ssh_password_migration_wait_failed");
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
    fn embedded_policy_never_reintroduces_the_public_password() {
        assert!(PASSWORD_SCRIPT.contains("password_matches_legacy"));
        assert!(PASSWORD_SCRIPT.contains("chage -d 0"));
        assert!(PASSWORD_SCRIPT.contains("openssl rand -hex 12"));
        assert!(!PASSWORD_SCRIPT.contains("tune:tune"));
    }
}
