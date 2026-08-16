//! Installed-fabric table (spec §2.5.1) and its persistence boundary.
//!
//! Pure — no file I/O here, checked by `cargo check -p mat-device
//! --no-default-features` like the rest of `core`. [`FabricStore`] holds
//! the in-memory table; a save happens by handing the current entries to a
//! [`FabricPersist`] implementation the caller injects. The only concrete
//! implementation (file-backed, JSON via `mat_core::fsatomic`) lives under
//! `crate::net::store`, gated behind the `net` feature — this module never
//! names a filesystem path or touches `std::fs`.

use serde::{Deserialize, Serialize};

/// `serde(with = "fixed_bytes")` for `[u8; N]` fields. Serde's blanket
/// array impl only covers small fixed lengths (0..=32) — `root_public_key`
/// here is 65 bytes, past that cutoff — so the SEC1-uncompressed-point-
/// sized fields need this explicit (de)serializer, generic over `N` so one
/// module serves all three array widths below.
mod fixed_bytes {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S, const N: usize>(bytes: &[u8; N], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // A plain slice serializes as a generic sequence (no fast byte-string
        // path — that's the separate `serde_bytes` crate, not worth pulling
        // in for this). Round-trips through JSON fine either way.
        bytes.as_slice().serialize(serializer)
    }

    pub fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
    where
        D: Deserializer<'de>,
    {
        let v: Vec<u8> = Vec::deserialize(deserializer)?;
        let len = v.len();
        v.try_into()
            .map_err(|_| serde::de::Error::invalid_length(len, &"fixed-size byte array"))
    }
}

/// One fabric this device has been commissioned onto — the credentials and
/// operational key material an `AddNOC` command installs (spec §11.17.6.13).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FabricEntry {
    pub fabric_index: u8,
    pub root_tlv: Vec<u8>,
    pub noc_tlv: Vec<u8>,
    pub icac_tlv: Option<Vec<u8>>,
    #[serde(with = "fixed_bytes")]
    pub op_private_key: [u8; 32],
    #[serde(with = "fixed_bytes")]
    pub ipk_operational: [u8; 16],
    pub node_id: u64,
    pub fabric_id: u64,
    #[serde(with = "fixed_bytes")]
    pub root_public_key: [u8; 65],
    pub admin_subject: u64,
    /// `AdminVendorID` from the `AddNOC` command that installed this fabric
    /// (spec §11.17.6.13.1) — the vendor id of the administrator (app/hub)
    /// that commissioned this device onto the fabric. `#[serde(default)]`
    /// so `fabrics.json` files persisted before this field existed still
    /// load (as `0`, an unassigned vendor id — spec §2.5.2).
    #[serde(default)]
    pub admin_vendor_id: u16,
}

/// Persistence boundary `core` calls through instead of touching a
/// filesystem directly. `save`/`load` operate on the full entry list (this
/// device expects at most a handful of fabrics, so whole-table rewrite on
/// every change is simplest and matches the KVS-style "small state,
/// atomic write" pattern used elsewhere in this workspace). Object-safe
/// (plain `String` error, no associated type) so [`FabricStore`] can hold
/// it as `Box<dyn FabricPersist>` without becoming generic itself.
/// `: Send` so `Box<dyn FabricPersist>` (and therefore `FabricStore`,
/// `Inner`) stays `Send` — `core::commissioning::CommissioningServer` wraps
/// `Inner` (which owns a `FabricStore`) in `Arc<Mutex<..>>`.
pub trait FabricPersist: Send {
    fn save(&self, entries: &[FabricEntry]) -> Result<(), String>;
    fn load(&self) -> Result<Vec<FabricEntry>, String>;
}

/// In-memory fabric table, optionally backed by a [`FabricPersist`]. With
/// no persist injected (`new`), the store is a plain in-memory table (used
/// by data-model unit tests that don't care about durability). With one
/// injected (`with_persist`), every mutation re-saves the whole table.
pub struct FabricStore {
    entries: Vec<FabricEntry>,
    persist: Option<Box<dyn FabricPersist>>,
}

impl FabricStore {
    /// An empty, non-persisted store.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            persist: None,
        }
    }

    /// A store backed by `persist` — loads whatever's already there (an
    /// empty/missing backing store loads as zero entries; a `load` I/O
    /// error is also treated as zero entries, matching "first boot, no
    /// fabrics yet" — the alternative would be a fallible constructor for a
    /// case that's indistinguishable from first boot at this layer).
    pub fn with_persist(persist: Box<dyn FabricPersist>) -> Self {
        let entries = persist.load().unwrap_or_default();
        Self {
            entries,
            persist: Some(persist),
        }
    }

    pub fn entries(&self) -> &[FabricEntry] {
        &self.entries
    }

    /// The fabric index the next `insert` will use (spec §11.17: fabric
    /// indices are 1-based, `0` is reserved as "no fabric"). `max + 1`
    /// rather than `len + 1` — after a `remove` this table can have gaps
    /// (a fail-safe-expiry rollback removes from the middle, not just the
    /// end — see `remove`'s doc comment), and `len + 1` would hand out an
    /// index a still-live fabric already occupies.
    pub fn next_fabric_index(&self) -> u8 {
        self.entries
            .iter()
            .map(|e| e.fabric_index)
            .max()
            .map_or(1, |m| m + 1)
    }

    /// Appends `entry` and, if a persist backend is configured, saves the
    /// whole table. On a save failure the entry is rolled back out of
    /// memory too — a `FabricEntry` mat-device claims to hold but can't
    /// recover after a restart is worse than an `AddNOC` that visibly
    /// fails.
    pub fn insert(&mut self, entry: FabricEntry) -> Result<(), String> {
        self.entries.push(entry);
        if let Some(persist) = &self.persist {
            if let Err(e) = persist.save(&self.entries) {
                self.entries.pop();
                return Err(e);
            }
        }
        Ok(())
    }

    /// Removes the entry at `fabric_index`, if present, and (if a persist
    /// backend is configured) saves the resulting table. `Ok(false)` (not
    /// an error) if no entry has that index — callers rolling back a
    /// fail-safe expiry call this unconditionally and treat "wasn't there"
    /// as a no-op.
    ///
    /// Deliberately **asymmetric** with `insert`'s save-failure rollback.
    /// `insert` un-appends on a save error so the in-memory table never
    /// claims a fabric that didn't actually make it to disk. `remove` does
    /// the opposite on a save error: the entry stays removed from memory
    /// even though the save failed (the error is still returned, so the
    /// caller knows persistence didn't happen). Every caller of `remove` is
    /// a fail-safe rollback undoing an `AddNOC` that was never confirmed by
    /// `CommissioningComplete` — putting the entry back in memory on a save
    /// error would leave a zombie fabric a CASE session could still
    /// authenticate against (its NOC and keys are real), potentially while
    /// `next_fabric_index` has already handed its index to a new attempt.
    /// That's worse than the alternative failure mode (the removed fabric
    /// reappearing after a restart if the save is never retried) — a
    /// zombie that's live *right now* beats one that only might come back
    /// later.
    pub fn remove(&mut self, fabric_index: u8) -> Result<bool, String> {
        let Some(pos) = self
            .entries
            .iter()
            .position(|e| e.fabric_index == fabric_index)
        else {
            return Ok(false);
        };
        self.entries.remove(pos);
        if let Some(persist) = &self.persist {
            persist.save(&self.entries)?;
        }
        Ok(true)
    }
}

impl Default for FabricStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(fabric_index: u8) -> FabricEntry {
        FabricEntry {
            fabric_index,
            root_tlv: vec![1, 2, 3],
            noc_tlv: vec![4, 5, 6],
            icac_tlv: None,
            op_private_key: [7u8; 32],
            ipk_operational: [8u8; 16],
            node_id: 0x1234,
            fabric_id: 0xABCD,
            root_public_key: [9u8; 65],
            admin_subject: 0xAA,
            admin_vendor_id: 0xFFF1,
        }
    }

    #[test]
    fn old_fabrics_json_without_admin_vendor_id_still_loads() {
        let mut v = serde_json::to_value(entry(1)).unwrap();
        v.as_object_mut().unwrap().remove("admin_vendor_id");
        let e: FabricEntry = serde_json::from_value(v).unwrap();
        assert_eq!(e.admin_vendor_id, 0);
    }

    #[test]
    fn next_fabric_index_starts_at_one_and_increments() {
        let mut store = FabricStore::new();
        assert_eq!(store.next_fabric_index(), 1);
        store.insert(entry(1)).unwrap();
        assert_eq!(store.next_fabric_index(), 2);
    }

    #[test]
    fn insert_without_persist_just_keeps_it_in_memory() {
        let mut store = FabricStore::new();
        store.insert(entry(1)).unwrap();
        assert_eq!(store.entries(), &[entry(1)]);
    }

    struct FailingPersist;
    impl FabricPersist for FailingPersist {
        fn save(&self, _entries: &[FabricEntry]) -> Result<(), String> {
            Err("disk full".to_string())
        }
        fn load(&self) -> Result<Vec<FabricEntry>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn insert_rolls_back_on_save_failure() {
        let mut store = FabricStore::with_persist(Box::new(FailingPersist));
        let err = store.insert(entry(1)).unwrap_err();
        assert_eq!(err, "disk full");
        assert!(store.entries().is_empty());
    }

    #[test]
    fn remove_deletes_matching_entry() {
        let mut store = FabricStore::new();
        store.insert(entry(1)).unwrap();
        store.insert(entry(2)).unwrap();
        assert!(store.remove(1).unwrap());
        assert_eq!(store.entries(), &[entry(2)]);
    }

    #[test]
    fn remove_returns_false_when_not_found() {
        let mut store = FabricStore::new();
        store.insert(entry(1)).unwrap();
        assert!(!store.remove(2).unwrap());
        assert_eq!(store.entries().len(), 1);
    }

    #[test]
    fn next_fabric_index_skips_removed_indices() {
        let mut store = FabricStore::new();
        store.insert(entry(1)).unwrap();
        store.insert(entry(2)).unwrap();
        assert!(store.remove(1).unwrap());
        assert_eq!(store.next_fabric_index(), 3); // len+1 だと 2 で衝突していた
    }

    /// A `FabricPersist` whose `save` can be toggled to fail after
    /// construction (unlike `FailingPersist`, which always fails and so
    /// can't be used to get an entry into a persisted store's memory in
    /// the first place via `insert`). `Arc<AtomicBool>` rather than `Cell`
    /// so the flag stays reachable from the test after the persist is
    /// moved into `Box<dyn FabricPersist>` — `AtomicBool` is `Sync`, unlike
    /// `Cell`, which the shared `Arc` requires.
    struct FlakySavePersist {
        fail_save: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl FabricPersist for FlakySavePersist {
        fn save(&self, _entries: &[FabricEntry]) -> Result<(), String> {
            if self.fail_save.load(std::sync::atomic::Ordering::SeqCst) {
                Err("disk full".to_string())
            } else {
                Ok(())
            }
        }
        fn load(&self) -> Result<Vec<FabricEntry>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn remove_drops_from_memory_even_when_persist_save_fails() {
        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        store.insert(entry(1)).unwrap();

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let err = store.remove(1).unwrap_err();
        assert_eq!(err, "disk full");
        // Asymmetric with `insert`'s rollback (see `remove`'s doc comment):
        // the entry is gone from memory regardless of the save failure.
        assert!(store.entries().is_empty());
    }
}
