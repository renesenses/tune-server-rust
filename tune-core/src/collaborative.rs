use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoomEvent {
    pub room_id: String,
    pub action: String,
    pub data: serde_json::Value,
    pub from: Option<String>,
}

pub struct RoomMember {
    pub id: String,
    pub tx: mpsc::Sender<String>,
}

pub struct Room {
    pub id: String,
    pub members: Vec<RoomMember>,
    pub created_at: Instant,
}

pub struct RoomManager {
    rooms: HashMap<String, Room>,
}

impl Default for RoomManager {
    fn default() -> Self {
        Self::new()
    }
}

impl RoomManager {
    pub fn new() -> Self {
        Self {
            rooms: HashMap::new(),
        }
    }

    pub fn create_room(&mut self, room_id: &str) -> bool {
        if self.rooms.contains_key(room_id) {
            return false;
        }
        self.rooms.insert(
            room_id.to_string(),
            Room {
                id: room_id.to_string(),
                members: Vec::new(),
                created_at: Instant::now(),
            },
        );
        true
    }

    pub fn join(&mut self, room_id: &str, member_id: &str, tx: mpsc::Sender<String>) -> bool {
        let room = match self.rooms.get_mut(room_id) {
            Some(r) => r,
            None => return false,
        };
        room.members.retain(|m| m.id != member_id);
        room.members.push(RoomMember {
            id: member_id.to_string(),
            tx,
        });
        true
    }

    pub fn leave(&mut self, room_id: &str, member_id: &str) {
        if let Some(room) = self.rooms.get_mut(room_id) {
            room.members.retain(|m| m.id != member_id);
            if room.members.is_empty() {
                self.rooms.remove(room_id);
            }
        }
    }

    pub async fn broadcast(&self, room_id: &str, message: &str, exclude: Option<&str>) {
        if let Some(room) = self.rooms.get(room_id) {
            for member in &room.members {
                if exclude == Some(member.id.as_str()) {
                    continue;
                }
                member.tx.send(message.to_string()).await.ok();
            }
        }
    }

    pub fn list_rooms(&self) -> Vec<serde_json::Value> {
        self.rooms
            .values()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "members": r.members.len(),
                    "created_at_secs": r.created_at.elapsed().as_secs(),
                })
            })
            .collect()
    }

    pub fn room_info(&self, room_id: &str) -> Option<serde_json::Value> {
        self.rooms.get(room_id).map(|r| {
            serde_json::json!({
                "id": r.id,
                "members": r.members.iter().map(|m| &m.id).collect::<Vec<_>>(),
                "member_count": r.members.len(),
            })
        })
    }

    pub fn delete_room(&mut self, room_id: &str) -> bool {
        self.rooms.remove(room_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_join_leave() {
        let mut mgr = RoomManager::new();
        assert!(mgr.create_room("party-1"));
        assert!(!mgr.create_room("party-1")); // duplicate

        let (tx, _rx) = mpsc::channel(10);
        assert!(mgr.join("party-1", "user-a", tx.clone()));
        assert!(mgr.join("party-1", "user-b", tx));

        assert_eq!(mgr.room_info("party-1").unwrap()["member_count"], 2);

        mgr.leave("party-1", "user-a");
        assert_eq!(mgr.room_info("party-1").unwrap()["member_count"], 1);

        mgr.leave("party-1", "user-b");
        assert!(mgr.room_info("party-1").is_none()); // auto-deleted
    }

    #[tokio::test]
    async fn broadcast_reaches_members() {
        let mut mgr = RoomManager::new();
        mgr.create_room("room-1");

        let (tx1, mut rx1) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);
        mgr.join("room-1", "a", tx1);
        mgr.join("room-1", "b", tx2);

        mgr.broadcast("room-1", "hello", None).await;
        assert_eq!(rx1.recv().await.unwrap(), "hello");
        assert_eq!(rx2.recv().await.unwrap(), "hello");
    }

    #[tokio::test]
    async fn broadcast_exclude() {
        let mut mgr = RoomManager::new();
        mgr.create_room("room-1");

        let (tx1, mut rx1) = mpsc::channel(10);
        let (tx2, mut rx2) = mpsc::channel(10);
        mgr.join("room-1", "a", tx1);
        mgr.join("room-1", "b", tx2);

        mgr.broadcast("room-1", "from-a", Some("a")).await;
        assert!(rx1.try_recv().is_err()); // excluded
        assert_eq!(rx2.recv().await.unwrap(), "from-a");
    }
}
