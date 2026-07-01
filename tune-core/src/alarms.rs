use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::orchestrator::PlaybackOrchestrator;

const POLL_INTERVAL_SECS: u64 = 30;
const SNOOZE_DEFAULT_MIN: i64 = 5;

// ─── French public holidays ────────────────────────────────────

fn easter(year: i32) -> (u32, u32) {
    let a = year % 19;
    let (b, c) = (year / 100, year % 100);
    let (d, e) = (b / 4, b % 4);
    let f = (b + 8) / 25;
    let g = (b - f + 1) / 3;
    let h = (19 * a + b - d - g + 15) % 30;
    let (i, k) = (c / 4, c % 4);
    let l = (32 + 2 * e + 2 * i - h - k) % 7;
    let m = (a + 11 * h + 22 * l) / 451;
    let month = (h + l - 7 * m + 114) / 31;
    let day = (h + l - 7 * m + 114) % 31 + 1;
    (month as u32, day as u32)
}

fn is_french_holiday(year: i32, month: u32, day: u32) -> bool {
    let fixed = [
        (1, 1),
        (5, 1),
        (5, 8),
        (7, 14),
        (8, 15),
        (11, 1),
        (11, 11),
        (12, 25),
    ];
    if fixed.contains(&(month, day)) {
        return true;
    }
    use chrono::NaiveDate;
    let (em, ed) = easter(year);
    if let Some(easter_date) = NaiveDate::from_ymd_opt(year, em, ed) {
        let check = NaiveDate::from_ymd_opt(year, month, day).unwrap();
        let delta = (check - easter_date).num_days();
        // Lundi de Pâques (+1), Ascension (+39), Lundi de Pentecôte (+50)
        if delta == 1 || delta == 39 || delta == 50 {
            return true;
        }
    }
    false
}

// ─── Day parsing ───────────────────────────────────────────────

/// Parse 7-char bitmask "1010100" (Mon..Sun) into day indices (0=Mon..6=Sun).
fn parse_days_of_week(mask: &str) -> Vec<u32> {
    mask.chars()
        .enumerate()
        .filter_map(|(i, c)| if c == '1' { Some(i as u32) } else { None })
        .collect()
}

fn parse_days(days_str: Option<&str>) -> Vec<u32> {
    let s = match days_str {
        Some(s) if !s.is_empty() => s.trim().to_lowercase(),
        _ => return vec![0, 1, 2, 3, 4], // weekdays
    };
    match s.as_str() {
        "daily" => return (0..7).collect(),
        "weekdays" => return vec![0, 1, 2, 3, 4],
        "weekends" => return vec![5, 6],
        _ => {}
    }
    let day_names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let mut result: Vec<u32> = s
        .split(',')
        .filter_map(|p| {
            let p = p.trim();
            if let Some(idx) = day_names.iter().position(|n| *n == p) {
                Some(idx as u32)
            } else {
                p.parse::<u32>().ok().filter(|&v| v <= 6)
            }
        })
        .collect();
    result.sort();
    result.dedup();
    result
}

/// Resolve active days for an alarm.  Prefers `days_of_week` (7-char
/// bitmask) when present; falls back to legacy `days` (CSV/named).
fn resolve_alarm_days(alarm: &serde_json::Value) -> Vec<u32> {
    if let Some(dow) = alarm.get("days_of_week").and_then(|v| v.as_str()) {
        if dow.len() == 7 && dow.chars().all(|c| c == '0' || c == '1') {
            return parse_days_of_week(dow);
        }
    }
    parse_days(alarm.get("days").and_then(|v| v.as_str()))
}

/// Parse multi_zone_ids JSON array string into a Vec of zone IDs.
fn parse_multi_zone_ids(alarm: &serde_json::Value) -> Vec<i64> {
    alarm
        .get("multi_zone_ids")
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<Vec<i64>>(s).ok())
        .unwrap_or_default()
}

// ─── Snooze ────────────────────────────────────────────────────

struct SnoozeState {
    snoozed: HashMap<i64, Instant>,
    durations: HashMap<i64, u64>,
}

impl SnoozeState {
    fn new() -> Self {
        Self {
            snoozed: HashMap::new(),
            durations: HashMap::new(),
        }
    }

    fn snooze(&mut self, alarm_id: i64, minutes: u64) {
        self.snoozed.insert(alarm_id, Instant::now());
        self.durations.insert(alarm_id, minutes.max(1).min(60) * 60);
    }

    fn pop_ready(&mut self) -> Vec<i64> {
        let ready: Vec<i64> = self
            .snoozed
            .iter()
            .filter(|(id, start)| {
                let dur = self.durations.get(id).copied().unwrap_or(300);
                start.elapsed().as_secs() >= dur
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &ready {
            self.snoozed.remove(id);
            self.durations.remove(id);
        }
        ready
    }

    fn is_snoozed(&self, alarm_id: i64) -> bool {
        self.snoozed.contains_key(&alarm_id)
    }

    fn cancel(&mut self, alarm_id: i64) -> bool {
        self.snoozed.remove(&alarm_id).is_some()
    }
}

// ─── Fade-in ───────────────────────────────────────────────────

async fn fade_in_volume(
    orchestrator: &PlaybackOrchestrator,
    zone_id: i64,
    device_id: Option<String>,
    target: f64,
    duration_s: u64,
) {
    if duration_s == 0 {
        let _ = orchestrator
            .set_volume(zone_id, target, device_id.as_deref())
            .await;
        return;
    }
    let steps = (duration_s * 2).max(1);
    let step_delay = std::time::Duration::from_millis((duration_s * 1000) / steps);
    for i in 0..=steps {
        let vol = (i as f64 / steps as f64) * target;
        let _ = orchestrator
            .set_volume(zone_id, vol, device_id.as_deref())
            .await;
        tokio::time::sleep(step_delay).await;
    }
}

// ─── Scheduler ─────────────────────────────────────────────────

pub struct AlarmScheduler {
    db: Arc<dyn DbBackend>,
    orchestrator: Arc<PlaybackOrchestrator>,
    snooze: Mutex<SnoozeState>,
}

impl AlarmScheduler {
    pub fn with_backend(db: Arc<dyn DbBackend>, orchestrator: Arc<PlaybackOrchestrator>) -> Self {
        Self {
            db,
            orchestrator,
            snooze: Mutex::new(SnoozeState::new()),
        }
    }

    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("alarm_scheduler_started");
            let mut fired_today: HashSet<i64> = HashSet::new();
            let mut last_date = String::new();
            let mut ticker =
                tokio::time::interval(std::time::Duration::from_secs(POLL_INTERVAL_SECS));
            loop {
                ticker.tick().await;
                if let Err(e) = self.tick(&mut fired_today, &mut last_date).await {
                    warn!(error = %e, "alarm_scheduler_error");
                }
            }
        })
    }

    pub async fn snooze(&self, alarm_id: i64, minutes: Option<i64>) -> Result<(), String> {
        let mins = minutes.unwrap_or(SNOOZE_DEFAULT_MIN).max(1).min(60) as u64;
        self.snooze.lock().await.snooze(alarm_id, mins);
        info!(alarm_id, minutes = mins, "alarm_snoozed");
        Ok(())
    }

    pub async fn cancel_snooze(&self, alarm_id: i64) -> bool {
        self.snooze.lock().await.cancel(alarm_id)
    }

    async fn tick(
        &self,
        fired_today: &mut HashSet<i64>,
        last_date: &mut String,
    ) -> Result<(), String> {
        use chrono::{Datelike, Local, Timelike};
        let now = Local::now();
        let today = now.format("%Y-%m-%d").to_string();

        if *last_date != today {
            fired_today.clear();
            *last_date = today.clone();
        }

        // Check snoozed alarms
        let snoozed_ready = self.snooze.lock().await.pop_ready();
        for alarm_id in snoozed_ready {
            if let Ok(Some(alarm)) = self.get_alarm(alarm_id)
                && alarm_enabled(&alarm)
            {
                fired_today.insert(alarm_id);
                self.fire_alarm(&alarm).await;
            }
        }

        // Check scheduled alarms
        let alarms = self.list_enabled_alarms()?;
        let dow = now.weekday().num_days_from_monday(); // 0=Mon..6=Sun

        for alarm in &alarms {
            let alarm_id = alarm.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            if alarm_id == 0 || fired_today.contains(&alarm_id) {
                continue;
            }
            if self.snooze.lock().await.is_snoozed(alarm_id) {
                continue;
            }

            let days = resolve_alarm_days(alarm);
            if !days.contains(&dow) {
                continue;
            }

            if alarm
                .get("skip_holidays")
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
                != 0
                && is_french_holiday(now.year(), now.month(), now.day())
            {
                info!(alarm_id, date = %today, "alarm_skipped_holiday");
                fired_today.insert(alarm_id);
                continue;
            }

            let alarm_time = alarm.get("time").and_then(|v| v.as_str()).unwrap_or("");
            let parts: Vec<&str> = alarm_time.split(':').collect();
            if parts.len() < 2 {
                continue;
            }
            let (h, m) = match (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                (Ok(h), Ok(m)) => (h, m),
                _ => continue,
            };

            if now.hour() == h && now.minute() == m {
                fired_today.insert(alarm_id);
                self.fire_alarm(alarm).await;
            }
        }

        Ok(())
    }

    /// Fire an alarm: play its source on the target zone(s) with optional
    /// fade-in.  Public so the test endpoint (`POST /alarms/{id}/test`) can
    /// trigger it directly.
    pub async fn fire_alarm(&self, alarm: &serde_json::Value) {
        let alarm_id = alarm["id"].as_i64().unwrap_or(0);
        let zone_id = alarm["zone_id"].as_i64();
        let source_type = alarm["source_type"].as_str().unwrap_or("radio");
        let source_id = alarm["source_id"]
            .as_str()
            .or_else(|| alarm["source_id"].as_i64().map(|_| ""))
            .unwrap_or("");
        let source_id_str = if source_id.is_empty() {
            alarm["source_id"]
                .as_i64()
                .map(|n| n.to_string())
                .unwrap_or_default()
        } else {
            source_id.to_string()
        };
        let volume = alarm["volume"].as_f64().unwrap_or(0.5);
        let volume = if volume > 1.0 { volume / 100.0 } else { volume };
        let fade_s = alarm["fade_duration_s"]
            .as_u64()
            .or_else(|| alarm["fade_in_seconds"].as_u64())
            .unwrap_or(60);

        // Collect target zones: multi_zone_ids if set, else single zone_id,
        // else fallback to first available zone.
        let multi_zones = parse_multi_zone_ids(alarm);
        let target_zones: Vec<i64> = if !multi_zones.is_empty() {
            multi_zones
        } else {
            let z = zone_id.unwrap_or_else(|| {
                crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                    .list()
                    .unwrap_or_default()
                    .first()
                    .and_then(|z| z.id)
                    .unwrap_or(1)
            });
            vec![z]
        };

        info!(
            alarm_id,
            name = alarm["name"].as_str().unwrap_or(""),
            zones = ?target_zones,
            source = format!("{source_type}:{source_id_str}"),
            "alarm_triggered"
        );

        let zone_repo = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone());

        for &target_zone in &target_zones {
            let device_id = zone_repo
                .get(target_zone)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id);

            let req = crate::orchestrator::PlayRequest {
                zone_id: target_zone,
                output_device_id: device_id.clone(),
                track_id: None,
                source: Some(source_type.to_string()),
                source_id: Some(source_id_str.clone()),
                title: None,
                artist_name: None,
                album_title: None,
                cover_url: None,
                duration_ms: None,
                seek_ms: None,
                temp_file_path: None,
            };
            if let Err(e) = self.orchestrator.play(req).await {
                warn!(alarm_id, zone_id = target_zone, error = %e, "alarm_play_error");
            }

            // Fade in volume in background
            let orch = self.orchestrator.clone();
            tokio::spawn(async move {
                fade_in_volume(&orch, target_zone, device_id, volume, fade_s).await;
            });
        }

        // Update last_fired_at
        let now_str = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        self.db
            .execute(
                "UPDATE alarms SET last_fired_at = ? WHERE id = ?",
                &[&now_str as &dyn ToSqlValue, &alarm_id],
            )
            .ok();

        // One-shot: disable after firing
        if alarm["one_shot"].as_i64().unwrap_or(0) != 0 {
            self.db
                .execute(
                    "UPDATE alarms SET enabled = '0' WHERE id = ?",
                    &[&alarm_id as &dyn ToSqlValue],
                )
                .ok();
            info!(alarm_id, "alarm_one_shot_disabled");
        }
    }

    fn list_enabled_alarms(&self) -> Result<Vec<serde_json::Value>, String> {
        use crate::db::backend::SqlValue;
        let rows = self.db.query_many(
            "SELECT id, name, time, days, zone_id, source_type, source_id, volume, fade_duration_s, fade_in_seconds, one_shot, skip_holidays, enabled, days_of_week, multi_zone_ids FROM alarms WHERE enabled = '1'",
            &[],
        )?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.get(0).and_then(SqlValue::as_i64).unwrap_or(0),
                    "name": r.get(1).and_then(SqlValue::as_str).unwrap_or(""),
                    "time": r.get(2).and_then(SqlValue::as_str).unwrap_or(""),
                    "days": r.get(3).and_then(SqlValue::as_str),
                    "zone_id": r.get(4).and_then(SqlValue::as_i64),
                    "source_type": r.get(5).and_then(SqlValue::as_str),
                    "source_id": r.get(6).and_then(SqlValue::as_str),
                    "volume": r.get(7).and_then(SqlValue::as_f64),
                    "fade_duration_s": r.get(8).and_then(SqlValue::as_i64),
                    "fade_in_seconds": r.get(9).and_then(SqlValue::as_i64),
                    "one_shot": r.get(10).and_then(SqlValue::as_i64),
                    "skip_holidays": r.get(11).and_then(SqlValue::as_i64),
                    "enabled": r.get(12).and_then(SqlValue::as_i64).unwrap_or(0),
                    "days_of_week": r.get(13).and_then(SqlValue::as_str),
                    "multi_zone_ids": r.get(14).and_then(SqlValue::as_str),
                })
            })
            .collect())
    }

    /// Retrieve a single alarm by ID. Public so the test endpoint can use it.
    pub fn get_alarm(&self, id: i64) -> Result<Option<serde_json::Value>, String> {
        use crate::db::backend::SqlValue;
        let row = self.db.query_one(
            "SELECT id, name, time, days, zone_id, source_type, source_id, volume, fade_duration_s, fade_in_seconds, one_shot, skip_holidays, enabled, days_of_week, multi_zone_ids FROM alarms WHERE id = ?",
            &[&id as &dyn ToSqlValue],
        )?;
        Ok(row.map(|r| {
            serde_json::json!({
                "id": r.get(0).and_then(SqlValue::as_i64).unwrap_or(0),
                "name": r.get(1).and_then(SqlValue::as_str).unwrap_or(""),
                "time": r.get(2).and_then(SqlValue::as_str).unwrap_or(""),
                "days": r.get(3).and_then(SqlValue::as_str),
                "zone_id": r.get(4).and_then(SqlValue::as_i64),
                "source_type": r.get(5).and_then(SqlValue::as_str),
                "source_id": r.get(6).and_then(SqlValue::as_str),
                "volume": r.get(7).and_then(SqlValue::as_f64),
                "fade_duration_s": r.get(8).and_then(SqlValue::as_i64),
                "fade_in_seconds": r.get(9).and_then(SqlValue::as_i64),
                "one_shot": r.get(10).and_then(SqlValue::as_i64),
                "skip_holidays": r.get(11).and_then(SqlValue::as_i64),
                "enabled": r.get(12).and_then(SqlValue::as_i64).unwrap_or(0),
                "days_of_week": r.get(13).and_then(SqlValue::as_str),
                "multi_zone_ids": r.get(14).and_then(SqlValue::as_str),
            })
        }))
    }
}

fn alarm_enabled(alarm: &serde_json::Value) -> bool {
    alarm["enabled"].as_i64().unwrap_or(0) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_days_default() {
        assert_eq!(parse_days(None), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn parse_days_daily() {
        assert_eq!(parse_days(Some("daily")), vec![0, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn parse_days_weekends() {
        assert_eq!(parse_days(Some("weekends")), vec![5, 6]);
    }

    #[test]
    fn parse_days_numeric() {
        assert_eq!(parse_days(Some("0,2,4")), vec![0, 2, 4]);
    }

    #[test]
    fn parse_days_named() {
        assert_eq!(parse_days(Some("mon,wed,fri")), vec![0, 2, 4]);
    }

    #[test]
    fn parse_days_of_week_bitmask() {
        assert_eq!(parse_days_of_week("1111111"), vec![0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(parse_days_of_week("1010100"), vec![0, 2, 4]);
        assert_eq!(parse_days_of_week("0000011"), vec![5, 6]);
        assert_eq!(parse_days_of_week("0000000"), Vec::<u32>::new());
    }

    #[test]
    fn resolve_alarm_days_prefers_bitmask() {
        let alarm = serde_json::json!({
            "days": "0,1,2,3,4",
            "days_of_week": "0000011"
        });
        assert_eq!(resolve_alarm_days(&alarm), vec![5, 6]);
    }

    #[test]
    fn resolve_alarm_days_falls_back_to_legacy() {
        let alarm = serde_json::json!({
            "days": "weekends"
        });
        assert_eq!(resolve_alarm_days(&alarm), vec![5, 6]);
    }

    #[test]
    fn parse_multi_zone_ids_valid() {
        let alarm = serde_json::json!({ "multi_zone_ids": "[1,3,5]" });
        assert_eq!(parse_multi_zone_ids(&alarm), vec![1, 3, 5]);
    }

    #[test]
    fn parse_multi_zone_ids_empty() {
        let alarm = serde_json::json!({});
        assert!(parse_multi_zone_ids(&alarm).is_empty());
    }

    #[test]
    fn easter_2024() {
        assert_eq!(easter(2024), (3, 31)); // March 31
    }

    #[test]
    fn easter_2025() {
        assert_eq!(easter(2025), (4, 20)); // April 20
    }

    #[test]
    fn french_holidays() {
        assert!(is_french_holiday(2025, 1, 1));
        assert!(is_french_holiday(2025, 5, 1));
        assert!(is_french_holiday(2025, 7, 14));
        assert!(is_french_holiday(2025, 12, 25));
        // Easter Monday 2025 = April 21
        assert!(is_french_holiday(2025, 4, 21));
        // Regular day
        assert!(!is_french_holiday(2025, 3, 15));
    }
}
