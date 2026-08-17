//! File-backed [`FabricPersist`](crate::core::fabric_store::FabricPersist)
//! implementation — the only place `mat-device` writes fabric state to
//! disk. Whole-table JSON via `serde_json`, written atomically (tmp +
//! fsync + rename) with `mat_core::fsatomic::write_atomic` so a crash
//! mid-write can't corrupt or truncate the file (same discipline
//! `mat-controller`'s KVS uses).

use std::path::{Path, PathBuf};

use crate::core::fabric_store::{FabricEntry, FabricPersist};

/// Persists the fabric table as one JSON file at a fixed path.
pub struct FileFabricStore {
    path: PathBuf,
}

impl FileFabricStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl FabricPersist for FileFabricStore {
    fn save(&self, entries: &[FabricEntry]) -> Result<(), String> {
        let bytes = serde_json::to_vec(entries).map_err(|e| e.to_string())?;
        mat_core::fsatomic::write_atomic(&self.path, &bytes).map_err(|e| e.to_string())?;
        // fabrics.json holds op_private_key/ipk_operational in plaintext —
        // restrict it to the owner (Linux-only workspace, matches the
        // rustix-based permission handling used elsewhere in `net`).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())
    }

    fn load(&self) -> Result<Vec<FabricEntry>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Convenience: builds a [`FileFabricStore`] rooted at `dir` (a fixed
/// filename inside it, `fabrics.json`) — the shape callers (`matd`'s device
/// bootstrap) actually want, one directory in, one store out.
pub fn store_in_dir(dir: &Path) -> FileFabricStore {
    FileFabricStore::new(dir.join("fabrics.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::fabric_store::FabricStore;

    fn entry(fabric_index: u8) -> FabricEntry {
        FabricEntry {
            fabric_index,
            root_tlv: vec![1, 2, 3],
            noc_tlv: vec![4, 5, 6],
            icac_tlv: Some(vec![7, 8]),
            op_private_key: [9u8; 32],
            ipk_operational: [10u8; 16],
            node_id: 0x1234_5678,
            fabric_id: 0xABCD,
            root_public_key: [11u8; 65],
            admin_subject: 0xAA,
            admin_vendor_id: 0xFFF1,
            label: String::new(),
        }
    }

    #[test]
    fn save_then_load_roundtrips_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in_dir(dir.path());
        store.save(std::slice::from_ref(&entry(1))).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, vec![entry(1)]);
    }

    /// fabrics.json holds op_private_key/ipk_operational plaintext — must
    /// not be group/world readable.
    #[test]
    fn save_sets_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let store = store_in_dir(dir.path());
        store.save(std::slice::from_ref(&entry(1))).unwrap();
        let mode = std::fs::metadata(dir.path().join("fabrics.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn load_with_no_file_yet_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_in_dir(dir.path());
        assert_eq!(store.load().unwrap(), Vec::new());
    }

    #[test]
    fn fabric_store_with_persist_reloads_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut fs = FabricStore::with_persist(Box::new(store_in_dir(dir.path())));
            fs.insert(entry(1)).unwrap();
            fs.insert(entry(2)).unwrap();
        }
        // Fresh `FabricStore` over the same directory picks up both entries
        // (proves `insert` really persists, not just holds in memory).
        let fs2 = FabricStore::with_persist(Box::new(store_in_dir(dir.path())));
        assert_eq!(fs2.entries(), &[entry(1), entry(2)]);
    }
}
