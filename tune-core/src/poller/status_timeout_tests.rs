use super::*;

/// Output whose `get_status` never returns — models a transport doing
/// blocking I/O against a dead device (Chromecast gone mid-connection).
struct HungOutput;

#[async_trait::async_trait]
impl OutputTarget for HungOutput {
    fn name(&self) -> &str {
        "hung"
    }
    fn device_id(&self) -> &str {
        "hung"
    }
    fn output_type(&self) -> &str {
        "test"
    }
    async fn pause(&self) -> Result<(), String> {
        Err("n/a".into())
    }
    async fn resume(&self) -> Result<(), String> {
        Err("n/a".into())
    }
    async fn stop(&self) -> Result<(), String> {
        Err("n/a".into())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("n/a".into())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Err("n/a".into())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Err("n/a".into())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        std::future::pending::<()>().await;
        unreachable!()
    }
    async fn is_available(&self) -> bool {
        true
    }
}

/// Output that answers immediately.
struct FastOutput;

#[async_trait::async_trait]
impl OutputTarget for FastOutput {
    fn name(&self) -> &str {
        "fast"
    }
    fn device_id(&self) -> &str {
        "fast"
    }
    fn output_type(&self) -> &str {
        "test"
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, _volume: f64) -> Result<(), String> {
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Playing,
            ..Default::default()
        })
    }
    fn signal_path_status(&self) -> Option<OutputSignalPathStatus> {
        Some(OutputSignalPathStatus {
            bit_perfect: true,
            sample_transport: crate::outputs::traits::OutputSampleTransport::NativeInteger,
            dsp: crate::outputs::traits::OutputDspState::Inactive,
            volume: crate::outputs::traits::OutputVolumeState::Unity,
            reasons: Vec::new(),
        })
    }
    fn dsp_metrics(&self) -> Option<OutputDspMetrics> {
        Some(OutputDspMetrics {
            eq_overs: 3,
            eq_non_finite_samples: 1,
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

fn arc(output: Box<dyn OutputTarget>) -> Arc<Mutex<Box<dyn OutputTarget>>> {
    Arc::new(Mutex::new(output))
}

#[tokio::test]
async fn hung_transport_times_out_instead_of_stalling_the_poller() {
    let out = arc(Box::new(HungOutput));
    let res = get_status_with_signal_path_bounded(&out, Some(Duration::from_millis(50))).await;
    let err = res.expect_err("a hung get_status must yield an error, not block");
    assert!(err.contains("timed out"), "unexpected error: {err}");
}

#[tokio::test]
async fn hung_lock_holder_times_out_too() {
    // An orchestrator call stuck inside the output holds its lock; the
    // poller must not wait behind it forever.
    let out = arc(Box::new(FastOutput));
    let _held = out.lock().await;
    let res = get_status_with_signal_path_bounded(&out, Some(Duration::from_millis(50))).await;
    assert!(res.is_err(), "a held output lock must not stall the poller");
}

#[tokio::test]
async fn healthy_transport_passes_through() {
    let out = arc(Box::new(FastOutput));
    let (status, signal_path, dsp_metrics) =
        get_status_with_signal_path_bounded(&out, Some(Duration::from_secs(5)))
            .await
            .unwrap();
    assert_eq!(status.state, TransportState::Playing);
    assert_eq!(signal_path.unwrap().bit_perfect, true);
    assert_eq!(dsp_metrics.unwrap().eq_overs, 3);
}

#[tokio::test]
async fn timeout_disabled_preserves_unbounded_behavior() {
    // TUNE_POLLER_STATUS_TIMEOUT_SECS=0 → rollback to the pre-fix path.
    let out = arc(Box::new(FastOutput));
    let (status, signal_path, dsp_metrics) = get_status_with_signal_path_bounded(&out, None)
        .await
        .unwrap();
    assert_eq!(status.state, TransportState::Playing);
    assert!(signal_path.is_some());
    assert_eq!(dsp_metrics.unwrap().eq_non_finite_samples, 1);
}
