use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartyStatus {
    pub active: bool,
    pub zone_id: Option<i64>,
    pub zone_name: Option<String>,
    pub current_track: Option<PartyTrack>,
    pub queue_length: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartyTrack {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartyQueueItem {
    pub position: usize,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub is_current: bool,
    pub votes: i32,
}

pub struct PartyVoteStore {
    votes: Arc<Mutex<HashMap<(i64, usize), i32>>>,
}

impl Default for PartyVoteStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PartyVoteStore {
    pub fn new() -> Self {
        Self {
            votes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn increment(&self, zone_id: i64, position: usize) -> i32 {
        let mut votes = self.votes.lock().await;
        let entry = votes.entry((zone_id, position)).or_insert(0);
        *entry += 1;
        info!(zone_id, position, votes = *entry, "party_vote");
        *entry
    }

    pub async fn get_votes(&self, zone_id: i64) -> HashMap<usize, i32> {
        let votes = self.votes.lock().await;
        let mut result = HashMap::new();
        for ((zid, pos), count) in votes.iter() {
            if *zid == zone_id {
                result.insert(*pos, *count);
            }
        }
        result
    }

    pub async fn swap_positions(&self, zone_id: i64, pos_a: usize, pos_b: usize) {
        let mut votes = self.votes.lock().await;
        let a = votes.remove(&(zone_id, pos_a)).unwrap_or(0);
        let b = votes.remove(&(zone_id, pos_b)).unwrap_or(0);
        if a != 0 {
            votes.insert((zone_id, pos_b), a);
        }
        if b != 0 {
            votes.insert((zone_id, pos_a), b);
        }
    }

    pub async fn clear(&self, zone_id: i64) -> usize {
        let mut votes = self.votes.lock().await;
        let keys: Vec<_> = votes
            .keys()
            .filter(|(zid, _)| *zid == zone_id)
            .cloned()
            .collect();
        let count = keys.len();
        for k in keys {
            votes.remove(&k);
        }
        info!(zone_id, cleared = count, "party_votes_reset");
        count
    }
}

pub fn bubble_up_voted(queue: &mut [PartyQueueItem], current_position: usize) {
    let len = queue.len();
    if len < 2 {
        return;
    }

    for i in (current_position + 2..len).rev() {
        if queue[i].votes > queue[i - 1].votes && !queue[i - 1].is_current {
            queue.swap(i, i - 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn vote_increment() {
        let store = PartyVoteStore::new();
        let v1 = store.increment(1, 3).await;
        assert_eq!(v1, 1);
        let v2 = store.increment(1, 3).await;
        assert_eq!(v2, 2);
    }

    #[tokio::test]
    async fn get_votes_filters_zone() {
        let store = PartyVoteStore::new();
        store.increment(1, 0).await;
        store.increment(1, 1).await;
        store.increment(2, 0).await;

        let z1 = store.get_votes(1).await;
        assert_eq!(z1.len(), 2);
        let z2 = store.get_votes(2).await;
        assert_eq!(z2.len(), 1);
    }

    #[tokio::test]
    async fn clear_zone_votes() {
        let store = PartyVoteStore::new();
        store.increment(1, 0).await;
        store.increment(1, 1).await;
        store.increment(2, 0).await;

        let cleared = store.clear(1).await;
        assert_eq!(cleared, 2);

        let z1 = store.get_votes(1).await;
        assert!(z1.is_empty());
        let z2 = store.get_votes(2).await;
        assert_eq!(z2.len(), 1);
    }

    #[tokio::test]
    async fn swap_positions() {
        let store = PartyVoteStore::new();
        store.increment(1, 2).await;
        store.increment(1, 2).await;
        store.increment(1, 3).await;

        store.swap_positions(1, 2, 3).await;

        let votes = store.get_votes(1).await;
        assert_eq!(votes.get(&2).copied().unwrap_or(0), 1);
        assert_eq!(votes.get(&3).copied().unwrap_or(0), 2);
    }

    #[test]
    fn bubble_up_reorders() {
        let mut queue = vec![
            PartyQueueItem {
                position: 0,
                title: "Current".into(),
                artist: None,
                album: None,
                is_current: true,
                votes: 0,
            },
            PartyQueueItem {
                position: 1,
                title: "Low".into(),
                artist: None,
                album: None,
                is_current: false,
                votes: 1,
            },
            PartyQueueItem {
                position: 2,
                title: "High".into(),
                artist: None,
                album: None,
                is_current: false,
                votes: 5,
            },
        ];

        bubble_up_voted(&mut queue, 0);
        assert_eq!(queue[1].title, "High");
        assert_eq!(queue[2].title, "Low");
    }

    #[test]
    fn party_status_serialize() {
        let status = PartyStatus {
            active: true,
            zone_id: Some(1),
            zone_name: Some("Salon".into()),
            current_track: Some(PartyTrack {
                title: "Song".into(),
                artist: Some("Artist".into()),
                album: None,
                cover_path: None,
            }),
            queue_length: 5,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["active"], true);
        assert_eq!(json["queue_length"], 5);
    }
}
