//! File-backed [`FabricPersist`](crate::core::fabric_store::FabricPersist)
//! implementation — the only place `mat-device` writes fabric state to
//! disk. Whole-table JSON via `serde_json`, written atomically (tmp +
//! fsync + rename) with `mat_core::fsatomic::write_atomic` so a crash
//! mid-write can't corrupt or truncate the file (same discipline
//! `mat-controller`'s KVS uses).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::access_control::{AclDeviceEntry, AclPersist};
use crate::core::datamodel::BasicInfoPersist;
use crate::core::fabric_store::{FabricEntry, FabricPersist};
use crate::core::group_key_management::{GroupKeyMapEntry, GroupKeyPersist, GroupKeySet};

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

/// File-backed [`AclPersist`](crate::core::access_control::AclPersist)
/// implementation — same JSON-via-`serde_json` +
/// `mat_core::fsatomic::write_atomic` discipline as [`FileFabricStore`]
/// above. Unlike `fabrics.json`, `acl.json` holds no key material (just
/// privilege/subject/target bookkeeping), so it doesn't get the
/// owner-only permission restriction `FileFabricStore::save` applies.
pub struct FileAclStore {
    path: PathBuf,
}

impl FileAclStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl AclPersist for FileAclStore {
    fn save(&self, entries: &[AclDeviceEntry]) -> Result<(), String> {
        let bytes = serde_json::to_vec(entries).map_err(|e| e.to_string())?;
        mat_core::fsatomic::write_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }

    fn load(&self) -> Result<Vec<AclDeviceEntry>, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Convenience: builds a [`FileAclStore`] rooted at `dir` (`acl.json`) —
/// mirrors [`store_in_dir`] above.
pub fn acl_store_in_dir(dir: &Path) -> FileAclStore {
    FileAclStore::new(dir.join("acl.json"))
}

/// Whole-record shape persisted at `<dir>/basic_info.json` — just the two
/// writable BasicInformation attributes (`FileAclStore`'s whole-table JSON
/// shape scaled down to a single struct, since there's no per-fabric or
/// per-entry list here).
#[derive(Serialize, Deserialize)]
struct BasicInfoRecord {
    node_label: String,
    location: String,
}

/// File-backed [`BasicInfoPersist`](crate::core::datamodel::BasicInfoPersist)
/// implementation — same JSON-via-`serde_json` +
/// `mat_core::fsatomic::write_atomic` discipline as [`FileAclStore`] above.
/// No key material here either, so no owner-only permission restriction.
pub struct FileBasicInfoStore {
    path: PathBuf,
}

impl FileBasicInfoStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl BasicInfoPersist for FileBasicInfoStore {
    fn save(&self, node_label: &str, location: &str) -> Result<(), String> {
        let record = BasicInfoRecord {
            node_label: node_label.to_string(),
            location: location.to_string(),
        };
        let bytes = serde_json::to_vec(&record).map_err(|e| e.to_string())?;
        mat_core::fsatomic::write_atomic(&self.path, &bytes).map_err(|e| e.to_string())
    }
}

/// Convenience: builds a [`FileBasicInfoStore`] rooted at `dir`
/// (`basic_info.json`) — mirrors [`acl_store_in_dir`] above.
pub fn basic_info_in_dir(dir: &Path) -> FileBasicInfoStore {
    FileBasicInfoStore::new(dir.join("basic_info.json"))
}

/// Loads `<dir>/basic_info.json`'s `(node_label, location)` pair for
/// `Node::with_root_endpoint_persisted`'s initial values. Unlike
/// `AclPersist`/`FabricPersist::load` (which return `Result` and let the
/// caller decide how to treat an error), this collapses "file doesn't
/// exist yet" (first boot) and "corrupt JSON" alike to the spec-default
/// `("", "XX")` — `BasicInfoPersist` is save-only (module doc), so there's
/// no trait method to route this through; the caller (`device::Device::
/// new`) has no better fallback than the spec default either way.
pub fn load_basic_info(dir: &Path) -> (String, String) {
    let path = dir.join("basic_info.json");
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<BasicInfoRecord>(&bytes) {
            Ok(record) => (record.node_label, record.location),
            Err(_) => (String::new(), "XX".to_string()),
        },
        Err(_) => (String::new(), "XX".to_string()),
    }
}

/// `group_keys.json` の 1 ファイル分（keyset と map を一緒に保存 —
/// 片方だけ新しい状態は意味を持たないので whole-table を 1 write にする）。
#[derive(Serialize, Deserialize, Default)]
struct GroupKeyFile {
    keysets: Vec<GroupKeySet>,
    map: Vec<GroupKeyMapEntry>,
}

/// File-backed [`GroupKeyPersist`] — epoch key を平文で持つので
/// `FileFabricStore` と同じく owner-only (0600)。
pub struct FileGroupKeyStore {
    path: PathBuf,
}

impl FileGroupKeyStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl GroupKeyPersist for FileGroupKeyStore {
    fn save(&self, keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String> {
        let file = GroupKeyFile {
            keysets: keysets.to_vec(),
            map: map.to_vec(),
        };
        let bytes = serde_json::to_vec(&file).map_err(|e| e.to_string())?;
        mat_core::fsatomic::write_atomic(&self.path, &bytes).map_err(|e| e.to_string())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())
    }

    fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: GroupKeyFile =
                    serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                Ok((file.keysets, file.map))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), Vec::new())),
            Err(e) => Err(e.to_string()),
        }
    }
}

/// Convenience: builds a [`FileGroupKeyStore`] rooted at `dir`
/// (`group_keys.json`) — mirrors [`acl_store_in_dir`] above.
pub fn group_key_store_in_dir(dir: &Path) -> FileGroupKeyStore {
    FileGroupKeyStore::new(dir.join("group_keys.json"))
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

    fn acl_entry(fabric_index: u8) -> AclDeviceEntry {
        AclDeviceEntry {
            privilege: 5,
            auth_mode: 2,
            subjects: vec![112233],
            targets_raw: None,
            fabric_index,
        }
    }

    #[test]
    fn acl_save_then_load_roundtrips_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = acl_store_in_dir(dir.path());
        store.save(std::slice::from_ref(&acl_entry(1))).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, vec![acl_entry(1)]);
    }

    #[test]
    fn acl_load_with_no_file_yet_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = acl_store_in_dir(dir.path());
        assert_eq!(store.load().unwrap(), Vec::new());
    }

    /// `acl.json` を実ファイルへ save し、別インスタンスの `AclStore` が
    /// それを load して復元できることを、`AclStore::with_persist` +
    /// `AccessControlHandler` を通して確認する（`entries_for` は
    /// `core::access_control` 内部 private なので、公開 API である
    /// `read` 経由で見る）。
    #[test]
    fn acl_store_with_persist_reloads_across_instances() {
        use crate::core::access_control::{
            decode_entries_for_test, AccessControlHandler, AclStore,
        };
        use crate::core::datamodel::{ClusterHandler, ReadCtx};
        use mat_controller::im;

        let dir = tempfile::tempdir().unwrap();
        {
            let store = AclStore::with_persist(Box::new(acl_store_in_dir(dir.path())));
            store.add_case_admin(1, 112233);
        }
        // Fresh `AclStore` over the same directory picks up the persisted
        // entry (proves `add_case_admin` really persists, not just holds it
        // in memory).
        let store2 = AclStore::with_persist(Box::new(acl_store_in_dir(dir.path())));
        let h = AccessControlHandler::new(store2);
        let ctx = ReadCtx {
            fabric_index: 1,
            ..ReadCtx::default()
        };
        let entries = decode_entries_for_test(&h.read(im::ATTR_ACL, &ctx).unwrap());
        assert_eq!(entries, vec![(5u8, 2u8, vec![112233u64], 1u8)]);
    }

    #[test]
    fn basic_info_save_then_load_roundtrips_node_label_and_location() {
        let dir = tempfile::tempdir().unwrap();
        let store = basic_info_in_dir(dir.path());
        store.save("Living Room", "JP").unwrap();
        assert_eq!(
            load_basic_info(dir.path()),
            ("Living Room".to_string(), "JP".to_string())
        );
    }

    /// No `basic_info.json` yet (first boot) falls back to the spec
    /// defaults `("", "XX")` rather than erroring.
    #[test]
    fn basic_info_load_with_no_file_yet_is_spec_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_basic_info(dir.path()),
            (String::new(), "XX".to_string())
        );
    }

    /// Corrupt JSON falls back to the same spec default as a missing file
    /// — `load_basic_info` never propagates a parse error to its caller
    /// (module doc).
    #[test]
    fn basic_info_load_with_corrupt_file_is_spec_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("basic_info.json"), b"not json").unwrap();
        assert_eq!(
            load_basic_info(dir.path()),
            (String::new(), "XX".to_string())
        );
    }

    #[test]
    fn group_key_store_with_persist_reloads_across_instances_and_is_owner_only() {
        use crate::core::group_key_management::{GroupKeySet, GroupKeyStore};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        {
            let store = GroupKeyStore::with_persist(Box::new(group_key_store_in_dir(dir.path())));
            store.upsert_keyset(1, 42, [7u8; 16]).unwrap();
            store.replace_fabric_map(1, vec![(10, 42)]);
        }
        let mode = std::fs::metadata(dir.path().join("group_keys.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let store2 = GroupKeyStore::with_persist(Box::new(group_key_store_in_dir(dir.path())));
        assert_eq!(
            store2.keysets(),
            vec![GroupKeySet {
                fabric_index: 1,
                keyset_id: 42,
                epoch_key0: [7u8; 16]
            }]
        );
        assert_eq!(store2.map_entries_for(1), vec![(10, 42)]);
    }

    #[test]
    fn group_key_load_with_no_file_yet_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (k, m) = group_key_store_in_dir(dir.path()).load().unwrap();
        assert!(k.is_empty() && m.is_empty());
    }
}
