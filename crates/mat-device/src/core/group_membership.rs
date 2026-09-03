//! Groups クラスタの membership 帳簿（fabric × group × endpoint）— 全
//! bridged endpoint の `GroupsHandler` が共有する。groupcast 受信は
//! 「この group の member endpoint はどれか」をここで引き、`GroupTable`
//! 属性（GroupKeyManagement）もここから派生する。永続化は
//! `GroupMembershipPersist`（`net::store::FileGroupMembershipStore` =
//! `<store_dir>/groups.json`）。パターンは `core::access_control::AclStore`。
use std::sync::{Arc, Mutex};

use mat_controller::im;
use mat_controller::sync::locked;
use serde::{Deserialize, Serialize};

/// endpoint あたりの group table 容量（spec §1.3.4 は実装定義）。fabric
/// 横断で数える（従来の `GroupsHandler` と同じ）。`GetGroupMembership` の
/// `Capacity` はこの残数。
pub const GROUP_TABLE_CAPACITY: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    pub fabric_index: u8,
    pub group_id: u16,
    pub endpoint: u16,
}

pub trait GroupMembershipPersist: Send {
    fn save(&self, members: &[GroupMember]) -> Result<(), String>;
    fn load(&self) -> Result<Vec<GroupMember>, String>;
}

#[derive(Default)]
struct Inner {
    members: Vec<GroupMember>,
    persist: Option<Box<dyn GroupMembershipPersist>>,
}

#[derive(Clone, Default)]
pub struct GroupMembershipStore(Arc<Mutex<Inner>>);

impl GroupMembershipStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_persist(persist: Box<dyn GroupMembershipPersist>) -> Self {
        let members = match persist.load() {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "group membership: load failed; starting empty");
                Vec::new()
            }
        };
        Self(Arc::new(Mutex::new(Inner {
            members,
            persist: Some(persist),
        })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        locked(&self.0)
    }

    fn save(guard: &Inner) {
        if let Some(persist) = &guard.persist {
            if let Err(e) = persist.save(&guard.members) {
                tracing::warn!(error = %e, "group membership: save failed; keeping in-memory state");
            }
        }
    }

    pub fn contains(&self, fabric_index: u8, group_id: u16, endpoint: u16) -> bool {
        self.lock().members.contains(&GroupMember {
            fabric_index,
            group_id,
            endpoint,
        })
    }

    pub fn count_for_endpoint(&self, endpoint: u16) -> usize {
        self.lock()
            .members
            .iter()
            .filter(|m| m.endpoint == endpoint)
            .count()
    }

    /// 既存なら `Ok` no-op、endpoint の件数が `GROUP_TABLE_CAPACITY` に
    /// 達していれば `STATUS_RESOURCE_EXHAUSTED`。
    pub fn add(&self, fabric_index: u8, group_id: u16, endpoint: u16) -> Result<(), u8> {
        let mut guard = self.lock();
        let member = GroupMember {
            fabric_index,
            group_id,
            endpoint,
        };
        if guard.members.contains(&member) {
            return Ok(());
        }
        if guard
            .members
            .iter()
            .filter(|m| m.endpoint == endpoint)
            .count()
            >= GROUP_TABLE_CAPACITY
        {
            return Err(im::STATUS_RESOURCE_EXHAUSTED);
        }
        guard.members.push(member);
        Self::save(&guard);
        Ok(())
    }

    /// 削除できたら true（無ければ false）。
    pub fn remove(&self, fabric_index: u8, group_id: u16, endpoint: u16) -> bool {
        let mut guard = self.lock();
        let before = guard.members.len();
        guard.members.retain(|m| {
            *m != GroupMember {
                fabric_index,
                group_id,
                endpoint,
            }
        });
        let removed = guard.members.len() != before;
        if removed {
            Self::save(&guard);
        }
        removed
    }

    pub fn remove_all(&self, fabric_index: u8, endpoint: u16) {
        let mut guard = self.lock();
        let before = guard.members.len();
        guard
            .members
            .retain(|m| !(m.fabric_index == fabric_index && m.endpoint == endpoint));
        if guard.members.len() != before {
            Self::save(&guard);
        }
    }

    pub fn groups_for(&self, fabric_index: u8, endpoint: u16) -> Vec<u16> {
        self.lock()
            .members
            .iter()
            .filter(|m| m.fabric_index == fabric_index && m.endpoint == endpoint)
            .map(|m| m.group_id)
            .collect()
    }

    pub fn endpoints_for(&self, fabric_index: u8, group_id: u16) -> Vec<u16> {
        self.lock()
            .members
            .iter()
            .filter(|m| m.fabric_index == fabric_index && m.group_id == group_id)
            .map(|m| m.endpoint)
            .collect()
    }

    /// `(fabric_index, group_id)` の重複なし・初出順（join 集合と GroupTable 用）。
    pub fn groups_by_fabric(&self) -> Vec<(u8, u16)> {
        let mut out: Vec<(u8, u16)> = Vec::new();
        for m in &self.lock().members {
            if !out.contains(&(m.fabric_index, m.group_id)) {
                out.push((m.fabric_index, m.group_id));
            }
        }
        out
    }

    pub fn purge_fabric(&self, fabric_index: u8) {
        let mut guard = self.lock();
        guard.members.retain(|m| m.fabric_index != fabric_index);
        Self::save(&guard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_is_idempotent_and_scoped_by_fabric_group_endpoint() {
        let s = GroupMembershipStore::new();
        assert_eq!(s.add(1, 10, 2), Ok(()));
        assert_eq!(s.add(1, 10, 2), Ok(()));
        assert_eq!(s.add(1, 10, 3), Ok(()));
        assert_eq!(s.add(2, 10, 2), Ok(()));
        assert!(s.contains(1, 10, 2));
        assert!(!s.contains(1, 11, 2));
        assert_eq!(s.endpoints_for(1, 10), vec![2, 3]);
        assert_eq!(s.groups_for(1, 2), vec![10]);
        assert_eq!(s.groups_by_fabric(), vec![(1, 10), (2, 10)]);
    }

    #[test]
    fn capacity_is_per_endpoint_across_fabrics() {
        let s = GroupMembershipStore::new();
        for g in 1..=GROUP_TABLE_CAPACITY as u16 {
            assert_eq!(s.add(if g % 2 == 0 { 1 } else { 2 }, g, 2), Ok(()));
        }
        assert_eq!(
            s.add(1, 0x100, 2),
            Err(mat_controller::im::STATUS_RESOURCE_EXHAUSTED)
        );
        assert_eq!(
            s.add(1, 0x100, 3),
            Ok(()),
            "another endpoint has its own capacity"
        );
        assert_eq!(s.count_for_endpoint(2), GROUP_TABLE_CAPACITY);
    }

    #[test]
    fn remove_remove_all_and_purge() {
        let s = GroupMembershipStore::new();
        s.add(1, 10, 2).unwrap();
        s.add(1, 11, 2).unwrap();
        s.add(1, 10, 3).unwrap();
        s.add(2, 10, 2).unwrap();
        assert!(s.remove(1, 10, 2));
        assert!(!s.remove(1, 10, 2));
        assert_eq!(s.groups_for(1, 2), vec![11]);
        s.remove_all(1, 2);
        assert!(s.groups_for(1, 2).is_empty());
        assert_eq!(s.endpoints_for(1, 10), vec![3]);
        s.purge_fabric(1);
        assert_eq!(s.groups_by_fabric(), vec![(2, 10)]);
    }

    struct MemPersist(std::sync::Arc<std::sync::Mutex<Vec<GroupMember>>>);
    impl GroupMembershipPersist for MemPersist {
        fn save(&self, members: &[GroupMember]) -> Result<(), String> {
            *self.0.lock().unwrap() = members.to_vec();
            Ok(())
        }
        fn load(&self) -> Result<Vec<GroupMember>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[test]
    fn mutations_persist_and_reload() {
        let cell = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        {
            let s = GroupMembershipStore::with_persist(Box::new(MemPersist(cell.clone())));
            s.add(1, 10, 2).unwrap();
            s.add(1, 11, 2).unwrap();
            s.remove(1, 11, 2);
        }
        let s2 = GroupMembershipStore::with_persist(Box::new(MemPersist(cell)));
        assert_eq!(s2.groups_for(1, 2), vec![10]);
    }

    #[test]
    fn remove_all_no_save_when_nothing_removed() {
        let save_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct CountingPersist(
            std::sync::Arc<std::sync::Mutex<Vec<GroupMember>>>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        );
        impl GroupMembershipPersist for CountingPersist {
            fn save(&self, members: &[GroupMember]) -> Result<(), String> {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *self.0.lock().unwrap() = members.to_vec();
                Ok(())
            }
            fn load(&self) -> Result<Vec<GroupMember>, String> {
                Ok(self.0.lock().unwrap().clone())
            }
        }

        let cell = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = GroupMembershipStore::with_persist(Box::new(CountingPersist(
            cell.clone(),
            save_count.clone(),
        )));
        // Initial load calls save once (empty load)
        save_count.store(0, std::sync::atomic::Ordering::SeqCst);

        // remove_all on endpoint with no memberships should not save
        s.remove_all(1, 99);
        assert_eq!(
            save_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "remove_all should not save when no members removed"
        );

        // remove_all on endpoint with actual membership should save
        s.add(1, 10, 2).unwrap();
        save_count.store(0, std::sync::atomic::Ordering::SeqCst);
        s.remove_all(1, 2);
        assert_eq!(
            save_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "remove_all should save when members are removed"
        );
    }
}
