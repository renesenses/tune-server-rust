use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputType {
    Local,
    Dlna,
    Airplay,
    Chromecast,
    Bluos,
    Openhome,
    Squeezebox,
    Oaat,
}

impl OutputType {
    pub fn priority(self) -> u8 {
        match self {
            Self::Oaat => 8,
            Self::Openhome => 7,
            Self::Bluos => 6,
            Self::Squeezebox => 5,
            Self::Dlna => 4,
            Self::Chromecast => 3,
            Self::Airplay => 2,
            Self::Local => 1,
        }
    }
}

impl std::fmt::Display for OutputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => write!(f, "local"),
            Self::Dlna => write!(f, "dlna"),
            Self::Airplay => write!(f, "airplay"),
            Self::Chromecast => write!(f, "chromecast"),
            Self::Bluos => write!(f, "bluos"),
            Self::Openhome => write!(f, "openhome"),
            Self::Squeezebox => write!(f, "squeezebox"),
            Self::Oaat => write!(f, "oaat"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub id: String,
    pub name: String,
    pub device_type: OutputType,
    pub host: String,
    pub port: u16,
    pub available: bool,
    pub capabilities: HashMap<String, serde_json::Value>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub location: Option<String>,
    pub airplay_version: Option<String>,
    pub mac_address: Option<String>,
}

impl DiscoveredDevice {
    pub fn new(id: String, name: String, device_type: OutputType, host: String, port: u16) -> Self {
        Self {
            id,
            name,
            device_type,
            host,
            port,
            available: true,
            capabilities: HashMap::new(),
            manufacturer: None,
            model: None,
            location: None,
            airplay_version: None,
            mac_address: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Alternative {
    pub id: String,
    pub name: String,
    pub device_type: OutputType,
}

pub fn dedup_devices(devices: Vec<DiscoveredDevice>) -> Vec<DiscoveredDevice> {
    let mut by_host: HashMap<String, Vec<DiscoveredDevice>> = HashMap::new();
    for dev in devices {
        if dev
            .manufacturer
            .as_deref()
            .is_some_and(|m| m.to_lowercase().contains("mozaik"))
        {
            continue;
        }
        by_host.entry(dev.host.clone()).or_default().push(dev);
    }

    let mut result = Vec::new();
    for (_host, mut group) in by_host {
        group.sort_by_key(|b| std::cmp::Reverse(b.device_type.priority()));
        let mut primary = group.remove(0);
        if !group.is_empty() {
            let alts: Vec<Alternative> = group
                .iter()
                .map(|d| Alternative {
                    id: d.id.clone(),
                    name: d.name.clone(),
                    device_type: d.device_type,
                })
                .collect();
            primary.capabilities.insert(
                "alternatives".to_string(),
                serde_json::to_value(alts).unwrap_or_default(),
            );
        }
        result.push(primary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_keeps_highest_priority() {
        let devices = vec![
            DiscoveredDevice::new(
                "dlna-1".into(),
                "Speaker".into(),
                OutputType::Dlna,
                "192.168.1.50".into(),
                1400,
            ),
            DiscoveredDevice::new(
                "oh-1".into(),
                "Speaker".into(),
                OutputType::Openhome,
                "192.168.1.50".into(),
                1400,
            ),
        ];
        let result = dedup_devices(devices);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].device_type, OutputType::Openhome);
        assert!(result[0].capabilities.contains_key("alternatives"));
    }

    #[test]
    fn dedup_filters_self() {
        let mut dev = DiscoveredDevice::new(
            "self".into(),
            "Tune".into(),
            OutputType::Dlna,
            "127.0.0.1".into(),
            8888,
        );
        dev.manufacturer = Some("Mozaik Labs".into());
        let result = dedup_devices(vec![dev]);
        assert!(result.is_empty());
    }
}
