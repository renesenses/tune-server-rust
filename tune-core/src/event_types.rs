use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    PlaybackStarted,
    PlaybackStopped,
    PlaybackPaused,
    PlaybackResumed,
    TrackChanged,
    VolumeChanged,
    QueueChanged,
    SeekChanged,
    ShuffleChanged,
    RepeatChanged,
    DeviceDiscovered,
    DeviceLost,
    ScanStarted,
    ScanProgress,
    ScanComplete,
    LibraryTrackAdded,
    LibraryTrackRemoved,
    LibraryTrackUpdated,
    ZoneCreated,
    ZoneDeleted,
    ZoneUpdated,
    ProfileSwitched,
    PartyTrackAdded,
    PartyVote,
    ServiceConnected,
    ServiceDisconnected,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypedEvent {
    pub event_type: EventType,
    pub source: String,
    pub data: EventData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventData {
    PlaybackStarted(PlaybackStartedData),
    PlaybackStopped(PlaybackStoppedData),
    TrackChanged(TrackChangedData),
    VolumeChanged(VolumeChangedData),
    DeviceDiscovered(DeviceDiscoveredData),
    ScanProgress(ScanProgressData),
    Generic(serde_json::Value),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStartedData {
    pub zone_id: i64,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackStoppedData {
    pub zone_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackChangedData {
    pub zone_id: i64,
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeChangedData {
    pub zone_id: i64,
    pub volume: f64,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceDiscoveredData {
    pub device_id: String,
    pub name: String,
    pub device_type: String,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgressData {
    pub scanned: usize,
    pub total: usize,
    pub current_path: Option<String>,
}

impl TypedEvent {
    pub fn playback_started(zone_id: i64, title: &str, artist: Option<&str>, track_id: Option<i64>) -> Self {
        Self {
            event_type: EventType::PlaybackStarted,
            source: "playback".into(),
            data: EventData::PlaybackStarted(PlaybackStartedData {
                zone_id,
                track_id,
                title: title.to_string(),
                artist_name: artist.map(String::from),
            }),
        }
    }

    pub fn playback_stopped(zone_id: i64) -> Self {
        Self {
            event_type: EventType::PlaybackStopped,
            source: "playback".into(),
            data: EventData::PlaybackStopped(PlaybackStoppedData { zone_id }),
        }
    }

    pub fn track_changed(zone_id: i64, data: TrackChangedData) -> Self {
        Self {
            event_type: EventType::TrackChanged,
            source: "playback".into(),
            data: EventData::TrackChanged(data),
        }
    }

    pub fn volume_changed(zone_id: i64, volume: f64, muted: bool) -> Self {
        Self {
            event_type: EventType::VolumeChanged,
            source: "playback".into(),
            data: EventData::VolumeChanged(VolumeChangedData {
                zone_id,
                volume,
                muted,
            }),
        }
    }

    pub fn scan_progress(scanned: usize, total: usize, path: Option<&str>) -> Self {
        Self {
            event_type: EventType::ScanProgress,
            source: "scanner".into(),
            data: EventData::ScanProgress(ScanProgressData {
                scanned,
                total,
                current_path: path.map(String::from),
            }),
        }
    }

    pub fn generic(event_type: EventType, source: &str, data: serde_json::Value) -> Self {
        Self {
            event_type,
            source: source.to_string(),
            data: EventData::Generic(data),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_type_serialize() {
        let json = serde_json::to_value(EventType::PlaybackStarted).unwrap();
        assert_eq!(json, "playback_started");
    }

    #[test]
    fn event_type_deserialize() {
        let et: EventType = serde_json::from_str("\"track_changed\"").unwrap();
        assert_eq!(et, EventType::TrackChanged);
    }

    #[test]
    fn playback_started_event() {
        let evt = TypedEvent::playback_started(1, "Time", Some("Pink Floyd"), Some(42));
        assert_eq!(evt.event_type, EventType::PlaybackStarted);
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["event_type"], "playback_started");
    }

    #[test]
    fn volume_changed_event() {
        let evt = TypedEvent::volume_changed(1, 0.75, false);
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["data"]["volume"], 0.75);
        assert_eq!(json["data"]["muted"], false);
    }

    #[test]
    fn scan_progress_event() {
        let evt = TypedEvent::scan_progress(50, 100, Some("/music/album"));
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["data"]["scanned"], 50);
        assert_eq!(json["data"]["total"], 100);
    }

    #[test]
    fn generic_event() {
        let evt = TypedEvent::generic(
            EventType::Error,
            "system",
            serde_json::json!({"message": "disk full"}),
        );
        assert_eq!(evt.event_type, EventType::Error);
    }

    #[test]
    fn all_event_types_exist() {
        let types = [
            EventType::PlaybackStarted,
            EventType::PlaybackStopped,
            EventType::TrackChanged,
            EventType::VolumeChanged,
            EventType::DeviceDiscovered,
            EventType::ScanProgress,
            EventType::LibraryTrackAdded,
            EventType::ZoneCreated,
            EventType::PartyTrackAdded,
            EventType::Error,
        ];
        assert_eq!(types.len(), 10);
    }
}
