use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{info, warn};

const MAX_ALERTS: usize = 50;
const STALL_THRESHOLD_S: u64 = 30;
const ERROR_SPIKE_WINDOW_S: u64 = 300;
const ERROR_SPIKE_THRESHOLD: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertLevel {
    Ok,
    Warning,
    Critical,
}

impl AlertLevel {
    fn severity(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Warning => 1,
            Self::Critical => 2,
        }
    }
}

fn worst(a: AlertLevel, b: AlertLevel) -> AlertLevel {
    if b.severity() > a.severity() { b } else { a }
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthAlert {
    pub timestamp: String,
    pub level: AlertLevel,
    pub category: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    pub status: AlertLevel,
    #[serde(flatten)]
    pub details: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub status: AlertLevel,
    pub uptime_seconds: u64,
    pub checks: std::collections::HashMap<String, CheckResult>,
    pub alerts: Vec<HealthAlert>,
}

pub struct HealthMonitorConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub memory_warning_mb: u64,
    pub memory_critical_mb: u64,
    pub disk_warning_gb: u64,
    pub disk_critical_gb: u64,
    pub db_path: String,
}

impl Default for HealthMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 60,
            memory_warning_mb: 500,
            memory_critical_mb: 1024,
            disk_warning_gb: 5,
            disk_critical_gb: 1,
            db_path: "tune_server.db".into(),
        }
    }
}

pub struct AdvancedHealthMonitor {
    config: HealthMonitorConfig,
    start_time: Instant,
    alerts: Mutex<VecDeque<HealthAlert>>,
    last_status: Mutex<AlertLevel>,
    zone_positions: Mutex<std::collections::HashMap<i64, (i64, Instant)>>,
}

impl AdvancedHealthMonitor {
    pub fn new(config: HealthMonitorConfig) -> Self {
        Self {
            config,
            start_time: Instant::now(),
            alerts: Mutex::new(VecDeque::with_capacity(MAX_ALERTS)),
            last_status: Mutex::new(AlertLevel::Ok),
            zone_positions: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    pub async fn status(&self) -> AlertLevel {
        *self.last_status.lock().await
    }

    pub async fn alerts(&self) -> Vec<HealthAlert> {
        self.alerts.lock().await.iter().cloned().collect()
    }

    pub async fn run_checks(&self) -> HealthReport {
        let mut checks = std::collections::HashMap::new();
        let mut overall = AlertLevel::Ok;

        let mem = self.check_memory();
        overall = worst(overall, mem.status);
        checks.insert("memory".into(), mem);

        let disk = self.check_disk();
        overall = worst(overall, disk.status);
        checks.insert("disk".into(), disk);

        *self.last_status.lock().await = overall;

        HealthReport {
            status: overall,
            uptime_seconds: self.uptime_seconds(),
            checks,
            alerts: self.alerts().await,
        }
    }

    pub fn check_memory(&self) -> CheckResult {
        let rss_mb = get_rss_mb().unwrap_or(0.0);
        let status = if rss_mb >= self.config.memory_critical_mb as f64 {
            AlertLevel::Critical
        } else if rss_mb >= self.config.memory_warning_mb as f64 {
            AlertLevel::Warning
        } else {
            AlertLevel::Ok
        };

        if status != AlertLevel::Ok {
            let msg = format!("Mémoire: {rss_mb:.0}MB");
            let _ = self.add_alert_sync(status, "memory", &msg);
        }

        CheckResult {
            status,
            details: serde_json::json!({
                "value_mb": rss_mb,
                "threshold_mb": self.config.memory_critical_mb,
            }),
        }
    }

    pub fn check_disk(&self) -> CheckResult {
        let free_gb = disk_free_gb(&self.config.db_path).unwrap_or(0.0);
        let status = if free_gb < self.config.disk_critical_gb as f64 {
            AlertLevel::Critical
        } else if free_gb < self.config.disk_warning_gb as f64 {
            AlertLevel::Warning
        } else {
            AlertLevel::Ok
        };

        if status != AlertLevel::Ok {
            let msg = format!("Disque: {free_gb:.1}Go restants");
            let _ = self.add_alert_sync(status, "disk", &msg);
        }

        CheckResult {
            status,
            details: serde_json::json!({ "free_gb": free_gb }),
        }
    }

    pub async fn check_playback_stall(
        &self,
        zones: &[(i64, bool, i64)], // (zone_id, is_playing, position_ms)
    ) -> CheckResult {
        let now = Instant::now();
        let mut positions = self.zone_positions.lock().await;
        let mut stalled = Vec::new();

        for &(zone_id, is_playing, pos_ms) in zones {
            if is_playing {
                if let Some(&(prev_pos, prev_time)) = positions.get(&zone_id) {
                    let elapsed = now.duration_since(prev_time).as_secs();
                    if elapsed >= STALL_THRESHOLD_S && pos_ms == prev_pos {
                        stalled.push(zone_id);
                    }
                }
                positions.insert(zone_id, (pos_ms, now));
            } else {
                positions.remove(&zone_id);
            }
        }

        let status = if stalled.is_empty() {
            AlertLevel::Ok
        } else {
            AlertLevel::Warning
        };

        if !stalled.is_empty() {
            let msg = format!("Lecture bloquée sur zones: {:?}", stalled);
            self.add_alert(status, "playback", &msg).await;
        }

        CheckResult {
            status,
            details: serde_json::json!({ "stalled_zones": stalled }),
        }
    }

    pub async fn check_error_spike(&self, error_timestamps: &[u64]) -> CheckResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff = now.saturating_sub(ERROR_SPIKE_WINDOW_S);
        let recent_count = error_timestamps.iter().filter(|&&ts| ts >= cutoff).count();

        let status = if recent_count > ERROR_SPIKE_THRESHOLD {
            AlertLevel::Warning
        } else {
            AlertLevel::Ok
        };

        if status != AlertLevel::Ok {
            let msg =
                format!("{recent_count} erreurs en 5 minutes (seuil: {ERROR_SPIKE_THRESHOLD})");
            self.add_alert(status, "errors", &msg).await;
        }

        CheckResult {
            status,
            details: serde_json::json!({ "count_5min": recent_count }),
        }
    }

    async fn add_alert(&self, level: AlertLevel, category: &str, message: &str) {
        let alert = HealthAlert {
            timestamp: now_iso(),
            level,
            category: category.into(),
            message: message.into(),
        };
        let mut alerts = self.alerts.lock().await;
        if alerts.len() >= MAX_ALERTS {
            alerts.pop_front();
        }
        alerts.push_back(alert);
    }

    fn add_alert_sync(&self, level: AlertLevel, category: &str, message: &str) -> bool {
        let alert = HealthAlert {
            timestamp: now_iso(),
            level,
            category: category.into(),
            message: message.into(),
        };
        if let Ok(mut alerts) = self.alerts.try_lock() {
            if alerts.len() >= MAX_ALERTS {
                alerts.pop_front();
            }
            alerts.push_back(alert);
            true
        } else {
            false
        }
    }
}

fn now_iso() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("{secs}"))
}

fn get_rss_mb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    if line.starts_with("VmRSS:") {
                        line.split_whitespace()
                            .nth(1)
                            .and_then(|s| s.parse::<f64>().ok())
                            .map(|kb| kb / 1024.0)
                    } else {
                        None
                    }
                })
            })
    }
    #[cfg(target_os = "macos")]
    {
        use std::process::Command;
        let pid = std::process::id();
        Command::new("ps")
            .args(["-o", "rss=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<f64>()
                    .ok()
                    .map(|kb| kb / 1024.0)
            })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

fn disk_free_gb(path: &str) -> Option<f64> {
    let parent = std::path::Path::new(path)
        .parent()
        .unwrap_or(std::path::Path::new("."));
    let output = std::process::Command::new("df")
        .arg("-k")
        .arg(parent)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().nth(1)?;
    let avail_kb: f64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_status() {
        assert_eq!(
            worst(AlertLevel::Ok, AlertLevel::Warning),
            AlertLevel::Warning
        );
        assert_eq!(
            worst(AlertLevel::Warning, AlertLevel::Ok),
            AlertLevel::Warning
        );
        assert_eq!(
            worst(AlertLevel::Warning, AlertLevel::Critical),
            AlertLevel::Critical
        );
        assert_eq!(worst(AlertLevel::Ok, AlertLevel::Ok), AlertLevel::Ok);
    }

    #[test]
    fn memory_check_ok() {
        let monitor = AdvancedHealthMonitor::new(HealthMonitorConfig::default());
        let result = monitor.check_memory();
        assert_eq!(result.status, AlertLevel::Ok);
    }

    #[test]
    fn uptime_works() {
        let monitor = AdvancedHealthMonitor::new(HealthMonitorConfig::default());
        assert!(monitor.uptime_seconds() < 2);
    }

    #[tokio::test]
    async fn error_spike_below_threshold() {
        let monitor = AdvancedHealthMonitor::new(HealthMonitorConfig::default());
        let timestamps = vec![0, 1, 2]; // old timestamps
        let result = monitor.check_error_spike(&timestamps).await;
        assert_eq!(result.status, AlertLevel::Ok);
    }

    #[tokio::test]
    async fn error_spike_above_threshold() {
        let monitor = AdvancedHealthMonitor::new(HealthMonitorConfig::default());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let timestamps: Vec<u64> = (0..15).map(|i| now - i).collect();
        let result = monitor.check_error_spike(&timestamps).await;
        assert_eq!(result.status, AlertLevel::Warning);
    }

    #[tokio::test]
    async fn playback_stall_detection() {
        let monitor = AdvancedHealthMonitor::new(HealthMonitorConfig::default());
        let zones = vec![(1, true, 5000)];
        let r1 = monitor.check_playback_stall(&zones).await;
        assert_eq!(r1.status, AlertLevel::Ok);
    }

    #[tokio::test]
    async fn run_checks_returns_report() {
        let monitor = AdvancedHealthMonitor::new(HealthMonitorConfig::default());
        let report = monitor.run_checks().await;
        assert!(report.checks.contains_key("memory"));
        assert!(report.checks.contains_key("disk"));
    }
}
