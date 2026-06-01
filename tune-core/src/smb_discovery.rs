use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tracing::{debug, info};

const DEFAULT_SCAN_INTERVAL: u64 = 60;

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredSmbShare {
    pub host: String,
    pub share_name: String,
    pub host_name: String,
    pub last_seen: f64,
}

impl DiscoveredSmbShare {
    pub fn id(&self) -> String {
        format!("smb://{}/{}", self.host, self.share_name)
    }
}

pub struct SmbAutoDiscovery {
    scan_interval: u64,
    discovered: HashMap<String, DiscoveredSmbShare>,
    known_hosts: Vec<String>,
}

impl SmbAutoDiscovery {
    pub fn new(scan_interval: u64) -> Self {
        Self {
            scan_interval: if scan_interval > 0 {
                scan_interval
            } else {
                DEFAULT_SCAN_INTERVAL
            },
            discovered: HashMap::new(),
            known_hosts: Vec::new(),
        }
    }

    pub fn discovered(&self) -> &HashMap<String, DiscoveredSmbShare> {
        &self.discovered
    }

    pub fn scan_interval(&self) -> u64 {
        self.scan_interval
    }

    pub async fn scan(&mut self) {
        let hosts = discover_smb_hosts().await;
        self.known_hosts = hosts.clone();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        for host in &hosts {
            let shares = enumerate_shares(host).await;
            for share_name in shares {
                let share = DiscoveredSmbShare {
                    host: host.clone(),
                    share_name: share_name.clone(),
                    host_name: String::new(),
                    last_seen: now,
                };
                let key = share.id();
                self.discovered.insert(key, share);
            }
        }

        info!(
            hosts = hosts.len(),
            shares = self.discovered.len(),
            "smb_scan_complete"
        );
    }

    pub fn clear(&mut self) {
        self.discovered.clear();
        self.known_hosts.clear();
    }
}

impl Default for SmbAutoDiscovery {
    fn default() -> Self {
        Self::new(DEFAULT_SCAN_INTERVAL)
    }
}

async fn discover_smb_hosts() -> Vec<String> {
    if cfg!(target_os = "macos") {
        discover_hosts_macos().await
    } else if cfg!(target_os = "linux") {
        discover_hosts_linux().await
    } else if cfg!(target_os = "windows") {
        discover_hosts_windows().await
    } else {
        vec![]
    }
}

async fn discover_hosts_macos() -> Vec<String> {
    let output = tokio::process::Command::new("dns-sd")
        .args(["-B", "_smb._tcp", "local"])
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_dns_sd_output(&stdout)
        }
        Err(e) => {
            debug!(error = %e, "dns_sd_browse_failed");
            vec![]
        }
    }
}

async fn discover_hosts_linux() -> Vec<String> {
    let output = tokio::process::Command::new("avahi-browse")
        .args(["-tpr", "_smb._tcp"])
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_avahi_output(&stdout)
        }
        Err(e) => {
            debug!(error = %e, "avahi_browse_failed");
            vec![]
        }
    }
}

fn parse_dns_sd_output(output: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 7 && parts[1] == "Add" {
            hosts.push(parts[6].trim_end_matches('.').to_string());
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

fn parse_avahi_output(output: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in output.lines() {
        if !line.starts_with('=') {
            continue;
        }
        let fields: Vec<&str> = line.split(';').collect();
        if fields.len() >= 8 {
            let addr = fields[7].trim();
            if !addr.is_empty() {
                hosts.push(addr.to_string());
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

async fn enumerate_shares(host: &str) -> Vec<String> {
    if cfg!(target_os = "macos") {
        enumerate_shares_macos(host).await
    } else if cfg!(target_os = "windows") {
        enumerate_shares_windows(host).await
    } else {
        enumerate_shares_linux(host).await
    }
}

async fn enumerate_shares_macos(host: &str) -> Vec<String> {
    let output = tokio::process::Command::new("smbutil")
        .args(["view", &format!("//{host}")])
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_smbutil_shares(&stdout)
        }
        Err(_) => vec![],
    }
}

async fn enumerate_shares_linux(host: &str) -> Vec<String> {
    let output = tokio::process::Command::new("smbclient")
        .args(["-L", host, "-N"]) // -N = no password
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_smbclient_shares(&stdout)
        }
        Err(_) => vec![],
    }
}

/// Discover SMB hosts on Windows using `net view /all`.
async fn discover_hosts_windows() -> Vec<String> {
    let output = tokio::process::Command::new("net")
        .args(["view", "/all"])
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_net_view_hosts(&stdout)
        }
        Err(e) => {
            debug!(error = %e, "net_view_failed");
            vec![]
        }
    }
}

/// Enumerate shares on a Windows host using `net view \\host`.
async fn enumerate_shares_windows(host: &str) -> Vec<String> {
    let output = tokio::process::Command::new("net")
        .args(["view", &format!("\\\\{host}")])
        .output()
        .await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            parse_net_view_shares(&stdout)
        }
        Err(_) => vec![],
    }
}

/// Parse `net view /all` output to extract hostnames.
/// Lines look like: `\\HOSTNAME      Comment text`
fn parse_net_view_hosts(output: &str) -> Vec<String> {
    let mut hosts = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("\\\\") {
            if let Some(name) = rest.split_whitespace().next() {
                hosts.push(name.to_string());
            }
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Parse `net view \\host` output to extract share names.
/// Output has a separator line of dashes, then share entries.
fn parse_net_view_shares(output: &str) -> Vec<String> {
    let mut shares = Vec::new();
    let mut past_separator = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("---") {
            past_separator = true;
            continue;
        }
        if !past_separator || trimmed.is_empty() {
            continue;
        }
        // End of share list
        if trimmed.starts_with("The command completed") || trimmed.starts_with("La commande") {
            break;
        }
        let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
        if let Some(name) = parts.first() {
            let name = name.trim();
            if !name.is_empty() && !name.ends_with('$') {
                shares.push(name.to_string());
            }
        }
    }
    shares
}

fn parse_smbutil_shares(output: &str) -> Vec<String> {
    let mut shares = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("Disk")
            && let Some(name) = trimmed.split_whitespace().next()
            && !name.ends_with('$')
        {
            shares.push(name.to_string());
        }
    }
    shares
}

fn parse_smbclient_shares(output: &str) -> Vec<String> {
    let mut shares = Vec::new();
    let mut in_share_section = false;
    for line in output.lines() {
        if line.contains("Sharename") && line.contains("Type") {
            in_share_section = true;
            continue;
        }
        if in_share_section {
            if line.trim().starts_with('-') {
                continue;
            }
            if line.trim().is_empty() {
                in_share_section = false;
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 && parts[1] == "Disk" {
                let name = parts[0];
                if !name.ends_with('$') {
                    shares.push(name.to_string());
                }
            }
        }
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_id_format() {
        let share = DiscoveredSmbShare {
            host: "192.168.1.100".into(),
            share_name: "Music".into(),
            host_name: String::new(),
            last_seen: 0.0,
        };
        assert_eq!(share.id(), "smb://192.168.1.100/Music");
    }

    #[test]
    fn default_scan_interval() {
        let disc = SmbAutoDiscovery::default();
        assert_eq!(disc.scan_interval(), 60);
    }

    #[test]
    fn parse_smbclient_output() {
        let output = "\tSharename       Type      Comment\n\
                       \t---------       ----      -------\n\
                       \tMusic           Disk      Music Share\n\
                       \tIPC$            IPC       IPC Service\n\
                       \tPhotos          Disk      \n";
        let shares = parse_smbclient_shares(output);
        assert_eq!(shares, vec!["Music", "Photos"]);
    }

    #[test]
    fn parse_smbutil_output() {
        let output = "Share        Type\n\
                      -----        ----\n\
                      Music        Disk\n\
                      admin$       Disk\n\
                      Videos       Disk\n";
        let shares = parse_smbutil_shares(output);
        assert_eq!(shares, vec!["Music", "Videos"]);
    }

    #[test]
    fn parse_net_view_hosts_output() {
        let output = "Server Name            Remark\n\
                      -----------------------------------------------\n\
                      \\\\NAS-SYNOLOGY        Synology NAS\n\
                      \\\\DESKTOP-ABC        My Desktop\n\
                      \\\\MEDIA-SERVER       \n\
                      The command completed successfully.\n";
        let hosts = parse_net_view_hosts(output);
        assert_eq!(hosts, vec!["DESKTOP-ABC", "MEDIA-SERVER", "NAS-SYNOLOGY"]);
    }

    #[test]
    fn parse_net_view_shares_output() {
        let output = "Shared resources at \\\\NAS-SYNOLOGY\n\n\
                      Share name   Type  Used as  Comment\n\
                      -----------------------------------------------\n\
                      Music        Disk           Music files\n\
                      Photos       Disk           \n\
                      IPC$         IPC            Remote IPC\n\
                      ADMIN$       Disk           Remote Admin\n\
                      The command completed successfully.\n";
        let shares = parse_net_view_shares(output);
        assert_eq!(shares, vec!["Music", "Photos"]);
    }

    #[test]
    fn parse_net_view_shares_french() {
        let output = "Ressources partagées de \\\\MON-NAS\n\n\
                      Nom partagé  Type  Utilisé comme  Commentaire\n\
                      -----------------------------------------------\n\
                      Musique      Disque         Mes fichiers\n\
                      C$           Disque         Partage par défaut\n\
                      La commande s'est terminée correctement.\n";
        let shares = parse_net_view_shares(output);
        assert_eq!(shares, vec!["Musique"]);
    }

    #[test]
    fn clear_resets() {
        let mut disc = SmbAutoDiscovery::default();
        disc.discovered.insert(
            "smb://1.2.3.4/Music".into(),
            DiscoveredSmbShare {
                host: "1.2.3.4".into(),
                share_name: "Music".into(),
                host_name: String::new(),
                last_seen: 0.0,
            },
        );
        assert!(!disc.discovered().is_empty());
        disc.clear();
        assert!(disc.discovered().is_empty());
    }
}
