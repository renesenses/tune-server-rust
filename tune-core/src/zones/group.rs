use std::collections::HashMap;
use std::time::Instant;

use tracing::info;

#[derive(Debug, Clone)]
pub struct ZoneGroupInfo {
    pub group_id: String,
    pub leader_zone_id: i64,
    pub follower_zone_ids: Vec<i64>,
}

pub struct ZoneGroup {
    group_id: String,
    leader_zone_id: i64,
    follower_zone_ids: Vec<i64>,
    last_play_time: Option<Instant>,
}

impl ZoneGroup {
    pub fn new(group_id: String, leader_zone_id: i64, follower_zone_ids: Vec<i64>) -> Self {
        Self {
            group_id,
            leader_zone_id,
            follower_zone_ids,
            last_play_time: None,
        }
    }

    pub fn group_id(&self) -> &str {
        &self.group_id
    }

    pub fn leader_zone_id(&self) -> i64 {
        self.leader_zone_id
    }

    pub fn follower_zone_ids(&self) -> &[i64] {
        &self.follower_zone_ids
    }

    pub fn all_zone_ids(&self) -> Vec<i64> {
        let mut ids = vec![self.leader_zone_id];
        ids.extend_from_slice(&self.follower_zone_ids);
        ids
    }

    pub fn last_play_time(&self) -> Option<Instant> {
        self.last_play_time
    }

    pub fn mark_play(&mut self) {
        self.last_play_time = Some(Instant::now());
    }

    pub fn add_follower(&mut self, zone_id: i64) -> bool {
        if zone_id == self.leader_zone_id || self.follower_zone_ids.contains(&zone_id) {
            return false;
        }
        self.follower_zone_ids.push(zone_id);
        true
    }

    pub fn remove_follower(&mut self, zone_id: i64) -> bool {
        let before = self.follower_zone_ids.len();
        self.follower_zone_ids.retain(|&id| id != zone_id);
        self.follower_zone_ids.len() < before
    }

    pub fn contains(&self, zone_id: i64) -> bool {
        zone_id == self.leader_zone_id || self.follower_zone_ids.contains(&zone_id)
    }

    pub fn info(&self) -> ZoneGroupInfo {
        ZoneGroupInfo {
            group_id: self.group_id.clone(),
            leader_zone_id: self.leader_zone_id,
            follower_zone_ids: self.follower_zone_ids.clone(),
        }
    }
}

pub struct GroupManager {
    groups: HashMap<String, ZoneGroup>,
    zone_to_group: HashMap<i64, String>,
}

impl GroupManager {
    pub fn new() -> Self {
        Self {
            groups: HashMap::new(),
            zone_to_group: HashMap::new(),
        }
    }

    pub fn create_group(
        &mut self,
        leader_zone_id: i64,
        follower_zone_ids: Vec<i64>,
    ) -> ZoneGroupInfo {
        let group_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let group = ZoneGroup::new(group_id.clone(), leader_zone_id, follower_zone_ids.clone());
        let info = group.info();

        self.zone_to_group.insert(leader_zone_id, group_id.clone());
        for &fid in &follower_zone_ids {
            self.zone_to_group.insert(fid, group_id.clone());
        }
        self.groups.insert(group_id, group);

        info!(
            group = %info.group_id,
            leader = leader_zone_id,
            followers = ?follower_zone_ids,
            "zone_group_created"
        );
        info
    }

    pub fn dissolve_group(&mut self, group_id: &str) -> Option<ZoneGroupInfo> {
        let group = self.groups.remove(group_id)?;
        let info = group.info();
        for &zid in &info.follower_zone_ids {
            self.zone_to_group.remove(&zid);
        }
        self.zone_to_group.remove(&info.leader_zone_id);
        info!(group = group_id, "zone_group_dissolved");
        Some(info)
    }

    pub fn get_group(&self, group_id: &str) -> Option<&ZoneGroup> {
        self.groups.get(group_id)
    }

    pub fn get_group_mut(&mut self, group_id: &str) -> Option<&mut ZoneGroup> {
        self.groups.get_mut(group_id)
    }

    pub fn get_group_for_zone(&self, zone_id: i64) -> Option<&ZoneGroup> {
        self.zone_to_group
            .get(&zone_id)
            .and_then(|gid| self.groups.get(gid))
    }

    pub fn get_group_id_for_zone(&self, zone_id: i64) -> Option<&str> {
        self.zone_to_group.get(&zone_id).map(|s| s.as_str())
    }

    pub fn list_groups(&self) -> Vec<ZoneGroupInfo> {
        self.groups.values().map(|g| g.info()).collect()
    }

    pub fn has_active_groups(&self) -> bool {
        !self.groups.is_empty()
    }

    pub fn groups(&self) -> impl Iterator<Item = &ZoneGroup> {
        self.groups.values()
    }

    pub fn groups_mut(&mut self) -> impl Iterator<Item = &mut ZoneGroup> {
        self.groups.values_mut()
    }
}

impl Default for GroupManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_dissolve_group() {
        let mut mgr = GroupManager::new();
        let info = mgr.create_group(1, vec![2, 3]);
        assert_eq!(info.leader_zone_id, 1);
        assert_eq!(info.follower_zone_ids, vec![2, 3]);
        assert!(mgr.get_group_for_zone(1).is_some());
        assert!(mgr.get_group_for_zone(2).is_some());
        assert!(mgr.get_group_for_zone(4).is_none());

        let dissolved = mgr.dissolve_group(&info.group_id);
        assert!(dissolved.is_some());
        assert!(mgr.get_group_for_zone(1).is_none());
    }

    #[test]
    fn add_remove_follower() {
        let mut mgr = GroupManager::new();
        let info = mgr.create_group(1, vec![2]);
        let group = mgr.get_group_mut(&info.group_id).unwrap();
        assert!(group.add_follower(3));
        assert!(!group.add_follower(1));
        assert!(!group.add_follower(3));
        assert!(group.remove_follower(2));
        assert!(!group.remove_follower(2));
        assert_eq!(group.follower_zone_ids(), &[3]);
    }

    #[test]
    fn list_groups() {
        let mut mgr = GroupManager::new();
        mgr.create_group(1, vec![2]);
        mgr.create_group(3, vec![4]);
        assert_eq!(mgr.list_groups().len(), 2);
    }
}
