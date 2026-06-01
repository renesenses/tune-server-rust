use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueTrack {
    pub id: Option<i64>,
    pub source_id: Option<String>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub album_id: Option<i64>,
    pub duration_ms: u64,
    pub file_path: Option<String>,
    pub cover_path: Option<String>,
    pub source: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub channels: Option<u16>,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
}

pub struct PlayQueue {
    tracks: Vec<QueueTrack>,
    position: i64,
    shuffle: bool,
    repeat: RepeatMode,
    shuffle_order: Vec<usize>,
    shuffle_index: i64,
}

impl PlayQueue {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            position: -1,
            shuffle: false,
            repeat: RepeatMode::Off,
            shuffle_order: Vec::new(),
            shuffle_index: -1,
        }
    }

    pub fn tracks(&self) -> &[QueueTrack] {
        &self.tracks
    }

    pub fn position(&self) -> i64 {
        self.position
    }

    pub fn length(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.repeat
    }

    pub fn current(&self) -> Option<&QueueTrack> {
        if self.shuffle && !self.shuffle_order.is_empty() {
            let idx = self.shuffle_index.max(0) as usize;
            self.shuffle_order
                .get(idx)
                .and_then(|&i| self.tracks.get(i))
        } else if self.position >= 0 {
            self.tracks.get(self.position as usize)
        } else {
            None
        }
    }

    pub fn set_tracks(&mut self, tracks: Vec<QueueTrack>, start_position: usize) {
        self.tracks = tracks;
        self.position = if self.tracks.is_empty() {
            -1
        } else {
            (start_position.min(self.tracks.len().saturating_sub(1))) as i64
        };
        if self.shuffle {
            self.regenerate_shuffle();
        }
    }

    pub fn add_tracks(&mut self, tracks: Vec<QueueTrack>, at_position: Option<usize>) {
        if let Some(pos) = at_position {
            let idx = pos.min(self.tracks.len());
            for (i, track) in tracks.into_iter().enumerate() {
                self.tracks.insert(idx + i, track);
            }
            if (idx as i64) <= self.position {
                self.position += (self.tracks.len() - idx) as i64;
            }
        } else {
            self.tracks.extend(tracks);
        }
        if self.shuffle {
            self.regenerate_shuffle();
        }
    }

    pub fn remove_track(&mut self, pos: usize) -> Option<QueueTrack> {
        if pos >= self.tracks.len() {
            return None;
        }
        let track = self.tracks.remove(pos);
        let pos_i = pos as i64;
        if pos_i < self.position {
            self.position -= 1;
        } else if pos_i == self.position {
            self.position = self
                .position
                .min(self.tracks.len().saturating_sub(1) as i64);
        }
        if self.shuffle {
            self.regenerate_shuffle();
        }
        Some(track)
    }

    pub fn move_track(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.tracks.len() || to >= self.tracks.len() {
            return false;
        }
        let track = self.tracks.remove(from);
        self.tracks.insert(to, track);

        let pos = self.position as usize;
        if pos == from {
            self.position = to as i64;
        } else if from < pos && pos <= to {
            self.position -= 1;
        } else if to <= pos && pos < from {
            self.position += 1;
        }

        if self.shuffle {
            self.regenerate_shuffle();
        }
        true
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.position = -1;
        self.shuffle_order.clear();
        self.shuffle_index = -1;
    }

    pub fn set_shuffle(&mut self, enabled: bool) {
        self.shuffle = enabled;
        if enabled {
            self.regenerate_shuffle();
        } else {
            self.shuffle_order.clear();
            self.shuffle_index = -1;
        }
    }

    pub fn set_repeat(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    pub fn next(&mut self) -> Option<&QueueTrack> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One {
            return self.current();
        }

        if self.shuffle {
            self.shuffle_index += 1;
            if self.shuffle_index as usize >= self.shuffle_order.len() {
                if self.repeat == RepeatMode::All {
                    self.regenerate_shuffle();
                    self.shuffle_index = 0;
                } else {
                    return None;
                }
            }
            self.position = self.shuffle_order[self.shuffle_index as usize] as i64;
        } else {
            self.position += 1;
            if self.position as usize >= self.tracks.len() {
                if self.repeat == RepeatMode::All {
                    self.position = 0;
                } else {
                    return None;
                }
            }
        }
        self.current()
    }

    pub fn previous(&mut self) -> Option<&QueueTrack> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.shuffle {
            self.shuffle_index = (self.shuffle_index - 1).max(0);
            self.position = self.shuffle_order[self.shuffle_index as usize] as i64;
        } else {
            self.position = (self.position - 1).max(0);
        }
        self.current()
    }

    pub fn peek_next(&self) -> Option<&QueueTrack> {
        if self.tracks.is_empty() {
            return None;
        }
        if self.repeat == RepeatMode::One {
            return self.current();
        }

        if self.shuffle {
            let next_idx = self.shuffle_index + 1;
            if next_idx as usize >= self.shuffle_order.len() {
                if self.repeat == RepeatMode::All {
                    return self.tracks.first();
                }
                return None;
            }
            self.shuffle_order
                .get(next_idx as usize)
                .and_then(|&i| self.tracks.get(i))
        } else {
            let next_pos = self.position + 1;
            if next_pos as usize >= self.tracks.len() {
                if self.repeat == RepeatMode::All {
                    return self.tracks.first();
                }
                return None;
            }
            self.tracks.get(next_pos as usize)
        }
    }

    pub fn jump_to(&mut self, pos: usize) -> Option<&QueueTrack> {
        if pos >= self.tracks.len() {
            return None;
        }
        self.position = pos as i64;
        if self.shuffle
            && let Some(idx) = self.shuffle_order.iter().position(|&i| i == pos) {
                self.shuffle_index = idx as i64;
            }
        self.current()
    }

    fn regenerate_shuffle(&mut self) {
        let len = self.tracks.len();
        if len == 0 {
            self.shuffle_order.clear();
            self.shuffle_index = -1;
            return;
        }

        let mut indices: Vec<usize> = (0..len).collect();
        let current_pos = if self.position >= 0 && (self.position as usize) < len {
            Some(self.position as usize)
        } else {
            None
        };

        if let Some(cur) = current_pos {
            indices.retain(|&i| i != cur);
            fisher_yates_shuffle(&mut indices);
            indices.insert(0, cur);
            self.shuffle_index = 0;
        } else {
            fisher_yates_shuffle(&mut indices);
            self.shuffle_index = 0;
        }

        self.shuffle_order = indices;
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "position": self.position,
            "shuffle": self.shuffle,
            "repeat": self.repeat,
            "tracks": self.tracks,
            "length": self.tracks.len(),
        })
    }
}

impl Default for PlayQueue {
    fn default() -> Self {
        Self::new()
    }
}

fn fisher_yates_shuffle(slice: &mut [usize]) {
    use std::time::SystemTime;
    let mut seed = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    for i in (1..slice.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed as usize) % (i + 1);
        slice.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_track(id: i64, title: &str) -> QueueTrack {
        QueueTrack {
            id: Some(id),
            source_id: None,
            title: title.to_string(),
            artist_name: None,
            album_title: None,
            album_id: None,
            duration_ms: 180000,
            file_path: None,
            cover_path: None,
            source: None,
            format: None,
            sample_rate: None,
            bit_depth: None,
            channels: None,
            disc_number: None,
            track_number: None,
        }
    }

    #[test]
    fn empty_queue() {
        let q = PlayQueue::new();
        assert!(q.is_empty());
        assert!(q.current().is_none());
        assert_eq!(q.position(), -1);
    }

    #[test]
    fn set_tracks_and_navigate() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        assert_eq!(q.length(), 3);
        assert_eq!(q.current().unwrap().title, "A");

        q.next();
        assert_eq!(q.current().unwrap().title, "B");

        q.next();
        assert_eq!(q.current().unwrap().title, "C");

        assert!(q.next().is_none());
    }

    #[test]
    fn previous_bottoms_at_zero() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(2, "B")], 1);
        assert_eq!(q.current().unwrap().title, "B");

        q.previous();
        assert_eq!(q.current().unwrap().title, "A");

        q.previous();
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn repeat_all() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(2, "B")], 0);
        q.set_repeat(RepeatMode::All);
        q.next();
        assert_eq!(q.current().unwrap().title, "B");
        q.next();
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn repeat_one() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(2, "B")], 0);
        q.set_repeat(RepeatMode::One);
        q.next();
        assert_eq!(q.current().unwrap().title, "A");
        q.next();
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn shuffle_mode() {
        let mut q = PlayQueue::new();
        let tracks: Vec<QueueTrack> = (0..10).map(|i| make_track(i, &format!("T{i}"))).collect();
        q.set_tracks(tracks, 0);
        q.set_shuffle(true);
        assert_eq!(q.current().unwrap().title, "T0");
        let mut visited = vec![q.current().unwrap().title.clone()];
        for _ in 0..9 {
            q.next();
            if let Some(t) = q.current() {
                visited.push(t.title.clone());
            }
        }
        assert_eq!(visited.len(), 10);
    }

    #[test]
    fn peek_next_no_side_effects() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        let peeked = q.peek_next().unwrap().title.clone();
        assert_eq!(peeked, "B");
        assert_eq!(q.current().unwrap().title, "A");
    }

    #[test]
    fn jump_to() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        q.jump_to(2);
        assert_eq!(q.current().unwrap().title, "C");
    }

    #[test]
    fn remove_track() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            1,
        );
        q.remove_track(0);
        assert_eq!(q.length(), 2);
        assert_eq!(q.position(), 0);
        assert_eq!(q.current().unwrap().title, "B");
    }

    #[test]
    fn move_track() {
        let mut q = PlayQueue::new();
        q.set_tracks(
            vec![make_track(1, "A"), make_track(2, "B"), make_track(3, "C")],
            0,
        );
        q.move_track(0, 2);
        assert_eq!(q.position(), 2);
        assert_eq!(q.tracks()[0].title, "B");
        assert_eq!(q.tracks()[1].title, "C");
        assert_eq!(q.tracks()[2].title, "A");
    }

    #[test]
    fn add_tracks_at_position() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A"), make_track(3, "C")], 0);
        q.add_tracks(vec![make_track(2, "B")], Some(1));
        assert_eq!(q.length(), 3);
        assert_eq!(q.tracks()[1].title, "B");
    }

    #[test]
    fn clear() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A")], 0);
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.position(), -1);
    }

    #[test]
    fn fisher_yates_produces_permutation() {
        let mut indices: Vec<usize> = (0..20).collect();
        fisher_yates_shuffle(&mut indices);
        let mut sorted = indices.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn to_json() {
        let mut q = PlayQueue::new();
        q.set_tracks(vec![make_track(1, "A")], 0);
        let json = q.to_json();
        assert_eq!(json["position"], 0);
        assert_eq!(json["length"], 1);
    }
}
