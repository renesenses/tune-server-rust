use std::time::Duration;

use tokio::sync::Mutex;
use tracing::info;

use crate::outputs::OutputTarget;

pub struct CrossfadeHandler {
    pub enabled: bool,
    pub duration_s: f64,
    original_volume: Mutex<Option<f64>>,
    fading: Mutex<bool>,
}

impl CrossfadeHandler {
    pub fn new(enabled: bool, duration_s: f64) -> Self {
        Self {
            enabled,
            duration_s,
            original_volume: Mutex::new(None),
            fading: Mutex::new(false),
        }
    }

    pub async fn should_start_crossfade(
        &self,
        position_ms: i64,
        duration_ms: i64,
        is_radio: bool,
    ) -> bool {
        if !self.enabled || self.duration_s <= 0.0 || is_radio || duration_ms <= 0 {
            return false;
        }
        let remaining_ms = duration_ms - position_ms;
        let threshold_ms = (self.duration_s * 1000.0) as i64;
        remaining_ms <= threshold_ms && remaining_ms > (threshold_ms - 500)
    }

    pub async fn start_fade_out(&self, output: &dyn OutputTarget) -> Result<(), String> {
        {
            let fading = self.fading.lock().await;
            if *fading {
                return Ok(());
            }
        }

        let status = output.get_status().await?;
        let current_vol = status.volume;
        *self.original_volume.lock().await = Some(current_vol);
        *self.fading.lock().await = true;

        info!(
            device = output.name(),
            from_vol = current_vol,
            duration_s = self.duration_s,
            "crossfade_fade_out_start"
        );

        fade_volume(output, current_vol, 0.0, self.duration_s).await;
        Ok(())
    }

    pub async fn finish_fade_in(&self, output: &dyn OutputTarget) -> Result<(), String> {
        let target_vol = {
            let mut orig = self.original_volume.lock().await;
            match orig.take() {
                Some(v) => v,
                None => return Ok(()),
            }
        };

        info!(
            device = output.name(),
            to_vol = target_vol,
            duration_s = self.duration_s,
            "crossfade_fade_in_start"
        );

        fade_volume(output, 0.0, target_vol, self.duration_s).await;
        *self.fading.lock().await = false;

        info!(
            device = output.name(),
            restored_volume = target_vol,
            "crossfade_complete"
        );
        Ok(())
    }

    pub async fn cancel(&self, output: Option<&dyn OutputTarget>) {
        *self.fading.lock().await = false;
        let orig = self.original_volume.lock().await.take();
        if let (Some(vol), Some(out)) = (orig, output) {
            let _ = out.set_volume(vol).await;
        }
    }

    pub async fn is_fading(&self) -> bool {
        *self.fading.lock().await
    }
}

async fn fade_volume(output: &dyn OutputTarget, from: f64, to: f64, duration_s: f64) {
    if duration_s <= 0.0 {
        let _ = output.set_volume(to).await;
        return;
    }
    let steps = ((duration_s * 10.0) as u64).max(1);
    let step_delay = Duration::from_millis(((duration_s * 1000.0) / steps as f64) as u64);
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let vol = from + (to - from) * t;
        if output.set_volume(vol).await.is_err() {
            break;
        }
        tokio::time::sleep(step_delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_not_crossfade_when_disabled() {
        let cf = CrossfadeHandler::new(false, 5.0);
        assert!(!cf.should_start_crossfade(170000, 180000, false).await);
    }

    #[tokio::test]
    async fn should_not_crossfade_radio() {
        let cf = CrossfadeHandler::new(true, 5.0);
        assert!(!cf.should_start_crossfade(170000, 180000, true).await);
    }

    #[tokio::test]
    async fn should_crossfade_in_window() {
        let cf = CrossfadeHandler::new(true, 5.0);
        // 4800ms remaining, threshold=5000ms, within 500ms window
        assert!(cf.should_start_crossfade(175200, 180000, false).await);
    }

    #[tokio::test]
    async fn should_not_crossfade_too_early() {
        let cf = CrossfadeHandler::new(true, 5.0);
        // 10s remaining, threshold=5s
        assert!(!cf.should_start_crossfade(170000, 180000, false).await);
    }

    #[tokio::test]
    async fn should_not_crossfade_past_window() {
        let cf = CrossfadeHandler::new(true, 5.0);
        // 4000ms remaining — past the 500ms detection window
        assert!(!cf.should_start_crossfade(176000, 180000, false).await);
    }
}
