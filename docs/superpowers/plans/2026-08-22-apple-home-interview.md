# Apple Home interview 通過 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apple Home のペアリングが commissioning 後の interview（ACL 書き込み + root 適合性検査）を通過し、ホームアプリから matv の OnOff を操作できるようにする。

**Architecture:** device 側 IM に WriteRequest 経路を新設し、EP0 に AccessControl（AddNOC 自動 admin エントリ + fabric 全置換 write）、NetworkCommissioning(Ethernet)/GeneralDiagnostics/GroupKeyManagement の最小実装を足し、BasicInformation を必須属性まで充足する。ACL enforcement・永続化は M3 送り。

**Tech Stack:** Rust workspace（crates/mat-controller = IM wire codec, crates/mat-device = デバイス側スタック）。TLV は `mat_controller::tlv::{Reader, Writer}`。

**Spec:** `docs/superpowers/specs/2026-08-22-apple-home-interview-design.md`

## Global Constraints

- テストは TDD（RED を必ず目視してから実装）。各タスク末尾で `cargo test -p <crate>` green + commit
- `cargo fmt` はこの WSL の rustfmt が既存 `crates/mat-device/src/net/runtime.rs` に無関係な整形差分を作る（rustfmt バージョン揺れ）。fmt 後に `git status` を見て、**自分が編集していないファイルに差分が出たら `git checkout -- <file>` で破棄**する
- E2E ゲートはこのマシンでは `MAT_E2E_IFACE=eth0` が必須（既定 eth1 は DOWN）
- コミットメッセージは既存スタイル（`feat(mat-device): ...` 等、日本語 OK）+ Claude trailer
- 新しい IM 定数は `crates/mat-controller/src/im.rs` の既存ブロックの流儀（`pub const`、必要ならスペック節の doc コメント）で追加する

---

### Task 1: IM wire — device 側 WriteRequest decode / WriteResponse encode

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（`encode_write_request_inner` 周辺、~L1930-2030 に隣接させる）
- Test: 同ファイル内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既存 `encode_write_request_tlv(endpoint, cluster, attribute, data_tlv)`（wire 形の正）、既存の AttributePathIB decode 部品（`decode_read_request` が使う path parser を探して再利用する。見つからなければ同型を書く）
- Produces:
  ```rust
  pub struct WriteAttrIn {
      pub endpoint: Option<u16>,
      pub cluster: Option<u32>,
      pub attribute: Option<u32>,
      /// Data 要素を Anonymous タグに正規化した完全な TLV 要素 1 個
      pub data_tlv: Vec<u8>,
  }
  pub struct WriteRequestIn {
      pub timed: bool,
      pub suppress_response: bool,
      pub writes: Vec<WriteAttrIn>,
  }
  pub fn decode_write_request(payload: &[u8]) -> Result<WriteRequestIn, ImError>;
  /// results: (endpoint, cluster, attribute, status)
  pub fn encode_write_response(results: &[(u16, u32, u32, u8)]) -> Vec<u8>;
  ```

- [ ] **Step 1: RED — roundtrip テストを書く**

`encode_write_request_tlv` で作った wire を `decode_write_request` で読み戻す。Data の再タグ付け（Context(2) → Anonymous）も検証する:

```rust
#[test]
fn write_request_roundtrips_with_device_side_decoder() {
    let mut w = Writer::new();
    w.put_uint(Tag::Anonymous, 42);
    let data = w.finish();
    let payload = encode_write_request_tlv(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, &data);
    let req = decode_write_request(&payload).unwrap();
    assert!(!req.timed);
    assert_eq!(req.writes.len(), 1);
    let wr = &req.writes[0];
    assert_eq!(
        (wr.endpoint, wr.cluster, wr.attribute),
        (Some(0), Some(CLUSTER_ACCESS_CONTROL), Some(ATTR_ACL))
    );
    let mut r = Reader::new(&wr.data_tlv);
    let el = r.next().unwrap().unwrap();
    assert_eq!(el.tag, Tag::Anonymous);
    assert_eq!(el.value, Value::Uint(42));
}

#[test]
fn write_response_encodes_attribute_status_ibs() {
    let payload = encode_write_response(&[(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, 0x00)]);
    // 自前 decoder（decode_attribute_status_ib 経由の decode_write_response）で読み戻す
    assert_eq!(decode_write_response(&payload).unwrap(), 0x00);
}
```

- [ ] **Step 2: RED を確認** — `cargo test -p mat-controller write_request_roundtrips` がコンパイルエラーでなく「未定義」で fail するよう、先に空 stub（`todo!()` ではなく `Err(ImError::Malformed("unimplemented"))` / 空 Vec を返す実装）を置いてから走らせる
- [ ] **Step 3: GREEN — 実装** — `encode_write_request_inner`（wire の正）を読み、WriteRequestMessage `{0: SuppressResponse?, 1: TimedRequest, 2: WriteRequests[AttributeDataIB{1: Path(list), 2: Data}], ...}` を逆に辿る。Data 要素は `Writer::put_raw_element(Tag::Anonymous, <element bytes>)` で再タグ化。WriteResponse は `{1: [AttributeStatusIB{0: Path(list), 1: StatusIB{0: status}}]}` — 既存 `decode_write_response` が読める形が正（同関数のテストで裏取りする）
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-controller` 全 green
- [ ] **Step 5: Commit** — `feat(im): device 側 WriteRequest decode / WriteResponse encode`

### Task 2: datamodel — ClusterHandler::write + feature_map + Node::handle_write

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（trait 定義 ~L133、`read_attribute_value` ~L640、`handle_im` の opcode match）
- Modify: `crates/mat-controller/src/im.rs`（定数 `pub const STATUS_UNSUPPORTED_WRITE: u8 = 0x88;` を status ブロックへ）
- Test: `datamodel.rs` の tests mod

**Interfaces:**
- Consumes: Task 1 の `decode_write_request` / `encode_write_response`
- Produces:
  ```rust
  // ClusterHandler に追加（デフォルト実装付き）:
  /// 1 属性の write。Ok(()) = 受理（変更した属性 id は ctx.changed に push
  /// する）。Err(status) = AttributeStatusIB に載せる IM status。
  fn write(&mut self, _attribute: u32, _data_tlv: &[u8], _ctx: &mut InvokeCtx) -> Result<(), u8> {
      Err(im::STATUS_UNSUPPORTED_WRITE)
  }
  /// FeatureMap (0xFFFC) の値。NetworkCommissioning(Ethernet) だけが非 0。
  fn feature_map(&self) -> u32 { 0 }
  ```
  `Node::handle_im` が `OPCODE_WRITE_REQUEST` を `handle_write` に dispatch し、`ImOutcome { opcode: OPCODE_WRITE_RESPONSE, payload, changed }` を返す（changed は既存の購読レポート機構がそのまま拾う）

- [ ] **Step 1: RED — テストを書く**（既存 helpers `node_with_onoff` / `handle_im_ok` を使う）

```rust
/// Write 未対応クラスタへの write は AttributeStatusIB(UNSUPPORTED_WRITE) で
/// 応答する（StatusResponse で会話全体を落とさない）。
#[test]
fn write_to_read_only_attribute_reports_unsupported_write() {
    let mut node = node_with_onoff();
    let mut data = Writer::new();
    data.put_bool(Tag::Anonymous, true);
    let payload = im::encode_write_request_tlv(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &data.finish());
    let (op, resp) = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &payload);
    assert_eq!(op, im::OPCODE_WRITE_RESPONSE);
    assert_eq!(im::decode_write_response(&resp).unwrap(), im::STATUS_UNSUPPORTED_WRITE);
}

/// 未知 endpoint / 未知 cluster は per-path の status で応答する。
#[test]
fn write_to_unknown_paths_reports_path_scoped_status() {
    let mut node = node_with_onoff();
    let mut data = Writer::new();
    data.put_uint(Tag::Anonymous, 1);
    let payload = im::encode_write_request_tlv(9, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &data.finish());
    let (_, resp) = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &payload);
    assert_eq!(im::decode_write_response(&resp).unwrap(), im::STATUS_UNSUPPORTED_ENDPOINT);
}

/// FeatureMap はハンドラ申告値になる（Task 4 の NetworkCommissioning=ET 用の座金）。
#[test]
fn feature_map_global_reflects_the_handler() {
    struct FmHandler;
    impl ClusterHandler for FmHandler {
        fn cluster_id(&self) -> u32 { 0x0031 }
        fn attributes(&self) -> Vec<u32> { vec![] }
        fn read(&self, _: u32, _: &ReadCtx) -> Option<Vec<u8>> { None }
        fn invoke(&mut self, _: u32, _: &[u8], _: &mut InvokeCtx) -> InvokeReply {
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        }
        fn feature_map(&self) -> u32 { 0x04 }
    }
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    node.add_cluster(0, Box::new(FmHandler));
    let payload = encode_read_request_path(Some(0), Some(0x0031), Some(im::ATTR_FEATURE_MAP));
    let (_, resp) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &payload);
    let msg = decode_report_data_message(&resp).unwrap();
    assert_eq!(msg.reports[0].data, Some(serde_json::json!(4)));
}
```

- [ ] **Step 2: RED を確認** — `cargo test -p mat-device write_to_` / `feature_map_global`
- [ ] **Step 3: GREEN — 実装**
  - `read_attribute_value` の `im::ATTR_FEATURE_MAP => Some(uint_value(0))` を `Some(uint_value(u64::from(handler.feature_map())))` に
  - `handle_write(&mut self, payload, ctx) -> Result<ImOutcome, ImServerError>`: `decode_write_request` → 各 write ごとに endpoint/cluster/attribute を解決し、`handler.write` の Ok/Err を status に落とし、`encode_write_response` で応答。concrete path 必須（どれか None → その path は `STATUS_INVALID_COMMAND` 扱いでよい）。変更 path は invoke と同じく DataVersion bump + `ImOutcome::changed` に載せる（`handle_invoke` の後段 ~L695 の実装をそのまま踏襲）
  - `handle_im` の match に `im::OPCODE_WRITE_REQUEST => self.handle_write(payload, ctx)` を追加（現在は UnsupportedOpcode に落ちている）
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device` 全 green（既存 173 本 + 新規）
- [ ] **Step 5: Commit** — `feat(mat-device): IM WriteRequest の datamodel dispatch + per-cluster FeatureMap`

### Task 3: AccessControl クラスタ + AddNOC 自動 admin エントリ

**Files:**
- Create: `crates/mat-device/src/core/access_control.rs`（`mod.rs` に `pub mod access_control;` 追加）
- Modify: `crates/mat-device/src/core/commissioning.rs`（AddNOC 成功時・fabric 撤去時のフック）
- Modify: `crates/mat-device/src/device.rs`（EP0 への登録 + 共有ストア配線）
- Test: `access_control.rs` 内 + `commissioning.rs` 内

**Interfaces:**
- Consumes: Task 2 の `ClusterHandler::write`
- Produces:
  ```rust
  /// in-memory ACL ストア（永続化は M3 送り）。
  #[derive(Clone, Default)]
  pub struct AclStore(Arc<Mutex<Vec<AclDeviceEntry>>>);
  impl AclStore {
      pub fn new() -> Self;
      /// AddNOC (spec §11.17.6.8) の自動 admin エントリ。
      pub fn add_case_admin(&self, fabric_index: u8, case_admin_subject: u64);
      /// fabric 撤去時の purge（RemoveFabric / fail-safe rollback 共用）。
      pub fn purge_fabric(&self, fabric_index: u8);
  }
  #[derive(Debug, Clone, PartialEq)]
  pub struct AclDeviceEntry {
      pub privilege: u8,       // 1..=5
      pub auth_mode: u8,       // 1..=3
      pub subjects: Vec<u64>,  // null → 空
      pub targets_raw: Option<Vec<u8>>, // Context(4) の値要素を Anonymous に再タグした raw TLV（passthrough）
      pub fabric_index: u8,
  }
  pub struct AccessControlHandler { /* AclStore を保持 */ }
  impl AccessControlHandler { pub fn new(store: AclStore) -> Self; }
  ```
  - `CommissioningServer` に `pub fn set_acl_store(&mut self, store: AclStore)`（`Inner` に `Option<AclStore>` を持たせる）。`handle_add_noc` の成功パスで `add_case_admin(new_fabric_index, case_admin_subject)`（case_admin_subject は decode 済み、`commissioning.rs:971` 参照）。`handle_remove_fabric` の成功パスと `rollback_uncommitted_fabric` で `purge_fabric`
  - wire 形（read/write 共通、`AccessControlEntryStruct`）: array の各要素 struct に
    `Context(1)=privilege(uint), Context(2)=auth_mode(uint), Context(3)=subjects(array of uint | null), Context(4)=targets(array | null), Context(254)=fabric_index(uint)`。
    正は controller 側 `crates/mat-native/src/ops.rs::encode_acl_entries_tlv`（読んで完全一致させる）
  - 属性: `ATTR_ACL(0)` read/write、`SubjectsPerAccessControlEntry(2)=4`, `TargetsPerAccessControlEntry(3)=3`, `AccessControlEntriesPerFabric(4)=4`（定数は im.rs に `ATTR_ACL_SUBJECTS_PER_ENTRY` 等で追加）
  - read: `ctx.fabric_index` のエントリのみ（0 = PASE は空リスト）
  - write: decode 失敗 → `Err(STATUS_CONSTRAINT_ERROR)`、成功 → 書き込み fabric のエントリを**全置換**（entry の fabric_index フィールドは無視して書き込み fabric で上書き）、`ctx.changed.push(im::ATTR_ACL)`

- [ ] **Step 1: RED — access_control.rs のテストを書く**

```rust
#[test]
fn add_noc_style_admin_entry_reads_back_for_its_fabric_only() {
    let store = AclStore::new();
    store.add_case_admin(1, 112233);
    let h = AccessControlHandler::new(store);
    let tlv = h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 1 }).unwrap();
    // Reader で array→struct を辿り privilege=5, auth_mode=2, subjects=[112233], fabric_index=1 を検証
    // （groups.rs の decode_status_response テストヘルパと同じ流儀で書く）
    let entries = decode_entries_for_test(&tlv);
    assert_eq!(entries, vec![(5u8, 2u8, vec![112233u64], 1u8)]);
    // 他 fabric からは空
    let tlv = h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 2 }).unwrap();
    assert!(decode_entries_for_test(&tlv).is_empty());
}

#[test]
fn acl_write_replaces_own_fabric_entries_and_reports_change() {
    let store = AclStore::new();
    store.add_case_admin(1, 112233);
    store.add_case_admin(2, 445566);
    let mut h = AccessControlHandler::new(store.clone());
    // fabric1 が admin+hub の 2 エントリへ全置換（Apple の実書き込みの形）
    let data = encode_entries_for_test(&[(5, 2, vec![112233]), (5, 2, vec![778899])]);
    let mut ctx = InvokeCtx { fabric_index: 1, ..InvokeCtx::default() };
    assert_eq!(h.write(im::ATTR_ACL, &data, &mut ctx), Ok(()));
    assert_eq!(ctx.changed, vec![im::ATTR_ACL]);
    let entries = decode_entries_for_test(&h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 1 }).unwrap());
    assert_eq!(entries.len(), 2);
    // fabric2 は無傷
    assert_eq!(decode_entries_for_test(&h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 2 }).unwrap()).len(), 1);
}

#[test]
fn purge_fabric_drops_only_that_fabric() {
    let store = AclStore::new();
    store.add_case_admin(1, 111);
    store.add_case_admin(2, 222);
    store.purge_fabric(1);
    let h = AccessControlHandler::new(store);
    assert!(decode_entries_for_test(&h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 1 }).unwrap()).is_empty());
    assert_eq!(decode_entries_for_test(&h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 2 }).unwrap()).len(), 1);
}

#[test]
fn malformed_acl_write_is_constraint_error_and_leaves_store_intact() {
    let store = AclStore::new();
    store.add_case_admin(1, 111);
    let mut h = AccessControlHandler::new(store);
    let mut ctx = InvokeCtx { fabric_index: 1, ..InvokeCtx::default() };
    assert_eq!(h.write(im::ATTR_ACL, &[], &mut ctx), Err(im::STATUS_CONSTRAINT_ERROR));
    assert!(ctx.changed.is_empty());
    assert_eq!(decode_entries_for_test(&h.read(im::ATTR_ACL, &ReadCtx { fabric_index: 1 }).unwrap()).len(), 1);
}
```

`encode_entries_for_test` / `decode_entries_for_test` はテスト mod 内ヘルパ（wire 形は上記のとおり）。

- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — access_control.rs 実装**（decode は Reader、encode は Writer。targets_raw は `put_raw_element(Tag::Context(4), raw)` で書き戻す）
- [ ] **Step 4: RED→GREEN — commissioning.rs のフックをテストしてから実装**

```rust
#[test]
fn add_noc_installs_case_admin_acl_and_remove_fabric_purges_it() {
    // 既存の AddNOC 成功系テスト（`handle_add_noc` を通すテストを検索して流用）に
    // set_acl_store した AclStore を渡し、成功後に store の中身を検証。
    // その後 RemoveFabric を invoke して purge されることを検証。
}
```

- [ ] **Step 5: device.rs 配線** — `let acl_store = AclStore::new();` を作り `comm_server.set_acl_store(acl_store.clone())` + `node.add_cluster(0, Box::new(AccessControlHandler::new(acl_store)))`（`into_cluster_handlers` より前に set すること）
- [ ] **Step 6: GREEN を確認** — `cargo test -p mat-device` 全 green
- [ ] **Step 7: Commit** — `feat(mat-device): AccessControl クラスタ（ACL write 全置換 + AddNOC 自動 admin + fabric purge）`

### Task 4: root 必須クラスタの最小実装（NetworkCommissioning / GeneralDiagnostics / GroupKeyManagement）

**Files:**
- Create: `crates/mat-device/src/core/network_commissioning.rs` / `general_diagnostics.rs` / `group_key_management.rs`（mod.rs へ追加）
- Modify: `crates/mat-controller/src/im.rs`（`CLUSTER_NETWORK_COMMISSIONING=0x0031`, `CLUSTER_GENERAL_DIAGNOSTICS=0x0033` と各属性 id 定数）
- Modify: `crates/mat-device/src/device.rs`（EP0 登録。iface 名は `config.iface` から渡す）
- Test: 各ファイル内

**Interfaces:**
- Consumes: Task 2 の `feature_map()`
- Produces:
  ```rust
  pub struct NetworkCommissioningHandler { /* network_id: Vec<u8> (iface名) */ }
  impl NetworkCommissioningHandler { pub fn new(network_id: &str) -> Self; }
  // feature_map() = 0x04 (Ethernet)。attrs: MaxNetworks(0)=1,
  // Networks(1)=[{Context(0)=network_id bytes, Context(1)=connected bool}],
  // InterfaceEnabled(4)=true, LastNetworkingStatus(5)=null, LastNetworkID(6)=null,
  // LastConnectErrorValue(7)=null。コマンド無し（Ethernet）。

  pub struct GeneralDiagnosticsHandler { /* iface名, started: Instant */ }
  impl GeneralDiagnosticsHandler { pub fn new(iface: &str) -> Self; }
  // attrs: NetworkInterfaces(0)=[struct{0:name str,1:isOperational bool,
  //   4:hardwareAddress 6B bytes(zero),5:IPv4Addresses 空array,6:IPv6Addresses 空array,7:type uint 2}],
  // RebootCount(1)=0, UpTime(2)=起動からの秒(uint), TestEventTriggersEnabled(8)=false。
  // invoke: TestEventTrigger(0x00) → InvokeReply::Status(STATUS_CONSTRAINT_ERROR)
  //   （enable key 不一致の spec 挙動）、accepted_commands=[0x00]。

  pub struct GroupKeyManagementHandler;
  // attrs: GroupKeyMap(0)=空array, GroupTable(1)=空array,
  // MaxGroupsPerFabric(2)=16, MaxGroupKeysPerFabric(3)=1。コマンド未実装（既知ギャップ、
  // spec の申し送り節に記録）。
  ```

- [ ] **Step 1: RED — 3 ハンドラのテストを書く**（attributes()/read の TLV 値検証 + NetworkCommissioning の `feature_map()==0x04` + GeneralDiagnostics の TestEventTrigger → CONSTRAINT_ERROR。identify.rs のテスト構成をそのまま踏襲）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**（null は `Writer::put_null(Tag::Anonymous)`、struct/array は datamodel.rs の `ATTR_GC_BASIC_COMMISSIONING_INFO` の encode を参考に）
- [ ] **Step 4: device.rs 登録** — `node.add_cluster(0, ...)` ×3（iface 名は `config.iface.clone()`。`DeviceConfig` に iface が無い場合は `net` 層の保持場所を確認し、無ければ `new(network_id: &str)` に `"eth0"` 相当を渡す配線を device.rs 内で完結させる）
- [ ] **Step 5: GREEN を確認** — `cargo test -p mat-device`
- [ ] **Step 6: Commit** — `feat(mat-device): root 必須クラスタの最小実装（NetworkCommissioning/GeneralDiagnostics/GroupKeyManagement）`

### Task 5: BasicInformation 必須属性の充足 + NodeLabel write + UniqueID 永続化

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`BasicInformationHandler` ~L920、`with_root_endpoint`）
- Modify: `crates/mat-controller/src/im.rs`（属性 id 定数: `ATTR_BI_NODE_LABEL=0x0005`, `ATTR_BI_LOCATION=0x0006`, `ATTR_BI_HARDWARE_VERSION=0x0007`, `ATTR_BI_HARDWARE_VERSION_STRING=0x0008`, `ATTR_BI_SOFTWARE_VERSION=0x0009`, `ATTR_BI_SOFTWARE_VERSION_STRING=0x000A`, `ATTR_BI_UNIQUE_ID=0x0012`, `ATTR_BI_CAPABILITY_MINIMA=0x0013`, `ATTR_BI_SPECIFICATION_VERSION=0x0015`, `ATTR_BI_MAX_PATHS_PER_INVOKE=0x0016`）
- Modify: `crates/mat-device/src/device.rs`（UniqueID の生成・永続化）
- Test: datamodel.rs tests

**Interfaces:**
- Consumes: Task 2 の `ClusterHandler::write`
- Produces:
  - `Node::with_root_endpoint(vendor_id, product_id)` は既存署名のまま（既存テスト ~15 箇所を壊さない）。UniqueID は `pub fn with_root_endpoint_unique(vendor_id: u16, product_id: u16, unique_id: &str) -> Node` を新設し、既存 fn は `with_root_endpoint_unique(vendor_id, product_id, "matv-dev")` に委譲
  - BasicInformation 追加属性の値: NodeLabel(5)=""（write 可・in-memory・32 文字上限、超過は `Err(STATUS_CONSTRAINT_ERROR)`）、Location(6)="XX"、HardwareVersion(7)=1、HardwareVersionString(8)="matv"、SoftwareVersion(9)=1、SoftwareVersionString(10)=`env!("CARGO_PKG_VERSION")`、UniqueID(18)=コンストラクタ引数、CapabilityMinima(19)=struct{Context(0)=3, Context(1)=3}、SpecificationVersion(21)=0x0104_0000、MaxPathsPerInvoke(22)=1
  - device.rs: `store_dir/unique_id` ファイルがあれば読む、無ければ `getrandom` 16B → hex 32 文字を書いて使う。`Node::with_root_endpoint_unique(config.vendor_id, config.product_id, &unique_id)` に差し替え

- [ ] **Step 1: RED — テストを書く**（新属性の read 値、NodeLabel write→read 反映 + `ctx.changed==[ATTR_BI_NODE_LABEL]`、33 文字 write → CONSTRAINT_ERROR）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device` + `cargo test --workspace`
- [ ] **Step 5: Commit** — `feat(mat-device): BasicInformation の必須属性充足（NodeLabel write / UniqueID 永続化）`

### Task 6: ゲート一式 → jarvis 配布 → Apple 実機チェックポイント

**Files:**
- Modify: `docs/superpowers/specs/2026-08-22-apple-home-interview-design.md`（申し送り節を追記）

- [ ] **Step 1: 自動ゲート** — `cargo test --workspace` / `cargo clippy --workspace --all-targets` / fmt（Global Constraints の注意どおり）/ `MAT_E2E_IFACE=eth0 task e2e:device:m2-chip` / `MAT_E2E_IFACE=eth0 task e2e:device:m1` 全 PASS
- [ ] **Step 2: 配布** — `task dist:arm64` → `scp dist/arm64/matv jarvis:~/.local/bin/matv.new` → `ssh jarvis 'pkill -x matv; install -m755 ~/.local/bin/matv.new ~/.local/bin/matv && rm -f ~/.local/bin/matv.new'` → `~/matv-apple/` で store を空にして再起動（過去ログのコマンド形を踏襲。ssh が残る場合があるので背景化に注意）
- [ ] **Step 3: 人間チェックポイント** — QR を ASCII 描画してユーザーに提示（`uvx --from 'qrcode[pil]' python -c "..."`、payload は matv.stdout.log 1 行目。**毎回 QR を描画し、窓 15 分を開き直してから渡す**）。受け入れ = ホームアプリで部屋割り当てまで進み、タイルから OnOff トグルが matv ログ（Apple TV からの CASE + invoke (1,6,cmd)）で確認できること
- [ ] **Step 4: 申し送り** — 結果（通過 or 新しい停止点のログ抜粋）と deferred（ACL enforcement / 永続化 / GKM コマンド / Timed write）を spec の申し送り節へ追記して commit
