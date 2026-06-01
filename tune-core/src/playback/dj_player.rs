use std::time::Instant;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::audio::mixer::PcmMixer;

const MIX_SAMPLE_RATE: u32 = 44100;
const MIX_BIT_DEPTH: u16 = 16;
const MIX_CHANNELS: u16 = 2;

#[derive(Debug, Clone, Serialize)]
pub struct DeckState {
    pub track_title: Option<String>,
    pub artist_name: Option<String>,
    pub duration_ms: i64,
    pub position_ms: i64,
    pub playing: bool,
    pub gain: f32,
    pub bpm: Option<f64>,
    pub tempo_ratio: f64,
}

impl Default for DeckState {
    fn default() -> Self {
        Self {
            track_title: None,
            artist_name: None,
            duration_ms: 0,
            position_ms: 0,
            playing: false,
            gain: 1.0,
            bpm: None,
            tempo_ratio: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deck {
    A,
    B,
}

pub struct DualDeckPlayer {
    zone_id: i64,
    deck_a: Mutex<DeckState>,
    deck_b: Mutex<DeckState>,
    crossfader: Mutex<f64>,
    mixer: PcmMixer,
    active: Mutex<bool>,
    play_start: Mutex<Option<(Deck, Instant)>>,
}

impl DualDeckPlayer {
    pub fn new(zone_id: i64) -> Self {
        Self {
            zone_id,
            deck_a: Mutex::new(DeckState::default()),
            deck_b: Mutex::new(DeckState::default()),
            crossfader: Mutex::new(0.5),
            mixer: PcmMixer::new(MIX_CHANNELS, MIX_BIT_DEPTH, MIX_SAMPLE_RATE),
            active: Mutex::new(false),
            play_start: Mutex::new(None),
        }
    }

    pub async fn load_track(
        &self,
        deck: Deck,
        title: &str,
        artist: Option<&str>,
        duration_ms: i64,
        bpm: Option<f64>,
    ) {
        let state = DeckState {
            track_title: Some(title.into()),
            artist_name: artist.map(Into::into),
            duration_ms,
            position_ms: 0,
            playing: false,
            gain: 1.0,
            bpm,
            tempo_ratio: 1.0,
        };

        match deck {
            Deck::A => *self.deck_a.lock().await = state,
            Deck::B => *self.deck_b.lock().await = state,
        }

        info!(zone_id = self.zone_id, deck = ?deck, title, "dj_track_loaded");
    }

    pub async fn play(&self, deck: Deck) {
        let state = match deck {
            Deck::A => &self.deck_a,
            Deck::B => &self.deck_b,
        };
        let mut s = state.lock().await;
        s.playing = true;
        *self.play_start.lock().await = Some((deck, Instant::now()));
        *self.active.lock().await = true;
        info!(zone_id = self.zone_id, deck = ?deck, "dj_deck_play");
    }

    pub async fn pause(&self, deck: Deck) {
        let state = match deck {
            Deck::A => &self.deck_a,
            Deck::B => &self.deck_b,
        };
        let mut s = state.lock().await;
        if s.playing
            && let Some((d, start)) = self.play_start.lock().await.take()
                && d == deck {
                    s.position_ms += start.elapsed().as_millis() as i64;
                }
        s.playing = false;
        debug!(zone_id = self.zone_id, deck = ?deck, "dj_deck_pause");
    }

    pub async fn stop(&self, deck: Deck) {
        let state = match deck {
            Deck::A => &self.deck_a,
            Deck::B => &self.deck_b,
        };
        let mut s = state.lock().await;
        s.playing = false;
        s.position_ms = 0;
        debug!(zone_id = self.zone_id, deck = ?deck, "dj_deck_stop");
    }

    pub async fn set_gain(&self, deck: Deck, gain: f32) {
        let state = match deck {
            Deck::A => &self.deck_a,
            Deck::B => &self.deck_b,
        };
        state.lock().await.gain = gain.clamp(0.0, 2.0);
    }

    pub async fn set_tempo(&self, deck: Deck, ratio: f64) {
        let state = match deck {
            Deck::A => &self.deck_a,
            Deck::B => &self.deck_b,
        };
        state.lock().await.tempo_ratio = ratio.clamp(0.5, 2.0);
    }

    pub async fn set_crossfader(&self, value: f64) {
        *self.crossfader.lock().await = value.clamp(0.0, 1.0);
    }

    pub async fn sync_bpm(&self) {
        let bpm_a = self.deck_a.lock().await.bpm;
        let bpm_b = self.deck_b.lock().await.bpm;

        if let (Some(a), Some(b)) = (bpm_a, bpm_b)
            && a > 0.0 && b > 0.0 {
                let ratio = a / b;
                self.deck_b.lock().await.tempo_ratio = ratio;
                info!(bpm_a = a, bpm_b = b, ratio, "dj_bpm_synced");
            }
    }

    pub fn compute_deck_gains(crossfader: f64) -> (f32, f32) {
        let gain_a = (1.0 - crossfader).min(1.0) as f32;
        let gain_b = crossfader.min(1.0) as f32;
        (gain_a, gain_b)
    }

    pub async fn mix_buffers(&self, buf_a: &[u8], buf_b: &[u8]) -> Vec<u8> {
        let cf = *self.crossfader.lock().await;
        let (cf_a, cf_b) = Self::compute_deck_gains(cf);

        let gain_a = self.deck_a.lock().await.gain * cf_a;
        let gain_b = self.deck_b.lock().await.gain * cf_b;

        self.mixer.mix_buffers(&[buf_a, buf_b], &[gain_a, gain_b])
    }

    pub async fn status(&self) -> serde_json::Value {
        let deck_a = self.deck_a.lock().await.clone();
        let deck_b = self.deck_b.lock().await.clone();
        let crossfader = *self.crossfader.lock().await;
        let active = *self.active.lock().await;

        serde_json::json!({
            "zone_id": self.zone_id,
            "active": active,
            "crossfader": crossfader,
            "deck_a": deck_a,
            "deck_b": deck_b,
        })
    }

    pub async fn stop_all(&self) {
        self.stop(Deck::A).await;
        self.stop(Deck::B).await;
        *self.active.lock().await = false;
        info!(zone_id = self.zone_id, "dj_mode_stopped");
    }

    pub fn mix_spec() -> (u32, u16, u16) {
        (MIX_SAMPLE_RATE, MIX_CHANNELS, MIX_BIT_DEPTH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn load_and_play() {
        let dj = DualDeckPlayer::new(1);
        dj.load_track(Deck::A, "Song A", Some("Artist A"), 180000, Some(120.0))
            .await;
        dj.play(Deck::A).await;

        let a = dj.deck_a.lock().await;
        assert!(a.playing);
        assert_eq!(a.track_title.as_deref(), Some("Song A"));
    }

    #[tokio::test]
    async fn crossfader_gains() {
        let (ga, gb) = DualDeckPlayer::compute_deck_gains(0.0);
        assert_eq!(ga, 1.0);
        assert_eq!(gb, 0.0);

        let (ga, gb) = DualDeckPlayer::compute_deck_gains(1.0);
        assert_eq!(ga, 0.0);
        assert_eq!(gb, 1.0);

        let (ga, gb) = DualDeckPlayer::compute_deck_gains(0.5);
        assert_eq!(ga, 0.5);
        assert_eq!(gb, 0.5);
    }

    #[tokio::test]
    async fn set_gain_clamped() {
        let dj = DualDeckPlayer::new(1);
        dj.set_gain(Deck::A, 3.0).await;
        assert_eq!(dj.deck_a.lock().await.gain, 2.0);

        dj.set_gain(Deck::A, -1.0).await;
        assert_eq!(dj.deck_a.lock().await.gain, 0.0);
    }

    #[tokio::test]
    async fn bpm_sync() {
        let dj = DualDeckPlayer::new(1);
        dj.load_track(Deck::A, "Fast", None, 180000, Some(140.0))
            .await;
        dj.load_track(Deck::B, "Slow", None, 200000, Some(100.0))
            .await;
        dj.sync_bpm().await;

        let b = dj.deck_b.lock().await;
        assert!((b.tempo_ratio - 1.4).abs() < 0.01);
    }

    #[tokio::test]
    async fn stop_all_resets() {
        let dj = DualDeckPlayer::new(1);
        dj.load_track(Deck::A, "Song", None, 180000, None).await;
        dj.play(Deck::A).await;
        dj.stop_all().await;

        assert!(!*dj.active.lock().await);
        assert!(!dj.deck_a.lock().await.playing);
    }

    #[tokio::test]
    async fn status_json() {
        let dj = DualDeckPlayer::new(42);
        let s = dj.status().await;
        assert_eq!(s["zone_id"], 42);
        assert_eq!(s["active"], false);
        assert_eq!(s["crossfader"], 0.5);
    }

    #[tokio::test]
    async fn mix_spec_values() {
        let (sr, ch, bd) = DualDeckPlayer::mix_spec();
        assert_eq!(sr, 44100);
        assert_eq!(ch, 2);
        assert_eq!(bd, 16);
    }
}
