use std::time::Instant;

use tracing::{info, warn};

use crate::outputs::OutputTarget;

const LOCAL_LATENCY_MS: i64 = 10;
const DEFAULT_SAMPLES: usize = 5;
const SAMPLE_DELAY_MS: u64 = 100;

pub async fn measure_output_latency(output: &dyn OutputTarget, samples: usize) -> Option<i64> {
    let output_type = output.output_type();
    if output_type == "local" {
        return Some(LOCAL_LATENCY_MS);
    }

    let n = if samples == 0 {
        DEFAULT_SAMPLES
    } else {
        samples
    };
    let mut latencies = Vec::with_capacity(n);

    for _ in 0..n {
        let start = Instant::now();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), output.get_status()).await;
        match result {
            Ok(Ok(_)) => {
                let rtt_ms = start.elapsed().as_millis() as i64;
                latencies.push(rtt_ms / 2);
            }
            Ok(Err(e)) => {
                warn!(error = %e, "latency_probe_error");
            }
            Err(_) => {
                warn!("latency_probe_timeout");
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(SAMPLE_DELAY_MS)).await;
    }

    if latencies.is_empty() {
        return None;
    }

    latencies.sort();
    let median = latencies[latencies.len() / 2];
    let min = *latencies.first().unwrap();
    let max = *latencies.last().unwrap();

    info!(
        device = output.name(),
        median_ms = median,
        min_ms = min,
        max_ms = max,
        samples = latencies.len(),
        "latency_measured"
    );

    Some(median)
}

pub async fn auto_calibrate(
    leader: &dyn OutputTarget,
    followers: &[&dyn OutputTarget],
) -> Vec<(String, i64)> {
    let leader_latency = match measure_output_latency(leader, DEFAULT_SAMPLES).await {
        Some(l) => l,
        None => {
            warn!(device = leader.name(), "leader_latency_failed");
            return Vec::new();
        }
    };

    let mut results = Vec::new();
    for &follower in followers {
        let follower_latency = match measure_output_latency(follower, DEFAULT_SAMPLES).await {
            Some(l) => l,
            None => {
                warn!(device = follower.name(), "follower_latency_failed");
                continue;
            }
        };
        let offset = leader_latency - follower_latency;
        info!(
            follower = follower.name(),
            offset_ms = offset,
            leader_ms = leader_latency,
            follower_ms = follower_latency,
            "latency_calibrated"
        );
        results.push((follower.device_id().to_string(), offset));
    }

    results
}

#[derive(Debug, Clone)]
pub struct ZoneHealth {
    pub zone_id: i64,
    pub name: String,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub position_ok: bool,
}

pub async fn check_zone_health(zone_id: i64, name: &str, output: &dyn OutputTarget) -> ZoneHealth {
    let start = Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_secs(3), output.get_status()).await;

    match result {
        Ok(Ok(status)) => {
            let latency_ms = start.elapsed().as_millis() as i64;
            let degraded = latency_ms > 500;
            ZoneHealth {
                zone_id,
                name: name.to_string(),
                status: if degraded {
                    "degraded".into()
                } else {
                    "online".into()
                },
                latency_ms: Some(latency_ms),
                position_ok: status.position_ms > 0
                    || status.state == crate::outputs::TransportState::Stopped,
            }
        }
        Ok(Err(_)) | Err(_) => ZoneHealth {
            zone_id,
            name: name.to_string(),
            status: "offline".into(),
            latency_ms: None,
            position_ok: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_latency_constant() {
        assert_eq!(LOCAL_LATENCY_MS, 10);
    }
}
