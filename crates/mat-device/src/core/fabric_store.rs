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
    /// indices are 1-based, `0` is reserved as "no fabric").
    pub fn next_fabric_index(&self) -> u8 {
        self.entries.len() as u8 + 1
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
        }
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
}
