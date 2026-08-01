//! MAC-address identity helpers (chantier « renderers par MAC », 31/07).
//!
//! Three concerns, all display/identity oriented:
//! * [`normalize_mac`] — one canonical `AA:BB:CC:DD:EE:FF` form whatever the
//!   source prints (mDNS `deviceid`, BSD `arp` with single-digit octets,
//!   Windows dashes, bare hex).
//! * [`arp_lookup`] — recover a MAC from the OS ARP cache. Only meaningful
//!   right after we have exchanged IP traffic with the device, which is why
//!   the SSDP path calls it after fetching the description XML.
//! * [`vendor_for_mac`] — map the 24-bit OUI prefix to an audio brand for
//!   display when the protocol carries no manufacturer (AirPlay, Chromecast,
//!   BluOS). The table is generated from the IEEE registry (see
//!   `oui_audio.rs`), never hand-typed.

use super::device::DiscoveredDevice;
use super::oui_audio::OUI_AUDIO;

/// Canonicalise any common MAC spelling to `AA:BB:CC:DD:EE:FF`.
///
/// Accepts `:` or `-` separated groups of 1–2 hex digits (BSD `arp` prints
/// `0:11:22:33:44:5`), or 12 bare hex digits. Anything else is `None`.
pub fn normalize_mac(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.contains(':') || raw.contains('-') {
        let parts: Vec<&str> = raw.split([':', '-']).collect();
        if parts.len() != 6 {
            return None;
        }
        let mut out = Vec::with_capacity(6);
        for p in parts {
            if p.is_empty() || p.len() > 2 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
                return None;
            }
            out.push(format!("{:0>2}", p.to_ascii_uppercase()));
        }
        Some(out.join(":"))
    } else {
        if raw.len() != 12 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let up = raw.to_ascii_uppercase();
        Some(
            up.as_bytes()
                .chunks(2)
                .map(|c| std::str::from_utf8(c).unwrap())
                .collect::<Vec<_>>()
                .join(":"),
        )
    }
}

/// The audio brand behind a MAC's OUI prefix, if it is one we know.
pub fn vendor_for_mac(mac: &str) -> Option<&'static str> {
    let mac = normalize_mac(mac)?;
    let prefix = &mac[..8];
    OUI_AUDIO
        .binary_search_by(|(p, _)| (*p).cmp(prefix))
        .ok()
        .map(|i| OUI_AUDIO[i].1)
}

/// Look the `ip` up in the OS ARP cache.
///
/// Returns a normalised MAC, or `None` when the entry is absent/incomplete.
/// Spawns the system `arp` tool (Linux reads `/proc/net/arp` directly) — a
/// few ms, so call it once per newly discovered device, not per request.
pub fn arp_lookup(ip: &str) -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Some(mac) = arp_from_proc(ip) {
        return Some(mac);
    }

    let args: &[&str] = if cfg!(windows) {
        &["-a", ip]
    } else {
        &["-n", ip]
    };
    let output = std::process::Command::new("arp").args(args).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace()
        .filter_map(normalize_mac)
        .find(|m| m != "00:00:00:00:00:00" && m != "FF:FF:FF:FF:FF:FF")
}

#[cfg(target_os = "linux")]
fn arp_from_proc(ip: &str) -> Option<String> {
    let table = std::fs::read_to_string("/proc/net/arp").ok()?;
    for line in table.lines().skip(1) {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // IP HWtype Flags HWaddr Mask Device — 0x2 = complete entry.
        if cols.len() >= 4 && cols[0] == ip && cols[2] != "0x0" {
            return normalize_mac(cols[3])
                .filter(|m| m != "00:00:00:00:00:00" && m != "FF:FF:FF:FF:FF:FF");
        }
    }
    None
}

/// Fill in what the MAC can tell us about a freshly discovered device:
/// normalise an already-known MAC, recover one from ARP otherwise, and
/// derive the brand when the protocol brought no manufacturer. Existing
/// values always win — the description XML knows better than the OUI.
pub fn enrich_identity(device: &mut DiscoveredDevice) {
    match device.mac_address.clone() {
        Some(raw) => {
            if let Some(mac) = normalize_mac(&raw) {
                device.mac_address = Some(mac);
            } else if let Some(mac) = arp_lookup(&device.host) {
                // Some protocols stash an opaque id here (Chromecast's TXT
                // `id` is a UUID): a real MAC from ARP is strictly better,
                // but keep the opaque value when ARP has nothing.
                device.mac_address = Some(mac);
            }
        }
        None => device.mac_address = arp_lookup(&device.host),
    }
    if device.manufacturer.as_deref().is_none_or(str::is_empty) {
        if let Some(mac) = &device.mac_address {
            device.manufacturer = vendor_for_mac(mac).map(str::to_string);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_common_spellings() {
        assert_eq!(
            normalize_mac("aa:bb:cc:dd:ee:ff").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        // BSD arp drops leading zeroes.
        assert_eq!(
            normalize_mac("0:11:22:33:44:5").as_deref(),
            Some("00:11:22:33:44:05")
        );
        assert_eq!(
            normalize_mac("AA-BB-CC-DD-EE-FF").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(
            normalize_mac("aabbccddeeff").as_deref(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(normalize_mac("not a mac"), None);
        assert_eq!(normalize_mac("aa:bb:cc"), None);
    }

    #[test]
    fn oui_table_is_sorted_for_binary_search() {
        assert!(OUI_AUDIO.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn vendor_lookup_hits_known_brands() {
        // First entries of a few brands straight from the generated table —
        // the point is that lookup agrees with the table, not the values.
        let sonos = OUI_AUDIO.iter().find(|(_, b)| *b == "Sonos").unwrap();
        let mac = format!("{}:11:22:33", sonos.0);
        assert_eq!(vendor_for_mac(&mac), Some("Sonos"));
        assert_eq!(vendor_for_mac("02:00:00:00:00:01"), None);
    }

    #[test]
    fn enrich_prefers_existing_manufacturer() {
        use crate::discovery::device::{DiscoveredDevice, OutputType};
        let sonos = OUI_AUDIO.iter().find(|(_, b)| *b == "Sonos").unwrap();
        let mut dev = DiscoveredDevice::new(
            "id".into(),
            "Salon".into(),
            OutputType::Dlna,
            "203.0.113.1".into(), // TEST-NET: never in the ARP cache
            1400,
        );
        dev.mac_address = Some(format!("{}:11:22:33", sonos.0).to_lowercase());
        dev.manufacturer = Some("Yamaha Corporation".into());
        enrich_identity(&mut dev);
        // Existing manufacturer wins; the MAC still gets normalised.
        assert_eq!(dev.manufacturer.as_deref(), Some("Yamaha Corporation"));
        assert_eq!(
            dev.mac_address.as_deref(),
            Some(format!("{}:11:22:33", sonos.0).as_str())
        );

        dev.manufacturer = None;
        enrich_identity(&mut dev);
        assert_eq!(dev.manufacturer.as_deref(), Some("Sonos"));
    }
}
