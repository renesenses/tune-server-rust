use std::collections::VecDeque;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::info;

const MAX_ERRORS: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub struct SystemHealth {
    pub uptime_seconds: u64,
    pub version: String,
    pub platform: String,
    pub memory_usage_mb: Option<f64>,
    pub cpu_count: usize,
    pub disk_free_mb: Option<u64>,
}

pub struct HealthMonitor {
    start_time: Instant,
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self {
            start_time: Instant::now(),
        }
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    pub fn snapshot(&self) -> SystemHealth {
        let platform = if cfg!(target_os = "macos") {
            "macos"
        } else if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "windows") {
            "windows"
        } else {
            "unknown"
        };

        SystemHealth {
            uptime_seconds: self.uptime_seconds(),
            version: crate::version().to_string(),
            platform: platform.into(),
            memory_usage_mb: get_memory_usage_mb(),
            cpu_count: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
            disk_free_mb: None,
        }
    }
}

fn get_memory_usage_mb() -> Option<f64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if line.starts_with("VmRSS:") {
                    let kb: f64 = line
                        .split_whitespace()
                        .nth(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    return Some(kb / 1024.0);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorRecord {
    pub timestamp: u64,
    pub module: String,
    pub message: String,
    pub level: String,
}

pub struct ErrorBuffer {
    errors: Mutex<VecDeque<ErrorRecord>>,
    max_size: usize,
}

impl Default for ErrorBuffer {
    fn default() -> Self {
        Self::new(MAX_ERRORS)
    }
}

impl ErrorBuffer {
    pub fn new(max_size: usize) -> Self {
        Self {
            errors: Mutex::new(VecDeque::with_capacity(max_size)),
            max_size,
        }
    }

    pub async fn record(&self, module: &str, message: &str, level: &str) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut errors = self.errors.lock().await;
        if errors.len() >= self.max_size {
            errors.pop_front();
        }
        errors.push_back(ErrorRecord {
            timestamp,
            module: module.into(),
            message: message.into(),
            level: level.into(),
        });
    }

    pub async fn recent(&self, limit: usize) -> Vec<ErrorRecord> {
        let errors = self.errors.lock().await;
        errors.iter().rev().take(limit).cloned().collect()
    }

    pub async fn count(&self) -> usize {
        self.errors.lock().await.len()
    }

    pub async fn clear(&self) {
        self.errors.lock().await.clear();
    }

    pub async fn since(&self, timestamp: u64) -> Vec<ErrorRecord> {
        let errors = self.errors.lock().await;
        errors
            .iter()
            .filter(|e| e.timestamp >= timestamp)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_monitor_uptime() {
        let monitor = HealthMonitor::new();
        assert!(monitor.uptime_seconds() < 2);
    }

    #[test]
    fn health_snapshot() {
        let monitor = HealthMonitor::new();
        let snap = monitor.snapshot();
        assert!(!snap.version.is_empty());
        assert!(snap.cpu_count >= 1);
    }

    #[tokio::test]
    async fn error_buffer_record() {
        let buf = ErrorBuffer::new(10);
        buf.record("test", "something failed", "error").await;
        assert_eq!(buf.count().await, 1);

        let recent = buf.recent(5).await;
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].module, "test");
    }

    #[tokio::test]
    async fn error_buffer_overflow() {
        let buf = ErrorBuffer::new(3);
        for i in 0..5 {
            buf.record("mod", &format!("error {i}"), "error").await;
        }
        assert_eq!(buf.count().await, 3);
        let recent = buf.recent(10).await;
        assert_eq!(recent[0].message, "error 4");
    }

    #[tokio::test]
    async fn error_buffer_clear() {
        let buf = ErrorBuffer::new(10);
        buf.record("mod", "test", "warn").await;
        buf.clear().await;
        assert_eq!(buf.count().await, 0);
    }

    #[tokio::test]
    async fn error_buffer_since() {
        let buf = ErrorBuffer::new(10);
        buf.record("a", "old", "error").await;

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        buf.record("b", "new", "error").await;
        let filtered = buf.since(now).await;
        assert!(filtered.iter().all(|e| e.timestamp >= now));
    }
}
