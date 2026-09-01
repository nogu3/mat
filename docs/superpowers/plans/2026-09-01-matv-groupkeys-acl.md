# matv KeySetWrite + ACL enforcement + matd×matv 回帰テスト Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 仮想デバイス matv が `mat group provision` を受理し（KeySetWrite + group-key-map write）、ACL を実際に enforce し、matd 常駐 Subscribe + `mat listen` を matv 相手に回す E2E 回帰テストを整備する。

**Architecture:** `mat-device` の core 層（純粋、tokio/socket/file 禁止）に `GroupKeyStore`（`AclStore` と同型の `Arc<Mutex<..>>` 共有 state）と `AclStore::check` 判定を足し、`Node` のディスパッチ 3 点（`read_entries` / `handle_write` / `handle_invoke`）で enforce する。呼び出し元 subject は `net/runtime.rs` が `SecureSession::peer_node_id()` から `ReadCtx`/`InvokeCtx` の新フィールドに埋める。E2E は既存 `scripts/e2e-device-m1.sh` の流儀を踏襲した新スクリプト。

**Tech Stack:** Rust (edition 2021 workspace)、tokio（net 層のみ）、独自 TLV（`mat_controller::tlv::{Reader, Writer, Tag, Value}`）。

**Spec:** `docs/superpowers/specs/2026-09-01-matv-groupkeys-acl-design.md`

## Global Constraints

- `mat-device/src/core/` は tokio/socket/file 禁止（CI が `cargo check -p mat-device --no-default-features` で検査）。
- 各タスク完了時に `task check`（fmt:check + clippy `-D warnings` + test）緑でコミット。コミットメッセージは日本語・既存流儀（`feat(mat-device): ...` 等）。
- stdout 純 JSON / stderr tracing などの repo ルールは CLAUDE.md 参照（今回のコードは device 側なので通常触れない）。
- テストで使う定数・エンコーダは `mat_controller::im` の既存物を使う: `CLUSTER_GROUP_KEY_MANAGEMENT=0x003F`, `CMD_KEY_SET_WRITE=0x00`, `ATTR_GROUP_KEY_MAP=0x0000`, `ATTR_GROUP_TABLE=0x0001`, `encode_key_set_write_fields(keyset_id, epoch_key)` (im.rs:1353), `encode_group_key_map_tlv(&[(group_id, keyset_id)])` (im.rs:1375), `STATUS_CONSTRAINT_ERROR=0x87`, `STATUS_UNSUPPORTED_ACCESS=0x7E`, `STATUS_RESOURCE_EXHAUSTED=0x89`, `STATUS_UNSUPPORTED_COMMAND=0x81`。
- privilege 数値: View=1, ProxyView=2, Operate=3, Manage=4, Administer=5。auth_mode: PASE=1, CASE=2, Group=3。
- 実装前に必ず対象ファイルの現状を読むこと（このプランの行番号は base 5528861 時点）。

---

### Task 1: GroupKeyStore + KeySetWrite（core）

**Files:**
- Modify: `crates/mat-device/src/core/group_key_management.rs`（現状 143 行、モジュール doc も更新）

**Interfaces:**
- Produces: `GroupKeyStore`（`Clone`）: `new()`, `upsert_keyset(fabric_index: u8, keyset_id: u16, epoch_key0: [u8;16]) -> Result<(), u8>`, `keyset_exists(fabric_index: u8, keyset_id: u16) -> bool`, `purge_fabric(fabric_index: u8)`, `replace_fabric_map(fabric_index: u8, entries: Vec<(u16, u16)>)`, `append_map_entry(fabric_index: u8, group_id: u16, keyset_id: u16)`, `map_entries_for(fabric_index: u8) -> Vec<(u16, u16)>`, `all_map_entries() -> Vec<(u8, u16, u16)>`
- Produces: `GroupKeyManagementHandler::new(store: GroupKeyStore)`（**シグネチャ変更**: 現状は引数なし。呼び出し箇所は `crates/mat-device/src/device.rs:321` と本ファイル内テストのみ — device.rs 側の配線は Task 2 で行うが、コンパイルを通すため本タスクで `GroupKeyManagementHandler::new(GroupKeyStore::new())` 形式に仮更新してよい）

**設計メモ:**
- `GroupKeyStore(Arc<Mutex<GroupKeyInner>>)`、`GroupKeyInner { keysets: Vec<GroupKeySet>, map: Vec<GroupKeyMapEntry> }`。`AclStore`（`core/access_control.rs:71-100`）の形を踏襲。永続化なし（M3 送り、doc 明記）。lock は `access_control.rs` の `lock()` ヘルパと同じ poison 耐性パターンを踏襲する（`unwrap_or_else(PoisonError::into_inner)`）。
- `upsert_keyset`: 同 (fabric, keyset_id) があれば置換。無ければ fabric 内 keyset 数が `MAX_GROUP_KEYS_PER_FABRIC`(=1) 以上なら `Err(STATUS_RESOURCE_EXHAUSTED)`、未満なら push。
- KeySetWrite の CommandFields（クライアント実装 `im.rs:1353-1373` が正）: `struct{ Context(0): GroupKeySetStruct{ 0:GroupKeySetID(u16), 1:policy(u8, TrustFirst=0), 2:EpochKey0(16B octstr), 3:EpochStartTime0(u64), 4..7: null } }`。デコードは `Reader` で StructStart→Context(0) StructStart→フィールド走査。ID/policy/EpochKey0 必須、policy≠0 や EpochKey0 長≠16 は `STATUS_CONSTRAINT_ERROR`、TLV 破損は `STATUS_INVALID_COMMAND`（既存定数を grep、無ければ CONSTRAINT_ERROR に倒す — access_control の write の malformed 裁定に合わせる）。
- PASE ガード: `ctx.fabric_index == 0` → `STATUS_UNSUPPORTED_ACCESS`（`access_control.rs:240-242` と同じ）。
- `invoke` は `CMD_KEY_SET_WRITE` のみ受理、成功は `InvokeReply::Status(im::STATUS_SUCCESS)`（response command なし）。他コマンドは従来どおり `STATUS_UNSUPPORTED_COMMAND`。`accepted_commands()` → `vec![im::CMD_KEY_SET_WRITE]`。
- 既存テスト `declares_attributes_with_no_commands` は `accepted_commands` の期待を更新。

- [ ] **Step 1: 失敗するテストを書く** — `#[cfg(test)] mod tests` に追加（既存テストの流儀に合わせる）:

```rust
#[test]
fn key_set_write_stores_keyset_and_enforces_capacity() {
    let store = GroupKeyStore::new();
    let mut h = GroupKeyManagementHandler::new(store.clone());
    let fields = mat_controller::im::encode_key_set_write_fields(0x01AA, &[0x11; 16]);
    let mut ctx = InvokeCtx { fabric_index: 1, ..Default::default() };
    assert_eq!(
        h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut ctx),
        InvokeReply::Status(im::STATUS_SUCCESS)
    );
    assert!(store.keyset_exists(1, 0x01AA));
    // 同一 id への再 write は upsert（容量エラーにならない）
    assert_eq!(
        h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut ctx),
        InvokeReply::Status(im::STATUS_SUCCESS)
    );
    // 別 id は容量 1 超過
    let fields2 = mat_controller::im::encode_key_set_write_fields(0x01AB, &[0x22; 16]);
    assert_eq!(
        h.invoke(im::CMD_KEY_SET_WRITE, &fields2, &mut ctx),
        InvokeReply::Status(im::STATUS_RESOURCE_EXHAUSTED)
    );
    // 別 fabric は独立
    let mut ctx2 = InvokeCtx { fabric_index: 2, ..Default::default() };
    assert_eq!(
        h.invoke(im::CMD_KEY_SET_WRITE, &fields2, &mut ctx2),
        InvokeReply::Status(im::STATUS_SUCCESS)
    );
}

#[test]
fn key_set_write_rejects_pase_and_malformed() {
    let mut h = GroupKeyManagementHandler::new(GroupKeyStore::new());
    let fields = mat_controller::im::encode_key_set_write_fields(1, &[0u8; 16]);
    let mut pase = InvokeCtx::default(); // fabric_index 0 = PASE
    assert_eq!(
        h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut pase),
        InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS)
    );
    let mut ctx = InvokeCtx { fabric_index: 1, ..Default::default() };
    assert!(matches!(
        h.invoke(im::CMD_KEY_SET_WRITE, &[0xFF, 0x00], &mut ctx),
        InvokeReply::Status(_)
    ));
}

#[test]
fn purge_fabric_drops_that_fabrics_keysets_only() {
    let store = GroupKeyStore::new();
    store.upsert_keyset(1, 10, [0u8; 16]).unwrap();
    store.upsert_keyset(2, 20, [0u8; 16]).unwrap();
    store.purge_fabric(1);
    assert!(!store.keyset_exists(1, 10));
    assert!(store.keyset_exists(2, 20));
}
```

- [ ] **Step 2: 失敗を確認** — Run: `cargo test -p mat-device key_set_write` → コンパイルエラー（`GroupKeyStore` 未定義）を確認
- [ ] **Step 3: 実装** — 上記設計メモどおり `GroupKeyStore` + `invoke` の KeySetWrite 分岐を実装。既存テストの期待値（`accepted_commands`・`unknown_attribute_and_every_command_are_rejected` の cmd 0x00 期待）も現実に合わせて更新。`device.rs:321` は `GroupKeyManagementHandler::new(GroupKeyStore::new())` で仮通し。
- [ ] **Step 4: 通ることを確認** — Run: `cargo test -p mat-device group_key` → PASS、`cargo check -p mat-device --no-default-features` → OK
- [ ] **Step 5: Commit** — `git add -p` 相当で本タスクのファイルのみ。msg: `feat(mat-device): GroupKeyStore + KeySetWrite (spec §11.2.7.1)`

---

### Task 2: group-key-map write/read + purge 配線

**Files:**
- Modify: `crates/mat-device/src/core/group_key_management.rs`
- Modify: `crates/mat-device/src/core/commissioning.rs`（`set_acl_store` の隣に `set_group_key_store` を追加、purge 3箇所: `rollback_uncommitted_fabric` :918 相当、RemoveFabric :1242 相当、fail-safe :1283 相当 — `acl_store` の purge と同じ場所に並べる）
- Modify: `crates/mat-device/src/device.rs:319-323`（store を作って handler と comm_server 両方へ）

**Interfaces:**
- Consumes: Task 1 の `GroupKeyStore` 全 API
- Produces: `CommissioningServer::set_group_key_store(store: GroupKeyStore)`、`ATTR_GROUP_KEY_MAP` の write/read が機能する `GroupKeyManagementHandler`

**設計メモ:**
- `write`（`ClusterHandler::write` オーバーライド）: `attribute != ATTR_GROUP_KEY_MAP` → `Err(STATUS_UNSUPPORTED_WRITE)`。PASE（fabric 0）→ `Err(STATUS_UNSUPPORTED_ACCESS)`。データ形（クライアント実装 `im.rs:1375-1385` が正）: 全置換は `array[struct{1:group_id, 2:keyset_id}]`（fabricIndex 254 は無視して accessing fabric を使う）、`list_append=true` は struct 単体。group_id==0 または `!keyset_exists(fabric, keyset_id)` → `Err(STATUS_CONSTRAINT_ERROR)`。成功時 `ctx.changed.push(im::ATTR_GROUP_KEY_MAP)`。パターンは `access_control.rs:230-272` の write を読んで踏襲。
- `read`（`ATTR_GROUP_KEY_MAP`）: `ctx.fabric_filtered` を尊重（true → `map_entries_for(ctx.fabric_index)`、false → 全 fabric）。各要素 `struct{ Context(1)=group_id, Context(2)=keyset_id, Context(254)=fabric_index }`（`access_control.rs` の `write_acl_entry` :304 の fabricIndex 出力流儀を踏襲）。`ATTR_GROUP_TABLE` は空 array のまま + doc 明記。
- モジュール doc（:1-10）を現実（KeySetWrite/map write 実装済み、GroupTable endpoints と永続化と KeySetRead/Remove は groupcast タスク送り）に書き換え。

- [ ] **Step 1: 失敗するテストを書く**:

```rust
#[test]
fn group_key_map_write_replace_append_and_fabric_filtered_read() {
    let store = GroupKeyStore::new();
    let mut h = GroupKeyManagementHandler::new(store.clone());
    let mut ctx = InvokeCtx { fabric_index: 1, ..Default::default() };
    let ks = mat_controller::im::encode_key_set_write_fields(7, &[9u8; 16]);
    h.invoke(im::CMD_KEY_SET_WRITE, &ks, &mut ctx);

    // 全置換 write
    let data = mat_controller::im::encode_group_key_map_tlv(&[(0x000A, 7)]);
    h.write(im::ATTR_GROUP_KEY_MAP, &data, false, &mut ctx).unwrap();
    assert_eq!(ctx.changed, vec![im::ATTR_GROUP_KEY_MAP]);
    assert_eq!(store.map_entries_for(1), vec![(0x000A, 7)]);

    // 存在しない keyset 参照は CONSTRAINT_ERROR
    let bad = mat_controller::im::encode_group_key_map_tlv(&[(0x000B, 99)]);
    assert_eq!(
        h.write(im::ATTR_GROUP_KEY_MAP, &bad, false, &mut ctx),
        Err(im::STATUS_CONSTRAINT_ERROR)
    );

    // fabric_filtered read は自 fabric のみ・fabricIndex(254) 付き
    let tlv = h.read(im::ATTR_GROUP_KEY_MAP, &ReadCtx { fabric_index: 1, fabric_filtered: true }).unwrap();
    let mut r = Reader::new(&tlv);
    assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
    assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
    // 以降 Context(1)=0x000A / Context(2)=7 / Context(254)=1 を順不同で確認
}
```

（`ReadCtx` のフィールドは Task 4 で `subject` が足されるため、このテストでは struct literal でなく `ReadCtx::unfiltered(1)` / `ReadCtx::default()` ベースの構築が安全 — 実装時に判断。）

- [ ] **Step 2: 失敗を確認** — Run: `cargo test -p mat-device group_key_map_write` → FAIL（write が UNSUPPORTED_WRITE）
- [ ] **Step 3: 実装** — write/read + `set_group_key_store` + purge 3箇所 + `device.rs` 配線（store を 1 個作り `comm_server.set_group_key_store(gk_store.clone())` と `GroupKeyManagementHandler::new(gk_store)` に渡す）。purge のユニットテストは `commissioning.rs` の既存 `rollback`/`remove_fabric` テスト（:1823 付近）に GroupKeyStore の assert を足す形で最小限。
- [ ] **Step 4: 通ることを確認** — Run: `cargo test -p mat-device` → PASS、`cargo check -p mat-device --no-default-features` → OK
- [ ] **Step 5: Commit** — msg: `feat(mat-device): group-key-map write/read + fabric purge 配線（group provision 受理）`

---

### Task 3: ACL 判定 core（decode_targets / privilege 束 / AclStore::check）

**Files:**
- Modify: `crates/mat-device/src/core/access_control.rs`

**Interfaces:**
- Produces: `pub(crate) fn privilege_grants(entry_privilege: u8, required: u8) -> bool`、`pub(crate) struct AclTargetDev { pub cluster: Option<u32>, pub endpoint: Option<u16>, pub device_type: Option<u32> }`、`pub(crate) fn decode_targets(raw: &[u8]) -> Option<Vec<AclTargetDev>>`、`AclStore::check(&self, fabric_index: u8, subject: u64, required_privilege: u8, endpoint: u16, cluster: u32) -> bool`
- 定数: `pub(crate) const PRIVILEGE_VIEW: u8 = 1; PRIVILEGE_PROXY_VIEW: u8 = 2; PRIVILEGE_OPERATE: u8 = 3; PRIVILEGE_MANAGE: u8 = 4; PRIVILEGE_ADMINISTER: u8 = 5;`（`CASE_ADMIN_PRIVILEGE`/`CASE_ADMIN_AUTH_MODE` は新定数で置き換えるか併存させるかは実装判断）

**設計メモ:**
- `privilege_grants`: Administer→全部 true。Manage→{Manage,Operate,View}。Operate→{Operate,View}。ProxyView→{ProxyView,View}。View→{View}。
- `decode_targets`: `targets_raw` は「`Tag::Anonymous` に再タグされた array の raw TLV」（`AclDeviceEntry` doc :43-48）。`Reader` で ArrayStart→各 StructStart→`Context(0)`=cluster/`Context(1)`=endpoint/`Context(2)`=deviceType（各 null 可）→ContainerEnd。パース不能は `None`（呼び出し側で「どこにもマッチしない」扱い=安全側）。
- `AclStore::check`: `fabric_index == 0` は呼ばない前提（enforcement 側で PASE bypass）だが、防御的に `false`。エントリ走査: `e.fabric_index == fabric_index && e.auth_mode == 2(CASE) && (e.subjects.is_empty() || e.subjects.contains(&subject)) && privilege_grants(e.privilege, required)` かつ target マッチ（`targets_raw: None` → 無制限 / `Some` → decode して、いずれかの target が `cluster.map_or(true, |c| c == cluster) && endpoint.map_or(true, |ep| ep == endpoint) && device_type.is_none()` — device_type 制約付きは不一致=安全側）。
- CAT（0xFFFF_FFFD_xxxx_xxxx）は未対応・完全一致のみ、doc に明記。（**2026-09-02 に superseded**: CAT subject マッチ実装済み、`Subject::matches` / `tests/acl_cat_subject.rs`）
- モジュール doc :14-18 の「ACL enforcement は未実装のため即時の実害はなく」を書き換え（save 失敗黙認の理由付けが enforcement 実装後も成立するよう文面を調整: ACL が直前状態のまま残っても「古い正当な状態」であり安全性は落ちない）。

- [ ] **Step 1: 失敗するテストを書く**:

```rust
#[test]
fn check_matches_subject_privilege_and_targets() {
    let store = AclStore::new();
    store.add_case_admin(1, 112233); // Administer / CASE / 制限なし
    assert!(store.check(1, 112233, PRIVILEGE_ADMINISTER, 0, 0x001F));
    assert!(store.check(1, 112233, PRIVILEGE_VIEW, 2, 0x0006));
    assert!(!store.check(1, 999, PRIVILEGE_VIEW, 2, 0x0006));   // subject 不一致
    assert!(!store.check(2, 112233, PRIVILEGE_VIEW, 2, 0x0006)); // fabric 不一致
    assert!(!store.check(0, 112233, PRIVILEGE_VIEW, 2, 0x0006)); // fabric 0 は常に false
}

#[test]
fn check_respects_privilege_lattice() {
    let store = AclStore::new();
    store.replace_fabric(1, vec![AclDeviceEntry {
        privilege: PRIVILEGE_OPERATE, auth_mode: 2,
        subjects: vec![5], targets_raw: None, fabric_index: 1,
    }]);
    assert!(store.check(1, 5, PRIVILEGE_VIEW, 2, 0x0006));
    assert!(store.check(1, 5, PRIVILEGE_OPERATE, 2, 0x0006));
    assert!(!store.check(1, 5, PRIVILEGE_MANAGE, 2, 0x0006));
    assert!(!store.check(1, 5, PRIVILEGE_ADMINISTER, 0, 0x001F));
}

#[test]
fn check_with_cluster_target_limits_scope() {
    // targets_raw は write と同形の raw TLV を Writer で組む:
    // array[ struct{ Context(0)=0x0006 } ]（cluster=OnOff のみ許可）
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), 0x0006);
    w.end_container();
    w.end_container();
    let store = AclStore::new();
    store.replace_fabric(1, vec![AclDeviceEntry {
        privilege: PRIVILEGE_OPERATE, auth_mode: 2,
        subjects: vec![5], targets_raw: Some(w.finish()), fabric_index: 1,
    }]);
    assert!(store.check(1, 5, PRIVILEGE_OPERATE, 2, 0x0006));
    assert!(!store.check(1, 5, PRIVILEGE_OPERATE, 2, 0x0008)); // 他 cluster は拒否
}

#[test]
fn empty_subjects_entry_is_a_wildcard() {
    let store = AclStore::new();
    store.replace_fabric(1, vec![AclDeviceEntry {
        privilege: PRIVILEGE_VIEW, auth_mode: 2,
        subjects: vec![], targets_raw: None, fabric_index: 1,
    }]);
    assert!(store.check(1, 424242, PRIVILEGE_VIEW, 2, 0x0006));
}
```

（`targets_raw` の正確な再タグ形式は `AclDeviceEntry` の doc と `write` の実装 :230-272 を読み、既存の round-trip テストが作る raw と同形にすること。`replace_fabric` の既存シグネチャは実装を読んで合わせる。）

- [ ] **Step 2: 失敗を確認** — Run: `cargo test -p mat-device check_` → コンパイルエラー（`check` 未定義）
- [ ] **Step 3: 実装** — 設計メモどおり。
- [ ] **Step 4: 通ることを確認** — Run: `cargo test -p mat-device access_control` → PASS
- [ ] **Step 5: Commit** — msg: `feat(mat-device): AclStore::check — subject/privilege束/target の ACL 判定 (spec §9.10)`

---

### Task 4: 必要 privilege trait メソッド + Node ディスパッチ enforcement

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`ReadCtx`/`InvokeCtx` に `subject: u64` 追加、`ClusterHandler` に privilege 3 メソッド追加、`Node` に `acl: Option<AclStore>` + `set_acl_store`、enforcement を `read_entries` :653 / `handle_write` :937 / `handle_invoke` :840 に挿入）
- Modify: privilege オーバーライド先: `core/access_control.rs`（read/write ACL=Administer）、`core/group_key_management.rs`（KeySetWrite=Administer、map write=Manage、read=View のまま）、`core/commissioning.rs`（GeneralCommissioning / OperationalCredentials / AdministratorCommissioning の invoke=Administer）、`core/identify.rs`（invoke=Manage）、`core/datamodel.rs` 内 `BasicInformationHandler`（NodeLabel/Location write=Manage）
- Modify: `crates/mat-device/src/device.rs`（`node.set_acl_store(acl_store.clone())` を `add_cluster` 群の前に追加）
- Modify: `ReadCtx`/`InvokeCtx` を構築している全箇所（`grep -rn "ReadCtx {" "InvokeCtx {" "ReadCtx::" crates/mat-device/` で全数確認。`net/runtime.rs` の構築箇所は fabric_index しか無いので `subject: 0` で仮埋め — 本配線は Task 5）

**Interfaces:**
- Consumes: Task 3 の `AclStore::check` / `PRIVILEGE_*` 定数 / `privilege_grants`
- Produces:
  - `ReadCtx { fabric_index: u8, fabric_filtered: bool, subject: u64 }`（`Default` は subject 0。`ReadCtx::unfiltered(fabric_index)` も subject 0）
  - `InvokeCtx { attestation_challenge, changed, fabric_index, subject: u64 }`
  - `ClusterHandler` に default メソッド 3 つ:
    ```rust
    fn read_privilege(&self, _attribute: u32) -> u8 { access_control::PRIVILEGE_VIEW }
    fn write_privilege(&self, _attribute: u32) -> u8 { access_control::PRIVILEGE_OPERATE }
    fn invoke_privilege(&self, _command: u32) -> u8 { access_control::PRIVILEGE_OPERATE }
    ```
  - `Node::set_acl_store(&mut self, store: AclStore)`

**設計メモ:**
- **enforcement の共通判定**（`Node` のプライベートヘルパ）:
  ```rust
  /// fabric_index==0 (PASE) は implicit Administer (spec §9.10.5)。
  /// acl 未設定 (テスト用 Node) は enforcement 無効 = 全許可。
  fn allowed(&self, ctx_fabric: u8, subject: u64, required: u8, endpoint: u16, cluster: u32) -> bool {
      if ctx_fabric == 0 { return true; }
      match &self.acl {
          Some(store) => store.check(ctx_fabric, subject, required, endpoint, cluster),
          None => true,
      }
  }
  ```
  `acl: None` = 全許可のフォールバックが既存ユニットテスト（`Node` を素で組む多数のテスト）を壊さないための要。`device.rs` の本組み立てだけが `set_acl_store` する。
- `read_entries`: パス展開後・`ClusterHandler::read` 呼び出し前に判定。required は `handler.read_privilege(attr)`（global 属性 0xFFF8-0xFFFD は View 固定）。**wildcard 由来のパスは不許可を黙って落とす**（UNSUPPORTED_ATTRIBUTE の wildcard 扱い :653 付近の既存分岐と同じ流儀）、**具体パスは `STATUS_UNSUPPORTED_ACCESS` の status entry**（既存の UNSUPPORTED_ATTRIBUTE status entry 生成と同じ型 `ReportEntryOut` を使う）。
- `handle_write`: 各 AttributeDataIB 処理の頭で `handler.write_privilege(attr)` 判定、不許可は per-entry `AttributeStatusIB` に `STATUS_UNSUPPORTED_ACCESS`（既存のエラー status 生成と同じ経路）。
- `handle_invoke`: `handler.invoke_privilege(cmd)` 判定、不許可は `InvokeReply::Status(STATUS_UNSUPPORTED_ACCESS)` 相当の status 応答（既存 UNSUPPORTED_COMMAND 応答と同じ組み立て）。
- オーバーライド一覧（Matter 1.4 spec のアクセス表と実装時に照合）:
  - `AccessControlHandler`: `read_privilege(ATTR_ACL)=Administer`, `write_privilege(ATTR_ACL)=Administer`（容量系属性 read は View のまま）
  - `GroupKeyManagementHandler`: `invoke_privilege(CMD_KEY_SET_WRITE)=Administer`, `write_privilege(ATTR_GROUP_KEY_MAP)=Manage`
  - commissioning 3 ハンドラ（GeneralCommissioning/OperationalCredentials/AdministratorCommissioning）: `invoke_privilege(_)=Administer`
  - `IdentifyHandler`: `invoke_privilege(_)=Manage`
  - `BasicInformationHandler`: `write_privilege(_)=Manage`
  - OnOff / Groups / Descriptor / NetworkCommissioning / GeneralDiagnostics: default のまま（NetworkCommissioning は spec 上 Administer だが invoke 実装が無い/読み取り主体なら default で可 — 実装を読んで invoke があるなら Administer に）
- PASE ガードの既存重複（`access_control.rs:240-242`、Task 1 の KeySetWrite ガード）はそのまま残す（core 単体テストが Node を介さないため防御が二重でも害なし）。

- [ ] **Step 1: 失敗するテストを書く** — `datamodel.rs` のテスト群に追加。既存のテスト用 Node 組み立てヘルパ（`with_root_endpoint` 系のテストを grep して流儀を確認）を使う:

```rust
#[test]
fn acl_denies_invoke_without_operate_and_read_without_view() {
    let mut node = /* 既存テストと同じ最小 Node + EP1 に OnOffHandler */;
    let acl = AclStore::new();
    // subject 7 に OnOff cluster への Operate だけを許可
    acl.replace_fabric(1, vec![AclDeviceEntry {
        privilege: PRIVILEGE_OPERATE, auth_mode: 2,
        subjects: vec![7], targets_raw: None, fabric_index: 1,
    }]);
    node.set_acl_store(acl);

    // 許可 subject: toggle が通る
    // 不許可 subject 8: handle_invoke が UNSUPPORTED_ACCESS ステータスを返す
    // 不許可 subject 8: 具体パス read が UNSUPPORTED_ACCESS の status entry
    // 不許可 subject 8: wildcard read は落ちるだけでエラーにならない
    // fabric_index 0 (PASE): ACL に関係なく通る
}
```

（invoke/read の組み立ては同ファイル既存テストの `handle_im` 呼び出しをコピーして流用。期待ペイロードのデコードも既存テストの decode ヘルパを使う。）

- [ ] **Step 2: 失敗を確認** — Run: `cargo test -p mat-device acl_denies` → コンパイルエラー（`set_acl_store` 未定義）
- [ ] **Step 3: 実装** — 設計メモどおり。`ReadCtx`/`InvokeCtx` の全構築箇所を grep して `subject` を追加（runtime は仮 0）。
- [ ] **Step 4: 通ることを確認** — Run: `cargo test -p mat-device && cargo check -p mat-device --no-default-features` → PASS。**既存テストが大量にある** — 落ちたものは「enforcement で正しく拒否されるようになった」のか「テスト Node に ACL が無いのに拒否された」のかを一件ずつ切り分けること（後者はバグ）。
- [ ] **Step 5: Commit** — msg: `feat(mat-device): ACL enforcement — read/write/invoke を AclStore::check でゲート`

---

### Task 5: runtime subject 配線 + 統合テスト

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`ReadCtx`/`InvokeCtx` 構築箇所 — `serve_secured_message` :1233-1244 付近、`serve_read_request_chunked` :1844、`serve_subscribe_request` :1472、`send_subscription_report` :1600 — に `session.peer_node_id()` を配線）
- Create: `crates/mat-device/tests/group_provision.rs`
- Modify: `crates/mat-device/tests/onoff_invoke.rs`（または新規 `acl_enforce.rs`）— ACL 拒否の E2E

**Interfaces:**
- Consumes: `SecureSession::peer_node_id() -> u64`（`mat-controller/src/session.rs:221`）、Task 4 の `subject` フィールド、`support::commission_directly`（`tests/support/mod.rs:105-293`）、`im::encode_key_set_write_fields` / `im::encode_group_key_map_tlv`
- Produces: なし（最終配線）

**設計メモ:**
- PASE セッション（`runtime.rs:830` の fabric 0 セッション）では `peer_node_id()` の値に意味が無い可能性があるが、enforcement は fabric 0 で bypass するので subject は素通しで害なし。
- `tests/group_provision.rs`: `commission_directly` で CASE セッションを取り、`mat group provision` 相当の 4 ステップを生 IM で実行:
  1. invoke(EP0, 0x003F, KeySetWrite, `encode_key_set_write_fields(0x01AA, &[0x5A;16])`) → SUCCESS
  2. write(EP0, 0x003F, ATTR_GROUP_KEY_MAP, `encode_group_key_map_tlv(&[(0x000A, 0x01AA)])`) → SUCCESS
  3. invoke(BRIDGED_EP, 0x0004, AddGroup(0x00), group 0x000A) → AddGroupResponse status 0
  4. read(EP0, 0x003F, ATTR_GROUP_KEY_MAP) → 書いた 1 エントリが fabricIndex 付きで返る
  invoke/write/read のワイヤ組み立ては `tests/onoff_invoke.rs` と `subscribe_loop.rs` の既存ヘルパ/流儀をコピーする（`SecureSession` のクライアント側 API `invoke_command`/`write_attribute`/`read_attribute` 相当が session.rs にあるはずなので grep して使う）。
- ACL 拒否テスト: commission 後、admin subject で ACL を `[admin(Administer), {privilege: View, subjects:[admin], ...}]` に**置換せず**、別案 — admin エントリを消すと後始末不能になるため、**admin エントリを残したまま「存在しない subject での判定」は unit で済んでいる**。E2E では逆向きを検証: ACL write で `{privilege: Operate, subjects:[admin]}` のみに置換 → 以後 ACL read（Administer 要求）が UNSUPPORTED_ACCESS で拒否され、OnOff toggle（Operate 要求）は通る。これで「enforcement が実セッションの subject で効いている」ことが閉ループで示せる。
- Subscribe 経路の enforcement 回帰: `subscribe_loop.rs` が admin subject で従来どおり通ること（priming が空にならないこと）は既存テストがそのまま担保する。

- [ ] **Step 1: 失敗するテストを書く** — `tests/group_provision.rs`（上記 4 ステップ）。ACL 拒否テストも書く。
- [ ] **Step 2: 失敗を確認** — Run: `cargo test -p mat-device --test group_provision` → FAIL（subject 未配線なら CASE セッションからの KeySetWrite が Administer 判定で落ちる、等）
- [ ] **Step 3: 実装** — runtime の `ReadCtx`/`InvokeCtx` 構築に `subject: session.peer_node_id()` を配線。
- [ ] **Step 4: 通ることを確認** — Run: `cargo test -p mat-device` → 全 PASS（`subscribe_loop` 含む）
- [ ] **Step 5: Commit** — msg: `feat(mat-device): CASE peer node id を IM ctx に配線、group provision / ACL 拒否の統合テスト`

---

### Task 6: e2e-device-m3.sh + Taskfile + 陳腐化 docs 修正

**Files:**
- Create: `scripts/e2e-device-m3.sh`（`e2e-device-m1.sh` :1-60 の流儀・`json_get` ヘルパ・cleanup trap をコピーして拡張）
- Modify: `Taskfile.yml`（`e2e:device:m2-chip` :77-80 の隣に `e2e:device:m3` を追加）
- Modify: `crates/mat-device/src/core/datamodel.rs` モジュール doc :12-14（"still no subscriptions" → Subscribe 実装済みの現実へ）
- Modify: `crates/mat-controller/src/im.rs:205-207`（STATUS_INVALID_ACTION doc の「device 側は Subscribe を落とす」旨の陳腐化記述を更新）
- Modify: `README.md` / `docs/` — matv の対応状況記述があれば grep（`rg -l "KeySetWrite|INVALID_ACTION" docs/ README.md ARCHITECTURE.md`）して現実に合わせる

**Interfaces:**
- Consumes: Task 1-5 のすべて（ビルド済みバイナリ経由）

**スクリプト骨子**（m1 の流儀で。厳密なフラグ名は `mat group provision --help` / `matd --help` / docs/commands.md を実行時に確認して合わせること）:

```bash
# 1. cargo build --release (workspace)
# 2. matv.toml 生成（m1 と同じ、store/port=0/iface=$IFACE）→ matv 起動、stdout JSON から qr/port
# 3. mat fabric init (throwaway MAT_STORE) → mat commission --setup-code <qr> --node 1
# 4. mat group provision --node 1 --group 10 ... → stdout JSON の status/exit 0 を assert
#    （フラグ形は docs/commands.md の group provision 節に従う）
# 5. matd 起動（同じ MAT_STORE、MAT_IFACE=$IFACE、ソケットは matd 既定 or 環境変数 —
#    docs/matd.md / matd --help で確認）→ matd status で node 1 の購読が
#    established になるまでポーリング（タイムアウト 30s）
# 6. mat listen --count 1 --timeout-ms 30000 をバックグラウンド起動（stdout をファイルへ）
# 7. mat onoff toggle 1 1（matd 経由になることを matd ログで確認可能ならなお良い）
# 8. listen の stdout JSON に onoff の変更イベントが 1 件あることを assert
# 9. cleanup: matd → matv の順に kill、workdir 削除
```

- [ ] **Step 1: スクリプトと Taskfile target を書く**（上記骨子。`set -euo pipefail`、m1 と同じ env `MAT_E2E_IFACE`/`MAT_E2E_TIMEOUT_S`）
- [ ] **Step 2: 実行して通す** — Run: `task e2e:device:m3`（このホストで実行。iface は m1 と同じ既定が通るはず — 通らなければ `MAT_E2E_IFACE` を調整し、必要な修正を加えて再実行）
- [ ] **Step 3: 陳腐化 docs を修正** — 上記 4 箇所 + grep 結果。
- [ ] **Step 4: 最終確認** — Run: `task check` → 緑。`task e2e:device:m1` も回して既存 E2E の無退行を確認。
- [ ] **Step 5: Commit** — msg: `test(e2e): matd 常駐 Subscribe + mat listen の matv 回帰 E2E (e2e:device:m3) + 陳腐化 doc 修正`

---

## Self-Review 済みメモ

- spec の全要件との対応: §1→Task 1,2 / §2→Task 3,4,5 / §3→Task 5,6 / §4→Task 6 + 完了後のメモリ更新（プラン外、メインセッションが実施）。
- 型整合: `GroupKeyStore` API は Task 1 定義を Task 2,5 が消費。`subject: u64`・`PRIVILEGE_*` は Task 3,4,5 で同名。
- 既知の不確定点（実装時にファイルを読んで解決する指示済み): `replace_fabric` の既存シグネチャ、SecureSession クライアント側 invoke/write ヘルパ名、`mat group provision`/`matd` の CLI フラグ、STATUS_INVALID_COMMAND 定数の有無。
