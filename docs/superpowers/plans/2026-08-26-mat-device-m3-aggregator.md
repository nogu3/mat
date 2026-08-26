# mat-device / matv — M3: Aggregator + bridged endpoints 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matv を設定ファイル駆動の Matter bridge（EP0 root / EP1 Aggregator / EP2〜 bridged OnOff endpoints、endpoint 採番は store 永続の自動台帳）にし、M2/Apple 申し送りの fabric scoping・ClusterRevision・DataVersion・永続化を払う。

**Architecture:** `mat-device` の core に BDBI クラスタと bridged endpoint ファクトリ（`kind` enum）を足し、`Node` の Descriptor 導出を bridge トポロジ対応に拡張する。endpoint 採番台帳と ACL/BasicInfo 永続化は net 層（ファイル I/O）に置き、core へは trait 注入で渡す（FabricStore と同じ流儀）。matv は `[[device]]` 配列を読む純 bridge になり、既存の EP1 ハードコードは廃止。

**Tech Stack:** Rust workspace 既存依存のみ（serde / serde_json / toml / getrandom / tokio）。新規外部依存なし。

**Spec:** `docs/superpowers/specs/2026-08-26-mat-device-m3-aggregator-design.md`（同ディレクトリの `2026-08-16-mat-device-m2-design.md` の M3 申し送り、`2026-08-22-apple-home-interview-design.md` の M3 送りが入力）

## Global Constraints

- `task check` 相当を最終ゲートで通す: `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace`
- **core 純度**: `crates/mat-device/src/core/` に tokio・ソケット・ファイル I/O を持ち込まない。`cargo check -p mat-device --no-default-features` が green を維持（CI 検査項目）
- 新規外部依存の追加禁止（workspace 既存 + RustCrypto 系のみ）
- e2e ゲートは `MAT_E2E_IFACE=eth0` で実行し、**成否は exit code で判定**（出力 grep での green 誤報告が M2 で実発生）
- 相互運用に関わる encode 修正は**ワイヤバイトを直接 assert** するテストを書く（lenient decoder 同士の roundtrip はエンコーダの欠落に盲目）
- コミットメッセージは репо の流儀（`feat(mat-device): ...` / `fix(im): ...` 等の Conventional Commits + 日本語サマリ）
- 各タスクは TDD: RED を確認してから GREEN

---

### Task 1: im.rs — bridge 定数と IsFabricFiltered の server-side decode

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（定数群 ~L21-160 / `decode_read_request` L1029 / `decode_subscribe_request` L1104 / `SubscribeRequestIn` L1091）
- Test: 同ファイル `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: 既存の `decode_attribute_requests` / `AttrPathIn` / `Reader`/`Value`
- Produces:
  ```rust
  pub const DEVICE_TYPE_AGGREGATOR: u32 = 0x000E;      // Device Library §11.2
  pub const DEVICE_TYPE_BRIDGED_NODE: u32 = 0x0013;    // Device Library §11.1
  pub const CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION: u32 = 0x0039; // spec §9.13
  pub const ATTR_BDBI_REACHABLE: u32 = 0x0011;         // spec §9.13.4
  pub const STATUS_UNSUPPORTED_ACCESS: u8 = 0x7E;      // spec §8.10.1
  // BDBI の NodeLabel/UniqueID は既存 ATTR_BI_NODE_LABEL(0x0005)/ATTR_BI_UNIQUE_ID(0x0012) を共用

  /// ReadRequestMessage の server-side decode（パス + IsFabricFiltered）。
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct ReadRequestIn {
      pub paths: Vec<AttrPathIn>,
      pub fabric_filtered: bool,
  }
  pub fn decode_read_request_message(payload: &[u8]) -> Result<ReadRequestIn, ImError>;
  // 既存 decode_read_request(payload) は decode_read_request_message(..)?.paths を返す薄い委譲に変更（後方互換）

  // SubscribeRequestIn に追加:
  pub fabric_filtered: bool,
  ```
- IsFabricFiltered のタグ: ReadRequest では `Context(3)` の Bool（`encode_read_request` L338 と同じ位置）、SubscribeRequest では `Context(7)` の Bool（`encode_subscribe_request` L1082 と同じ位置）。**フィールド欠落時は `true` とみなす**（fabric-scoped 属性を絞る側 = 開示が少ない側に倒す。chip 系は常に true を送る実測）

- [ ] **Step 1: RED — テストを書く**

```rust
#[test]
fn decode_read_request_message_extracts_fabric_filtered() {
    // encode_read_request は IsFabricFiltered=true を Context(3) に載せる
    // （このファイル既存の encode_read_request_cluster_is_fabric_filtered
    // テストが wire 位置を保証済み）。true がそのまま出ること。
    let payload = encode_read_request(0, CLUSTER_BASIC_INFORMATION, ATTR_VENDOR_ID);
    let req = decode_read_request_message(&payload).unwrap();
    assert_eq!(req.paths.len(), 1);
    assert!(req.fabric_filtered);
}

#[test]
fn decode_read_request_message_defaults_fabric_filtered_when_absent() {
    // IsFabricFiltered を載せない ReadRequest を手組み（AttributeRequests のみ）
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0));
    w.start_list(Tag::Anonymous);
    w.put_uint(Tag::Context(2), 0); // Endpoint
    w.put_uint(Tag::Context(3), u64::from(CLUSTER_BASIC_INFORMATION));
    w.put_uint(Tag::Context(4), u64::from(ATTR_VENDOR_ID));
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    let req = decode_read_request_message(&w.finish()).unwrap();
    assert!(req.fabric_filtered, "absent IsFabricFiltered must default to true");
}

#[test]
fn decode_subscribe_request_extracts_fabric_filtered() {
    let payload = encode_subscribe_request(1, 60, false, &[]);
    let req = decode_subscribe_request(&payload).unwrap();
    assert!(req.fabric_filtered);
}
```

（`decode_read_request` の既存テストが残っていることも確認 — 委譲後も挙動不変）

- [ ] **Step 2: RED を確認** — `cargo test -p mat-controller decode_read_request_message` が「未定義」で FAIL
- [ ] **Step 3: GREEN — 実装** — 定数追加（既存の定数ブロックの並びに挿入し、既存 doc コメントの流儀で spec 章番号を付ける）。`decode_read_request_message` は既存 `decode_read_request` の loop を移植し、`(Tag::Context(3), Value::Bool(b)) => fabric_filtered = Some(b)` の arm を追加。`decode_subscribe_request` の loop に `(Tag::Context(7), Value::Bool(b))` の arm を追加し、`SubscribeRequestIn` 構築で `fabric_filtered: fabric_filtered.unwrap_or(true)`
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-controller im::`
- [ ] **Step 5: Commit** — `feat(im): bridge 定数 + ReadRequest/SubscribeRequest の IsFabricFiltered decode`

---

### Task 2: ClusterRevision の実値化 + DataVersion のブート時乱数初期化

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`ClusterHandler` trait ~L137 / `read_attribute_value` L713 / `Node` L247 / `INITIAL_DATA_VERSION` L28）
- Modify: `crates/mat-device/src/core/onoff.rs` / `identify.rs` / `groups.rs` / `access_control.rs` / `network_commissioning.rs` / `general_diagnostics.rs` / `group_key_management.rs` / `commissioning.rs`（各 `impl ClusterHandler` に `revision()`）
- Modify: `crates/mat-device/src/device.rs`（seed 注入）
- Test: `datamodel.rs` / `device.rs` tests

**Interfaces:**
- Consumes: 既存 `ClusterHandler` trait / `Node::data_version`
- Produces:
  ```rust
  // ClusterHandler に追加（default 1 — テスト用モックはそのまま通る）:
  /// ClusterRevision (spec §7.13, id 0xFFFD)。実装クラスタは現行仕様の
  /// revision を返す（全クラスタ 1 固定だった M2 の既知ギャップの解消）。
  fn revision(&self) -> u16 { 1 }

  // Node に追加:
  /// 全 (endpoint, cluster) の初期 DataVersion (spec §7.10.3: ブートごとに
  /// 乱数初期化)。デフォルトは INITIAL_DATA_VERSION=1（既存テスト互換）。
  /// クラスタインスタンスごとの独立乱数ではなく node 単位の共通 base — 目的
  /// （前ブートのキャッシュ済み DataVersion との偶然一致の排除）には十分で、
  /// core に乱数源を持ち込まない（呼び出し側が値を渡す）。
  pub fn set_data_version_base(&mut self, base: u32);
  ```
- 各クラスタの `revision()` 実値（M2 spec 申し送りの列挙値 + Matter 1.4 現行値）: Descriptor **2** / BasicInformation **3** / OnOff **6** / Identify **4** / Groups **4** / AccessControl **2** / OperationalCredentials **1** / GeneralCommissioning **1** / AdministratorCommissioning **1** / NetworkCommissioning **2** / GeneralDiagnostics **2** / GroupKeyManagement **2**

- [ ] **Step 1: RED — テストを書く**（datamodel.rs tests）

```rust
#[test]
fn cluster_revision_reflects_handler_value() {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    // Descriptor は revision 2 を返す（1 固定だった M2 ギャップの解消）
    let req = im::encode_read_request(0, im::CLUSTER_DESCRIPTOR, im::ATTR_CLUSTER_REVISION);
    let (_, payload) = handle_im_ok(&mut node, im::OPCODE_READ_REQUEST, &req);
    let msg = decode_report_data_message(&payload).unwrap();
    assert_eq!(msg.reports[0].data, Some(serde_json::json!(2)));
}

#[test]
fn data_version_base_seeds_initial_version_and_bump() {
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    node.set_data_version_base(0xDEAD_BEEF);
    assert_eq!(node.data_version(0, im::CLUSTER_BASIC_INFORMATION), 0xDEAD_BEEF);
    // NodeLabel write で bump → base+1（既存の write→changed 経路を流用）
    let mut w = Writer::new();
    w.put_str(Tag::Anonymous, "x");
    let req = im::encode_write_request_tlv(0, im::CLUSTER_BASIC_INFORMATION, im::ATTR_BI_NODE_LABEL, &w.finish());
    let _ = handle_im_ok(&mut node, im::OPCODE_WRITE_REQUEST, &req);
    assert_eq!(node.data_version(0, im::CLUSTER_BASIC_INFORMATION), 0xDEAD_BEEF_u32.wrapping_add(1));
}
```

- [ ] **Step 2: RED を確認** — `cargo test -p mat-device cluster_revision_reflects` FAIL
- [ ] **Step 3: GREEN — 実装**
  - trait に `revision()` 追加、`read_attribute_value` の `im::ATTR_CLUSTER_REVISION => Some(uint_value(1))` を `Some(uint_value(u64::from(handler.revision())))` に変更
  - 各ハンドラに `fn revision(&self) -> u16 { N }` を実装（上の実値表。`commissioning.rs` は GeneralCommissioning/OperationalCredentials/AdministratorCommissioning の 3 つの `impl ClusterHandler` それぞれ）
  - `Node` に `version_base: u32` フィールド（`new()` で `INITIAL_DATA_VERSION`）。`data_version()` の `unwrap_or` と `handle_invoke`/`handle_write` の `or_insert` を `self.version_base` に差し替え
  - `device.rs` `Device::new`: `Node` 構築直後に `getrandom` 4 バイト → `node.set_data_version_base(u32::from_le_bytes(seed))`（`.map_err(|e| DeviceError::Io(std::io::Error::other(format!("os rng: {e}"))))?`）
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device`
- [ ] **Step 5: Commit** — `feat(mat-device): ClusterRevision 実値化 + DataVersion のブート時乱数 base`

---

### Task 3: OC 属性の fabric scoping + SupportedFabrics 容量 enforcement

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`ReadCtx` L102）
- Modify: `crates/mat-device/src/core/commissioning.rs`（`read_operational_credentials` L679 / `encode_nocs` L715 / `encode_fabrics` L736 / `encode_trusted_root_certificates` L755 / `handle_add_noc` L992）
- Modify: `crates/mat-device/src/net/runtime.rs`（`serve_read_request_chunked` L1825〜 / `serve_subscribe_request` L1467〜 / `ActiveSubscription` 構築 L1559 と dirty レポートの `ReadCtx` L1607 / その他 `ReadCtx {` 構築箇所 L1239, L1493, L1841 — コンパイルエラーで全箇所洗い出す）
- Test: `commissioning.rs` tests

**Interfaces:**
- Consumes: Task 1 の `decode_read_request_message` / `SubscribeRequestIn::fabric_filtered`
- Produces:
  ```rust
  // ReadCtx に追加（Copy/Default 維持。Default は false = 現行の無フィルタ挙動
  // なので既存テストは不変）:
  pub fabric_filtered: bool,

  // commissioning.rs:
  const SUPPORTED_FABRICS: u8 = 5; // ATTR_OC_SUPPORTED_FABRICS の 5 と単一定数に統一
  ```
- filtering 規約（spec §11.17.5 / §8.9.2.4）: `ctx.fabric_filtered == true` のとき NOCs / Fabrics / TrustedRootCertificates は **accessing fabric（`ctx.fabric_index`）のエントリのみ**返す。fabric_index 0（PASE）は空配列。`false` は現行どおり全件（deferred の「FabricFiltered=false の read」の厳密化はスコープ外のまま）。`CommissionedFabrics`/`SupportedFabrics` はスカラーなのでフィルタ対象外
- `handle_add_noc`: decode 成功直後・`NOC_STATUS_MISSING_CSR` 判定より前に容量チェック — `self.store.entries().len() >= usize::from(SUPPORTED_FABRICS)` なら `noc_status(NOC_STATUS_TABLE_FULL)`（既存クロージャ L1006 を使う）
- runtime: `serve_read_request_chunked` は `decode_read_request_message` に切替え `ReadCtx { fabric_index, fabric_filtered: req.fabric_filtered }`。`serve_subscribe_request` は priming と `ActiveSubscription`（**フィールド `fabric_filtered: bool` を追加**）の両方へ伝搬し、dirty/keep-alive レポートの `ReadCtx`（L1607 付近）でも使う。それ以外の `ReadCtx` 構築箇所（`handle_im` 経由の invoke/write パス）は `fabric_filtered: true`（chip 系の read 既定と同じ側）

- [ ] **Step 1: RED — テストを書く**（commissioning.rs tests。既存の `nocs_tlv`/Fabrics 読みテスト L1999〜 の構成を流用し、fabric 2 本インストール済みの server に対して）

```rust
#[test]
fn oc_reads_are_fabric_scoped_when_fabric_filtered() {
    // install_fabric を 2 回（既存テストヘルパの流儀で fabric 1, 2 を作る）
    // fabric_filtered=true + fabric_index=1 → NOCs/Fabrics/TrustedRoots が
    // fabric 1 の 1 エントリのみ。fabric_index=0 (PASE) → 空配列。
    // fabric_filtered=false → 従来どおり 2 エントリ（回帰ガード）。
    // 検証は decode_report_data ではなく TLV Reader で配列要素数と
    // Context(254)=FabricIndex を直接 assert（wire バイト直接主義）。
}

#[test]
fn add_noc_rejects_sixth_fabric_with_table_full() {
    // install_fabric を 5 回 → 6 回目の AddNOC が
    // InvokeReply::Data { response_command: RESP_NOC, .. } かつ
    // NOCResponse の StatusCode == NOC_STATUS_TABLE_FULL(5)。
    // fabrics.len() は 5 のまま。
}
```

- [ ] **Step 2: RED を確認** — `cargo test -p mat-device oc_reads_are_fabric_scoped` FAIL（`fabric_filtered` フィールド未定義のコンパイルエラーでも RED 扱い）
- [ ] **Step 3: GREEN — 実装** — `encode_nocs`/`encode_fabrics`/`encode_trusted_root_certificates` に `ctx: &ReadCtx` を渡し、`let entries = self.store.entries().iter().filter(|e| !ctx.fabric_filtered || e.fabric_index == ctx.fabric_index)` で絞る。runtime の伝搬と `ActiveSubscription` フィールド追加
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device` + `cargo test --workspace`（runtime の既存テスト回帰）
- [ ] **Step 5: Commit** — `feat(mat-device): OC 属性の fabric scoping + SupportedFabrics=5 の AddNOC 容量 enforcement`

---

### Task 4: DescriptorHandler — 複数 device type と静的 PartsList

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`DescriptorHandler` L992〜）
- Test: 同ファイル tests

**Interfaces:**
- Consumes: Task 1 の `DEVICE_TYPE_AGGREGATOR` / `DEVICE_TYPE_BRIDGED_NODE`
- Produces:
  ```rust
  pub struct DescriptorHandler {
      device_types: Vec<u32>,
      /// この endpoint 自身の PartsList（EP0 以外用 — EP0 は従来どおり
      /// Node::read_attribute_value が registry から導出して intercept）。
      /// EP1 Aggregator が bridged EP 群を静的に持つ（設定反映は再起動のみ
      /// なので動的導出は不要 — YAGNI）。
      parts: Vec<u16>,
  }
  impl DescriptorHandler {
      pub fn for_device(device_type: u32) -> Self;              // 既存 — vec![device_type], parts 空
      pub fn for_device_types(device_types: &[u32]) -> Self;    // 新規 — bridged EP 用
      pub fn with_parts(self, parts: Vec<u16>) -> Self;         // 新規 — EP1 Aggregator 用 builder
  }
  ```
- `read(ATTR_DEVICE_TYPE_LIST)` は `device_types` 全件を DeviceTypeStruct（revision 1）で並べる。`read(ATTR_PARTS_LIST)` は `self.parts` をエンコード（空なら従来と同じ空配列）。`revision()` は Task 2 で 2 済み

- [ ] **Step 1: RED — テストを書く**

```rust
#[test]
fn descriptor_multi_device_types_and_static_parts() {
    let mut node = Node::new();
    node.add_endpoint(2, vec![Box::new(
        DescriptorHandler::for_device_types(&[im::DEVICE_TYPE_ON_OFF_LIGHT, im::DEVICE_TYPE_BRIDGED_NODE]),
    )]);
    node.add_endpoint(1, vec![Box::new(
        DescriptorHandler::for_device(im::DEVICE_TYPE_AGGREGATOR).with_parts(vec![2]),
    )]);
    // EP2 DeviceTypeList: 2 エントリ [{0:0x0100,1:1},{0:0x0013,1:1}]
    // EP1 PartsList: [2]
    //（decode_report_data_message の json で assert — 既存テストの流儀）
}
```

- [ ] **Step 2: RED を確認** — FAIL（`for_device_types` 未定義）
- [ ] **Step 3: GREEN — 実装**（`device_type: u32` フィールドの既存参照 2 箇所も `device_types` に追随）
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device`
- [ ] **Step 5: Commit** — `feat(mat-device): DescriptorHandler の複数 device type + 静的 PartsList 対応`

---

### Task 5: Bridged Device Basic Information クラスタ (0x0039)

**Files:**
- Create: `crates/mat-device/src/core/bridged_device_basic_information.rs`
- Modify: `crates/mat-device/src/core/mod.rs`（`pub mod bridged_device_basic_information;` 追加）
- Test: 新ファイル内 tests

**Interfaces:**
- Consumes: `ClusterHandler` trait / Task 1 の `CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION` / `ATTR_BDBI_REACHABLE`
- Produces:
  ```rust
  /// Bridged Device Basic Information (spec §9.13) — bridged endpoint の
  /// 名前と到達性。NodeLabel は設定ファイルの name が正本（read-only —
  /// コントローラ側 write を許すと再起動で config 値に巻き戻って混乱する。
  /// コントローラは自分のローカル名を別途持てる）。Reachable は M3 では
  /// 常に true（mando 不達判定は M4）。
  pub struct BridgedDeviceBasicInformationHandler {
      node_label: String,
      unique_id: String,
  }
  impl BridgedDeviceBasicInformationHandler {
      pub fn new(node_label: &str, unique_id: &str) -> Self;
  }
  ```
- attributes: `[ATTR_BI_NODE_LABEL, ATTR_BDBI_REACHABLE, ATTR_BI_UNIQUE_ID]`。read: NodeLabel=str / Reachable=bool true / UniqueID=str。`revision()` = **3**。コマンド無し（`invoke` は `STATUS_UNSUPPORTED_COMMAND`）。write はデフォルト（拒否）のまま

- [ ] **Step 1: RED — テストを書く**（`identify.rs` のテスト構成を踏襲: ハンドラ単体で `attributes()`/`read` の TLV 値を Reader で直接検証 + `revision() == 3` + 未実装属性は `None`）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**（値エンコードは `datamodel.rs` の `str_value`/`bool_value` 相当を手元で組む — `Writer::put_str`/`put_bool` に `Tag::Anonymous`）
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device bridged_device`
- [ ] **Step 5: Commit** — `feat(mat-device): Bridged Device Basic Information クラスタ（NodeLabel/Reachable/UniqueID）`

---

### Task 6: DeviceKind とbridged endpoint ファクトリ

**Files:**
- Create: `crates/mat-device/src/core/bridge.rs`
- Modify: `crates/mat-device/src/core/mod.rs`
- Test: 新ファイル内 tests

**Interfaces:**
- Consumes: Task 4 の `DescriptorHandler::for_device_types` / Task 5 の `BridgedDeviceBasicInformationHandler` / 既存 `OnOffHandler::new()` / `IdentifyHandler::new()` / `GroupsHandler::new(identify_state)`
- Produces:
  ```rust
  /// 設定ファイルの kind enum。種別追加は「ここに 1 値 +
  /// build_bridged_endpoint に 1 分岐」で完結する（M3 spec の拡張可能性
  /// 要件）。serde 綴りは設定ファイルの正本表記。
  #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
  pub enum DeviceKind {
      #[serde(rename = "onoff-light")]
      OnOffLight,
  }

  /// 1 つの bridged endpoint に載せるクラスタ一式と、外側（runtime/ログ）
  /// へ渡す状態ハンドル。
  pub struct BridgedEndpoint {
      pub clusters: Vec<Box<dyn ClusterHandler>>,
      pub onoff_state: std::sync::Arc<std::sync::atomic::AtomicBool>,
  }

  pub fn build_bridged_endpoint(kind: DeviceKind, name: &str, unique_id: &str) -> BridgedEndpoint;
  ```
- OnOffLight のクラスタ構成（この順で登録）: `DescriptorHandler::for_device_types(&[DEVICE_TYPE_ON_OFF_LIGHT, DEVICE_TYPE_BRIDGED_NODE])` / `BridgedDeviceBasicInformationHandler::new(name, unique_id)` / `IdentifyHandler` / `GroupsHandler` / `OnOffHandler`（On/Off Light 必須クラスタ = M2 EP1 構成 + BDBI）

- [ ] **Step 1: RED — テストを書く**（`build_bridged_endpoint(OnOffLight, "Living", "uid-1")` → clusters の `cluster_id()` 集合が {Descriptor, BDBI, Identify, Groups, OnOff} / `toml`+serde で `"onoff-light"` が `DeviceKind::OnOffLight` にデシリアライズされる / 未知 kind 文字列はエラー）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device bridge`
- [ ] **Step 5: Commit** — `feat(mat-device): DeviceKind enum と bridged endpoint ファクトリ`

---

### Task 7: endpoint 採番台帳（store/endpoints.json）

**Files:**
- Create: `crates/mat-device/src/net/endpoint_ledger.rs`
- Modify: `crates/mat-device/src/net/mod.rs`（`pub mod endpoint_ledger;`）
- Test: 新ファイル内 tests（tempfile — net 層なのでファイル I/O 可）

**Interfaces:**
- Consumes: serde_json（workspace 既存依存）
- Produces:
  ```rust
  /// bridged endpoint の採番開始値（EP0=root, EP1=Aggregator の次）。
  pub const FIRST_BRIDGED_ENDPOINT: u16 = 2;

  /// 設定ファイルの device id → endpoint id の永続台帳
  /// (`<store_dir>/endpoints.json`)。単調増加・再利用禁止（Matter bridge の
  /// endpoint 安定要件, spec §9.12.2.2）。削除された id のエントリも残す
  /// （tombstone）— 同じ id の再追加は旧 endpoint を復元し、コントローラの
  /// アクセサリ対応が生き返る。
  pub struct EndpointLedger { /* path: PathBuf, next: u16, map: BTreeMap<String, u16> */ }
  impl EndpointLedger {
      /// 無ければ { next: FIRST_BRIDGED_ENDPOINT, map: {} }。壊れた/不整合
      /// ファイル（next <= map の最大値）は next を max+1 に修復して読む。
      pub fn load(store_dir: &std::path::Path) -> std::io::Result<Self>;
      /// 既知 id は既存値、新規 id は next を払い出して map へ（save は
      /// 呼び出し側が全 assign 後に 1 回）。
      pub fn assign(&mut self, id: &str) -> u16;
      pub fn save(&self) -> std::io::Result<()>;
  }
  ```
- JSON 形: `{"next": 5, "map": {"living-light": 2, "bedroom-light": 3}}`（serde derive した private struct で読み書き）

- [ ] **Step 1: RED — テストを書く**（新規 store で assign 3 回 → 2,3,4 / save→load 往復で不変 / 既知 id は再 assign でも同値 / map から消さず新 id 追加 → 5（tombstone 越し単調増加）/ 壊れ JSON・next 不整合の修復）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device endpoint_ledger`
- [ ] **Step 5: Commit** — `feat(mat-device): endpoint 採番台帳（endpoints.json、単調増加・tombstone 復元）`

---

### Task 8: Device の bridge 組み立てと matv の `[[device]]` 設定（純 bridge 化）

**Files:**
- Modify: `crates/mat-device/src/device.rs`（`DeviceConfig` L63 / `Device::new` の EP1 ブロック L254-270 / `onoff_state` フィールド L160）
- Modify: `crates/mat-device/src/net/runtime.rs`（`test_config()` L1901 — `devices: vec![]` 追加のみ）
- Modify: `crates/mat-device/tests/support/mod.rs`（`device_config` L63 — device 1 台追加 + `pub const BRIDGED_EP: u16 = 2;`）
- Modify: `crates/mat-device/tests/onoff_invoke.rs` / `subscribe_loop.rs` / `self_commission_live.rs`（endpoint 1 → `support::BRIDGED_EP`。self_commission_live が endpoint を参照していなければ触らない）
- Modify: `crates/matv/src/main.rs`（`FileConfig` L48 / `load_config` L107 / `run` L137）
- Modify: `crates/matv/tests/cli.rs`（生成 config に `[[device]]` 追加）
- Test: device.rs tests / matv main.rs tests

**Interfaces:**
- Consumes: Task 6 `DeviceKind`/`build_bridged_endpoint` / Task 7 `EndpointLedger`/`FIRST_BRIDGED_ENDPOINT` / Task 4 `with_parts`
- Produces:
  ```rust
  // device.rs:
  /// 設定ファイル 1 [[device]] 分。id は台帳キー（安定・改名不可）、name は
  /// BDBI NodeLabel。バリデーション（id 一意・非空・name 32 文字以内）は
  /// matv::load_config の責務 — Device::new は与えられたものを組むだけ。
  #[derive(Debug, Clone)]
  pub struct VirtualDeviceConfig {
      pub id: String,
      pub kind: crate::core::bridge::DeviceKind, // matv 側からは mat_device::core::bridge::DeviceKind
      pub name: String,
  }
  // DeviceConfig に追加:
  pub devices: Vec<VirtualDeviceConfig>,
  // Device のフィールド変更:
  #[allow(dead_code)] // M4 で mando 転送/ログが消費
  onoff_states: Vec<(String, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
  ```
- `Device::new` の組み立て（既存 EP1 ハードコードブロックを置換）:
  1. `EndpointLedger::load(&config.store_dir)` → 全 `config.devices` を宣言順に `assign` → `save`（`DeviceError::Io`）
  2. `node.add_endpoint(1, vec![Box::new(DescriptorHandler::for_device(im::DEVICE_TYPE_AGGREGATOR).with_parts(bridged_eps.clone()))])`
  3. 各 device: `build_bridged_endpoint(d.kind, &d.name, &format!("{unique_id}-{}", d.id))` → `node.add_endpoint(ep, built.clusters)`、`onoff_states.push((d.id.clone(), built.onoff_state))`
- matv 側:
  ```rust
  #[derive(Debug, Deserialize)]
  struct FileDeviceConfig { id: String, kind: DeviceKind, name: String }
  // FileConfig に追加:
  #[serde(default, rename = "device")]
  devices: Vec<FileDeviceConfig>,
  ```
  `load_config` のバリデーション追加: devices 空 → `"config must declare at least one [[device]]"` / id 空 or 重複 → エラー / `name.chars().count() > 32` → エラー（BDBI NodeLabel の string32 制約）
- e2e/テスト用の標準 device ブロック（tests/cli.rs・support/mod.rs・後続 Task 11 のスクリプトで同じものを使う）:
  ```toml
  [[device]]
  id = "e2e-light"
  kind = "onoff-light"
  name = "E2E Light"
  ```

- [ ] **Step 1: RED — device.rs のトポロジテストを書く**（`#[tokio::test]` — `Device::new` は tokio runtime 必須。device.rs 内 tests は `self.node` に直接触れる）

```rust
#[tokio::test]
async fn bridge_topology_and_ledger_stability() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = |devices: Vec<VirtualDeviceConfig>| DeviceConfig {
        passcode: 20202021, discriminator: 0xF00, vendor_id: 0xFFF1, product_id: 0x8000,
        port: 0, store_dir: dir.path().to_path_buf(), iface: "lo".into(),
        attestation: AttestationMode::default(), devices,
    };
    let dev = |id: &str, name: &str| VirtualDeviceConfig {
        id: id.into(), kind: DeviceKind::OnOffLight, name: name.into(),
    };
    // 3 台 → EP0 PartsList [1,2,3,4] / EP1 は Aggregator + parts [2,3,4] /
    // EP2 の DeviceTypeList に 0x0100 と 0x0013 / BDBI NodeLabel が name
    let d1 = Device::new(cfg(vec![dev("a",""), dev("b",""), dev("c","")])).unwrap();
    // node への read は既存 datamodel テストの handle_im 流儀を関数化して使う
    ...
    drop(d1);
    // 再起動同等: 同じ store で b を外し d を足す → a=2, c=4 のまま、d=5
    let d2 = Device::new(cfg(vec![dev("a",""), dev("c",""), dev("d","")])).unwrap();
    ...
    drop(d2);
    // b を再追加 → 旧 EP3 が復元
    let d3 = Device::new(cfg(vec![dev("a",""), dev("b",""), dev("c",""), dev("d","")])).unwrap();
    ...
}
```

（assert は `node.handle_im` で `ATTR_PARTS_LIST` / `ATTR_DEVICE_TYPE_LIST` / BDBI `ATTR_BI_NODE_LABEL` を読み `decode_report_data_message` の json で行う）

- [ ] **Step 2: RED を確認** — `devices` フィールド未定義のコンパイルエラー = RED
- [ ] **Step 3: GREEN — device.rs 実装 + 全 DeviceConfig 構築箇所の追随**（runtime.rs `test_config` は `devices: vec![]`、tests/support は e2e-light 1 台 + `BRIDGED_EP` 公開、onoff_invoke/subscribe_loop の endpoint 参照を差し替え）
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device`（integration tests 込み）
- [ ] **Step 5: matv の RED — load_config バリデーションテストを書く**（既存 tests 流儀: devices 無し→エラー / id 重複→エラー / name 33 文字→エラー / 正常 3 台→Ok で devices.len()==3）
- [ ] **Step 6: matv RED を確認**
- [ ] **Step 7: matv GREEN — FileConfig/validation/run() の `devices` 変換実装 + tests/cli.rs の config 追随**
- [ ] **Step 8: GREEN を確認** — `cargo test -p matv`
- [ ] **Step 9: Commit** — `feat(mat-device): matv を純 bridge 化（EP1 Aggregator + 設定駆動 bridged endpoints + 採番台帳）`

---

### Task 9: ACL の永続化と write ガード（PASE 拒否・容量）

**Files:**
- Modify: `crates/mat-device/src/core/access_control.rs`（`AclStore` L51 / `AccessControlHandler::write`）
- Modify: `crates/mat-device/src/net/store.rs`（`FileAclStore` 追加）
- Modify: `crates/mat-device/src/device.rs`（`AclStore::new()` L213 → `with_persist`）
- Test: access_control.rs tests / net/store.rs tests

**Interfaces:**
- Consumes: Task 1 の `STATUS_UNSUPPORTED_ACCESS` / 既存 `im::STATUS_RESOURCE_EXHAUSTED`
- Produces:
  ```rust
  // access_control.rs（FabricPersist と同じ注入パターン — core にファイル I/O を持ち込まない）:
  pub trait AclPersist: Send {
      fn save(&self, entries: &[AclDeviceEntry]) -> Result<(), String>;
      fn load(&self) -> Result<Vec<AclDeviceEntry>, String>;
  }
  impl AclStore {
      pub fn with_persist(persist: Box<dyn AclPersist>) -> Self; // load 失敗は空 = 初回起動と同義
  }
  // AclDeviceEntry に #[derive(serde::Serialize, serde::Deserialize)] を追加

  // net/store.rs:
  pub fn acl_store_in_dir(dir: &Path) -> FileAclStore; // <dir>/acl.json、store_in_dir と同構成
  ```
- `AclStore` の内部を `Arc<Mutex<Vec<..>>>` → `Arc<Mutex<AclInner { entries, persist: Option<Box<dyn AclPersist>> }>>` に変更。**全 mutation（add_case_admin / purge_fabric / replace_fabric / append_to_fabric）後に save**。save 失敗は `tracing::warn` して続行（fabric 撤去の後始末を止めない — 31f4b44 / c4bbcb4 と同じ裁定。ACL enforcement 未実装のため即時の実害はなく、復旧はコントローラの再 write で可能）
- `AccessControlHandler::write` の冒頭に 2 ガードを追加（この順）:
  1. `ctx.fabric_index == 0` → `Err(im::STATUS_UNSUPPORTED_ACCESS)`（PASE セッションは fabric を持たず ACL を書けない）
  2. 容量: 全置換パスは `entries.len() > ACL_ENTRIES_PER_FABRIC(=4)` → `Err(im::STATUS_RESOURCE_EXHAUSTED)`、append パスは `self.store.entries_for(ctx.fabric_index).len() >= 4` → 同左（`ATTR_ACL_ENTRIES_PER_FABRIC` の申告値 4 と単一定数 `pub(crate) const ACL_ENTRIES_PER_FABRIC: usize = 4` に統一）

- [ ] **Step 1: RED — テストを書く**（永続化: `MemAclPersist`（テスト内 struct、`Arc<Mutex<Vec>>` 共有）で write→別インスタンス load 復元 / purge 後の save 反映。ガード: fabric_index=0 write → UNSUPPORTED_ACCESS / 5 エントリ全置換 → RESOURCE_EXHAUSTED / 4 件埋まった fabric への append → RESOURCE_EXHAUSTED。net/store.rs: acl.json 実ファイル往復）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device`
- [ ] **Step 5: device.rs 配線** — `let acl_store = AclStore::with_persist(Box::new(crate::net::store::acl_store_in_dir(&config.store_dir)));`（既存 L213 置換）。既存 workspace テストが green のまま
- [ ] **Step 6: Commit** — `feat(mat-device): ACL の store 永続化 + PASE write 拒否 + per-fabric 容量 enforcement`

---

### Task 10: BasicInformation の NodeLabel/Location 永続化と小粒 deferred

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`BasicInformationHandler` L1059 / `with_root_endpoint_unique` L305）
- Modify: `crates/mat-device/src/net/store.rs`（basic_info.json helper）
- Modify: `crates/mat-device/src/device.rs`（`load_or_create_unique_id` L130 の expect 排除 / `Node` 構築の差し替え）
- Test: datamodel.rs / store.rs / device.rs tests

**Interfaces:**
- Consumes: —
- Produces:
  ```rust
  // datamodel.rs:
  /// NodeLabel/Location の永続化先（AclPersist/FabricPersist と同じ注入
  /// パターン）。save のみ — 初期値は構築時に呼び出し側が渡す。
  pub trait BasicInfoPersist: Send {
      fn save(&self, node_label: &str, location: &str) -> Result<(), String>;
  }
  impl Node {
      /// with_root_endpoint_unique + 初期 NodeLabel/Location + persist。
      /// 既存 fn はすべて（label="", location="XX", persist なし）に委譲 —
      /// 既存 ~15 call site を壊さない。
      pub fn with_root_endpoint_persisted(
          vendor_id: u16, product_id: u16, unique_id: &str,
          node_label: String, location: String,
          persist: Box<dyn BasicInfoPersist>,
      ) -> Self;
  }

  // net/store.rs:
  /// <dir>/basic_info.json {"node_label": "...", "location": "XX"}
  pub fn basic_info_in_dir(dir: &Path) -> FileBasicInfoStore;        // BasicInfoPersist impl
  pub fn load_basic_info(dir: &Path) -> (String, String);            // 無し/壊れ → ("", "XX")
  ```
- `BasicInformationHandler` の変更:
  - フィールド追加: `location: String` / `persist: Option<Box<dyn BasicInfoPersist>>`（`read(ATTR_BI_LOCATION)` は固定 "XX" → `&self.location` に）
  - **NodeLabel write の dedup**: 新値 == 現値なら `Ok(())` で `ctx.changed` に積まない（無変化 dirty レポートの抑止 — Apple deferred）
  - **Location の write 対応**: `ATTR_BI_LOCATION` を write 可に。制約: UTF-8 で**ちょうど 2 文字**（spec §11.1.6.6 CountryCode）、違反は `STATUS_CONSTRAINT_ERROR`。dedup 同様
  - どちらも変化時のみ `persist.save(&self.node_label, &self.location)`（失敗は `tracing::warn` して write 自体は成功 — in-memory 状態が正、次の write で再試行される）
- `device.rs`:
  - `load_or_create_unique_id` の `getrandom(..).expect("os rng")` → `.map_err(|e| std::io::Error::other(format!("os rng: {e}")))?`（Apple deferred の getrandom エラー伝播）
  - `Node` 構築: `let (label, location) = load_basic_info(&config.store_dir); Node::with_root_endpoint_persisted(config.vendor_id, config.product_id, &unique_id, label, location, Box::new(basic_info_in_dir(&config.store_dir)))`

- [ ] **Step 1: RED — テストを書く**（dedup: 同値 write 2 回目で `ImOutcome::changed` が空 / Location: "JP" write→read 反映、"JPN"/"J" → CONSTRAINT_ERROR / persist: テスト内 `MemBasicInfoPersist` に write 後の値が届く、同値 write では呼ばれない / store.rs: basic_info.json 往復 + 欠損時 ("", "XX") / device.rs: 既存 unique_id テスト green 維持）
- [ ] **Step 2: RED を確認**
- [ ] **Step 3: GREEN — 実装**
- [ ] **Step 4: GREEN を確認** — `cargo test -p mat-device` + `cargo test --workspace`
- [ ] **Step 5: Commit** — `feat(mat-device): NodeLabel/Location の write 永続化・dedup + getrandom エラー伝播`

---

### Task 11: e2e ゲートの bridged 構成化とトポロジ assert

**Files:**
- Modify: `scripts/e2e-device-m1.sh`（matv.toml heredoc ~L34 以降の生成部）
- Modify: `scripts/e2e-device-m2-chip.sh`（`ENDPOINT=1` L74 / matv.toml heredoc L125 / commission 後の検証列）

**Interfaces:**
- Consumes: Task 8 の `[[device]]` スキーマ（標準ブロック: id="e2e-light", kind="onoff-light", name="E2E Light"）
- Produces: bridged 構成で通る 2 ゲート（`task e2e:device:m1` / `task e2e:device:m2-chip`）

- [ ] **Step 1: m1 スクリプト更新** — 生成する matv.toml 末尾に標準 `[[device]]` ブロックを追記（m1 は commission のみで endpoint 非依存 — config が新スキーマで valid になることだけが要件）
- [ ] **Step 2: m1 を実行して PASS 確認** — `MAT_E2E_IFACE=eth0 task e2e:device:m1`; exit code 0
- [ ] **Step 3: m2-chip スクリプト更新**
  - matv.toml heredoc に標準 `[[device]]` ブロック追記、`ENDPOINT=1` → `ENDPOINT=2`
  - commission 直後（既存 baseline read の前）にトポロジ検証を追加:
    ```bash
    # bridge topology: EP0 PartsList ⊇ {1,2} / EP1(Aggregator) PartsList = [2]
    assert_parts_list() {
        # $1=endpoint $2=expected-member
        local out
        out="$(chip descriptor read parts-list "$NODE_ID" "$1")" \
            || fail "descriptor read parts-list ep$1 failed (exit non-zero)"
        grep -Eq "\[[0-9]+\]: $2\b" <<<"$out" \
            || fail "ep$1 parts-list missing endpoint $2: $(grep -E 'PartsList|\[' <<<"$out" | head -5)"
    }
    assert_parts_list 0 1
    assert_parts_list 0 2
    assert_parts_list 1 2
    ```
    （chip-tool の parts-list 出力は `CHIP:TOO:   PartsList: N entries` + `CHIP:TOO:     [i]: <ep>` 形。**成否は関数の exit code で伝搬** — Global Constraints の教訓どおり `|| fail` を要素ごとに付ける）
- [ ] **Step 4: m2-chip を実行して PASS 確認** — `MAT_E2E_IFACE=eth0 task e2e:device:m2-chip`; exit code 0（commission → topology → EP2 OnOff toggle → 再起動 re-CASE → Subscribe の全列）
- [ ] **Step 5: Commit** — `test(e2e): device ゲートを bridged 構成に更新（EP2 操作 + parts-list 検証）`

---

### Task 12: 最終ゲート一式・mat describe 確認・申し送り

**Files:**
- Modify: `docs/superpowers/specs/2026-08-26-mat-device-m3-aggregator-design.md`（申し送り節を追記）

- [ ] **Step 1: 自動ゲート全列** — `cargo fmt --all -- --check` / `cargo clippy --workspace --all-targets -- -D warnings` / `cargo test --workspace` / `cargo check -p mat-device --no-default-features` / `MAT_E2E_IFACE=eth0 task e2e:device:m1` / `MAT_E2E_IFACE=eth0 task e2e:device:m2-chip` — すべて exit code 0
- [ ] **Step 2: mat describe のマトリョーシカ確認**（spec の M3 受け入れ条件）— 仮想デバイス 3 台の matv を一時 store で起動し、実 `mat` で commission → describe:
  ```bash
  WORK=$(mktemp -d)
  cat > "$WORK/matv.toml" <<'EOF'
  passcode = 20202021
  discriminator = 3840
  vendor_id = 65521
  product_id = 32768
  port = 5541
  store = "STORE_DIR"
  iface = "eth0"
  [[device]]
  id = "living-light"
  kind = "onoff-light"
  name = "Living Light"
  [[device]]
  id = "bedroom-light"
  kind = "onoff-light"
  name = "Bedroom Light"
  [[device]]
  id = "goodnight"
  kind = "onoff-light"
  name = "Goodnight Scene"
  EOF
  sed -i "s|STORE_DIR|$WORK/store|" "$WORK/matv.toml"
  cargo build --release -p matv -p mat
  ./target/release/matv --config "$WORK/matv.toml" >"$WORK/matv.json" 2>"$WORK/matv.log" &
  # commission と describe は e2e-device-m1.sh の呼び出し形（mat fabric init →
  # mat --iface eth0 commission --setup-code ... --paa-dir "$WORK/store/paa"）を
  # そのまま流用。describe は同スクリプトの mat 呼び出しと同じ store/iface 指定で:
  #   ./target/release/mat --iface eth0 describe <node>
  ```
  合格条件: describe の出力に EP1（device type Aggregator 0x000E, parts [2,3,4]）と EP2/3/4（On/Off Light + Bridged Node、それぞれの NodeLabel）が正しく現れること。出力をタスクレポートに貼る
- [ ] **Step 3: 申し送り節を spec に追記** — 完了時刻・ゲート結果・実装中の設計逸脱（レジャー裁定）・M4 送り事項（Reachable 実判定 / mando 転送 / Apple Home 実機ゲート / dirty チャンク化ほか spec「送る」リスト）を `2026-08-26-mat-device-m3-aggregator-design.md` 末尾へ
- [ ] **Step 4: Commit** — `docs(superpowers): M3 完了時の申し送り（bridged 構成ゲート green）`

---

## Self-Review 済み事項（計画時の裁定メモ）

- **spec カバレッジ**: トポロジ=Task 4/6/8、設定=Task 8、台帳=Task 7/8、fabric scoping+容量=Task 3、ClusterRevision=Task 2、DataVersion=Task 2、ACL/NodeLabel 永続化=Task 9/10、小粒 deferred=Task 9(PASE 拒否)/10(dedup・Location・getrandom)、受け入れ条件=Task 8(単体トポロジ+台帳)/11(chip-tool)/12(mat describe)
- **ACL capacity enforcement** は spec の明示列挙外だが「触るファイルで自然に拾う」枠として Task 9 に含めた（AccessControlHandler::write を書き換えるため）
- **BDBI NodeLabel は read-only**（設計判断）: 設定ファイルが名前の正本。コントローラ write を許すと restart で巻き戻る
- **DataVersion は node 単位の共通乱数 base**（クラスタごとの独立乱数ではない）: 目的に十分で core 純度を保つ。spec 文言からの意図的簡略化として Task 2 の doc コメントに明記
