pub mod device;
pub mod mac;
pub mod mdns;
pub mod minimal_dmr;
mod oui_audio;
pub mod renderer_identity;
pub mod ssdp;
pub mod xml_parser;

/// Best-effort real system hostname, used for this instance's network identity
/// (mDNS instance name, peer-info "name"). Order:
///   1. an explicit `HOSTNAME` / `COMPUTERNAME` override (non-empty), then
///   2. the OS hostname via `gethostname(2)` (`hostname::get()`),
///   3. `"tune-server"` only as a last resort.
///
/// The old env-only derivation fell straight to `"tune-server"` under systemd
/// (where `HOSTNAME` is unset), so every instance collided on the same mDNS
/// name/host and peer-info name — servers renamed `(2)`, or invisible (#1112,
/// #1127). `gethostname(2)` returns the kernel hostname regardless of env.
///
/// NOTE: deliberately NOT used by the license fingerprint (`license.rs`), which
/// keeps its own stable derivation — shifting it would invalidate live licenses.
pub fn system_hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME").or_else(|_| std::env::var("COMPUTERNAME")) {
        let h = h.trim();
        if !h.is_empty() {
            return h.to_string();
        }
    }
    hostname::get()
        .ok()
        .and_then(|s| s.into_string().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "tune-server".into())
}

/// Sanitize a hostname into a single DNS-safe label for an mDNS host record
/// (`<label>.local.`). Strips a trailing `.local` (macOS reports e.g.
/// `Studio.local`, which would otherwise double to `Studio.local.local.`) and
/// maps any non `[A-Za-z0-9-]` char to `-`. Never empty.
pub fn mdns_host_label(hostname: &str) -> String {
    let base = hostname.trim().trim_end_matches(".local");
    let label: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let label = label.trim_matches('-').to_string();
    if label.is_empty() {
        "tune-server".into()
    } else {
        label
    }
}

#[cfg(test)]
mod hostname_tests {
    use super::*;

    #[test]
    fn system_hostname_never_empty() {
        assert!(!system_hostname().is_empty());
    }

    #[test]
    fn mdns_host_label_strips_local_and_sanitizes() {
        assert_eq!(mdns_host_label("Mac-Studio-6.local"), "Mac-Studio-6");
        assert_eq!(mdns_host_label("Tune Server"), "Tune-Server");
        assert_eq!(mdns_host_label("héllo.local"), "h-llo");
        assert_eq!(mdns_host_label(""), "tune-server");
        assert_eq!(mdns_host_label(".local"), "tune-server");
    }
}
