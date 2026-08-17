# AdministratorCommissioning（ECM）+ fabric 後始末 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Android HA アプリ（Google Play Services commissioner）の multi-admin 引き渡しを通す — OpenCommissioningWindow（ECM）で窓を再 open し、HA サーバが verifier ベース PASE で commission できるようにする。

**Architecture:** cluster 0x3C を `CommissioningServer`（core は同期・純粋）に追加し、窓オープンの副作用（PASE 設定切替・mDNS CM=2 広告・期限タイマー）は runtime が dispatch 後に stage された `WindowRequest` を回収して適用する（AddNOC の fabric 差分検知と同じ既存パターン）。PASE responder は passcode 由来と verifier 素材（97B）の 2 モード化。OC クラスタに UpdateFabricLabel / RemoveFabric を追加。

**Tech Stack:** Rust（tokio）、既存 crate: mat-controller（spake2p / im / tlv）+ mat-device（core / net）。

**Spec:** `docs/superpowers/specs/2026-08-18-admin-commissioning-design.md`

## Global Constraints

- spec 準拠先: Matter spec §11.19（AdministratorCommissioning）/ §11.17.6.11-12（UpdateFabricLabel / RemoveFabric）/ §4.13（PASE）
- OpenBasicCommissioningWindow・timed enforcement・窓の永続化はスコープ外（spec の「スコープ外」節）
- core（`crates/mat-device/src/core/`）は `cargo check -p mat-device --no-default-features` が通る純粋層のまま（tokio/socket 禁止）
- 各タスク完了時に `cargo test --workspace` green + `cargo clippy --workspace --all-targets` クリーン
- コミットは各タスクの末尾で行う（メッセージは各タスクの Step に記載）

## 事前確認済みの既存部品（再調査不要）

- `Spake2pVerifier::from_verifier_material(&[u8; 97])` — `crates/mat-controller/src/spake2p.rs:317`（実装・テスト済み）
- `FabricStore::remove(fabric_index: u8) -> Result<bool, String>` — `crates/mat-device/src/core/fabric_store.rs:169`
- `im::encode_invoke_response_status(endpoint, cluster, command, status, cluster_status: Option<u8>)` — `crates/mat-controller/src/im.rs:1798`（cluster_status 対応済み）
- `MdnsAdvertiser::remove_operational(compressed_fabric_id, node_id)` — `crates/mat-device/src/net/mdns.rs:206`
- `CLUSTER_ADMIN_COMMISSIONING: u32 = 0x003C` — `crates/mat-controller/src/commissioning.rs:56`（定数のみ既存）

---

### Task 1: InvokeReply にクラスタ固有ステータスを追加

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`InvokeReply` enum と `handle_invoke` の encode 分岐、~line 698）

**Interfaces:**
- Produces: `InvokeReply::ClusterStatus { status: u8, cluster_status: u8 }` — Task 3 の Busy/PAKEParameterError/WindowNotOpen が使う

- [ ] **Step 1: Write the failing test**（datamodel.rs の tests に追加。`handle_im_ok` / `Node::with_root_endpoint` は既存テストヘルパ）

```rust
/// クラスタ固有ステータス（spec §8.10.1 の cluster-status フィールド）を
/// 返せること。AdministratorCommissioning の Busy(2) 等が使う。
#[test]
fn invoke_reply_cluster_status_encodes_cluster_specific_code() {
    struct Failing;
    impl ClusterHandler for Failing {
        fn cluster_id(&self) -> u32 {
            0x9999_0003
        }
        fn attributes(&self) -> Vec<u32> {
            vec![]
        }
        fn read(&self, _attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
            None
        }
        fn invoke(&mut self, _command: u32, _fields: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 2, // Busy
            }
        }
    }
    let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
    node.add_cluster(0, Box::new(Failing));
    let req = im::encode_invoke_request(0, 0x9999_0003, 0, None);
    let (opcode, payload) = handle_im_ok(&mut node, im::OPCODE_INVOKE_REQUEST, &req);
    assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
    let out = decode_invoke_response(&payload).unwrap();
    assert_eq!(out.status, im::STATUS_FAILURE);
    assert_eq!(out.cluster_status, Some(2));
}
```

注: `im::STATUS_FAILURE` が未定義なら `crates/mat-controller/src/im.rs` の STATUS 定数群に `pub const STATUS_FAILURE: u8 = 0x01;` を追加する（spec §8.10 の FAILURE）。`decode_invoke_response` が `cluster_status` を返すことは `InvokeOutcome` 定義（im.rs:149-152）で確認済み。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --package mat-device invoke_reply_cluster_status`
Expected: FAIL（`ClusterStatus` variant が存在しないコンパイルエラー）

- [ ] **Step 3: Implement**

`InvokeReply` に variant を追加:

```rust
pub enum InvokeReply {
    Status(u8),
    /// spec §8.10.1: IM status + クラスタ固有ステータス（例:
    /// AdministratorCommissioning の Busy(2)/PAKEParameterError(3)/
    /// WindowNotOpen(4)）。status は通常 STATUS_FAILURE。
    ClusterStatus { status: u8, cluster_status: u8 },
    Data {
        response_command: u32,
        fields_tlv: Vec<u8>,
    },
}
```

`handle_invoke` の encode 分岐（既存 `InvokeReply::Status(status) =>` の隣）:

```rust
InvokeReply::ClusterStatus {
    status,
    cluster_status,
} => im::encode_invoke_response_status(
    req.endpoint,
    req.cluster,
    req.command,
    status,
    Some(cluster_status),
),
```

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → 全 green

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/datamodel.rs crates/mat-controller/src/im.rs
git commit -m "feat(mat-device): InvokeReply にクラスタ固有ステータス variant を追加"
```

---

### Task 2: PASE responder の ECM モード（verifier 素材ベース）

**Files:**
- Modify: `crates/mat-device/src/core/pase.rs`（`PaseVerifierConfig` の 2 モード化、`handle_pake1` ~line 204）

**Interfaces:**
- Consumes: `Spake2pVerifier::from_verifier_material(&[u8; 97]) -> Result<Spake2pVerifier, SpakeError>`（既存）
- Produces: `pub enum PaseSecret { Passcode(u32), VerifierMaterial([u8; 97]) }` と `PaseVerifierConfig { pub secret: PaseSecret, pub salt: Vec<u8>, pub iterations: u32, pub responder_session_id: u16 }` — Task 4 の runtime と既存呼び出し元（`net/runtime.rs:622` と `net/pase.rs`）が使う

- [ ] **Step 1: Write the failing test**（pase.rs の既存テスト群に追加。既存の「フルハンドシェイク成立」テスト（`pase_core_full_handshake` 相当）のセットアップを流用し、initiator 側は `mat_controller::pase` の initiator ヘルパ or 既存テストと同じ手組み）

```rust
/// ECM（OpenCommissioningWindow）: passcode ではなく verifier 素材
/// （w0‖L, 97B）で responder を構成してもハンドシェイクが成立する。
/// verifier は initiator と同じ passcode/salt/iterations から
/// `compute_verifier` で作る（実運用では OCW が 97B を直接渡してくる）。
#[test]
fn ecm_verifier_material_handshake_succeeds() {
    use mat_controller::spake2p::compute_verifier;
    let salt = [0x5A; 16];
    let iterations = 1000;
    let material = compute_verifier(31415926, &salt, iterations);
    let config = PaseVerifierConfig {
        secret: PaseSecret::VerifierMaterial(material),
        salt: salt.to_vec(),
        iterations,
        responder_session_id: 0xB0B2,
    };
    // 既存 full-handshake テストと同じ initiator 駆動（passcode 31415926）で
    // PBKDFParamRequest → ... → Pake3 まで進め、Established を assert する。
    // （既存テストのボイラープレートをこの config で呼ぶだけ）
}
```

（実装時は既存 full-handshake テストの本体をヘルパ関数 `drive_full_handshake(config, passcode) -> PaseOutput` に抽出して両テストから呼ぶ。既存テストの重複コピーはしない）

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --package mat-device ecm_verifier_material`
Expected: FAIL（`PaseSecret` 未定義のコンパイルエラー）

- [ ] **Step 3: Implement**

```rust
/// PASE の secret 供給源。`Passcode` は起動時窓（QR の passcode から
/// w0/w1 を導出）、`VerifierMaterial` は ECM 窓（OpenCommissioningWindow が
/// 渡す w0‖L をそのまま使う — こちらは w1 を知らない）。
#[derive(Clone)]
pub enum PaseSecret {
    Passcode(u32),
    VerifierMaterial([u8; 97]),
}

pub struct PaseVerifierConfig {
    pub secret: PaseSecret,
    pub salt: Vec<u8>,
    pub iterations: u32,
    pub responder_session_id: u16,
}
```

`handle_pake1` の verifier 構成を分岐:

```rust
let verifier = match &self.config.secret {
    PaseSecret::Passcode(passcode) => {
        Spake2pVerifier::from_passcode(*passcode, &self.config.salt, self.config.iterations)
    }
    PaseSecret::VerifierMaterial(material) => Spake2pVerifier::from_verifier_material(material)
        .map_err(PaseCoreError::Spake)?,
};
```

（`PaseCoreError` に Spake variant が無ければ既存のエラー変換に合わせる — `from_verifier_material` の `Err` は素材長 97 固定なので実質起きない。既存エラー型に `Spake(SpakeError)` が無い場合は `PaseCoreError::Crypto` 等既存の近い variant を使う。）

既存呼び出し元 2 箇所を `secret: PaseSecret::Passcode(config.passcode)` 形に更新（`net/runtime.rs:622` の構築と、`core/pase.rs` 内テスト）。

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → 全 green

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/pase.rs crates/mat-device/src/net/runtime.rs
git commit -m "feat(mat-device): PASE responder に ECM モード（verifier 素材ベース）を追加"
```

---

### Task 3: AdministratorCommissioning クラスタ（core）

**Files:**
- Modify: `crates/mat-device/src/core/commissioning.rs`（cluster 0x3C ハンドラ + `Inner` に窓状態 + `into_cluster_handlers` を 3 つ組に）
- Modify: `crates/mat-controller/src/commissioning.rs`（`decode_open_commissioning_window` / `encode_open_commissioning_window`（テスト用）。RevokeCommissioning はフィールド無しなので decoder 不要）
- Modify: `crates/mat-device/src/core/datamodel.rs`（`InvokeCtx` に `pub fabric_index: u8` を追加 — AdminFabricIndex の記録用）
- Modify: `crates/mat-device/src/net/runtime.rs`（`serve_secured_message` の `InvokeCtx` 構築に `fabric_index` を渡す。`into_cluster_handlers` 呼び出し元（`device.rs` の `add_cluster` 群）を 3 つ組に）

**Interfaces:**
- Consumes: `InvokeReply::ClusterStatus`（Task 1）
- Produces:
  - `pub struct WindowRequest { pub verifier: [u8; 97], pub discriminator: u16, pub iterations: u32, pub salt: Vec<u8>, pub timeout_s: u16 }`
  - `CommissioningServer::take_pending_window_request(&self) -> Option<WindowRequest>`（Task 4 の runtime が dispatch 後に回収）
  - `CommissioningServer::close_admin_window(&self)`（Task 4 が期限満了/CommissioningComplete 時に属性を閉状態へ戻すのに使う）
  - `into_cluster_handlers(&self) -> (Box<dyn ClusterHandler>, Box<dyn ClusterHandler>, Box<dyn ClusterHandler>)`（3 つ目が cluster 0x3C）
  - クラスタ定数: `mat_controller::commissioning::{CMD_OPEN_COMMISSIONING_WINDOW = 0, CMD_REVOKE_COMMISSIONING = 2}`、属性 `ATTR_AC_WINDOW_STATUS = 0 / ATTR_AC_ADMIN_FABRIC_INDEX = 1 / ATTR_AC_ADMIN_VENDOR_ID = 2`（commissioning.rs 内 const）

- [ ] **Step 1: Write the failing tests**（commissioning.rs tests。`test_server` / `drive_invoke` / `expect_data` は既存ヘルパ。`drive_invoke` は cluster 引数を取るので 0x3C を渡す）

```rust
/// OCW 成功: WindowStatus=1、Admin 属性が呼び出し元 fabric を反映、
/// WindowRequest が stage される。
#[test]
fn open_commissioning_window_stages_request_and_updates_attrs() {
    let mut server = commissioned_server(); // fabric_index=1 が入っている既存ヘルパ
    let material = [0x42u8; 97];
    let fields = encode_open_commissioning_window(300, &material, 0x0ABC, 1000, &[0x5A; 16]);
    // InvokeCtx に fabric_index=1 を積んで駆動する（drive_invoke を
    // ctx 指定版に拡張するか、テスト用に invoke_command を直接呼ぶ）
    let reply = server.invoke_command(
        CLUSTER_ADMIN_COMMISSIONING,
        CMD_OPEN_COMMISSIONING_WINDOW,
        &fields,
        &InvokeCtx { fabric_index: 1, ..test_ctx() },
    );
    assert_eq!(reply, InvokeReply::Status(im::STATUS_SUCCESS));
    let req = server.take_pending_window_request().expect("staged");
    assert_eq!(req.discriminator, 0x0ABC);
    assert_eq!(req.timeout_s, 300);
    assert_eq!(req.verifier, material);
    // 属性: WindowStatus=1(EnhancedWindowOpen), AdminFabricIndex=1,
    // AdminVendorId=登録済み fabric の admin_vendor_id(0xFFF1)
    let (_, _, ac) = server.into_cluster_handlers();
    let tlv = ac.read(ATTR_AC_WINDOW_STATUS, &ReadCtx::default()).unwrap();
    let mut r = Reader::new(&tlv);
    assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
}

/// 窓が既に開いていれば Busy(2)。
#[test]
fn open_commissioning_window_while_open_returns_busy() {
    let mut server = commissioned_server();
    let fields = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
    let ctx = InvokeCtx { fabric_index: 1, ..test_ctx() };
    server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW, &fields, &ctx);
    let reply = server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW, &fields, &ctx);
    assert_eq!(reply, InvokeReply::ClusterStatus { status: im::STATUS_FAILURE, cluster_status: 2 });
}

/// パラメータ検証: verifier 長 ≠97 / iterations 範囲外(1000..=100000) /
/// salt 長範囲外(16..=32) は PAKEParameterError(3)。timeout 範囲外
/// (180..=900) は INVALID_COMMAND。
#[test]
fn open_commissioning_window_rejects_bad_parameters() {
    let mut server = commissioned_server();
    let ctx = InvokeCtx { fabric_index: 1, ..test_ctx() };
    let bad_iter = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 999, &[0x5A; 16]);
    assert_eq!(
        server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW, &bad_iter, &ctx),
        InvokeReply::ClusterStatus { status: im::STATUS_FAILURE, cluster_status: 3 }
    );
    let bad_salt = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 8]);
    assert_eq!(
        server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW, &bad_salt, &ctx),
        InvokeReply::ClusterStatus { status: im::STATUS_FAILURE, cluster_status: 3 }
    );
    let bad_timeout = encode_open_commissioning_window(60, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
    assert_eq!(
        server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW, &bad_timeout, &ctx),
        InvokeReply::Status(im::STATUS_INVALID_COMMAND)
    );
}

/// Revoke: 開いていれば閉じ、閉じていれば WindowNotOpen(4)。
#[test]
fn revoke_commissioning_closes_or_rejects() {
    let mut server = commissioned_server();
    let ctx = InvokeCtx { fabric_index: 1, ..test_ctx() };
    assert_eq!(
        server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_REVOKE_COMMISSIONING, &[], &ctx),
        InvokeReply::ClusterStatus { status: im::STATUS_FAILURE, cluster_status: 4 }
    );
    let fields = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
    server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW, &fields, &ctx);
    assert_eq!(
        server.invoke_command(CLUSTER_ADMIN_COMMISSIONING, CMD_REVOKE_COMMISSIONING, &[], &ctx),
        InvokeReply::Status(im::STATUS_SUCCESS)
    );
    // 閉じた後の属性は WindowStatus=0 / Admin* は null
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --package mat-device open_commissioning_window`
Expected: FAIL（コンパイルエラー: 定数・メソッド未定義）

- [ ] **Step 3: Implement**

mat-controller 側 codec（`crates/mat-controller/src/commissioning.rs`、`decode_add_noc` と同じ `scan_struct_fields`/`take_bytes`/`take_u64` 流儀）:

```rust
pub const CMD_OPEN_COMMISSIONING_WINDOW: u32 = 0x00;
pub const CMD_REVOKE_COMMISSIONING: u32 = 0x02;

/// OpenCommissioningWindow（spec §11.19.8.1）:
/// `{0: CommissioningTimeout(u16), 1: PAKEPasscodeVerifier(97B),
///   2: Discriminator, 3: Iterations(u32), 4: Salt}`
pub fn decode_open_commissioning_window(
    fields: &[u8],
) -> Result<(u16, Vec<u8>, u16, u32, Vec<u8>), CommissionError> { /* scan_struct_fields で 0..4 を取る */ }

pub fn encode_open_commissioning_window(
    timeout_s: u16,
    verifier: &[u8],
    discriminator: u16,
    iterations: u32,
    salt: &[u8],
) -> Vec<u8> { /* Writer で同 struct を組む（テスト・将来の mat CLI 用） */ }
```

mat-device 側（commissioning.rs）:

- `Inner` に追加: `admin_window: Option<AdminWindow>`（`struct AdminWindow { fabric_index: u8, vendor_id: u16 }` — 属性応答用）と `pending_window_request: Option<WindowRequest>`
- `handle_open_commissioning_window(&mut self, fields_tlv, ctx)`: decode → 検証（timeout 180..=900 → INVALID_COMMAND / verifier len==97・iterations 1000..=100000・salt len 16..=32 → ClusterStatus(3)）→ `admin_window` が Some なら ClusterStatus(2) → 成功: `admin_window = Some(..)`（vendor_id は `ctx.fabric_index` に対応する fabric の `admin_vendor_id`、見つからなければ 0）、`pending_window_request = Some(..)`、`InvokeReply::Status(SUCCESS)`
- `handle_revoke_commissioning`: `admin_window` が None なら ClusterStatus(4)、Some なら None にして SUCCESS（runtime 側の実閉窓は Task 4 — revoke も `take_pending_close` 的な仕掛けは持たず、runtime が invoke 応答後に `admin_window_is_open()` を読んで同期する方式にする。下記 Produces 参照）
- 属性 read（3 つ目のハンドラ）: `WindowStatus`（0 or 1 の Uint）、`AdminFabricIndex` / `AdminVendorId`（open 中は Uint、閉窓中は TLV Null — `Writer::put_null` が無ければ tlv.rs に追加）
- `into_cluster_handlers` を 3 つ組に変更し、`device.rs` の登録箇所と既存テスト（`(gc, oc) =` の分割束縛 4 箇所程度）を追随
- `CommissioningServer` に `take_pending_window_request()` / `admin_window_is_open() -> bool` / `close_admin_window()`（期限満了・CommissioningComplete 用: `admin_window = None`）を追加
- `InvokeCtx` に `pub fabric_index: u8` 追加（`Default` は 0）。runtime の `serve_secured_message` の `InvokeCtx` 構築（~line 984）に `fabric_index` を渡す

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → 全 green（`into_cluster_handlers` の呼び出し元の追随漏れはここで露見する）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/commissioning.rs crates/mat-device/src/core/datamodel.rs crates/mat-controller/src/commissioning.rs crates/mat-device/src/net/runtime.rs crates/mat-device/src/net/device.rs
git commit -m "feat(mat-device): AdministratorCommissioning クラスタ（OCW/Revoke/属性、core 層）"
```

---

### Task 4: runtime の ECM 窓 — 状態機・mDNS CM=2・PASE 切替

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`CommissioningWindow` enum ~line 216、PASE 分岐 ~line 616、dispatch 後の window request 回収、期限 branch ~line 792、CommissioningComplete close ~line 1056）
- Modify: `crates/mat-device/src/core/mdns_records.rs`（`CommissionableAdvert` に `pub cm: u8` を追加、`commissionable_txt` の `CM=1` 固定を `format!("CM={}", ad.cm)` に）

**Interfaces:**
- Consumes: `take_pending_window_request()` / `admin_window_is_open()` / `close_admin_window()`（Task 3）、`PaseSecret`（Task 2）
- Produces: `CommissioningWindow::EnhancedOpen { until: Instant, request: WindowRequest }` variant（runtime 内部）

- [ ] **Step 1: Write the failing test**（runtime.rs tests。`a_timed_invoke_on_the_same_exchange_is_served_not_dropped` と同じ生 UDP ハーネス流儀で、ただし窓・mDNS はプロセス統合が要るので、ここは**分解して 2 本**にする）

```rust
/// dispatch 後に WindowRequest が stage されていれば、runtime の窓が
/// EnhancedOpen になり ECM 用 PASE 設定が得られる（純粋ロジック部分の
/// 単体テスト — apply_window_request を関数に切り出してテストする）。
#[test]
fn apply_window_request_transitions_to_enhanced_open() {
    let req = WindowRequest {
        verifier: [0x42; 97],
        discriminator: 0x0ABC,
        iterations: 1000,
        salt: vec![0x5A; 16],
        timeout_s: 300,
    };
    let window = apply_window_request(req.clone());
    let CommissioningWindow::EnhancedOpen { until, request } = window else {
        panic!("expected EnhancedOpen");
    };
    assert_eq!(request.discriminator, 0x0ABC);
    assert!(until > Instant::now());
    // ECM 中の PASE 設定が verifier 素材になること
    let config = pase_config_for_window(&window, /*boot passcode*/ 20202021, /*boot salt*/ &[0u8; 32], 0x1234);
    assert!(matches!(config.secret, PaseSecret::VerifierMaterial(m) if m == [0x42; 97]));
    assert_eq!(config.iterations, 1000);
    assert_eq!(config.salt, vec![0x5A; 16]);
}

/// 窓 variant ごとの mDNS 広告パラメータ（CM 値と discriminator）。
#[test]
fn commissionable_advert_params_reflect_window_kind() {
    let boot = CommissioningWindow::Open { until: Instant::now() + Duration::from_secs(60) };
    assert_eq!(advert_params_for_window(&boot, 3210), Some((3210, 1)));
    let req = WindowRequest { verifier: [0x42; 97], discriminator: 0x0ABC, iterations: 1000, salt: vec![0x5A; 16], timeout_s: 300 };
    let ecm = CommissioningWindow::EnhancedOpen { until: Instant::now() + Duration::from_secs(300), request: req };
    assert_eq!(advert_params_for_window(&ecm, 3210), Some((0x0ABC, 2)));
    assert_eq!(advert_params_for_window(&CommissioningWindow::Closed, 3210), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --package mat-device apply_window_request commissionable_advert_params`
Expected: FAIL（関数・variant 未定義）

- [ ] **Step 3: Implement**

- `CommissioningWindow` に `EnhancedOpen { until: Instant, request: WindowRequest }` を追加。`is_open()` は EnhancedOpen でも true。`commissioning_window_deadline` は両 Open variant の `until` を見る
- 切り出しヘルパ（テスト対象）:

```rust
/// stage された WindowRequest から ECM 窓状態を作る（spec §11.19.8.1 の
/// CommissioningTimeout をそのまま期限にする）。
fn apply_window_request(request: WindowRequest) -> CommissioningWindow {
    let until = Instant::now() + Duration::from_secs(u64::from(request.timeout_s));
    CommissioningWindow::EnhancedOpen { until, request }
}

/// 現在の窓に対応する PASE 設定。boot 窓は passcode、ECM 窓は verifier 素材。
fn pase_config_for_window(
    window: &CommissioningWindow,
    boot_passcode: u32,
    boot_salt: &[u8],
    responder_session_id: u16,
) -> PaseVerifierConfig { /* 上のテストのとおり */ }

/// 現在の窓に対応する commissionable 広告の (discriminator, CM)。Closed は None。
fn advert_params_for_window(window: &CommissioningWindow, boot_discriminator: u16) -> Option<(u16, u8)>
```

- runtime 配線:
  - `serve_secured_message` の dispatch 後（AddNOC 検知と同じ場所）に `comm_server.take_pending_window_request()` をチェック。Some なら: `**window = apply_window_request(req)` + mDNS を `advert_params_for_window` の値で `set_commissionable(Some(..))`（`CommissionableAdvert` の `cm`/`discriminator` に反映、`instance` は新規 `random_hex_name()`）
  - RevokeCommissioning の反映: dispatch 後、`window` が EnhancedOpen なのに `!comm_server.admin_window_is_open()` なら close（goodbye + `Closed`）
  - 期限満了 branch（~line 792）: EnhancedOpen でも同じ close 処理 + `comm_server.close_admin_window()`（属性を閉状態に戻す）
  - CommissioningComplete 成功時の close（~line 1056）にも `comm_server.close_admin_window()` を追加
  - PASE 受付（~line 616）: `PaseVerifierConfig` の構築を `pase_config_for_window(&window, config.passcode, pase_salt, local_session_id)` に置換
- `CommissionableAdvert` に `pub cm: u8` を追加し、既存の構築 2 箇所（`bring_up_mdns` と mdns_records のテスト）は `cm: 1` を渡す。`commissionable_txt` の期待値テスト（`CM=1` 固定のもの）を `cm` 反映で更新

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → 全 green

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/net/runtime.rs crates/mat-device/src/core/mdns_records.rs
git commit -m "feat(mat-device): ECM 窓の runtime 配線（動的再 open・CM=2 広告・PASE 切替）"
```

---

### Task 5: UpdateFabricLabel（OC 62/9）

**Files:**
- Modify: `crates/mat-device/src/core/fabric_store.rs`（`FabricEntry` に `label`）
- Modify: `crates/mat-device/src/core/commissioning.rs`（コマンドハンドラ + `encode_fabrics` ~line 444 への反映）
- Modify: `crates/mat-controller/src/commissioning.rs`（`CMD_UPDATE_FABRIC_LABEL = 0x09` + `decode_update_fabric_label` / `encode_update_fabric_label`）

**Interfaces:**
- Produces: `FabricEntry.label: String`（serde default で後方互換）— Task 6 と runtime は既存フィールドのみ使うため影響なし

- [ ] **Step 1: Write the failing tests**

```rust
/// UpdateFabricLabel: NOCResponse(Ok) を返し、store に永続化され、
/// Fabrics 属性の読みに Label が反映される。
#[test]
fn update_fabric_label_persists_and_reflects_in_fabrics_attr() {
    let mut server = commissioned_server();
    let fields = encode_update_fabric_label("Alexa-1");
    let ctx = InvokeCtx { fabric_index: 1, ..test_ctx() };
    let (_, resp) = expect_data(server.invoke_command(
        CLUSTER_OPERATIONAL_CREDENTIALS, CMD_UPDATE_FABRIC_LABEL, &fields, &ctx,
    ));
    let (status, fabric_index) = decode_noc_response(&resp).unwrap();
    assert_eq!(status, 0);
    assert_eq!(fabric_index, Some(1));
    assert_eq!(server.fabrics()[0].label, "Alexa-1");
    // Fabrics 属性（ATTR_OC_FABRICS）の TLV に "Alexa-1" の utf8 文字列が
    // Label フィールド（context tag 5, spec §11.17.5.20 FabricDescriptorStruct）
    // として現れることを Reader で assert
}

/// 対象は「呼び出しセッションの fabric」（spec: fabric-scoped コマンド）。
/// ctx.fabric_index の fabric が存在しなければ InvalidFabricIndex(0x0A)。
#[test]
fn update_fabric_label_unknown_fabric_returns_invalid_fabric_index() {
    let mut server = test_server(); // fabric なし
    let fields = encode_update_fabric_label("x");
    let ctx = InvokeCtx { fabric_index: 7, ..test_ctx() };
    let (_, resp) = expect_data(server.invoke_command(
        CLUSTER_OPERATIONAL_CREDENTIALS, CMD_UPDATE_FABRIC_LABEL, &fields, &ctx,
    ));
    let (status, _) = decode_noc_response(&resp).unwrap();
    assert_eq!(status, 0x0A);
}

/// 後方互換: label キーの無い既存 fabrics.json が空文字 label で読めること
/// （fabric_store.rs の既存 persist round-trip テスト群に、label 欠落 JSON
/// からの deserialize テストを 1 本追加）。
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --package mat-device update_fabric_label`
Expected: FAIL（コンパイルエラー）

- [ ] **Step 3: Implement**

- `FabricEntry` に `#[serde(default)] pub label: String` を追加（既存構築箇所は `label: String::new()`）
- codec: `decode_update_fabric_label(fields) -> Result<String, CommissionError>`（`{0: Label(utf8, max 32)}`）+ encode（テスト用）。Label 長 >32 は INVALID_COMMAND
- ハンドラ: `ctx.fabric_index` の fabric を探し、無ければ `noc_status(0x0A)`、あれば label 更新 + persist（store の insert/persist 既存経路に `update_label(fabric_index, label)` を追加）+ 成功応答 `InvokeReply::Data { response_command: RESP_NOC, fields_tlv: encode_noc_response(0, Some(fabric_index)) }`
- `encode_fabrics()`（Fabrics 属性）の FabricDescriptorStruct に Label（context tag 5）を出力

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → 全 green

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/fabric_store.rs crates/mat-device/src/core/commissioning.rs crates/mat-controller/src/commissioning.rs
git commit -m "feat(mat-device): UpdateFabricLabel（label 永続化 + Fabrics 属性反映）"
```

---

### Task 6: RemoveFabric（OC 62/10）

**Files:**
- Modify: `crates/mat-device/src/core/commissioning.rs`（ハンドラ + `take_removed_fabric()`）
- Modify: `crates/mat-controller/src/commissioning.rs`（`CMD_REMOVE_FABRIC = 0x0A` は既存 `encode_remove_fabric`（line ~161）の対になる `decode_remove_fabric` を追加）
- Modify: `crates/mat-device/src/net/runtime.rs`（dispatch 後に removed fabric を回収 → mDNS `remove_operational` + 自セッション fabric なら応答送信後にセッション破棄）

**Interfaces:**
- Consumes: `FabricStore::remove(fabric_index) -> Result<bool, String>`（既存）、`MdnsAdvertiser::remove_operational(compressed_fabric_id, node_id)`（既存）
- Produces: `CommissioningServer::take_removed_fabric(&self) -> Option<FabricEntry>`（runtime が mDNS 撤去とセッション判定に使う）

- [ ] **Step 1: Write the failing tests**

```rust
/// RemoveFabric: NOCResponse(Ok) + store から消え、removed が stage される。
#[test]
fn remove_fabric_removes_and_stages_entry() {
    let mut server = commissioned_server();
    let ctx = InvokeCtx { fabric_index: 1, ..test_ctx() };
    let (_, resp) = expect_data(server.invoke_command(
        CLUSTER_OPERATIONAL_CREDENTIALS, CMD_REMOVE_FABRIC, &encode_remove_fabric(1), &ctx,
    ));
    let (status, fabric_index) = decode_noc_response(&resp).unwrap();
    assert_eq!(status, 0);
    assert_eq!(fabric_index, Some(1));
    assert!(server.fabrics().is_empty());
    assert_eq!(server.take_removed_fabric().map(|e| e.fabric_index), Some(1));
}

/// 存在しない index は InvalidFabricIndex(0x0A)。
#[test]
fn remove_fabric_unknown_index_returns_invalid_fabric_index() {
    let mut server = commissioned_server();
    let ctx = InvokeCtx { fabric_index: 1, ..test_ctx() };
    let (_, resp) = expect_data(server.invoke_command(
        CLUSTER_OPERATIONAL_CREDENTIALS, CMD_REMOVE_FABRIC, &encode_remove_fabric(9), &ctx,
    ));
    let (status, _) = decode_noc_response(&resp).unwrap();
    assert_eq!(status, 0x0A);
    assert_eq!(server.fabrics().len(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --package mat-device remove_fabric`
Expected: FAIL（コンパイルエラー）

- [ ] **Step 3: Implement**

- codec: `decode_remove_fabric(fields) -> Result<u8, CommissionError>`（`{0: FabricIndex}`）
- ハンドラ: `store.remove(idx)` が Ok(false)（不在）なら `noc_status(0x0A)`、Ok(true) なら削除済み entry（remove 前に clone）を `removed_fabric: Option<FabricEntry>` に stage して成功応答 `InvokeReply::Data { response_command: RESP_NOC, fields_tlv: encode_noc_response(0, Some(idx)) }`。persist は `remove` の既存実装が行う（fabric_store.rs:169 — persist 経路を確認し、無ければ追加）
- runtime（`serve_secured_message` の dispatch 後）: `take_removed_fabric()` が Some なら (a) `ctx.mdns.remove_operational(compressed_fabric_id(&entry.root_public_key... /* operational_advert と同じ導出 */), entry.node_id)`、(b) 削除された fabric_index が現行セッションの fabric_index と一致するなら、**応答送信後に** そのループ iteration を最後にセッションを終了する（`serve_secured_message` から「セッションを閉じるべき」を戻り値 bool で返し、呼び出し元 `run` が `current_session = None` にする — 既存の戻り値 `()` を `enum ServeOutcome { Continue, DropSession }` に変える最小変更）

- [ ] **Step 4: Run tests**

Run: `cargo test --workspace` → 全 green

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/commissioning.rs crates/mat-controller/src/commissioning.rs crates/mat-device/src/net/runtime.rs
git commit -m "feat(mat-device): RemoveFabric（store/mDNS 撤去 + 自セッション削除時のセッション終了）"
```

---

### Task 7: 統合検証（自動ゲート + 実機）

**Files:**
- Modify: `docs/superpowers/specs/2026-08-18-admin-commissioning-design.md`（完了時の申し送り節を追記）

**Interfaces:**
- Consumes: Task 1-6 の全成果

- [ ] **Step 1: 自動リグレッション 3 系統**

```bash
cargo test --workspace          # 全 green
cargo clippy --workspace --all-targets   # クリーン
task e2e:device:m2-chip         # chip-tool ゲート（commission/OnOff/再起動/Subscribe）
```

- [ ] **Step 2: matter-server WS ゲート（ECM 検証込み）**

jarvis に `task dist:arm64` → `~/.local/bin/matv` 配布 → `~/matv-ha/` を新 store で起動。
jarvis の `~/ms-test`（matter-server 1.1.7 導入済み）を `--enable-test-net-dcl` で起動し、
WS `commission_on_network`（setup_pin_code=63852174, ip_addr=192.168.1.190）→
`COMMISSION_OK` と OnOff toggle 往復を確認（本セッションで使った
`jarvis_commission.py` / `toggle_test.py` の手順）。

- [ ] **Step 3: スマホ E2E（人間チェックポイント）**

matv を新 store で再起動（窓 15 分）→ ユーザーに QR（ASCII 描画）+ manual code を渡し、
Android HA アプリから追加してもらう。期待: スマホの一時 fabric → OCW → HA サーバが
ECM PASE で commission → HA にデバイス出現 → スマホ fabric が RemoveFabric で消える
（`fabrics.json` に HA の 1 fabric のみ残る）。失敗時は `matv.stderr.log`（RUST_LOG=debug）
と pcap で切り分け。

- [ ] **Step 4: 申し送りの記録とコミット**

結果（通過・発見事項・deferred）を spec の「完了時の申し送り」節に追記して commit:

```bash
git add docs/superpowers/specs/2026-08-18-admin-commissioning-design.md
git commit -m "docs(superpowers): AdministratorCommissioning 完了時の申し送り"
```
