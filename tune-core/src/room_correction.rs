use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single point on a measured frequency response curve.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrequencyPoint {
    /// Frequency in Hz (20 .. 20000 typical).
    pub frequency_hz: f64,
    /// Measured amplitude in dB SPL (relative).
    pub amplitude_db: f64,
}

/// Filter type used in parametric correction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterType {
    Peaking,
    LowShelf,
    HighShelf,
    Notch,
}

/// A single parametric EQ correction filter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrectionFilter {
    /// Center / corner frequency in Hz.
    pub frequency_hz: f64,
    /// Gain in dB (negative = cut, positive = boost).
    pub gain_db: f64,
    /// Quality factor (bandwidth). Typical range 0.5 .. 10.
    pub q_factor: f64,
    /// Type of filter to apply.
    pub filter_type: FilterType,
}

/// A complete room correction profile for one zone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomProfile {
    /// Human-readable name ("Living room", "Studio A").
    pub name: String,
    /// Zone this profile belongs to.
    pub zone_id: String,
    /// Parametric EQ filters derived from measurement.
    pub filters: Vec<CorrectionFilter>,
    /// ISO-8601 creation timestamp.
    pub created_at: String,
    /// Raw measurement data (JSON-encoded FrequencyPoint array) for
    /// reference / re-analysis.  `None` when the profile was created
    /// manually without measurement.
    pub measurement_data: Option<String>,
}

// ---------------------------------------------------------------------------
// Settings key helpers
// ---------------------------------------------------------------------------

fn profile_key(zone_id: &str) -> String {
    format!("room_profile_{zone_id}")
}

const INDEX_KEY: &str = "room_profile_index";

/// Load the index of zone IDs that have profiles.
fn load_index(settings: &SettingsRepo) -> Vec<String> {
    settings
        .get(INDEX_KEY)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the index of zone IDs.
fn save_index(settings: &SettingsRepo, ids: &[String]) -> Result<(), String> {
    let json = serde_json::to_string(ids).map_err(|e| e.to_string())?;
    settings.set(INDEX_KEY, &json)
}

// ---------------------------------------------------------------------------
// CRUD operations
// ---------------------------------------------------------------------------

/// Save (create or update) a room profile for a zone.
pub fn save_profile(backend: &Arc<dyn DbBackend>, profile: &RoomProfile) -> Result<(), String> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let json = serde_json::to_string(profile).map_err(|e| e.to_string())?;
    settings.set(&profile_key(&profile.zone_id), &json)?;

    // Ensure the zone ID is in the index.
    let mut index = load_index(&settings);
    if !index.contains(&profile.zone_id) {
        index.push(profile.zone_id.clone());
        save_index(&settings, &index)?;
    }
    Ok(())
}

/// Load the room profile for a specific zone, if any.
pub fn load_profile(backend: &Arc<dyn DbBackend>, zone_id: &str) -> Option<RoomProfile> {
    let settings = SettingsRepo::with_backend(backend.clone());
    settings
        .get(&profile_key(zone_id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

/// List all stored room profiles.
pub fn list_profiles(backend: &Arc<dyn DbBackend>) -> Vec<RoomProfile> {
    let settings = SettingsRepo::with_backend(backend.clone());
    let index = load_index(&settings);
    index
        .iter()
        .filter_map(|zone_id| {
            settings
                .get(&profile_key(zone_id))
                .ok()
                .flatten()
                .and_then(|s| serde_json::from_str(&s).ok())
        })
        .collect()
}

/// Delete the room profile for a zone.
pub fn delete_profile(backend: &Arc<dyn DbBackend>, zone_id: &str) -> Result<bool, String> {
    let settings = SettingsRepo::with_backend(backend.clone());

    // Remove from settings.
    let existed = settings.get(&profile_key(zone_id)).ok().flatten().is_some();
    if existed {
        settings.delete(&profile_key(zone_id)).ok();
    }

    // Remove from index.
    let mut index = load_index(&settings);
    let before = index.len();
    index.retain(|id| id != zone_id);
    if index.len() != before {
        save_index(&settings, &index)?;
    }

    Ok(existed)
}

// ---------------------------------------------------------------------------
// Auto-EQ algorithm
// ---------------------------------------------------------------------------

/// Target curve: flat response (0 dB at every frequency).
/// A "house curve" could be added later (slight bass boost, treble rolloff).
fn target_amplitude(_frequency_hz: f64) -> f64 {
    0.0
}

/// Generate correction filters from a set of measured frequency points.
///
/// Algorithm (simplified auto-EQ):
///   1. Compare each measurement point to the target curve.
///   2. Where the deviation exceeds the `threshold_db` (default 3 dB),
///      create an inverse parametric filter:
///      - Peaks (measurement > target) => cut (negative gain, peaking filter).
///      - Dips  (measurement < target) => boost (positive gain, peaking filter).
///   3. Q factor is estimated from the width of the deviation region.
///   4. Extreme corrections are clamped to +/- 12 dB for safety.
///
/// This is intentionally simple -- not a full psychoacoustic model.
pub fn generate_correction_from_measurements(
    measurements: &[FrequencyPoint],
) -> Vec<CorrectionFilter> {
    const THRESHOLD_DB: f64 = 3.0;
    const MAX_CORRECTION_DB: f64 = 12.0;
    const DEFAULT_Q: f64 = 2.0;

    if measurements.is_empty() {
        return Vec::new();
    }

    let mut filters = Vec::new();

    // Scan for deviations that exceed the threshold.
    let mut i = 0;
    while i < measurements.len() {
        let pt = &measurements[i];
        let target = target_amplitude(pt.frequency_hz);
        let deviation = pt.amplitude_db - target;

        if deviation.abs() > THRESHOLD_DB {
            // Find the extent of this deviation region to estimate Q.
            let region_start = i;
            let mut peak_idx = i;
            let mut peak_dev = deviation.abs();

            let mut j = i + 1;
            while j < measurements.len() {
                let d = (measurements[j].amplitude_db
                    - target_amplitude(measurements[j].frequency_hz))
                .abs();
                if d < THRESHOLD_DB {
                    break;
                }
                if d > peak_dev {
                    peak_dev = d;
                    peak_idx = j;
                }
                j += 1;
            }
            let region_end = j;

            // Use the peak of the deviation region as the filter center.
            let center = &measurements[peak_idx];
            let center_deviation = center.amplitude_db - target_amplitude(center.frequency_hz);

            // Estimate Q from bandwidth: Q ~ f_center / bandwidth.
            let f_low = measurements[region_start].frequency_hz;
            let f_high = measurements[region_end.min(measurements.len()) - 1].frequency_hz;
            let bandwidth = f_high - f_low;
            let q = if bandwidth > 0.0 {
                (center.frequency_hz / bandwidth).clamp(0.5, 10.0)
            } else {
                DEFAULT_Q
            };

            // Inverse gain, clamped.
            let gain = (-center_deviation).clamp(-MAX_CORRECTION_DB, MAX_CORRECTION_DB);

            // Choose filter type based on frequency range.
            let filter_type = if center.frequency_hz <= 80.0 {
                FilterType::LowShelf
            } else if center.frequency_hz >= 12000.0 {
                FilterType::HighShelf
            } else {
                FilterType::Peaking
            };

            filters.push(CorrectionFilter {
                frequency_hz: center.frequency_hz,
                gain_db: gain,
                q_factor: q,
                filter_type,
            });

            i = region_end;
        } else {
            i += 1;
        }
    }

    filters
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_measurements_produce_no_filters() {
        let filters = generate_correction_from_measurements(&[]);
        assert!(filters.is_empty());
    }

    #[test]
    fn flat_response_produces_no_filters() {
        let points: Vec<FrequencyPoint> = (1..=10)
            .map(|i| FrequencyPoint {
                frequency_hz: i as f64 * 1000.0,
                amplitude_db: 0.0,
            })
            .collect();
        let filters = generate_correction_from_measurements(&points);
        assert!(filters.is_empty());
    }

    #[test]
    fn peak_above_threshold_generates_cut() {
        let points = vec![
            FrequencyPoint {
                frequency_hz: 900.0,
                amplitude_db: 0.0,
            },
            FrequencyPoint {
                frequency_hz: 1000.0,
                amplitude_db: 6.0,
            },
            FrequencyPoint {
                frequency_hz: 1100.0,
                amplitude_db: 0.0,
            },
        ];
        let filters = generate_correction_from_measurements(&points);
        assert_eq!(filters.len(), 1);
        assert!(filters[0].gain_db < 0.0, "peak should be cut");
        assert_eq!(filters[0].filter_type, FilterType::Peaking);
    }

    #[test]
    fn dip_below_threshold_generates_boost() {
        let points = vec![
            FrequencyPoint {
                frequency_hz: 900.0,
                amplitude_db: 0.0,
            },
            FrequencyPoint {
                frequency_hz: 1000.0,
                amplitude_db: -5.0,
            },
            FrequencyPoint {
                frequency_hz: 1100.0,
                amplitude_db: 0.0,
            },
        ];
        let filters = generate_correction_from_measurements(&points);
        assert_eq!(filters.len(), 1);
        assert!(filters[0].gain_db > 0.0, "dip should be boosted");
    }

    #[test]
    fn low_frequency_uses_low_shelf() {
        let points = vec![
            FrequencyPoint {
                frequency_hz: 40.0,
                amplitude_db: 8.0,
            },
            FrequencyPoint {
                frequency_hz: 60.0,
                amplitude_db: 10.0,
            },
            FrequencyPoint {
                frequency_hz: 80.0,
                amplitude_db: 6.0,
            },
            FrequencyPoint {
                frequency_hz: 200.0,
                amplitude_db: 0.0,
            },
        ];
        let filters = generate_correction_from_measurements(&points);
        assert!(!filters.is_empty());
        assert_eq!(filters[0].filter_type, FilterType::LowShelf);
    }

    #[test]
    fn correction_clamped_to_12db() {
        let points = vec![
            FrequencyPoint {
                frequency_hz: 900.0,
                amplitude_db: 0.0,
            },
            FrequencyPoint {
                frequency_hz: 1000.0,
                amplitude_db: 20.0,
            },
            FrequencyPoint {
                frequency_hz: 1100.0,
                amplitude_db: 0.0,
            },
        ];
        let filters = generate_correction_from_measurements(&points);
        assert_eq!(filters.len(), 1);
        assert!(
            filters[0].gain_db >= -12.0,
            "correction must be clamped to -12 dB"
        );
    }
}
