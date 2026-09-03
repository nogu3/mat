# matv groupcast 実受信 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matv（`mat-device`）が `mat group invoke` の groupcast（IPv6 multicast、group session）を受信・復号・ACL 判定して member endpoint に適用し、鍵・GroupKeyMap・Groups membership を再起動越しに保持する。

**Architecture:** 永続化は既存の `AclPersist`/`FileAclStore` パターンを `GroupKeyStore` と新設 `GroupMembershipStore` に複製する。受信は `net/group_rx.rs` に「純関数の分類・復号・検証パイプライン」（`classify_group_datagram`）と「ソケット + multicast join 差分同期」（`GroupSocket`）を置き、`runtime::run` の `select!` に 1 ブランチ足して `Node::handle_group_invoke`（新設、`handle_invoke` の後半を共通化）へ流す。ACL は `Subject` に group 形を足し `AclStore::check` を auth mode で分岐させる。応答は送らない。

**Tech Stack:** Rust 2021、`mat-controller` の公開 API のみ（`crypto::open_message` / `message::MessageHeader` / `group::{build_group_datagram, group_multicast_addr, GROUP_SECURITY_FLAGS}` / `fabric::{compressed_fabric_id, derive_ipk_operational, derive_group_session_id}` / `kvs::GroupCredentials` / `tlv` / `im`）、`socket2 0.6` + `tokio::net::UdpSocket`（mat-device は既に依存）、`serde_json` + `mat_core::fsatomic::write_atomic`。

**Spec:** `docs/superpowers/specs/2026-09-03-matv-groupcast-rx-design.md`

## Global Constraints

- 変更は `crates/mat-device/**`、`crates/matv/**`、`scripts/e2e-device-m3.sh`、docs のみ。`mat-core` / `mat-controller` / `mat-native` / `crates/mat` / `matd` には触らない。
- 各タスク末: `cargo test -p mat-device --features net` 緑、`cargo fmt --all`、`cargo clippy -p mat-device --features net --all-targets -- -D warnings`（Task 9 は `-p matv` も）。
- コミットは日本語 subject + 末尾トレーラ:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01SNgdnaxwYTVSmcYYGRKQSK
  ```
- 鍵素材（epoch key）の `Debug` は redact（`FabricEntry` / `GroupCredentials` と同じ方針、repo は公開）。永続 JSON に鍵を書くファイルは `0o600`。
- ステータス定数は `mat_controller::im::STATUS_*`、ロックは `mat_controller::sync::locked`。
- group 経路は応答を送らない: 拒否・失敗は全部 `tracing::debug!` + drop。
- ワイヤ定数: group session type = `security_flags & 0x03 == 0x01`（`mat_controller::group::GROUP_SECURITY_FLAGS`）、privacy フラグ = `0x80`、multicast 宛先ポート 5540、`GroupInfoMapStruct` = `{1: GroupId, 2: Endpoints, 254: FabricIndex}`（GroupName 省略）。
- 統合テストは `#![cfg(feature = "net")]` + `mod support;`、matv は同時 1 CASE セッション。

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/mat-device/src/core/group_key_management.rs` | `GroupKeySet`/`GroupKeyMapEntry` 公開 + serde、`GroupKeyPersist`、`with_persist`、`keysets()`（T1）; `GroupKeyManagementHandler::new(store, membership)` + `ATTR_GROUP_TABLE`（T3） |
| `crates/mat-device/src/core/group_membership.rs` | 新設: `GroupMembershipStore` / `GroupMember` / `GroupMembershipPersist` / `GROUP_TABLE_CAPACITY`（T2） |
| `crates/mat-device/src/core/groups.rs` | `GroupsHandler` を store 委譲に（T2） |
| `crates/mat-device/src/core/bridge.rs`, `src/device.rs` | endpoint + membership を handler に配線、両 store の persist 注入、group socket 保持（T2, T3, T6b） |
| `crates/mat-device/src/core/commissioning.rs` | `set_group_membership_store` + purge 配線（T2） |
| `crates/mat-device/src/core/mod.rs` | `pub mod group_membership; pub mod group_invoke;`（T2, T5） |
| `crates/mat-device/src/net/store.rs` | `FileGroupKeyStore` / `FileGroupMembershipStore`（T1, T2） |
| `crates/mat-device/src/core/access_control.rs` | `Subject::group_id` / `Subject::group` / `check` の auth mode 分岐（T4） |
| `crates/mat-device/src/core/group_invoke.rs` | 新設: `GroupInvokeIn` + `decode_group_invoke_request`（T5） |
| `crates/mat-device/src/core/datamodel.rs` | `invoke_on_endpoint` 切り出し + `handle_group_invoke`（T5） |
| `crates/mat-device/src/net/group_rx.rs` | 新設: `GroupReplayGuard` / `GroupRxDeps` / `GroupDrop` / `classify_group_datagram` / `desired_group_addrs`（T6）; `GroupSocket` / `GroupRx`（T7） |
| `crates/mat-device/src/net/mod.rs`, `src/net/runtime.rs` | select! ブランチ・join 同期・unicast 側の group drop（T7） |
| `crates/mat-device/tests/group_receive.rs` | 統合閉ループ（T8） |
| `crates/matv/src/main.rs`, `scripts/e2e-device-m3.sh`, README/docs | 設定・出力・e2e・doc（T9） |

---

### Task 1: GroupKeyStore の永続化

**Files:**
- Modify: `crates/mat-device/src/core/group_key_management.rs`（モジュール doc、`GroupKeySet`/`GroupKeyMapEntry`/`GroupKeyInner`、`GroupKeyStore` impl 60-172 行付近、tests）
- Modify: `crates/mat-device/src/net/store.rs`（`FileAclStore` の後ろ、tests）
- Modify: `crates/mat-device/src/device.rs`（265-272 行付近の `GroupKeyStore::new()` 配線とコメント）

**Interfaces:**
- Produces:
  - `pub struct GroupKeySet { pub fabric_index: u8, pub keyset_id: u16, pub epoch_key0: [u8; 16] }`（`Clone, PartialEq, Eq, Serialize, Deserialize`、手書き `Debug` で鍵 redact）
  - `pub struct GroupKeyMapEntry { pub fabric_index: u8, pub group_id: u16, pub keyset_id: u16 }`（`Debug, Clone, PartialEq, Eq, Serialize, Deserialize`）
  - `pub trait GroupKeyPersist: Send { fn save(&self, keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String>; fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String>; }`
  - `GroupKeyStore::with_persist(persist: Box<dyn GroupKeyPersist>) -> Self`
  - `GroupKeyStore::keysets(&self) -> Vec<GroupKeySet>`
  - `net::store::{FileGroupKeyStore, group_key_store_in_dir(dir: &Path) -> FileGroupKeyStore}`（`<dir>/group_keys.json`）

- [ ] **Step 1: 失敗するテストを書く**

`group_key_management.rs` の tests 末尾:

```rust
    struct MemPersist(std::sync::Arc<std::sync::Mutex<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>)>>);
    impl GroupKeyPersist for MemPersist {
        fn save(&self, keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String> {
            *self.0.lock().unwrap() = (keysets.to_vec(), map.to_vec());
            Ok(())
        }
        fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// 変異 4 種（upsert / replace map / append map / purge）が毎回 save され、
    /// 別インスタンスが load で同じ状態に戻る。
    #[test]
    fn mutations_persist_and_reload_in_a_new_instance() {
        let cell = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new())));
        {
            let store = GroupKeyStore::with_persist(Box::new(MemPersist(cell.clone())));
            store.upsert_keyset(1, 42, [7u8; 16]).unwrap();
            store.replace_fabric_map(1, vec![(10, 42)]);
            store.append_map_entry(1, 11, 42);
            store.upsert_keyset(2, 9, [1u8; 16]).unwrap();
            store.purge_fabric(2);
        }
        let store2 = GroupKeyStore::with_persist(Box::new(MemPersist(cell)));
        assert_eq!(
            store2.keysets(),
            vec![GroupKeySet { fabric_index: 1, keyset_id: 42, epoch_key0: [7u8; 16] }]
        );
        assert_eq!(store2.map_entries_for(1), vec![(10, 42), (11, 42)]);
        assert!(store2.map_entries_for(2).is_empty());
    }

    struct FailingPersist;
    impl GroupKeyPersist for FailingPersist {
        fn save(&self, _: &[GroupKeySet], _: &[GroupKeyMapEntry]) -> Result<(), String> {
            Err("disk full".into())
        }
        fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String> {
            Err("corrupt".into())
        }
    }

    /// load 失敗は空から開始、save 失敗は in-memory を進める（AclStore と同じ）。
    #[test]
    fn persist_failures_do_not_block_the_store() {
        let store = GroupKeyStore::with_persist(Box::new(FailingPersist));
        assert!(store.keysets().is_empty());
        store.upsert_keyset(1, 42, [7u8; 16]).unwrap();
        assert!(store.keyset_exists(1, 42));
    }

    #[test]
    fn keyset_debug_redacts_the_epoch_key() {
        let s = format!("{:?}", GroupKeySet { fabric_index: 1, keyset_id: 42, epoch_key0: [0xAB; 16] });
        assert!(s.contains("REDACTED") && !s.contains("171") && !s.contains("ab"), "{s}");
    }
```

`net/store.rs` の tests 末尾:

```rust
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
        let mode = std::fs::metadata(dir.path().join("group_keys.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let store2 = GroupKeyStore::with_persist(Box::new(group_key_store_in_dir(dir.path())));
        assert_eq!(
            store2.keysets(),
            vec![GroupKeySet { fabric_index: 1, keyset_id: 42, epoch_key0: [7u8; 16] }]
        );
        assert_eq!(store2.map_entries_for(1), vec![(10, 42)]);
    }

    #[test]
    fn group_key_load_with_no_file_yet_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (k, m) = group_key_store_in_dir(dir.path()).load().unwrap();
        assert!(k.is_empty() && m.is_empty());
    }
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat-device --features net --lib group_key_management::tests`
Expected: コンパイルエラー（`GroupKeyPersist` / `with_persist` / `keysets` 未定義、struct が private）

- [ ] **Step 3: 実装**

`group_key_management.rs`:

```rust
use serde::{Deserialize, Serialize};

/// デバイス上の 1 KeySet（epoch key 0 のみ — モジュール doc）。`Debug` は
/// 鍵を伏せる（`FabricEntry`/`GroupCredentials` と同じ方針）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeySet {
    pub fabric_index: u8,
    pub keyset_id: u16,
    pub epoch_key0: [u8; 16],
}

impl std::fmt::Debug for GroupKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupKeySet")
            .field("fabric_index", &self.fabric_index)
            .field("keyset_id", &self.keyset_id)
            .field("epoch_key0", &"[REDACTED]")
            .finish()
    }
}

/// `GroupKeyMapStruct` 1 件（spec §11.2.7.6: GroupId → KeySetID）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeyMapEntry {
    pub fabric_index: u8,
    pub group_id: u16,
    pub keyset_id: u16,
}

/// 永続化境界（`core::access_control::AclPersist` と同型: whole-table
/// save/load、具象は `net::store::FileGroupKeyStore`）。
pub trait GroupKeyPersist: Send {
    fn save(&self, keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String>;
    fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String>;
}

#[derive(Default)]
struct GroupKeyInner {
    keysets: Vec<GroupKeySet>,
    map: Vec<GroupKeyMapEntry>,
    persist: Option<Box<dyn GroupKeyPersist>>,
}
```

`impl GroupKeyStore` に追加:

```rust
    /// `persist` から復元して開始する。load 失敗（壊れた JSON 等）は warn
    /// して空から始める — 起動を止めない。
    pub fn with_persist(persist: Box<dyn GroupKeyPersist>) -> Self {
        let (keysets, map) = match persist.load() {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(error = %e, "group key store: load failed; starting empty");
                (Vec::new(), Vec::new())
            }
        };
        Self(Arc::new(Mutex::new(GroupKeyInner {
            keysets,
            map,
            persist: Some(persist),
        })))
    }

    /// 全 fabric の KeySet（groupcast 受信の復号候補列挙用）。
    pub fn keysets(&self) -> Vec<GroupKeySet> {
        self.lock().keysets.clone()
    }

    /// save 失敗は warn して in-memory を進める（`AclStore::save` と同じ理由:
    /// 直前の正当な状態が残るだけで安全性は落ちない）。
    fn save(guard: &GroupKeyInner) {
        if let Some(persist) = &guard.persist {
            if let Err(e) = persist.save(&guard.keysets, &guard.map) {
                tracing::warn!(error = %e, "group key store: save failed; keeping in-memory state");
            }
        }
    }
```

`upsert_keyset`（両パス）/ `purge_fabric` / `replace_fabric_map` / `append_map_entry` の末尾（guard を落とす前）に `Self::save(&guard);` を入れる。`GroupKeyStore` の `#[derive(Default)]` は `GroupKeyInner` の `persist: None` で成立するのでそのまま。モジュール doc の「永続化は未実装（既知ギャップ、groupcast タスク送り）」と `GroupKeyStore` doc の「永続化なし（M3 送り）」を「`with_persist` で `<store_dir>/group_keys.json`（`net::store::FileGroupKeyStore`）に永続化」に書き換える。

`net/store.rs`（`use crate::core::group_key_management::{GroupKeyMapEntry, GroupKeyPersist, GroupKeySet};` を追加）:

```rust
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
                let file: GroupKeyFile = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                Ok((file.keysets, file.map))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Vec::new(), Vec::new())),
            Err(e) => Err(e.to_string()),
        }
    }
}

pub fn group_key_store_in_dir(dir: &Path) -> FileGroupKeyStore {
    FileGroupKeyStore::new(dir.join("group_keys.json"))
}
```

`device.rs`: `let gk_store = crate::core::group_key_management::GroupKeyStore::with_persist(Box::new(crate::net::store::group_key_store_in_dir(&config.store_dir)));` にし、直上コメントの「Task 1 時点で非永続 … 既知ギャップ」を「`<store_dir>/group_keys.json` に永続化（`AclStore` と同じ file-backed persist 注入）」へ。

- [ ] **Step 4: 通過確認**

Run: `cargo test -p mat-device --features net --lib group_key_management::tests` と `cargo test -p mat-device --features net --lib store::tests`
Expected: 全 PASS（既存テストは `GroupKeyStore::new()` のまま通る）

- [ ] **Step 5: fmt / clippy / 全体テスト / Commit**

```bash
git add crates/mat-device/src/core/group_key_management.rs crates/mat-device/src/net/store.rs crates/mat-device/src/device.rs
git commit -m "feat(mat-device): GroupKeyStore を group_keys.json に永続化（レーン A フェーズ 2 Task 1）"
```

---

### Task 2: GroupMembershipStore（新設）と GroupsHandler の委譲・永続化・purge 配線

**Files:**
- Create: `crates/mat-device/src/core/group_membership.rs`
- Modify: `crates/mat-device/src/core/mod.rs`（`pub mod group_membership;`）
- Modify: `crates/mat-device/src/core/groups.rs`（struct / new / contains / add / GetGroupMembership / RemoveGroup / RemoveAllGroups、tests の `handler()`、モジュール doc「永続化は M3 送り」）
- Modify: `crates/mat-device/src/core/bridge.rs`（`build_bridged_endpoint` に `endpoint: u16, membership: &GroupMembershipStore` を追加、tests）
- Modify: `crates/mat-device/src/device.rs`（membership store 生成 + `build_bridged_endpoint` 呼び出し + `comm_server.set_group_membership_store`）
- Modify: `crates/mat-device/src/core/commissioning.rs`（277-283 行付近の `Inner` フィールド、331 行付近に setter、purge 3 箇所 925/928・1268/1271・1312/1315 行付近）
- Modify: `crates/mat-device/src/net/store.rs`（`FileGroupMembershipStore` / `group_membership_in_dir`、tests）

**Interfaces:**
- Produces:
  - `pub const GROUP_TABLE_CAPACITY: usize = 16;`（groups.rs から移動、`pub`）
  - `pub struct GroupMember { pub fabric_index: u8, pub group_id: u16, pub endpoint: u16 }`（`Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize`）
  - `pub trait GroupMembershipPersist: Send { fn save(&self, members: &[GroupMember]) -> Result<(), String>; fn load(&self) -> Result<Vec<GroupMember>, String>; }`
  - `#[derive(Clone, Default)] pub struct GroupMembershipStore(...)` with `new()`, `with_persist(Box<dyn GroupMembershipPersist>)`, `contains(fabric: u8, group: u16, endpoint: u16) -> bool`, `add(fabric, group, endpoint) -> Result<(), u8>`, `remove(fabric, group, endpoint) -> bool`, `remove_all(fabric, endpoint)`, `groups_for(fabric, endpoint) -> Vec<u16>`, `endpoints_for(fabric, group) -> Vec<u16>`, `groups_by_fabric() -> Vec<(u8, u16)>`, `count_for_endpoint(endpoint) -> usize`, `purge_fabric(fabric)`
  - `GroupsHandler::new(identify: IdentifyState, store: GroupMembershipStore, endpoint: u16)`
  - `bridge::build_bridged_endpoint(kind, name, unique_id, endpoint: u16, membership: &GroupMembershipStore)`
  - `CommissioningServer::set_group_membership_store(&mut self, store: GroupMembershipStore)`
  - `net::store::{FileGroupMembershipStore, group_membership_in_dir(dir) -> FileGroupMembershipStore}`（`<dir>/groups.json`、鍵無しなので権限制限なし）

- [ ] **Step 1: 失敗するテストを書く**

`group_membership.rs`（新規ファイルの末尾に tests）:

```rust
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
        assert_eq!(s.add(1, 0x100, 2), Err(mat_controller::im::STATUS_RESOURCE_EXHAUSTED));
        assert_eq!(s.add(1, 0x100, 3), Ok(()), "another endpoint has its own capacity");
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
}
```

`groups.rs` tests: `handler()` を `GroupsHandler::new(state, GroupMembershipStore::new(), 1)` に変え、追加:

```rust
    /// 2 つの endpoint の handler が同じ store を共有すると、同じ group への
    /// AddGroup が endpoint 横断の membership（groupcast のディスパッチ先）
    /// として見える。
    #[test]
    fn two_endpoints_sharing_a_store_are_both_members_of_the_group() {
        let store = GroupMembershipStore::new();
        let (_i1, s1) = IdentifyHandler::new();
        let (_i2, s2) = IdentifyHandler::new();
        let mut ep2 = GroupsHandler::new(s1, store.clone(), 2);
        let mut ep3 = GroupsHandler::new(s2, store.clone(), 3);
        assert_eq!(add_group(&mut ep2, 1, 10), im::STATUS_SUCCESS);
        assert_eq!(add_group(&mut ep3, 1, 10), im::STATUS_SUCCESS);
        assert_eq!(store.endpoints_for(1, 10), vec![2, 3]);
        // ep2 の RemoveGroup は ep3 の membership に影響しない
        let reply = ep2.invoke(im::CMD_REMOVE_GROUP, &group_fields(10), &mut fabric_ctx(1));
        assert_eq!(decode_status_response(&reply, RESP_REMOVE_GROUP).0, im::STATUS_SUCCESS);
        assert_eq!(store.endpoints_for(1, 10), vec![3]);
    }
```

`net/store.rs` tests:

```rust
    #[test]
    fn group_membership_with_persist_reloads_across_instances() {
        use crate::core::group_membership::GroupMembershipStore;
        let dir = tempfile::tempdir().unwrap();
        {
            let s = GroupMembershipStore::with_persist(Box::new(group_membership_in_dir(dir.path())));
            s.add(1, 10, 2).unwrap();
        }
        let s2 = GroupMembershipStore::with_persist(Box::new(group_membership_in_dir(dir.path())));
        assert_eq!(s2.endpoints_for(1, 10), vec![2]);
    }
```

`commissioning.rs` tests（既存の RemoveFabric / fail-safe rollback の purge テスト — `set_acl_store` を使っている 1839 行付近・1952 行付近のテスト — に membership store を並べて追加 assert）:

```rust
        let membership = crate::core::group_membership::GroupMembershipStore::new();
        server.set_group_membership_store(membership.clone());
        membership.add(1, 10, 2).unwrap();
        // ... 既存の RemoveFabric / rollback を実行した後:
        assert!(membership.groups_by_fabric().is_empty(), "purge must drop the fabric's memberships");
```
（既存テストの構造に合わせて、purge を確認している assert の隣に置く。2 テスト = RemoveFabric と fail-safe rollback の両方。）

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat-device --features net --lib group_membership` / `groups::tests`
Expected: コンパイルエラー（モジュール未定義 / `GroupsHandler::new` の引数不一致）

- [ ] **Step 3: 実装**

`core/group_membership.rs`:

```rust
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
        self.lock().members.contains(&GroupMember { fabric_index, group_id, endpoint })
    }

    pub fn count_for_endpoint(&self, endpoint: u16) -> usize {
        self.lock().members.iter().filter(|m| m.endpoint == endpoint).count()
    }

    /// 既存なら `Ok` no-op、endpoint の件数が `GROUP_TABLE_CAPACITY` に
    /// 達していれば `STATUS_RESOURCE_EXHAUSTED`。
    pub fn add(&self, fabric_index: u8, group_id: u16, endpoint: u16) -> Result<(), u8> {
        let mut guard = self.lock();
        let member = GroupMember { fabric_index, group_id, endpoint };
        if guard.members.contains(&member) {
            return Ok(());
        }
        if guard.members.iter().filter(|m| m.endpoint == endpoint).count() >= GROUP_TABLE_CAPACITY {
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
        guard.members.retain(|m| *m != GroupMember { fabric_index, group_id, endpoint });
        let removed = guard.members.len() != before;
        if removed {
            Self::save(&guard);
        }
        removed
    }

    pub fn remove_all(&self, fabric_index: u8, endpoint: u16) {
        let mut guard = self.lock();
        guard.members.retain(|m| !(m.fabric_index == fabric_index && m.endpoint == endpoint));
        Self::save(&guard);
    }

    pub fn groups_for(&self, fabric_index: u8, endpoint: u16) -> Vec<u16> {
        self.lock().members.iter()
            .filter(|m| m.fabric_index == fabric_index && m.endpoint == endpoint)
            .map(|m| m.group_id).collect()
    }

    pub fn endpoints_for(&self, fabric_index: u8, group_id: u16) -> Vec<u16> {
        self.lock().members.iter()
            .filter(|m| m.fabric_index == fabric_index && m.group_id == group_id)
            .map(|m| m.endpoint).collect()
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
```

`groups.rs`: `const GROUP_TABLE_CAPACITY` を削除して `use crate::core::group_membership::{GroupMembershipStore, GROUP_TABLE_CAPACITY};`。struct は `{ identify: IdentifyState, store: GroupMembershipStore, endpoint: u16 }`、`new(identify, store, endpoint)`。`contains` → `self.store.contains(fabric, group_id, self.endpoint)`。`add`:

```rust
    fn add(&mut self, fabric: u8, group_id: u16) -> u8 {
        if group_id == 0 {
            return im::STATUS_CONSTRAINT_ERROR;
        }
        match self.store.add(fabric, group_id, self.endpoint) {
            Ok(()) => im::STATUS_SUCCESS,
            Err(status) => status,
        }
    }
```
`GetGroupMembership`: `let mine = self.store.groups_for(fabric, self.endpoint); let matching: Vec<u16> = mine.iter().copied().filter(|g| requested.is_empty() || requested.contains(g)).collect();` capacity = `(GROUP_TABLE_CAPACITY - self.store.count_for_endpoint(self.endpoint)) as u64`。`RemoveGroup`: `else if self.store.remove(fabric, group_id, self.endpoint) { SUCCESS } else { NOT_FOUND }`。`RemoveAllGroups`: `self.store.remove_all(fabric, self.endpoint);`。モジュール doc の「永続化は M3 送り: テーブルは in-memory で、再起動で消える」を「membership は `GroupMembershipStore`（`groups.json` に永続化）を全 endpoint で共有」に。

`bridge.rs`: `pub fn build_bridged_endpoint(kind: DeviceKind, name: &str, unique_id: &str, endpoint: u16, membership: &GroupMembershipStore) -> BridgedEndpoint`、`GroupsHandler::new(identify_state, membership.clone(), endpoint)`。tests は `build_bridged_endpoint(DeviceKind::OnOffLight, "Living", "uid-1", 2, &GroupMembershipStore::new())`。

`device.rs`: `let membership = GroupMembershipStore::with_persist(Box::new(crate::net::store::group_membership_in_dir(&config.store_dir)));` を `gk_store` の直後に置き `comm_server.set_group_membership_store(membership.clone());`、ループ内 `build_bridged_endpoint(device.kind, &device.name, &..., *endpoint, &membership)`。

`commissioning.rs`: `Inner` に `group_membership_store: Option<GroupMembershipStore>`（`None` 初期化）、setter を `set_group_key_store` の隣に（doc は同趣旨）、purge 3 箇所に

```rust
            if let Some(store) = &self.group_membership_store {
                store.purge_fabric(fabric_index);
            }
```

`net/store.rs`: `FileGroupMembershipStore { path }` / `GroupMembershipPersist` impl（`FileAclStore` と同型: `serde_json::to_vec(members)` + `write_atomic`、権限制限なし、NotFound → 空）/ `group_membership_in_dir(dir) = dir.join("groups.json")`。

`core/mod.rs`: `pub mod group_membership;`。

- [ ] **Step 4: 通過確認**

Run: `cargo test -p mat-device --features net`
Expected: 全 PASS（`bridge`/`groups`/`commissioning`/`store`/`device` テスト含む）

- [ ] **Step 5: fmt / clippy / Commit**

```bash
git add crates/mat-device/src/core/group_membership.rs crates/mat-device/src/core/mod.rs crates/mat-device/src/core/groups.rs crates/mat-device/src/core/bridge.rs crates/mat-device/src/device.rs crates/mat-device/src/core/commissioning.rs crates/mat-device/src/net/store.rs
git commit -m "feat(mat-device): Groups membership を共有 GroupMembershipStore に集約し groups.json に永続化（レーン A フェーズ 2 Task 2）"
```

---

### Task 3: GroupTable 属性の実配線

**Files:**
- Modify: `crates/mat-device/src/core/group_key_management.rs`（`GroupKeyManagementHandler` struct/new 174-182 行付近、`read` の `ATTR_GROUP_TABLE` 235-240 行付近、モジュール doc、tests の `GroupKeyManagementHandler::new(..)` 9 箇所）
- Modify: `crates/mat-device/src/device.rs`（`GroupKeyManagementHandler::new(gk_store)` 334-337 行付近）

**Interfaces:**
- Consumes: Task 2 の `GroupMembershipStore::{groups_by_fabric, endpoints_for}`
- Produces: `GroupKeyManagementHandler::new(store: GroupKeyStore, membership: GroupMembershipStore)`

- [ ] **Step 1: 失敗するテストを書く**

`group_key_management.rs` tests（`encode_group_key_map` の近くにテスト用デコーダを足す）:

```rust
    /// `GroupTable` の array を `(fabric_index, group_id, endpoints)` に戻す。
    fn decode_group_table(tlv: &[u8]) -> Vec<(u8, u16, Vec<u16>)> {
        let mut r = Reader::new(tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut out = Vec::new();
        loop {
            let el = r.next().unwrap().unwrap();
            match el.value {
                Value::ContainerEnd => break,
                Value::StructStart => {
                    let (mut fabric, mut group, mut eps) = (0u8, 0u16, Vec::new());
                    loop {
                        let e = r.next().unwrap().unwrap();
                        match (e.tag, e.value) {
                            (_, Value::ContainerEnd) => break,
                            (Tag::Context(1), Value::Uint(v)) => group = v as u16,
                            (Tag::Context(2), Value::ArrayStart) => loop {
                                let x = r.next().unwrap().unwrap();
                                match x.value {
                                    Value::ContainerEnd => break,
                                    Value::Uint(v) => eps.push(v as u16),
                                    other => panic!("unexpected {other:?}"),
                                }
                            },
                            (Tag::Context(254), Value::Uint(v)) => fabric = v as u8,
                            _ => {}
                        }
                    }
                    out.push((fabric, group, eps));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        out
    }

    #[test]
    fn group_table_is_derived_from_membership_and_fabric_filtered() {
        let membership = crate::core::group_membership::GroupMembershipStore::new();
        membership.add(1, 10, 2).unwrap();
        membership.add(1, 10, 3).unwrap();
        membership.add(2, 20, 2).unwrap();
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), membership);
        let filtered = h.read(im::ATTR_GROUP_TABLE, &ReadCtx { fabric_index: 1, fabric_filtered: true, ..ReadCtx::default() }).unwrap();
        assert_eq!(decode_group_table(&filtered), vec![(1, 10, vec![2, 3])]);
        let all = h.read(im::ATTR_GROUP_TABLE, &ReadCtx::unfiltered(1)).unwrap();
        assert_eq!(decode_group_table(&all), vec![(1, 10, vec![2, 3]), (2, 20, vec![2])]);
    }
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat-device --features net --lib group_key_management::tests::group_table`
Expected: コンパイルエラー（`new` の引数数）

- [ ] **Step 3: 実装**

```rust
pub struct GroupKeyManagementHandler {
    store: GroupKeyStore,
    membership: GroupMembershipStore,
}

impl GroupKeyManagementHandler {
    pub fn new(store: GroupKeyStore, membership: GroupMembershipStore) -> Self {
        Self { store, membership }
    }
```
（`#[derive(Default)]` は残す — 両 store が `Default`。）`read`:

```rust
            im::ATTR_GROUP_TABLE => {
                // spec §11.2.7.7 GroupInfoMapStruct {1: GroupId, 2: Endpoints,
                // 254: FabricIndex} — GroupName(3) は NameSupport=0 なので省略。
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                for (fabric_index, group_id) in self.membership.groups_by_fabric() {
                    if ctx.fabric_filtered && fabric_index != ctx.fabric_index {
                        continue;
                    }
                    w.start_struct(Tag::Anonymous);
                    w.put_uint(Tag::Context(1), u64::from(group_id));
                    w.start_array(Tag::Context(2));
                    for endpoint in self.membership.endpoints_for(fabric_index, group_id) {
                        w.put_uint(Tag::Anonymous, u64::from(endpoint));
                    }
                    w.end_container();
                    w.put_uint(Tag::Context(254), u64::from(fabric_index));
                    w.end_container();
                }
                w.end_container();
                Some(w.finish())
            }
```
`read` の doc と モジュール doc の「`ATTR_GROUP_TABLE` は空 array のまま」を更新。tests の既存 `new(..)` 9 箇所は `GroupKeyManagementHandler::new(store, GroupMembershipStore::new())` に（`use crate::core::group_membership::GroupMembershipStore;` を tests に追加）。`device.rs` は `GroupKeyManagementHandler::new(gk_store, membership.clone())`（`membership` は Task 2 で定義済み — `comm_server.set_group_membership_store` より後でも `clone` なので順序自由）。

- [ ] **Step 4: 通過確認 / Step 5: fmt / clippy / Commit**

```bash
git add crates/mat-device/src/core/group_key_management.rs crates/mat-device/src/device.rs
git commit -m "feat(mat-device): GroupTable 属性を membership から派生（レーン A フェーズ 2 Task 3）"
```

---

### Task 4: ACL の Group auth mode 照合（`Subject::group`）

**Files:**
- Modify: `crates/mat-device/src/core/access_control.rs`（`Subject` 119-135 行付近、`check` 276-294 行付近、tests）

**Interfaces:**
- Produces: `Subject { pub node_id: u64, pub cats: CaseAuthTags, pub group_id: Option<u16> }`、`Subject::group(group_id: u16) -> Self`、`check` が `AUTH_MODE_GROUP` エントリを group subject に対して照合

- [ ] **Step 1: 失敗するテストを書く**

```rust
    /// spec §6.6.2.1.3 Group auth mode: subject は GroupId。Group エントリは
    /// group session（`Subject::group`）だけに、CASE エントリは CASE session
    /// だけに効く。
    #[test]
    fn check_matches_group_entries_only_for_group_subjects() {
        let store = AclStore::new();
        store.set_entries_for_test(1, vec![
            AclDeviceEntry { privilege: PRIVILEGE_ADMINISTER, auth_mode: AUTH_MODE_CASE, subjects: vec![112233], targets_raw: None, fabric_index: 1 },
            AclDeviceEntry { privilege: PRIVILEGE_OPERATE, auth_mode: AUTH_MODE_GROUP, subjects: vec![10], targets_raw: None, fabric_index: 1 },
            AclDeviceEntry { privilege: PRIVILEGE_VIEW, auth_mode: AUTH_MODE_GROUP, subjects: vec![], targets_raw: None, fabric_index: 1 },
        ]);
        // group 10 は Operate まで、group 11 は wildcard 経由で View のみ
        assert!(store.check(1, Subject::group(10), PRIVILEGE_OPERATE, 2, 0x0006));
        assert!(!store.check(1, Subject::group(10), PRIVILEGE_MANAGE, 2, 0x0006));
        assert!(store.check(1, Subject::group(11), PRIVILEGE_VIEW, 2, 0x0006));
        assert!(!store.check(1, Subject::group(11), PRIVILEGE_OPERATE, 2, 0x0006));
        // group subject は CASE の Administer エントリに乗れない
        assert!(!store.check(1, Subject::group(10), PRIVILEGE_ADMINISTER, 0, 0x001F));
        // CASE subject は Group エントリに乗れない（subject 値 10 の node id でも）
        assert!(!store.check(1, Subject::node(10), PRIVILEGE_OPERATE, 2, 0x0006));
        assert!(store.check(1, Subject::node(112233), PRIVILEGE_ADMINISTER, 0, 0x001F));
        // 他 fabric には効かない
        assert!(!store.check(2, Subject::group(10), PRIVILEGE_VIEW, 2, 0x0006));
    }
```

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-device --features net --lib access_control::tests::check_matches_group` → コンパイルエラー（`Subject::group` 未定義）

- [ ] **Step 3: 実装**

```rust
pub struct Subject {
    pub node_id: u64,
    pub cats: CaseAuthTags,
    /// `Some(group_id)` = group session の subject（spec §6.6.2.1.3、
    /// groupcast 受信）。`None` = CASE/PASE。ACL の照合は auth mode ごとに
    /// 排他: Group エントリは `Some` にだけ、CASE エントリは `None` にだけ。
    pub group_id: Option<u16>,
}

impl Subject {
    pub fn new(node_id: u64, cats: CaseAuthTags) -> Self {
        Self { node_id, cats, group_id: None }
    }
    pub fn node(node_id: u64) -> Self { Self::new(node_id, CaseAuthTags::default()) }
    /// group session の subject（node id 0、CAT なし）。
    pub fn group(group_id: u16) -> Self {
        Self { node_id: 0, cats: CaseAuthTags::default(), group_id: Some(group_id) }
    }
```

`check` の `e.auth_mode == AUTH_MODE_CASE && (e.subjects.is_empty() || ...)` を `subject_matches_entry(subject, e)` に置換:

```rust
/// auth mode 別の subject 照合（`check` の 1 条件）。空 `subjects` はどちらの
/// auth mode でも wildcard。
fn subject_matches_entry(subject: Subject, e: &AclDeviceEntry) -> bool {
    match (e.auth_mode, subject.group_id) {
        (AUTH_MODE_CASE, None) => {
            e.subjects.is_empty() || e.subjects.iter().any(|&s| subject.matches(s))
        }
        (AUTH_MODE_GROUP, Some(group_id)) => {
            e.subjects.is_empty() || e.subjects.contains(&u64::from(group_id))
        }
        _ => false,
    }
}
```
`check` の doc の「auth_mode が CASE」の記述と `AUTH_MODE_CASE` 定数 doc（フェーズ 1 の「照合はフェーズ 2 まで CASE のみ」）を現状に更新。`Subject` を struct リテラルで組む箇所（`/usr/bin/grep -rn "Subject {" crates/mat-device/src`）があれば `group_id: None` を足す。

- [ ] **Step 4: 通過確認 / Step 5: fmt / clippy / Commit**

```bash
git add crates/mat-device/src/core/access_control.rs
git commit -m "feat(mat-device): ACL check が Group auth mode エントリを group subject に照合（レーン A フェーズ 2 Task 4）"
```

---

### Task 5: group InvokeRequest デコーダと `Node::handle_group_invoke`

**Files:**
- Create: `crates/mat-device/src/core/group_invoke.rs`
- Modify: `crates/mat-device/src/core/mod.rs`（`pub mod group_invoke;`）
- Modify: `crates/mat-device/src/core/datamodel.rs`（`handle_invoke` 1001-1130 行付近の後半を `invoke_on_endpoint` に切り出し、`handle_group_invoke` 追加、tests）

**Interfaces:**
- Produces:
  - `pub struct GroupInvokeIn { pub cluster: u32, pub command: u32, pub fields_tlv: Vec<u8> }`（`Debug, Clone, PartialEq, Eq`）
  - `pub fn decode_group_invoke_request(payload: &[u8]) -> Result<Vec<GroupInvokeIn>, mat_controller::im::ImError>`
  - `Node::handle_group_invoke(&mut self, endpoints: &[u16], invokes: &[GroupInvokeIn], ctx: &mut InvokeCtx) -> Vec<(u16, u32, u32)>`

- [ ] **Step 1: 失敗するテストを書く**

`group_invoke.rs` tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im;
    use mat_controller::tlv::{Tag, Writer};

    #[test]
    fn decodes_the_controllers_group_invoke_with_and_without_fields() {
        let payload = im::encode_group_invoke_request(im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None);
        assert_eq!(
            decode_group_invoke_request(&payload).unwrap(),
            vec![GroupInvokeIn { cluster: im::CLUSTER_ON_OFF, command: im::CMD_ON_OFF_ON, fields_tlv: Vec::new() }]
        );
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 3);
        w.end_container();
        let fields = w.finish();
        let payload = im::encode_group_invoke_request(0x0008, 0x04, Some(&fields));
        let out = decode_group_invoke_request(&payload).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].cluster, out[0].command), (0x0008, 0x04));
        assert_eq!(out[0].fields_tlv, fields);
    }

    /// endpoint 付き CommandPath（unicast 形）も受理し endpoint は無視する。
    #[test]
    fn tolerates_an_endpoint_in_the_command_path() {
        let payload = im::encode_invoke_request(2, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None);
        let out = decode_group_invoke_request(&payload).unwrap();
        assert_eq!((out[0].cluster, out[0].command), (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE));
    }

    #[test]
    fn malformed_or_pathless_requests_are_errors() {
        assert!(decode_group_invoke_request(&[0xFF, 0x00]).is_err());
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous); // CommandDataIB without a CommandPath
        w.end_container();
        w.end_container();
        w.end_container();
        assert!(decode_group_invoke_request(&w.finish()).is_err());
    }
}
```
（`im::encode_invoke_request` の実引数は `crates/mat-controller/src/im.rs` の定義に合わせる — `(endpoint, cluster, command, fields: Option<&[u8]>)` 想定。`CMD_ON_OFF_TOGGLE` 定数名も im.rs で確認。）

`datamodel.rs` tests（`acl_gates_invoke_by_subject_fabric_and_privilege` の近く）:

```rust
    /// group invoke は member endpoint 全部に適用し、changed を endpoint 付きで
    /// 返す。ACL は `Subject::group` × Group エントリで判定。
    #[test]
    fn handle_group_invoke_applies_to_every_member_endpoint_under_group_acl() {
        use crate::core::group_invoke::GroupInvokeIn;
        let mut node = node_with_onoff(); // endpoint 1 に OnOff
        let (onoff2, state2) = crate::core::onoff::OnOffHandler::new();
        node.add_endpoint(2, vec![Box::new(onoff2)]);
        let store = AclStore::new();
        store.set_entries_for_test(1, vec![AclDeviceEntry {
            privilege: PRIVILEGE_OPERATE,
            auth_mode: AUTH_MODE_GROUP,
            subjects: vec![10],
            targets_raw: None,
            fabric_index: 1,
        }]);
        node.set_acl_store(store);
        let toggle = [GroupInvokeIn { cluster: im::CLUSTER_ON_OFF, command: im::CMD_ON_OFF_TOGGLE, fields_tlv: Vec::new() }];
        let mut ctx = InvokeCtx { fabric_index: 1, subject: Subject::group(10), ..InvokeCtx::default() };
        let changed = node.handle_group_invoke(&[1, 2], &toggle, &mut ctx);
        assert_eq!(changed, vec![(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF), (2, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF)]);
        assert!(state2.load(std::sync::atomic::Ordering::SeqCst));
        // 別 group / 別 fabric は ACL で落ちて無変化
        let mut other = InvokeCtx { fabric_index: 1, subject: Subject::group(11), ..InvokeCtx::default() };
        assert!(node.handle_group_invoke(&[1, 2], &toggle, &mut other).is_empty());
        assert!(state2.load(std::sync::atomic::Ordering::SeqCst));
        // 存在しない endpoint / cluster は黙って skip
        let unknown = [GroupInvokeIn { cluster: 0x7FFF, command: 0, fields_tlv: Vec::new() }];
        assert!(node.handle_group_invoke(&[1, 9], &unknown, &mut ctx).is_empty());
    }
```
（`node_with_onoff()` と `OnOffHandler::new() -> (handler, Arc<AtomicBool>)` は既存 — `crates/mat-device/src/core/onoff.rs` と datamodel tests の既存 fixture を確認して名前を合わせる。）

- [ ] **Step 2: 失敗確認** — コンパイルエラー（モジュール/メソッド未定義）

- [ ] **Step 3: 実装**

`core/group_invoke.rs`:

```rust
//! group 宛 InvokeRequest（spec §8.2.5 / §8.9.4）のデコーダ。
//! `mat_controller::im::decode_invoke_request` は CommandPath に Endpoint を
//! 要求する（unicast 形）が、groupcast の CommandPath は endpoint を持たない
//! （`im::encode_group_invoke_request` が作る形）ので、同じ構造を endpoint
//! 任意で読む独自版。複数 CommandDataIB を全部返す（controller は 1 件）。
use mat_controller::im::ImError;
use mat_controller::tlv::{copy_value, skip_container, Reader, Tag, Value, Writer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInvokeIn {
    pub cluster: u32,
    pub command: u32,
    /// CommandFields を `Tag::Anonymous` に再タグした raw TLV（空 = フィールド無し）。
    pub fields_tlv: Vec<u8>,
}

pub fn decode_group_invoke_request(payload: &[u8]) -> Result<Vec<GroupInvokeIn>, ImError> {
    let mut r = Reader::new(payload);
    let el = r.next()?.ok_or(ImError::Malformed("empty invoke request"))?;
    if el.value != Value::StructStart {
        return Err(ImError::Malformed("invoke request is not a struct"));
    }
    let mut out = Vec::new();
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated invoke request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::ArrayStart) => loop {
                let e = r.next()?.ok_or(ImError::Malformed("truncated invoke requests"))?;
                match e.value {
                    Value::ContainerEnd => break,
                    Value::StructStart => out.push(decode_command_data_ib(&mut r)?),
                    Value::ArrayStart | Value::ListStart => skip_container(&mut r)?,
                    _ => {}
                }
            },
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(&mut r)?,
            _ => {}
        }
    }
    Ok(out)
}

fn decode_command_data_ib(r: &mut Reader) -> Result<GroupInvokeIn, ImError> {
    let (mut cluster, mut command, mut fields_tlv) = (None, None, Vec::new());
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated command data ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => loop {
                let e = r.next()?.ok_or(ImError::Malformed("truncated command path"))?;
                match (e.tag, e.value) {
                    (_, Value::ContainerEnd) => break,
                    // Context(0) = Endpoint: group 形には無く、あっても無視
                    (Tag::Context(1), Value::Uint(v)) => {
                        cluster = Some(u32::try_from(v).map_err(|_| ImError::Malformed("cluster out of range"))?)
                    }
                    (Tag::Context(2), Value::Uint(v)) => {
                        command = Some(u32::try_from(v).map_err(|_| ImError::Malformed("command out of range"))?)
                    }
                    (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
                    _ => {}
                }
            },
            (Tag::Context(1), Value::StructStart) => {
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, Value::StructStart)?;
                fields_tlv = w.finish();
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(GroupInvokeIn {
        cluster: cluster.ok_or(ImError::Malformed("command path without cluster"))?,
        command: command.ok_or(ImError::Malformed("command path without command"))?,
        fields_tlv,
    })
}
```
（`ImError` に `From<TlvError>` があるかは `im.rs` の `decode_invoke_request` が `r.next()?` している事実から Yes。`copy_value`/`skip_container` は `mat_controller::tlv` で pub — access_control.rs が使っている。）

`datamodel.rs`:

```rust
    /// `handle_invoke` と `handle_group_invoke` の共通部: endpoint/cluster を
    /// 引き、ACL（`invoke_privilege`）を通し、`invoke` して changed を
    /// `(endpoint, cluster, attribute)` に写し DataVersion を bump する。
    /// `Err(status)` = endpoint/cluster 不在・ACL 拒否（呼び側が応答するか
    /// ログするかを決める）。
    fn invoke_on_endpoint(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: &[u8],
        ctx: &mut InvokeCtx,
    ) -> Result<(InvokeReply, Vec<(u16, u32, u32)>), u8> {
        let Some((_, clusters)) = self.endpoints.iter_mut().find(|(id, _)| *id == endpoint) else {
            return Err(im::STATUS_UNSUPPORTED_ENDPOINT);
        };
        let Some(handler) = clusters.iter_mut().find(|h| h.cluster_id() == cluster) else {
            return Err(im::STATUS_UNSUPPORTED_CLUSTER);
        };
        if !acl_allows(&self.acl, ctx.fabric_index, ctx.subject, handler.invoke_privilege(command), endpoint, cluster) {
            return Err(im::STATUS_UNSUPPORTED_ACCESS);
        }
        ctx.changed.clear();
        let reply = handler.invoke(command, fields_tlv, ctx);
        let changed: Vec<(u16, u32, u32)> = ctx.changed.drain(..).map(|a| (endpoint, cluster, a)).collect();
        if !changed.is_empty() {
            let version = self.versions.entry((endpoint, cluster)).or_insert(self.version_base);
            *version = version.wrapping_add(1);
        }
        Ok((reply, changed))
    }

    /// groupcast の Invoke（spec §8.2.5: 応答なし）: group の member
    /// `endpoints` × `invokes` の全組み合わせに `invoke_on_endpoint`。拒否・
    /// 不在・非 SUCCESS は debug ログのみ。戻りは購読の dirty に流す changed。
    pub fn handle_group_invoke(
        &mut self,
        endpoints: &[u16],
        invokes: &[crate::core::group_invoke::GroupInvokeIn],
        ctx: &mut InvokeCtx,
    ) -> Vec<(u16, u32, u32)> {
        let mut changed = Vec::new();
        for &endpoint in endpoints {
            for inv in invokes {
                match self.invoke_on_endpoint(endpoint, inv.cluster, inv.command, &inv.fields_tlv, ctx) {
                    Ok((reply, mut c)) => {
                        if let InvokeReply::Status(status) = reply {
                            if status != im::STATUS_SUCCESS {
                                tracing::debug!(endpoint, cluster = inv.cluster, command = inv.command, status, "group invoke: command status");
                            }
                        }
                        changed.append(&mut c);
                    }
                    Err(status) => tracing::debug!(endpoint, cluster = inv.cluster, command = inv.command, status, "group invoke: not applied"),
                }
            }
        }
        changed
    }
```
`handle_invoke` は decode 後 `match self.invoke_on_endpoint(req.endpoint, req.cluster, req.command, &req.fields_tlv, ctx)` — `Err(status)` は従来どおり `encode_invoke_response_status(req.endpoint, req.cluster, req.command, status, None)` の `ImOutcome::unchanged`、`Ok((reply, changed))` は既存の `resp_payload` 組み立て + `ImOutcome { changed, .. }`。挙動不変（既存 datamodel テスト全部が回帰テスト）。`InvokeReply` が `Debug`/`PartialEq` でなければ `if let` だけにする。

- [ ] **Step 4: 通過確認** — `cargo test -p mat-device --features net`
- [ ] **Step 5: fmt / clippy / Commit**

```bash
git add crates/mat-device/src/core/group_invoke.rs crates/mat-device/src/core/mod.rs crates/mat-device/src/core/datamodel.rs
git commit -m "feat(mat-device): group InvokeRequest デコーダと Node::handle_group_invoke（レーン A フェーズ 2 Task 5）"
```

---

### Task 6: `net/group_rx.rs` — リプレイ検査と分類・復号パイプライン（純関数）

**Files:**
- Create: `crates/mat-device/src/net/group_rx.rs`
- Modify: `crates/mat-device/src/net/mod.rs`（`pub mod group_rx;`）

**Interfaces:**
- Consumes: Task 1 `GroupKeyStore::{keysets, map_entries_for}`、Task 2 `GroupMembershipStore::{endpoints_for, groups_by_fabric}`、Task 5 `decode_group_invoke_request`、`crate::core::fabric_store::FabricEntry`
- Produces:
  - `pub struct GroupReplayGuard` with `new()`, `accept(&mut self, fabric_index: u8, source_node_id: u64, counter: u32) -> bool`
  - `pub struct GroupRxDeps<'a> { pub fabrics: &'a [FabricEntry], pub gk_store: &'a GroupKeyStore, pub membership: &'a GroupMembershipStore }`
  - `#[derive(Debug, PartialEq, Eq)] pub enum GroupDrop { HeaderDecode, NotGroupSession, Privacy, NoSource, NotGroupDestination, NoKeyset { candidates: usize }, NotMapped, NoMembers, Replay, NotInvoke, Malformed }`
  - `pub struct GroupInvokeBatch { pub fabric_index: u8, pub group_id: u16, pub source_node_id: u64, pub endpoints: Vec<u16>, pub invokes: Vec<GroupInvokeIn> }`
  - `pub fn classify_group_datagram(buf: &[u8], deps: &GroupRxDeps<'_>, replay: &mut GroupReplayGuard) -> Result<GroupInvokeBatch, GroupDrop>`
  - `pub fn desired_group_addrs(fabrics: &[FabricEntry], membership: &GroupMembershipStore) -> HashSet<Ipv6Addr>`
  - consts `pub const SESSION_TYPE_MASK: u8 = 0x03; pub const SESSION_TYPE_GROUP: u8 = 0x01; pub const PRIVACY_FLAG: u8 = 0x80; pub const REPLAY_TABLE_CAPACITY: usize = 64;`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::group_key_management::GroupKeyStore;
    use crate::core::group_membership::GroupMembershipStore;
    use mat_controller::fabric::{compressed_fabric_id, derive_group_session_id, derive_ipk_operational};
    use mat_controller::group::build_group_datagram;
    use mat_controller::im;
    use mat_controller::kvs::GroupCredentials;

    const FABRIC_ID: u64 = 0x2233_4455;
    const EPOCH: [u8; 16] = [9u8; 16];
    const SOURCE: u64 = 660_033;

    /// テスト fabric（root 公開鍵は形だけ — cfid は決定的なら何でもよい）。
    fn fabric(fabric_index: u8) -> FabricEntry {
        FabricEntry {
            fabric_index,
            root_tlv: Vec::new(),
            noc_tlv: Vec::new(),
            icac_tlv: None,
            op_private_key: [1u8; 32],
            ipk_operational: [2u8; 16],
            node_id: 1,
            fabric_id: FABRIC_ID,
            root_public_key: [4u8; 65],
            admin_subject: SOURCE,
            admin_vendor_id: 0xFFF1,
            label: String::new(),
        }
    }

    fn creds(f: &FabricEntry, epoch: &[u8; 16]) -> GroupCredentials {
        let op = derive_ipk_operational(epoch, &compressed_fabric_id(&f.root_public_key, f.fabric_id));
        GroupCredentials { session_id: derive_group_session_id(&op), encryption_key: op }
    }

    fn provisioned() -> (Vec<FabricEntry>, GroupKeyStore, GroupMembershipStore) {
        let gk = GroupKeyStore::new();
        gk.upsert_keyset(1, 42, EPOCH).unwrap();
        gk.replace_fabric_map(1, vec![(10, 42)]);
        let m = GroupMembershipStore::new();
        m.add(1, 10, 2).unwrap();
        m.add(1, 10, 3).unwrap();
        (vec![fabric(1)], gk, m)
    }

    fn datagram(f: &FabricEntry, epoch: &[u8; 16], counter: u32, group: u16) -> Vec<u8> {
        build_group_datagram(&creds(f, epoch), SOURCE, counter, 0x42, group, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None).unwrap()
    }

    #[test]
    fn a_provisioned_group_datagram_yields_the_member_endpoints_and_invoke() {
        let (fabrics, gk, m) = provisioned();
        let mut replay = GroupReplayGuard::new();
        let dg = datagram(&fabrics[0], &EPOCH, 100, 10);
        let batch = classify_group_datagram(&dg, &GroupRxDeps { fabrics: &fabrics, gk_store: &gk, membership: &m }, &mut replay).unwrap();
        assert_eq!((batch.fabric_index, batch.group_id, batch.source_node_id), (1, 10, SOURCE));
        assert_eq!(batch.endpoints, vec![2, 3]);
        assert_eq!(batch.invokes.len(), 1);
        assert_eq!((batch.invokes[0].cluster, batch.invokes[0].command), (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE));
    }

    #[test]
    fn drops_are_classified() {
        let (fabrics, gk, m) = provisioned();
        let deps = GroupRxDeps { fabrics: &fabrics, gk_store: &gk, membership: &m };
        let mut replay = GroupReplayGuard::new();
        // 別 epoch = GKH 不一致（高確率）or 復号失敗 → NoKeyset
        let other = datagram(&fabrics[0], &[3u8; 16], 1, 10);
        assert!(matches!(classify_group_datagram(&other, &deps, &mut replay), Err(GroupDrop::NoKeyset { .. })));
        // map に無い group
        let unmapped = datagram(&fabrics[0], &EPOCH, 2, 11);
        assert_eq!(classify_group_datagram(&unmapped, &deps, &mut replay).unwrap_err(), GroupDrop::NotMapped);
        // mapped だが member なし
        gk.append_map_entry(1, 12, 42);
        let nomember = datagram(&fabrics[0], &EPOCH, 3, 12);
        assert_eq!(classify_group_datagram(&nomember, &deps, &mut replay).unwrap_err(), GroupDrop::NoMembers);
        // リプレイ: 同 counter の 2 通目
        let dg = datagram(&fabrics[0], &EPOCH, 50, 10);
        assert!(classify_group_datagram(&dg, &deps, &mut replay).is_ok());
        assert_eq!(classify_group_datagram(&dg, &deps, &mut replay).unwrap_err(), GroupDrop::Replay);
        // unicast 形の header（session type 0）
        let mut uni = dg.clone();
        uni[3] &= !SESSION_TYPE_MASK; // security flags byte: header layout は message.rs（flags, session id(2), security flags）
        assert_eq!(classify_group_datagram(&uni, &deps, &mut replay).unwrap_err(), GroupDrop::NotGroupSession);
        // privacy ビット
        let mut priv_dg = datagram(&fabrics[0], &EPOCH, 51, 10);
        priv_dg[3] |= PRIVACY_FLAG;
        assert_eq!(classify_group_datagram(&priv_dg, &deps, &mut replay).unwrap_err(), GroupDrop::Privacy);
        // ゴミ
        assert_eq!(classify_group_datagram(&[0u8; 3], &deps, &mut replay).unwrap_err(), GroupDrop::HeaderDecode);
    }

    #[test]
    fn replay_guard_is_strictly_increasing_per_source_and_bounded() {
        let mut g = GroupReplayGuard::new();
        assert!(g.accept(1, 7, 10));
        assert!(!g.accept(1, 7, 10));
        assert!(!g.accept(1, 7, 9));
        assert!(g.accept(1, 7, 11));
        assert!(g.accept(2, 7, 1), "another fabric is another window");
        for src in 100..(100 + REPLAY_TABLE_CAPACITY as u64) {
            assert!(g.accept(1, src, 1));
        }
        // 最古（fabric 1, source 7）が退去 → 同じ counter がまた通る
        assert!(g.accept(1, 7, 11));
    }

    #[test]
    fn desired_addrs_follow_membership_times_fabric() {
        let (fabrics, _gk, m) = provisioned();
        let set = desired_group_addrs(&fabrics, &m);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&mat_controller::group::group_multicast_addr(FABRIC_ID, 10)));
        m.add(1, 11, 2).unwrap();
        assert_eq!(desired_group_addrs(&fabrics, &m).len(), 2);
        // fabric が無い membership は join しない
        m.add(9, 5, 2).unwrap();
        assert_eq!(desired_group_addrs(&fabrics, &m).len(), 2);
    }
}
```
（security flags のバイト位置 `[3]` は `crates/mat-controller/src/message.rs` の `MessageHeader::encode` を読んで確定する: flags(1) + session id(2) + security flags(1) なら index 3。違えば直す。`FabricEntry` のフィールド一式は `core/fabric_store.rs:45-62` を見て埋める。）

- [ ] **Step 2: 失敗確認** — コンパイルエラー

- [ ] **Step 3: 実装**

```rust
//! groupcast 受信（spec §4.15 group session、§8.2.5 group 宛 Invoke）の
//! 純関数側: header 分類 → 復号候補 (GKH 一致 keyset) の試行復号 →
//! GroupKeyMap / membership / リプレイ検査 → group InvokeRequest デコード。
//! ソケットと join は `GroupSocket`（同ファイル、Task 7）、Node への適用は
//! `runtime`。応答は送らない（全 drop は `GroupDrop` で理由を返し、runtime が
//! debug ログにする）。
use std::collections::{HashSet, VecDeque};
use std::net::Ipv6Addr;

use mat_controller::crypto::open_message;
use mat_controller::fabric::{compressed_fabric_id, derive_group_session_id, derive_ipk_operational};
use mat_controller::group::group_multicast_addr;
use mat_controller::im;
use mat_controller::message::{Destination, MessageHeader, PROTOCOL_ID_INTERACTION_MODEL};

use crate::core::fabric_store::FabricEntry;
use crate::core::group_invoke::{decode_group_invoke_request, GroupInvokeIn};
use crate::core::group_key_management::GroupKeyStore;
use crate::core::group_membership::GroupMembershipStore;

/// security flags の session type（spec §4.4.1.4）: 下位 2 bit、0b01 = group。
pub const SESSION_TYPE_MASK: u8 = 0x03;
pub const SESSION_TYPE_GROUP: u8 = 0x01;
/// P フラグ（privacy 処理済み）— 未対応で drop。
pub const PRIVACY_FLAG: u8 = 0x80;
/// リプレイ表の上限 `(fabric, source)` 数。超えたら最古を退去。
pub const REPLAY_TABLE_CAPACITY: usize = 64;

/// spec §4.5.4.2 の group data message counter 検査の簡略版: `(fabric,
/// source node)` ごとに最終 counter を持ち、それ以下は drop。bitmap window
/// は持たず順序逆転は捨てる側に倒す（mat は各 egress に同一 datagram を
/// 流すので、実用上の役目は重複排除）。初見の source は trust-first で受理。
pub struct GroupReplayGuard {
    seen: VecDeque<((u8, u64), u32)>,
}

impl GroupReplayGuard {
    pub fn new() -> Self { Self { seen: VecDeque::new() } }

    pub fn accept(&mut self, fabric_index: u8, source_node_id: u64, counter: u32) -> bool {
        let key = (fabric_index, source_node_id);
        if let Some(pos) = self.seen.iter().position(|(k, _)| *k == key) {
            if counter <= self.seen[pos].1 {
                return false;
            }
            self.seen[pos].1 = counter;
            return true;
        }
        if self.seen.len() >= REPLAY_TABLE_CAPACITY {
            self.seen.pop_front();
        }
        self.seen.push_back((key, counter));
        true
    }
}

impl Default for GroupReplayGuard { fn default() -> Self { Self::new() } }

pub struct GroupRxDeps<'a> {
    pub fabrics: &'a [FabricEntry],
    pub gk_store: &'a GroupKeyStore,
    pub membership: &'a GroupMembershipStore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupDrop {
    HeaderDecode,
    NotGroupSession,
    Privacy,
    NoSource,
    NotGroupDestination,
    NoKeyset { candidates: usize },
    NotMapped,
    NoMembers,
    Replay,
    NotInvoke,
    Malformed,
}

#[derive(Debug)]
pub struct GroupInvokeBatch {
    pub fabric_index: u8,
    pub group_id: u16,
    pub source_node_id: u64,
    pub endpoints: Vec<u16>,
    pub invokes: Vec<GroupInvokeIn>,
}

pub fn classify_group_datagram(
    buf: &[u8],
    deps: &GroupRxDeps<'_>,
    replay: &mut GroupReplayGuard,
) -> Result<GroupInvokeBatch, GroupDrop> {
    let (header, _) = MessageHeader::decode(buf).map_err(|_| GroupDrop::HeaderDecode)?;
    if header.security_flags & SESSION_TYPE_MASK != SESSION_TYPE_GROUP {
        return Err(GroupDrop::NotGroupSession);
    }
    if header.security_flags & PRIVACY_FLAG != 0 {
        return Err(GroupDrop::Privacy);
    }
    let source = header.source_node_id.ok_or(GroupDrop::NoSource)?;
    let Destination::Group(group_id) = header.destination else {
        return Err(GroupDrop::NotGroupDestination);
    };

    // spec §4.15.3: GKH が一致する keyset を全 fabric から集めて順に試す。
    let mut candidates = 0usize;
    let mut opened = None;
    for ks in deps.gk_store.keysets() {
        let Some(f) = deps.fabrics.iter().find(|f| f.fabric_index == ks.fabric_index) else { continue };
        let operational = derive_ipk_operational(&ks.epoch_key0, &compressed_fabric_id(&f.root_public_key, f.fabric_id));
        if derive_group_session_id(&operational) != header.session_id {
            continue;
        }
        candidates += 1;
        if let Ok((_, proto, payload)) = open_message(&operational, buf, source) {
            opened = Some((ks.fabric_index, ks.keyset_id, proto, payload));
            break;
        }
    }
    let Some((fabric_index, keyset_id, proto, payload)) = opened else {
        return Err(GroupDrop::NoKeyset { candidates });
    };
    if !deps.gk_store.map_entries_for(fabric_index).contains(&(group_id, keyset_id)) {
        return Err(GroupDrop::NotMapped);
    }
    let endpoints = deps.membership.endpoints_for(fabric_index, group_id);
    if endpoints.is_empty() {
        return Err(GroupDrop::NoMembers);
    }
    if !replay.accept(fabric_index, source, header.message_counter) {
        return Err(GroupDrop::Replay);
    }
    if proto.protocol_id != PROTOCOL_ID_INTERACTION_MODEL || proto.opcode != im::OPCODE_INVOKE_REQUEST {
        return Err(GroupDrop::NotInvoke);
    }
    let invokes = decode_group_invoke_request(&payload).map_err(|_| GroupDrop::Malformed)?;
    Ok(GroupInvokeBatch { fabric_index, group_id, source_node_id: source, endpoints, invokes })
}

/// join すべき multicast アドレス集合 = 各 fabric × その fabric の membership
/// にある group（fabric が無い membership は無視）。
pub fn desired_group_addrs(fabrics: &[FabricEntry], membership: &GroupMembershipStore) -> HashSet<Ipv6Addr> {
    membership.groups_by_fabric().into_iter()
        .filter_map(|(fabric_index, group_id)| {
            fabrics.iter().find(|f| f.fabric_index == fabric_index)
                .map(|f| group_multicast_addr(f.fabric_id, group_id))
        })
        .collect()
}
```
（`MessageHeader::decode` のエラー型と `open_message` の第 3 引数（`session_source_node_id: u64`）は §0 の調査どおり。リプレイ検査は復号成功の後に置く — 復号できない偽 datagram で counter を汚さないため。）

- [ ] **Step 4: 通過確認** — `cargo test -p mat-device --features net --lib group_rx`
- [ ] **Step 5: fmt / clippy / Commit**

```bash
git add crates/mat-device/src/net/group_rx.rs crates/mat-device/src/net/mod.rs
git commit -m "feat(mat-device): groupcast 受信の分類・復号・検査パイプラインとリプレイ検査（レーン A フェーズ 2 Task 6a）"
```

---

### Task 7: GroupSocket + runtime / Device への配線

**Files:**
- Modify: `crates/mat-device/src/net/group_rx.rs`（`GroupSocket`、`GroupRx`、`group_recv`）
- Modify: `crates/mat-device/src/net/runtime.rs`（`run` 署名 684 行付近、`select!` 742 行付近、unicast 側 drop、`sync_group_joins`、tests の `run(..)` 呼び出し）
- Modify: `crates/mat-device/src/device.rs`（`DeviceConfig.group_port`、`Device { group: GroupRx }`、`Device::new` の bind、`group_local_addr`、`run`）
- Modify: `crates/mat-device/tests/support/mod.rs`（`device_config` に `group_port: 0`）、その他 `DeviceConfig { .. }` リテラル（`/usr/bin/grep -rn "DeviceConfig {" crates/mat-device crates/matv`）

**Interfaces:**
- Produces:
  - `pub struct GroupSocket { .. }` with `bind(port: u16, iface_index: u32) -> std::io::Result<Self>`, `local_addr(&self) -> std::io::Result<SocketAddr>`, `async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>`, `sync_joins(&mut self, desired: &HashSet<Ipv6Addr>)`
  - `pub struct GroupRx { pub socket: Option<GroupSocket>, pub gk_store: GroupKeyStore, pub membership: GroupMembershipStore }`
  - `pub async fn group_recv(socket: &Option<GroupSocket>, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)>`（`None` は `std::future::pending`）
  - `DeviceConfig.group_port: u16`、`Device::group_local_addr(&self) -> Option<SocketAddr>`
  - `runtime::run(transport, local_addr, config, node, comm_server, group: GroupRx)`

- [ ] **Step 1: 失敗するテストを書く**

`group_rx.rs` tests:

```rust
    #[tokio::test]
    async fn group_socket_binds_ephemeral_and_survives_failed_joins() {
        let mut s = GroupSocket::bind(0, 1 /* lo */).unwrap();
        assert_ne!(s.local_addr().unwrap().port(), 0);
        let mut want = HashSet::new();
        want.insert(mat_controller::group::group_multicast_addr(FABRIC_ID, 10));
        s.sync_joins(&want); // lo は IFF_MULTICAST 無し → join 失敗でも panic しない
        s.sync_joins(&HashSet::new()); // leave 側も同様
    }

    #[tokio::test]
    async fn two_group_sockets_can_share_one_port() {
        let a = GroupSocket::bind(0, 1).unwrap();
        let port = a.local_addr().unwrap().port();
        let _b = GroupSocket::bind(port, 1).expect("SO_REUSEPORT lets a second socket bind the same port");
    }
```

`device.rs` tests に:

```rust
    #[test]
    fn group_socket_is_bound_on_the_configured_port() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = test_config(dir.path()); // 既存の DeviceConfig fixture 名に合わせる
        cfg.group_port = 0;
        let d = Device::new(cfg).unwrap();
        assert!(d.group_local_addr().unwrap().port() != 0);
    }
```

- [ ] **Step 2: 失敗確認** — コンパイルエラー

- [ ] **Step 3: 実装**

`group_rx.rs` に追加:

```rust
use std::net::{SocketAddr, Ipv6Addr};
use socket2::{Domain, Protocol, Socket, Type};

/// groupcast 受信ソケット: `[::]:port`（既定 5540）を SO_REUSEADDR +
/// SO_REUSEPORT で bind（同ホストの他プロセスと共存）、multicast join は
/// `sync_joins` で差分管理。`mat_controller::transport::UdpTransport` に
/// join API が無いので mat-device で直接組む。
pub struct GroupSocket {
    socket: tokio::net::UdpSocket,
    iface_index: u32,
    joined: HashSet<Ipv6Addr>,
}

impl GroupSocket {
    pub fn bind(port: u16, iface_index: u32) -> std::io::Result<Self> {
        let sock = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        sock.set_only_v6(true)?;
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;
        sock.bind(&SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), port).into())?;
        sock.set_nonblocking(true)?;
        let socket = tokio::net::UdpSocket::from_std(std::net::UdpSocket::from(sock))?;
        Ok(Self { socket, iface_index, joined: HashSet::new() })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> { self.socket.local_addr() }

    pub async fn recv_from(&self, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
        self.socket.recv_from(buf).await
    }

    /// desired との差分だけ join/leave。失敗は warn（`lo` は join 不可 —
    /// テスト経路）で、成功分だけ `joined` に記録する（失敗分は次回再試行）。
    pub fn sync_joins(&mut self, desired: &HashSet<Ipv6Addr>) {
        for addr in desired.difference(&self.joined.clone()) {
            match self.socket.join_multicast_v6(addr, self.iface_index) {
                Ok(()) => { self.joined.insert(*addr); tracing::info!(%addr, iface_index = self.iface_index, "groupcast: joined"); }
                Err(e) => tracing::warn!(%addr, iface_index = self.iface_index, error = %e, "groupcast: join failed (will retry)"),
            }
        }
        for addr in self.joined.clone().difference(desired) {
            if let Err(e) = self.socket.leave_multicast_v6(addr, self.iface_index) {
                tracing::warn!(%addr, error = %e, "groupcast: leave failed");
            }
            self.joined.remove(addr);
            tracing::info!(%addr, "groupcast: left");
        }
    }
}

/// `runtime::run` が受け取る groupcast 一式（`Device` が組む）。
pub struct GroupRx {
    pub socket: Option<GroupSocket>,
    pub gk_store: GroupKeyStore,
    pub membership: GroupMembershipStore,
}

/// `select!` 用: ソケットが無ければ永遠に pending（`subscription_deadline`
/// と同じ流儀）。
pub async fn group_recv(socket: &Option<GroupSocket>, buf: &mut [u8]) -> std::io::Result<(usize, SocketAddr)> {
    match socket {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}
```

`device.rs`: `DeviceConfig` に

```rust
    /// groupcast 受信ソケットの UDP ポート。Matter の multicast 宛先は
    /// 5540 固定なので本番は 5540（SO_REUSEPORT で他プロセスと共存）。
    /// `0` = エフェメラル（統合テストが `group_local_addr` で知る）。
    pub group_port: u16,
```
`Device` に `group: crate::net::group_rx::GroupRx` フィールド、`Device::new` の unicast bind の直後:

```rust
        let iface_index = mat_controller::dnssd::iface_index(&config.iface).unwrap_or_else(|e| {
            tracing::warn!(iface = %config.iface, error = %e, "groupcast: interface index unknown; joining on the kernel default");
            0
        });
        let group_socket = match crate::net::group_rx::GroupSocket::bind(config.group_port, iface_index) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(port = config.group_port, error = %e, "groupcast receive socket did not bind — device serves unicast only");
                None
            }
        };
        let group = crate::net::group_rx::GroupRx { socket: group_socket, gk_store: gk_store.clone(), membership: membership.clone() };
```
（`gk_store`/`membership` は Task 1/2 で `Device::new` 内に既にある。`dnssd::iface_index` が pub かは `crates/mat-controller/src/dnssd.rs:117` で確認済み。）`pub fn group_local_addr(&self) -> Option<SocketAddr> { self.group.socket.as_ref().and_then(|s| s.local_addr().ok()) }`、`run` は `crate::net::runtime::run(self.transport, self.local_addr, self.config, self.node, self.comm_server, self.group)`。

`runtime.rs`:
- `use crate::net::group_rx::{classify_group_datagram, desired_group_addrs, group_recv, GroupReplayGuard, GroupRx, GroupRxDeps, SESSION_TYPE_MASK};`
- `run(..., mut group: GroupRx)`。loop の前に `let mut replay = GroupReplayGuard::new(); let mut gbuf = [0u8; MAX_DATAGRAM];`。
- `fn sync_group_joins(group: &mut GroupRx, comm_server: &CommissioningServer) { if let Some(sock) = group.socket.as_mut() { sock.sync_joins(&desired_group_addrs(&comm_server.fabrics(), &group.membership)); } }` を loop の**先頭**（`tokio::select!` の直前）で毎回呼ぶ（差分ゼロなら no-op、AddGroup/RemoveFabric/fail-safe 後の追従をこれ 1 箇所で賄う）。
- unicast ブランチ: `MessageHeader::decode` 直後に

```rust
                if header.security_flags & SESSION_TYPE_MASK != 0 {
                    tracing::debug!(peer = %peer, security_flags = header.security_flags, "group-session datagram on the unicast socket dropped (the group socket serves those)");
                    continue;
                }
```
- 新ブランチ（`recv = transport.recv_from(..)` の次）:

```rust
            grecv = group_recv(&group.socket, &mut gbuf) => {
                let Ok((n, from)) = grecv else { continue };
                let fabrics = comm_server.fabrics();
                let deps = GroupRxDeps { fabrics: &fabrics, gk_store: &group.gk_store, membership: &group.membership };
                match classify_group_datagram(&gbuf[..n], &deps, &mut replay) {
                    Ok(batch) => {
                        let mut ctx = InvokeCtx {
                            fabric_index: batch.fabric_index,
                            subject: Subject::group(batch.group_id),
                            ..InvokeCtx::default()
                        };
                        let changed = node.handle_group_invoke(&batch.endpoints, &batch.invokes, &mut ctx);
                        tracing::debug!(peer = %from, fabric_index = batch.fabric_index, group_id = batch.group_id, source_node_id = batch.source_node_id, endpoints = ?batch.endpoints, changed = changed.len(), "groupcast invoke applied");
                        if let Some(sub) = subscription.as_mut() {
                            sub.note_changed(&changed);
                        }
                    }
                    Err(reason) => tracing::debug!(peer = %from, len = n, ?reason, "groupcast datagram dropped"),
                }
            }
```
- runtime のテストで `run(` を直接呼ぶ箇所があれば `GroupRx { socket: None, gk_store: GroupKeyStore::new(), membership: GroupMembershipStore::new() }` を渡す。`DeviceConfig` リテラル（runtime tests fixture、device.rs tests、`tests/support/mod.rs::device_config`、`crates/matv/src/main.rs`）に `group_port: 0`（matv は Task 9 で設定値に）。
- モジュール doc（runtime.rs 冒頭 "Wire classification"）に group socket の 1 段落を追加。

- [ ] **Step 4: 通過確認** — `cargo test -p mat-device --features net`（統合テスト含む — group socket は port 0 で bind される）
- [ ] **Step 5: fmt / clippy / Commit**

```bash
git add crates/mat-device/src/net/group_rx.rs crates/mat-device/src/net/runtime.rs crates/mat-device/src/device.rs crates/mat-device/tests/support/mod.rs crates/matv/src/main.rs
git commit -m "feat(mat-device): groupcast 受信ソケット・multicast join 同期・runtime 配線（レーン A フェーズ 2 Task 6b）"
```

---

### Task 8: 統合テスト `tests/group_receive.rs`

**Files:**
- Create: `crates/mat-device/tests/group_receive.rs`

**Interfaces:**
- Consumes: `support::{commission_directly, device_config, fast_cfg, BRIDGED_EP, DEVICE_NODE_ID}`、`Device::{local_addr, group_local_addr}`、mat-controller pub: `commissioning::CommissioningFabric`（`rcac_tlv`, `fabric_id`, `admin_credentials()`）、`cert::MatterCert::parse`、`fabric::{compressed_fabric_id, derive_ipk_operational, derive_group_session_id}`、`kvs::GroupCredentials`、`group::build_group_datagram`、`case::establish`、`im::{encode_key_set_write_fields, encode_group_key_map_tlv, encode_add_group_fields, CLUSTER_*, CMD_*, ATTR_*}`

- [ ] **Step 1: テストを書く**

```rust
//! Closed-loop proof that matv receives groupcast: provision a group over
//! CASE (KeySetWrite / group-key-map / AddGroup / Group ACL entry), then
//! send a real group-session datagram — built with the controller's own
//! `mat_controller::group::build_group_datagram` from the same epoch key —
//! straight to the device's group socket, and read the OnOff attribute back
//! over CASE. Also pins replay rejection, ACL enforcement on the group
//! subject, and that keys + membership survive a device restart.
//!
//! The datagram is sent as plain unicast UDP to `Device::group_local_addr()`
//! (the header still says `Destination::Group`): loopback has no
//! IFF_MULTICAST, so the multicast leg is covered by `task e2e:device:m3`
//! on a real interface instead.
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use mat_controller::case;
use mat_controller::cert::MatterCert;
use mat_controller::commissioning::CommissioningFabric;
use mat_controller::fabric::{compressed_fabric_id, derive_group_session_id, derive_ipk_operational};
use mat_controller::group::build_group_datagram;
use mat_controller::im;
use mat_controller::kvs::GroupCredentials;
use mat_controller::session::SecureSession;
use mat_controller::tlv::{Tag, Writer};
use mat_controller::transport::{Transport, UdpTransport};

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config, DEVICE_NODE_ID};

const ADMIN_NODE_ID: u64 = 660_033;
const FABRIC_ID: u64 = 0x2233_4455;
const KEYSET_ID: u16 = 42;
const GROUP_ID: u16 = 0x000A;
const EPOCH_KEY: [u8; 16] = [0x5A; 16];
const PRIVILEGE_OPERATE: u8 = 3;
const PRIVILEGE_ADMINISTER: u8 = 5;
const AUTH_MODE_CASE: u8 = 2;
const AUTH_MODE_GROUP: u8 = 3;

fn put_entry(w: &mut Writer, privilege: u8, auth_mode: u8, subjects: &[u64]) {
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(privilege));
    w.put_uint(Tag::Context(2), u64::from(auth_mode));
    w.start_array(Tag::Context(3));
    for s in subjects { w.put_uint(Tag::Anonymous, *s); }
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), 1);
    w.end_container();
}

fn acl_tlv(with_group: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    put_entry(&mut w, PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]);
    if with_group {
        put_entry(&mut w, PRIVILEGE_OPERATE, AUTH_MODE_GROUP, &[u64::from(GROUP_ID)]);
    }
    w.end_container();
    w.finish()
}

fn loopback(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), addr.port())
}

fn group_creds(fabric: &CommissioningFabric) -> GroupCredentials {
    let rcac = MatterCert::parse(&fabric.rcac_tlv).expect("rcac parses");
    let operational = derive_ipk_operational(&EPOCH_KEY, &compressed_fabric_id(&rcac.pub_key, fabric.fabric_id));
    GroupCredentials { session_id: derive_group_session_id(&operational), encryption_key: operational }
}

async fn send_group_toggle(creds: &GroupCredentials, to: SocketAddr, counter: u32) {
    let dg = build_group_datagram(creds, ADMIN_NODE_ID, counter, 0x1234, GROUP_ID, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None).unwrap();
    let sock = tokio::net::UdpSocket::bind("[::1]:0").await.unwrap();
    sock.send_to(&dg, to).await.unwrap();
}

async fn read_onoff(session: &mut SecureSession) -> bool {
    let cfg = support::fast_cfg();
    session
        .read_attribute_json(support::BRIDGED_EP, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("on-off read")
        .as_bool()
        .expect("on-off is a bool")
}

/// Polls until the OnOff attribute equals `want` (the datagram is applied
/// asynchronously by the device loop) — or fails after ~2 s.
async fn expect_onoff(session: &mut SecureSession, want: bool, why: &str) {
    for _ in 0..20 {
        if read_onoff(session).await == want { return; }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{why}: on-off did not become {want}");
}

async fn provision_group(session: &mut SecureSession) {
    let cfg = support::fast_cfg();
    let resp = session.invoke_for_data(0, im::CLUSTER_GROUP_KEY_MANAGEMENT, im::CMD_KEY_SET_WRITE, Some(&im::encode_key_set_write_fields(KEYSET_ID, &EPOCH_KEY)), None, &cfg).await.expect("KeySetWrite");
    assert_eq!(resp.status, im::STATUS_SUCCESS);
    session.write_attribute_tlv(0, im::CLUSTER_GROUP_KEY_MANAGEMENT, im::ATTR_GROUP_KEY_MAP, &im::encode_group_key_map_tlv(&[(GROUP_ID, KEYSET_ID)]), None, &cfg).await.expect("group-key-map write");
    let resp = session.invoke_for_data(support::BRIDGED_EP, im::CLUSTER_GROUPS, im::CMD_ADD_GROUP, Some(&im::encode_add_group_fields(GROUP_ID, "grp")), None, &cfg).await.expect("AddGroup");
    assert_eq!(resp.status, im::STATUS_SUCCESS);
    session.write_attribute_tlv(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &acl_tlv(true), None, &cfg).await.expect("ACL write with the group entry");
}

#[tokio::test]
async fn groupcast_toggle_is_applied_replay_rejected_acl_enforced_and_state_persists() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    let addr = loopback(device.local_addr());
    let group_addr = loopback(device.group_local_addr().expect("group socket bound"));
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der")).expect("paa.der");
    let device_task = tokio::spawn(async move { let _ = device.run().await; });

    let fabric = CommissioningFabric::generate(FABRIC_ID, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    provision_group(&mut session).await;
    let creds = group_creds(&fabric);

    // 1. Toggle via groupcast: off -> on.
    assert!(!read_onoff(&mut session).await);
    send_group_toggle(&creds, group_addr, 100).await;
    expect_onoff(&mut session, true, "first group toggle").await;

    // 2. Replay (same counter): no second toggle.
    send_group_toggle(&creds, group_addr, 100).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(read_onoff(&mut session).await, "a replayed datagram must not toggle again");

    // 3. Next counter toggles again: on -> off.
    send_group_toggle(&creds, group_addr, 101).await;
    expect_onoff(&mut session, false, "second group toggle").await;

    // 4. Without the Group ACL entry the datagram is dropped.
    let cfg = support::fast_cfg();
    session.write_attribute_tlv(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &acl_tlv(false), None, &cfg).await.expect("ACL write without the group entry");
    send_group_toggle(&creds, group_addr, 102).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!read_onoff(&mut session).await, "no Group ACL entry: the groupcast must not be applied");
    session.write_attribute_tlv(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &acl_tlv(true), None, &cfg).await.expect("ACL restore");

    // 5. Restart the device on the same store: keys, map and membership come
    //    back from disk and a fresh groupcast is applied.
    device_task.abort();
    let _ = device_task.await;
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device restart");
    let addr = loopback(device.local_addr());
    let group_addr = loopback(device.group_local_addr().expect("group socket bound"));
    let device_task = tokio::spawn(async move { let _ = device.run().await; });

    let admin = fabric.admin_credentials().expect("admin credentials");
    let transport = Arc::new(Transport::Udp(Arc::new(UdpTransport::bind().await.unwrap())));
    let mut session = None;
    for _ in 0..10 {
        match case::establish(Arc::clone(&transport), addr, &admin, DEVICE_NODE_ID, &cfg).await {
            Ok(s) => { session = Some(s); break; }
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    let mut session = session.expect("CASE after restart");
    let table = session.read_attribute_json(0, im::CLUSTER_GROUP_KEY_MANAGEMENT, im::ATTR_GROUP_TABLE, &cfg).await.expect("GroupTable read");
    assert_eq!(table.as_array().map(Vec::len), Some(1), "GroupTable after restart: {table}");
    let map = session.read_attribute_json(0, im::CLUSTER_GROUP_KEY_MANAGEMENT, im::ATTR_GROUP_KEY_MAP, &cfg).await.expect("GroupKeyMap read");
    assert_eq!(map.as_array().map(Vec::len), Some(1), "GroupKeyMap after restart: {map}");
    assert!(!read_onoff(&mut session).await, "OnOff state itself is not persisted");
    send_group_toggle(&creds, group_addr, 1).await; // fresh replay table after restart
    expect_onoff(&mut session, true, "group toggle after restart").await;

    device_task.abort();
    let _ = device_task.await;
}
```
（`fabric.admin_credentials()` の戻り型と `case::establish` の引数は `tests/support/mod.rs` の CASE ブロックをそのまま写す。`CommissioningFabric.rcac_tlv`/`fabric_id` は pub（確認済み）。`read_attribute_json` の JSON 形（数値キー object の array）は `group_provision.rs` の read-back assert を参照。）

- [ ] **Step 2: 通過確認** — `cargo test -p mat-device --features net --test group_receive`。落ちたら理由を切り分け（device ログを `RUST_LOG=debug` で見る: `groupcast datagram dropped` の `reason`）。src を直す必要があれば BLOCKED 報告。
- [ ] **Step 3: fmt / clippy / 全体テスト / Commit**

```bash
git add crates/mat-device/tests/group_receive.rs
git commit -m "test(mat-device): groupcast 受信の閉ループ — 適用・リプレイ拒否・ACL・再起動永続（レーン A フェーズ 2 Task 7）"
```

---

### Task 9: matv 設定・出力・docs・e2e m3 の groupcast ステップ・全体検証

**Files:**
- Modify: `crates/matv/src/main.rs`（`FileConfig.group_port`、`DeviceConfig` 組み立て、JSON 出力、tests の TOML 文字列は省略可のまま）
- Modify: `scripts/e2e-device-m3.sh`（`mat group provision` PASS 行 219 行付近の直後、matd 起動の前）
- Modify: README / docs の matv 節（`/usr/bin/grep -rn "matv" README.md docs/*.md | head` で該当ファイルを特定; `docs/superpowers/specs/2026-09-03-matv-groupcast-rx-design.md` は触らない）+ `ARCHITECTURE.md` の matv/groupcast 記述があれば 1 行

- [ ] **Step 1: matv**

`FileConfig`:
```rust
    /// groupcast 受信ポート（既定 5540 = Matter multicast の宛先。`0` =
    /// エフェメラル、テスト用）。SO_REUSEPORT で他プロセスと共存する。
    #[serde(default = "default_group_port")]
    group_port: u16,
```
`fn default_group_port() -> u16 { 5540 }`、`DeviceConfig { group_port: cfg.group_port, .. }`、JSON に `"group_port": device.group_local_addr().map(|a| a.port())`。`crates/matv/src/main.rs` の既存 config テストは `group_port` 省略で 5540 になることを 1 件 assert。

- [ ] **Step 2: e2e m3**

`echo "==> PASS: mat group provision ..."` の直後に挿入:

```bash
echo "==> mat group invoke (multicast) — group=$GROUP_ID cluster=onoff command=on endpoint=$DEVICE_EP" >&2
MAT_STORE="$MAT_STORE_DIR" \
    ./target/release/mat --iface "$IFACE" group invoke -g "$GROUP_ID" -c onoff --command on -e "$DEVICE_EP" >&2
sleep 1
READ_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" read --node "$NODE_ID" --endpoint "$DEVICE_EP" --cluster onoff --attribute on-off
)"
echo "$READ_JSON"
[[ "$(json_get value "$READ_JSON")" == "true" ]] || {
    echo "groupcast did not reach matv: on-off is not true after mat group invoke on: $READ_JSON" >&2
    echo "-- matv stderr tail --" >&2; tail -n 40 "$DEVICE_STDERR" >&2
    exit 1
}
echo "==> PASS: groupcast on reached matv over multicast (on-off=true)" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" off --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
```
（`json_get`、`DEVICE_STDERR` 等の変数名はスクリプト既存のものに合わせる。`mat off` の引数形は同スクリプトの `mat on` 行と同じ。matv の `group_port` は e2e の matv.toml で未指定 = 5540。）

- [ ] **Step 3: docs** — matv の README/docs 節に「groupcast 受信: 5540/UDP（REUSEPORT、`group_port` で変更可、0 = エフェメラル）。KeySetWrite / GroupKeyMap / AddGroup / Group ACL が揃った group の Invoke を member endpoint に適用。鍵と membership は `<store>/group_keys.json` / `groups.json`。リプレイは (fabric, source) の単調 counter。privacy / group 宛 Read・Write は未対応」を 1 段落。

- [ ] **Step 4: 検証（直列）**

Run: `task check` → 緑（`mat-controller` の `send_invoke_emits_identical_datagram_on_each_egress` が落ちたら単独再実行で確認）
Run: `task e2e:device:m1` → PASS
Run: `task e2e:device:m3` → PASS（新ステップ "groupcast on reached matv over multicast" と既存の listen イベントの両方）。`MAT_E2E_IFACE` 既定 eth1。

- [ ] **Step 5: Commit**

```bash
git add crates/matv/src/main.rs scripts/e2e-device-m3.sh README.md docs
git commit -m "feat(matv): group_port 設定と groupcast 受信の e2e/docs（レーン A フェーズ 2 Task 8）"
```

---

## Self-Review

- **Spec coverage:** §2.1 → T1、§2.2 → T2、§2.3 → T3、§3.3 → T4、§3.2 手順 7-8（デコーダ・ディスパッチ） → T5、§3.2 手順 1-6 + §3.1 の desired 集合 → T6、§3.1 ソケット/join/§3.5 配線/unicast 側 drop → T7、§5 統合 → T8、§4 matv + §5 e2e + §6 運用ログ → T7/T9。§7 やらないこと: どのタスクも mat-controller を触らない。
- **Placeholder scan:** T5/T6/T8 で「定数名・引数を im.rs / fabric_store.rs / support で確認」と書いた箇所は、触れないクレートの正確な識別子に依存するため実装者の確認ポイントとして明示（コードは全て具体）。
- **Type consistency:** `GroupKeySet`（pub、T1）を T6 が `keysets()` 経由で使用。`GroupMembershipStore::{endpoints_for, groups_by_fabric}`（T2）を T3/T6。`GroupInvokeIn`/`decode_group_invoke_request`（T5）を T6a、`handle_group_invoke`（T5）を T6b。`GroupRx`/`group_recv`/`classify_group_datagram`/`desired_group_addrs`（T6a/b）を runtime。`Subject::group`（T4）を T5 テスト/T6b。`DeviceConfig.group_port`/`group_local_addr`（T7）を T7/T8。
