//! Persistent device-id → Matter bridged-endpoint-id ledger
//! (`<store_dir>/endpoints.json`). Endpoint ids must never be reused once
//! assigned to a device id (Matter bridge stability requirement, spec
//! §9.12.2.2) — this file exists so restarts and config edits keep the same
//! device mapped to the same endpoint. Entries for ids that are later
//! removed from config are kept as tombstones: re-adding the same id
//! restores its old endpoint (and the controller's existing pairing/scene
//! data for that accessory keeps working) instead of colliding with a
//! newly-issued one.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// bridged endpoint の採番開始値（EP0=root, EP1=Aggregator の次）。
pub const FIRST_BRIDGED_ENDPOINT: u16 = 2;

/// On-disk shape of `endpoints.json`.
#[derive(Debug, Serialize, Deserialize)]
struct LedgerFile {
    next: u16,
    map: BTreeMap<String, u16>,
}

/// 設定ファイルの device id → endpoint id の永続台帳
/// (`<store_dir>/endpoints.json`)。単調増加・再利用禁止（Matter bridge の
/// endpoint 安定要件, spec §9.12.2.2）。削除された id のエントリも残す
/// （tombstone）— 同じ id の再追加は旧 endpoint を復元し、コントローラの
/// アクセサリ対応が生き返る。
pub struct EndpointLedger {
    path: PathBuf,
    next: u16,
    map: BTreeMap<String, u16>,
}

impl EndpointLedger {
    /// 無ければ `{ next: FIRST_BRIDGED_ENDPOINT, map: {} }`。壊れた
    /// （パース不能な）ファイルは同じ初期状態として読む。パースできるが
    /// 不整合（`next <= map` の最大値）なファイルは `next` を `max + 1` に
    /// 修復して読む。
    pub fn load(store_dir: &Path) -> io::Result<Self> {
        let path = store_dir.join("endpoints.json");
        let fresh = || (FIRST_BRIDGED_ENDPOINT, BTreeMap::new());
        let (next, map) = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<LedgerFile>(&bytes) {
                Ok(file) => match file.map.values().copied().max() {
                    Some(max) if file.next <= max => (max + 1, file.map),
                    _ => (file.next, file.map),
                },
                // Can't recover a device-id → endpoint mapping out of bytes
                // that don't even parse — the best available fallback is a
                // fresh ledger rather than a hard failure.
                Err(_) => fresh(),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => fresh(),
            Err(e) => return Err(e),
        };
        Ok(Self { path, next, map })
    }

    /// 既知 id は既存値をそのまま返す。新規 id は `next` を払い出して
    /// map へ追加する。ディスクへの反映は `save` を呼び出し側が全 assign
    /// 後に 1 回行う。
    pub fn assign(&mut self, id: &str) -> u16 {
        if let Some(&endpoint) = self.map.get(id) {
            return endpoint;
        }
        let endpoint = self.next;
        self.next += 1;
        self.map.insert(id.to_string(), endpoint);
        endpoint
    }

    pub fn save(&self) -> io::Result<()> {
        let file = LedgerFile {
            next: self.next,
            map: self.map.clone(),
        };
        let bytes =
            serde_json::to_vec(&file).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        mat_core::fsatomic::write_atomic(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_store_assigns_sequentially_from_first_bridged_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EndpointLedger::load(dir.path()).unwrap();
        assert_eq!(ledger.assign("living-light"), 2);
        assert_eq!(ledger.assign("bedroom-light"), 3);
        assert_eq!(ledger.assign("kitchen-light"), 4);
    }

    #[test]
    fn save_then_load_roundtrips_assignments() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut ledger = EndpointLedger::load(dir.path()).unwrap();
            ledger.assign("living-light");
            ledger.assign("bedroom-light");
            ledger.save().unwrap();
        }
        let mut reloaded = EndpointLedger::load(dir.path()).unwrap();
        assert_eq!(reloaded.assign("living-light"), 2);
        assert_eq!(reloaded.assign("bedroom-light"), 3);
        // A brand-new id after reload continues the sequence, not restarting it.
        assert_eq!(reloaded.assign("kitchen-light"), 4);
    }

    #[test]
    fn known_id_reassigned_returns_same_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EndpointLedger::load(dir.path()).unwrap();
        let first = ledger.assign("living-light");
        let second = ledger.assign("living-light");
        assert_eq!(first, second);
        assert_eq!(first, 2);
    }

    /// A device id that's no longer referenced in config stays in the
    /// ledger's map (tombstone) — the ledger itself never deletes entries.
    /// A genuinely new id assigned afterwards still gets the next
    /// monotonically-increasing endpoint, never reusing a tombstoned one.
    #[test]
    fn new_id_after_tombstoned_entries_still_increases_monotonically() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = EndpointLedger::load(dir.path()).unwrap();
        ledger.assign("living-light");
        ledger.assign("bedroom-light");
        ledger.assign("kitchen-light");
        // "bedroom-light" is no longer in the caller's config, but the
        // ledger's map still holds it — assign() never removes entries, so
        // there's nothing to simulate deleting here beyond not calling it.
        // A brand-new id still gets 5, not a reused/freed id.
        assert_eq!(ledger.assign("hallway-light"), 5);
    }

    #[test]
    fn corrupt_json_repairs_to_fresh_state_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("endpoints.json"), b"not valid json {{{").unwrap();
        let mut ledger = EndpointLedger::load(dir.path()).unwrap();
        assert_eq!(ledger.assign("living-light"), FIRST_BRIDGED_ENDPOINT);
    }

    #[test]
    fn next_inconsistent_with_map_repairs_to_max_plus_one() {
        let dir = tempfile::tempdir().unwrap();
        // next=2 but map already has an entry at endpoint 4 — a hand-edited
        // or otherwise-corrupted file that would otherwise reissue endpoint
        // 2 or 3, colliding with... well, nothing here, but violating the
        // "never reuse" invariant in general. Must repair next to 5.
        std::fs::write(
            dir.path().join("endpoints.json"),
            br#"{"next":2,"map":{"living-light":2,"kitchen-light":4}}"#,
        )
        .unwrap();
        let mut ledger = EndpointLedger::load(dir.path()).unwrap();
        // Known id keeps its existing endpoint.
        assert_eq!(ledger.assign("living-light"), 2);
        assert_eq!(ledger.assign("kitchen-light"), 4);
        // New id gets max(map)+1 = 5, not the stale next=2.
        assert_eq!(ledger.assign("hallway-light"), 5);
    }
}
