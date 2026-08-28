use std::time::Instant;

use tracing::{info, warn};

use crate::outputs::OutputTarget;

const DEFAULT_SAMPLES: usize = 5;
const SAMPLE_DELAY_MS: u64 = 100;

/// Statistiques du trajet de COMMANDE vers une sortie.
///
/// Elles ne décrivent pas la latence audio : aucun timestamp de présentation,
/// tampon matériel ni délai acoustique n'entre dans cette mesure (#2215).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRttStats {
    pub samples: usize,
    pub min_ms: i64,
    pub p50_ms: i64,
    pub p95_ms: i64,
    pub p99_ms: i64,
    pub max_ms: i64,
    pub uncertainty_ms: i64,
}

fn percentile(sorted: &[i64], percent: usize) -> i64 {
    let rank = (percent * sorted.len()).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn control_rtt_stats(mut samples: Vec<i64>) -> Option<ControlRttStats> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let min_ms = samples[0];
    let max_ms = samples[samples.len() - 1];
    Some(ControlRttStats {
        samples: samples.len(),
        min_ms,
        p50_ms: percentile(&samples, 50),
        p95_ms: percentile(&samples, 95),
        p99_ms: percentile(&samples, 99),
        max_ms,
        uncertainty_ms: max_ms - min_ms,
    })
}

pub async fn measure_control_rtt(
    output: &dyn OutputTarget,
    samples: usize,
) -> Option<ControlRttStats> {
    let n = if samples == 0 {
        DEFAULT_SAMPLES
    } else {
        samples
    };
    let mut round_trips = Vec::with_capacity(n);

    for index in 0..n {
        let start = Instant::now();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(2), output.get_status()).await;
        match result {
            Ok(Ok(_)) => {
                let rtt_ms = start.elapsed().as_millis() as i64;
                round_trips.push(rtt_ms);
            }
            Ok(Err(e)) => {
                warn!(error = %e, "control_rtt_probe_error");
            }
            Err(_) => {
                warn!("control_rtt_probe_timeout");
            }
        }
        if index + 1 < n {
            tokio::time::sleep(std::time::Duration::from_millis(SAMPLE_DELAY_MS)).await;
        }
    }

    let stats = control_rtt_stats(round_trips)?;

    info!(
        device = output.name(),
        p50_ms = stats.p50_ms,
        p95_ms = stats.p95_ms,
        p99_ms = stats.p99_ms,
        min_ms = stats.min_ms,
        max_ms = stats.max_ms,
        uncertainty_ms = stats.uncertainty_ms,
        samples = stats.samples,
        "control_rtt_measured"
    );

    Some(stats)
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
    fn le_rtt_de_controle_n_est_jamais_divise_en_latence_audio() {
        let stats = control_rtt_stats(vec![20, 30, 40, 50, 60]).unwrap();

        assert_eq!(stats.min_ms, 20);
        assert_eq!(stats.p50_ms, 40);
        assert_eq!(stats.p95_ms, 60);
        assert_eq!(stats.p99_ms, 60);
        assert_eq!(stats.max_ms, 60);
        assert_eq!(stats.uncertainty_ms, 40);
    }

    #[test]
    fn aucune_reponse_ne_devient_pas_une_mesure_a_zero() {
        assert_eq!(control_rtt_stats(Vec::new()), None);
    }
}
