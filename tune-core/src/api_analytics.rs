use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;

const DEFAULT_CAPACITY: usize = 1000;

#[derive(Debug, Clone)]
struct RequestRecord {
    endpoint: String,
    method: String,
    status_code: u16,
    latency_ms: u32,
    _timestamp: Instant,
}

#[derive(Debug, Clone, Serialize)]
pub struct EndpointStats {
    pub endpoint: String,
    pub count: u64,
    pub error_count: u64,
    pub avg_latency_ms: f64,
    pub p50_latency_ms: u32,
    pub p95_latency_ms: u32,
    pub p99_latency_ms: u32,
    pub max_latency_ms: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiStats {
    pub total_requests: u64,
    pub total_errors: u64,
    pub error_rate_pct: f64,
    pub buffer_size: usize,
    pub top_endpoints: Vec<EndpointStats>,
    pub slowest_endpoints: Vec<EndpointStats>,
}

pub struct ApiAnalytics {
    records: Mutex<VecDeque<RequestRecord>>,
    capacity: usize,
}

impl ApiAnalytics {
    pub fn new(capacity: usize) -> Self {
        Self {
            records: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn record(&self, endpoint: &str, method: &str, status_code: u16, latency_ms: u32) {
        let record = RequestRecord {
            endpoint: endpoint.to_string(),
            method: method.to_string(),
            status_code,
            latency_ms,
            _timestamp: Instant::now(),
        };
        let mut records = self.records.lock().unwrap();
        if records.len() >= self.capacity {
            records.pop_front();
        }
        records.push_back(record);
    }

    pub fn stats(&self) -> ApiStats {
        let records = self.records.lock().unwrap();

        let total_requests = records.len() as u64;
        let total_errors = records.iter().filter(|r| r.status_code >= 400).count() as u64;
        let error_rate_pct = if total_requests > 0 {
            (total_errors as f64 / total_requests as f64 * 100.0 * 10.0).round() / 10.0
        } else {
            0.0
        };

        let mut endpoint_map: std::collections::HashMap<String, Vec<&RequestRecord>> =
            std::collections::HashMap::new();
        for r in records.iter() {
            endpoint_map
                .entry(format!("{} {}", r.method, r.endpoint))
                .or_default()
                .push(r);
        }

        let mut all_stats: Vec<EndpointStats> = endpoint_map
            .into_iter()
            .map(|(endpoint, reqs)| {
                let count = reqs.len() as u64;
                let error_count = reqs.iter().filter(|r| r.status_code >= 400).count() as u64;
                let mut latencies: Vec<u32> = reqs.iter().map(|r| r.latency_ms).collect();
                latencies.sort_unstable();
                let avg = if !latencies.is_empty() {
                    latencies.iter().map(|l| *l as f64).sum::<f64>() / latencies.len() as f64
                } else {
                    0.0
                };

                EndpointStats {
                    endpoint,
                    count,
                    error_count,
                    avg_latency_ms: (avg * 10.0).round() / 10.0,
                    p50_latency_ms: percentile(&latencies, 50),
                    p95_latency_ms: percentile(&latencies, 95),
                    p99_latency_ms: percentile(&latencies, 99),
                    max_latency_ms: latencies.last().copied().unwrap_or(0),
                }
            })
            .collect();

        let mut top_endpoints = all_stats.clone();
        top_endpoints.sort_by(|a, b| b.count.cmp(&a.count));
        top_endpoints.truncate(10);

        all_stats.sort_by(|a, b| b.p95_latency_ms.cmp(&a.p95_latency_ms));
        all_stats.truncate(10);

        ApiStats {
            total_requests,
            total_errors,
            error_rate_pct,
            buffer_size: records.len(),
            top_endpoints,
            slowest_endpoints: all_stats,
        }
    }
}

impl Default for ApiAnalytics {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

fn percentile(sorted: &[u32], pct: u32) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len() as f64 * pct as f64 / 100.0).ceil() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_stats() {
        let analytics = ApiAnalytics::new(100);
        analytics.record("/api/v1/system/stats", "GET", 200, 5);
        analytics.record("/api/v1/system/stats", "GET", 200, 10);
        analytics.record("/api/v1/zones", "GET", 200, 3);
        analytics.record("/api/v1/zones", "POST", 400, 15);

        let stats = analytics.stats();
        assert_eq!(stats.total_requests, 4);
        assert_eq!(stats.total_errors, 1);
        assert!(stats.error_rate_pct > 0.0);
        assert!(!stats.top_endpoints.is_empty());
    }

    #[test]
    fn ring_buffer_eviction() {
        let analytics = ApiAnalytics::new(3);
        analytics.record("/a", "GET", 200, 1);
        analytics.record("/b", "GET", 200, 2);
        analytics.record("/c", "GET", 200, 3);
        analytics.record("/d", "GET", 200, 4);

        let stats = analytics.stats();
        assert_eq!(stats.buffer_size, 3);
        assert!(stats
            .top_endpoints
            .iter()
            .all(|e| e.endpoint != "GET /a"));
    }

    #[test]
    fn percentile_computation() {
        let analytics = ApiAnalytics::new(100);
        for i in 1..=100 {
            analytics.record("/test", "GET", 200, i);
        }
        let stats = analytics.stats();
        let ep = &stats.top_endpoints[0];
        assert_eq!(ep.count, 100);
        assert!((49..=51).contains(&ep.p50_latency_ms), "p50={}", ep.p50_latency_ms);
        assert!(ep.p95_latency_ms >= 94);
        assert!(ep.p99_latency_ms >= 98);
        assert_eq!(ep.max_latency_ms, 100);
    }

    #[test]
    fn empty_stats() {
        let analytics = ApiAnalytics::new(100);
        let stats = analytics.stats();
        assert_eq!(stats.total_requests, 0);
        assert_eq!(stats.total_errors, 0);
        assert_eq!(stats.error_rate_pct, 0.0);
        assert!(stats.top_endpoints.is_empty());
    }
}
