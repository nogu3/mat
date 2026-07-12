# Phase 5 M4: matd native adapter 差し替え Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd の常駐プロセス内で mat-controller の warm CASE セッションを保持し、ホットパス（on/off・色・色温度・onoff read）を native 経路で処理する。未対応 op は既存 chip-tool ws にフォールバックする。

**Architecture:** `SecureSession` の借用ライフタイムを `Arc<UdpTransport>` 保持へ変え、warm session を長期保持可能にする（enabling refactor）。新 `matd::native::NativeBackend` が per-node に warm session をキャッシュ（ノード間並行・同一ノード直列、無期限保持・送信失敗時のみ再解決+再CASE）。`server::run_op` を native/chip-tool の分岐点にし、`ChipToolBackend` は lazy spawn 化する。CASE 確立と IM 送信は `async-trait` の `Establisher`/`NodeConn` seam の背後に置き、warm 再利用と失敗時再確立を実 CASE なしで CI テストする。

**Tech Stack:** Rust 2021, tokio, mat-controller（自作 Matter コントローラ）, async-trait, serde_json。

## Global Constraints

- ブランチは長期ブランチ `matter-controller`（main マージ禁止）。
- 公開 repo: 認証情報・実 IP・実 node_id・実証明書をコミットしない。テストのダミーは
  `fd00::`/`fe80::` と chip SDK フィクスチャのみ。
- chip-tool KVS フォーマットは v1.4.2.0 固定。
- stdout（socket 応答）は純粋な構造化 JSON のみ（mat スキーマ + `timestamp`）。人間装飾禁止。
- 診断は stderr へ構造化ログ（`tracing`）。
- `mat` の設計ルール（プロトコル直喋り禁止）はプロトコルを `mat-controller` crate に
  隔離することで守る。matd はそれを呼ぶだけ。
- group（groupcast 送信）は M4 では完全に chip-tool のまま（native 化は M5）。
- 各コミット前に `task check`（fmt:check + clippy -D warnings + test）を通す。
- エラー `kind` と exit code は経路によらず一致（README の表）: `timeout`=3 /
  `device_rejected`=4 / `unreachable`=5 / `session_failed`=6 / `other`=1。

---

## File Structure

- `crates/mat-controller/src/session.rs` — 決定 1: `SecureSession` を `Arc<UdpTransport>` 保持へ。
- `crates/mat-controller/src/case.rs` — 決定 1: `establish` が `Arc<UdpTransport>` を取る。
- `crates/mat-controller/src/im.rs` — 決定 5: move-to-color-temperature 定数 + エンコーダ。
- `crates/matd/Cargo.toml` + ルート `Cargo.toml` — mat-controller / async-trait を依存追加。
- `crates/matd/src/native.rs`（新規）— `NativeBackend`、`Establisher`/`NodeConn` seam、
  実 CASE establisher、per-node warm session 管理、エラー写像。
- `crates/matd/src/server.rs` — 決定 3: `run_op` の native/chip-tool 分岐、応答 body の共用。
- `crates/matd/src/backend.rs` — 決定 4: lazy spawn 化。
- `crates/matd/src/main.rs` + `lib.rs` — CLI フラグ（iface/fabric-index/issuer-index）、
  両 backend の配線、起動ログ。
- `crates/matd/tests/native_dispatch.rs`（新規）— routing + warm 状態機械の統合テスト。
- `crates/mat-controller/tests/live_matd_native.rs`（新規）+ `scripts/e2e-m4.sh` + `Taskfile.yml`
  — jarvis 実機受け入れ。

---

## Task 1: mat-controller — `SecureSession` の `Arc<UdpTransport>` 化（ライフタイム除去）

**Files:**
- Modify: `crates/mat-controller/src/session.rs`
- Modify: `crates/mat-controller/src/case.rs`
- Modify: `crates/mat-controller/tests/case_self_handshake.rs`
- Modify: `crates/mat-controller/tests/live_case_im.rs`
- Modify: `crates/mat-controller/tests/live_jarvis.rs`

**Interfaces:**
- Produces:
  - `SecureSession`（ライフタイムパラメータ無し）。`SecureSession::new(transport:
    Arc<UdpTransport>, peer: SocketAddr, local_session_id: u16, peer_session_id: u16,
    keys: SessionKeys, local_node_id: u64, peer_node_id: u64) -> Self`。
  - `case::establish(transport: Arc<UdpTransport>, peer: SocketAddr, creds:
    &FabricCredentials, peer_node_id: u64, cfg: &MrpConfig) -> Result<SecureSession,
    CaseError>`。
  - `SecureSession::read_attribute` / `invoke` / `recv` のシグネチャは不変（`&mut self`）。

このタスクはワイヤ挙動を変えない借用形態のみの変更。`case_self_handshake` の
ループバックテストが回帰ガードになる。

- [ ] **Step 1: 既存ループバックテストを Arc へ更新（まだ変更しない実装に対して失敗させる）**

`crates/mat-controller/tests/case_self_handshake.rs` の initiator 側呼び出しを更新する。
現状（341〜363 行目付近）:

```rust
        let initiator_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        // ...
        let mut session = case::establish(
            &initiator_transport,
            responder_addr,
            &creds,
            RESPONDER_NODE_ID,
            &cfg,
        )
        .await
        .expect("CASE establish should succeed over loopback");
```

を次に変える（`use std::sync::Arc;` をファイル冒頭に追加）:

```rust
        let initiator_transport = Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        );
        // ...
        let mut session = case::establish(
            Arc::clone(&initiator_transport),
            responder_addr,
            &creds,
            RESPONDER_NODE_ID,
            &cfg,
        )
        .await
        .expect("CASE establish should succeed over loopback");
```

- [ ] **Step 2: テストを走らせてコンパイルエラー（型不一致）で落ちるのを確認**

Run: `cargo test -p mat-controller --test case_self_handshake 2>&1 | tail -20`
Expected: FAIL（コンパイルエラー。`establish` はまだ `&UdpTransport` を取るため
`Arc<UdpTransport>` を渡すと型不一致）。

- [ ] **Step 3: `session.rs` の `SecureSession` を `Arc<UdpTransport>` 保持へ**

`crates/mat-controller/src/session.rs`:

冒頭の import に `use std::sync::Arc;` を追加。`transport` フィールドとライフタイムを外す:

```rust
pub struct SecureSession {
    transport: Arc<UdpTransport>,
    peer: SocketAddr,
    local_session_id: u16,
    peer_session_id: u16,
    keys: SessionKeys,
    local_node_id: u64,
    peer_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
}

impl SecureSession {
    pub fn new(
        transport: Arc<UdpTransport>,
        peer: SocketAddr,
        local_session_id: u16,
        peer_session_id: u16,
        keys: SessionKeys,
        local_node_id: u64,
        peer_node_id: u64,
    ) -> Self {
        Self {
            transport,
            peer,
            local_session_id,
            peer_session_id,
            keys,
            local_node_id,
            peer_node_id,
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
        }
    }
```

`impl<'t> SecureSession<'t>` を `impl SecureSession` に、`&mut self` メソッド内の
`self.transport.send_to(...)` / `self.transport.recv_from(...)` は `Arc` の Deref で
そのまま動く（変更不要）。他に `SecureSession<'` や `<'t>` が残っていれば全て除去する
（`grep -n "SecureSession<" crates/mat-controller/src/session.rs` で確認）。

- [ ] **Step 4: `case.rs` の `establish` を `Arc<UdpTransport>` へ**

`crates/mat-controller/src/case.rs`:

冒頭に `use std::sync::Arc;`。シグネチャと内部を変更:

```rust
pub async fn establish(
    transport: Arc<UdpTransport>,
    peer: SocketAddr,
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
) -> Result<SecureSession, CaseError> {
```

内部で `UnsecuredExchange::new(transport, peer)` に **借用**を渡している箇所は
`UnsecuredExchange::new(&transport, peer)` にする（`UnsecuredExchange<'t>` は
借用のまま据え置き、`exchange.rs` は無変更）。最後に `SecureSession::new(...)` を
構築して返す箇所へ `transport` を **move** で渡す:

```rust
    Ok(SecureSession::new(
        transport,
        peer,
        local_session_id,
        peer_session_id,
        keys,
        creds.node_id,
        peer_node_id,
    ))
```

（`SecureSession::new` の実引数名は既存コードに合わせる。`transport` は関数末尾で
1 回だけ move されるので、途中の `UnsecuredExchange::new(&transport, ...)` 借用と
競合しない。`UdpTransport` の戻り値 `local_session_id`/`peer_session_id`/`keys` は
既存の establish 本体が算出済みの変数をそのまま使う。）

- [ ] **Step 5: 残りのライブテストの呼び出しを Arc へ更新**

`crates/mat-controller/tests/live_case_im.rs`（48〜50 行目付近）:

```rust
    let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
    let mut session = case::establish(std::sync::Arc::clone(&transport), peer, &creds, device_node_id, &cfg)
        .await
        .expect("CASE establishment");
```

`crates/mat-controller/tests/live_jarvis.rs`（118〜121 行目付近）:

```rust
    let transport = std::sync::Arc::new(UdpTransport::bind().await.unwrap());
    let mut session = None;
    for peer in &peers {
        match case::establish(std::sync::Arc::clone(&transport), *peer, &creds, device_node_id, &mrp).await {
```

`SecureSession<'_>` を引数に取るヘルパ（`live_jarvis.rs` の `read_bool` /
`read_color_u8`、42・53 行目付近）のシグネチャから `<'_>` を外す:

```rust
async fn read_bool(s: &mut SecureSession, ep: u16, cfg: &MrpConfig) -> Result<bool, String> {
```

`exercise`（170 行目付近）の `session: &mut SecureSession<'_>` も `&mut SecureSession` へ。

- [ ] **Step 6: ループバックテストが通ることを確認（回帰ガード）**

Run: `cargo test -p mat-controller --test case_self_handshake 2>&1 | tail -10`
Expected: PASS（`case_establishes_and_reads_over_loopback` が緑。ワイヤ挙動不変を実証）。

- [ ] **Step 7: crate 全体の check**

Run: `cd crates/mat-controller && cargo test --lib 2>&1 | tail -5 && cargo clippy --all-targets -- -D warnings 2>&1 | tail -5`
Expected: 全 PASS、clippy 警告なし（ライブテストは `#[ignore]` でコンパイルのみ確認）。

- [ ] **Step 8: Commit**

```bash
git add crates/mat-controller/src/session.rs crates/mat-controller/src/case.rs \
  crates/mat-controller/tests/case_self_handshake.rs \
  crates/mat-controller/tests/live_case_im.rs crates/mat-controller/tests/live_jarvis.rs
git commit -m "refactor(mat-controller): SecureSession holds Arc<UdpTransport> (M4 enabling)

Remove the borrowed lifetime so warm sessions can be held long-term in a
HashMap across tokio::spawn boundaries (matd native backend). Wire
behavior unchanged; case_self_handshake loopback test is the regression
guard. UnsecuredExchange stays borrowing (transient inside establish).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 2: mat-controller — move-to-color-temperature 定数 + エンコーダ

**Files:**
- Modify: `crates/mat-controller/src/im.rs`

**Interfaces:**
- Consumes: `Writer` / `Tag`（既存 tlv、`encode_move_to_hue_and_saturation_fields` と同型）。
- Produces:
  - `pub const CMD_MOVE_TO_COLOR_TEMPERATURE: u32 = 0x0A;`
  - `pub const ATTR_COLOR_TEMPERATURE_MIREDS: u32 = 0x0007;`（read 用、E2E の照合に使う）
  - `pub fn encode_move_to_color_temperature_fields(mireds: u16, transition_time_ds: u16) -> Vec<u8>`

- [ ] **Step 1: 失敗するユニットテストを書く**

`crates/mat-controller/src/im.rs` の `#[cfg(test)] mod tests` に追加:

```rust
    #[test]
    fn move_to_color_temperature_fields_match_wire_shape() {
        // CommandFields (colorcontrol MoveToColorTemperature, cluster §3.2.11.10):
        // {0: ColorTemperatureMireds(u16), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToHueAndSaturation エンコーダと同じ手筋（anonymous struct + context tags）。
        let bytes = encode_move_to_color_temperature_fields(370, 30);
        // anonymous struct open (0x15) ... context-tagged uints ... close (0x18)
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // mireds=370=0x0172 が context tag 0 の u16 として載る（0x25 = ctx-tag u16）
        assert!(
            bytes.windows(4).any(|w| w == [0x25, 0x00, 0x72, 0x01]),
            "mireds 370 as ctx-tag-0 u16 little-endian, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u16 として載る
        assert!(
            bytes.windows(4).any(|w| w == [0x25, 0x01, 0x1E, 0x00]),
            "transition 30 as ctx-tag-1 u16 little-endian, got {bytes:02X?}"
        );
    }
```

- [ ] **Step 2: テストが未定義関数で落ちるのを確認**

Run: `cargo test -p mat-controller --lib move_to_color_temperature_fields 2>&1 | tail -10`
Expected: FAIL（`encode_move_to_color_temperature_fields` not found）。

- [ ] **Step 3: 定数とエンコーダを実装**

`crates/mat-controller/src/im.rs`、既存の colorcontrol 定数群
（`CMD_MOVE_TO_HUE_AND_SATURATION` 付近）に追加:

```rust
pub const ATTR_COLOR_TEMPERATURE_MIREDS: u32 = 0x0007;
pub const CMD_MOVE_TO_COLOR_TEMPERATURE: u32 = 0x0A;
```

`encode_move_to_hue_and_saturation_fields` の直後に追加:

```rust
/// CommandFields for colorcontrol MoveToColorTemperature (cluster spec
/// §3.2.11.10): `{0: ColorTemperatureMireds(u16), 1: TransitionTime(u16,
/// 0.1 s units), 2: OptionsMask(u8), 3: OptionsOverride(u8)}`. Options are
/// fixed to 0 (execute per the device's Options attribute), matching what
/// chip-tool sends by default.
pub fn encode_move_to_color_temperature_fields(mireds: u16, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(mireds));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}
```

- [ ] **Step 4: テストが通るのを確認**

Run: `cargo test -p mat-controller --lib move_to_color_temperature_fields 2>&1 | tail -10`
Expected: PASS。

念のため `put_uint` が 370 を u16（0x25）で符号化するか確認（tlv のミニ幅選択）。
もし 0x25 でなく別幅なら、テストの期待バイト列をエンコーダ実出力に合わせて 1 度修正し、
「context tag 0 に mireds、tag 1 に transition が載る」ことを担保する形へ（幅はエンコーダ
仕様に従う）。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/im.rs
git commit -m "feat(mat-controller): colorcontrol MoveToColorTemperature encoder

Constant (cmd 0x0A) + ColorTemperatureMireds attr (0x0007) + fielded
encoder, mirroring MoveToHueAndSaturation. Needed for matd native
color-temp hotpath (M4).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 3: matd — mat-controller / async-trait 依存追加 + `NativeBackend` 骨組みと資格情報ロード

**Files:**
- Modify: `Cargo.toml`（ルート、workspace.dependencies に mat-controller / async-trait）
- Modify: `crates/matd/Cargo.toml`
- Create: `crates/matd/src/native.rs`
- Modify: `crates/matd/src/lib.rs`

**Interfaces:**
- Consumes: `mat_controller::kvs::read_self_issue_materials(alpha_ini: &Path, main_ini:
  &Path, fabric_index: u8, issuer_index: u8) -> Result<SelfIssueMaterials, KvsError>`;
  `mat_controller::fabric::FabricCredentials::from_self_issued(SelfIssueMaterials) ->
  Result<FabricCredentials, FabricError>`; `mat_controller::dnssd::iface_index(name: &str)
  -> io::Result<u32>`; `mat_controller::transport::UdpTransport::bind() -> io::Result<Self>`.
- Produces:
  - `pub struct NativeConfig { pub store: PathBuf, pub iface: String, pub fabric_index: u8, pub issuer_index: u8 }`
  - `pub struct NativeBackend { /* private */ }`
  - `impl NativeBackend { pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError>; }`
    — KVS から資格情報を1回読み、NOC を自己発行し、`Arc<UdpTransport>` を bind、
    iface の scope_id を解決して保持する。失敗は `MatError`（`store_missing` /
    `session_failed` / `other` に写像）。

- [ ] **Step 1: 依存を追加**

ルート `Cargo.toml` の `[workspace.dependencies]` に追加:

```toml
mat-controller = { path = "crates/mat-controller" }
async-trait = "0.1"
```

`crates/matd/Cargo.toml` の `[dependencies]` に追加:

```toml
mat-controller.workspace = true
async-trait.workspace = true
```

`crates/matd/src/lib.rs` にモジュール宣言を追加（既存の `pub mod backend;` 等の並びへ）:

```rust
pub mod native;
```

- [ ] **Step 2: ビルドが通る空モジュールを置く（配線確認）**

`crates/matd/src/native.rs` を最小内容で作成:

```rust
//! matd の native バックエンド（Phase 5 M4）。
//!
//! mat-controller の warm CASE セッションを matd プロセス内に保持し、ホットパス
//! （on/off・色・色温度・onoff read）を chip-tool を介さず処理する。未対応 op は
//! server 層が chip-tool ws にフォールバックする。

use std::path::PathBuf;
use std::sync::Arc;

use mat_controller::fabric::FabricCredentials;
use mat_controller::transport::UdpTransport;
use mat_core::error::{ErrorKind, MatError};

/// native バックエンドの起動設定。
pub struct NativeConfig {
    /// chip-tool KVS のあるディレクトリ（chip-tool の --storage-directory と同一）。
    pub store: PathBuf,
    /// mDNS scope に使う Thread mesh の iface 名。
    pub iface: String,
    /// KVS fabric テーブルの index（jarvis 本番は 2、alpha は 1）。
    pub fabric_index: u8,
    /// CA issuer index（既定 0）。
    pub issuer_index: u8,
}

/// warm CASE セッションを per-node に保持する native バックエンド。
pub struct NativeBackend {
    creds: Arc<FabricCredentials>,
    transport: Arc<UdpTransport>,
    scope_id: u32,
}
```

Run: `cargo build -p matd 2>&1 | tail -10`
Expected: PASS（未使用フィールド警告は次ステップで解消）。

- [ ] **Step 3: 資格情報ロードの失敗テストを書く**

`crates/matd/src/native.rs` の末尾に:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_fails_cleanly_without_kvs() {
        // KVS が無いディレクトリでは store_missing 相当のエラーで即失敗し、
        // panic しない（matd 起動時に安全フォールバックへ落とす判断材料）。
        let dir = tempfile::tempdir().unwrap();
        let cfg = NativeConfig {
            store: dir.path().to_path_buf(),
            iface: "lo".to_string(),
            fabric_index: 1,
            issuer_index: 0,
        };
        let err = NativeBackend::build(&cfg).await.expect_err("no KVS present");
        assert!(
            matches!(err.kind, ErrorKind::StoreMissing | ErrorKind::Other),
            "unexpected kind: {:?}",
            err.kind
        );
    }
}
```

`crates/matd/Cargo.toml` の `[dev-dependencies]` に `tempfile.workspace = true` は既存
（確認のみ）。

- [ ] **Step 4: テストが未実装の `build` で落ちるのを確認**

Run: `cargo test -p matd --lib native::tests::build_fails_cleanly_without_kvs 2>&1 | tail -10`
Expected: FAIL（`build` not found）。

- [ ] **Step 5: `build` を実装**

`crates/matd/src/native.rs` の `NativeBackend` に `impl` を追加:

```rust
use mat_controller::{dnssd, kvs};

impl NativeBackend {
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して保持する。プロセス寿命で不変。
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        let alpha_ini = cfg.store.join("chip_tool_config.alpha.ini");
        let main_ini = cfg.store.join("chip_tool_config.ini");
        let materials = kvs::read_self_issue_materials(
            &alpha_ini,
            &main_ini,
            cfg.fabric_index,
            cfg.issuer_index,
        )
        .map_err(|e| {
            // KVS 欠落は store_missing、その他 KVS パース失敗は other。
            MatError::new(ErrorKind::StoreMissing, format!("native: read KVS credentials: {e}"))
        })?;
        let creds = FabricCredentials::from_self_issued(materials).map_err(|e| {
            MatError::new(ErrorKind::SessionFailed, format!("native: self-issue NOC: {e}"))
        })?;
        let scope_id = dnssd::iface_index(&cfg.iface).map_err(|e| {
            MatError::new(
                ErrorKind::Other,
                format!("native: resolve iface {:?} index: {e}", cfg.iface),
            )
        })?;
        let transport = UdpTransport::bind().await.map_err(|e| {
            MatError::new(ErrorKind::Other, format!("native: bind udp transport: {e}"))
        })?;
        Ok(Self {
            creds: Arc::new(creds),
            transport: Arc::new(transport),
            scope_id,
        })
    }
}
```

- [ ] **Step 6: テストが通るのを確認**

Run: `cargo test -p matd --lib native::tests::build_fails_cleanly_without_kvs 2>&1 | tail -10`
Expected: PASS。

（注: 資格情報が「読めた」ときの正常系は KVS フィクスチャが要るため、実機 E2E
（Task 8）で担保する。ここでは欠落時に安全に失敗する経路だけを CI で確認する。）

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/matd/Cargo.toml crates/matd/src/native.rs crates/matd/src/lib.rs
git commit -m "feat(matd): NativeBackend scaffold + credential loading (M4)

Add mat-controller/async-trait deps. NativeBackend::build reads chip-tool
KVS once, self-issues a NOC, binds a UDP transport, and resolves the
iface scope_id — cached for the process lifetime. Missing KVS fails
cleanly (store_missing) so matd can fall back to chip-tool.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 4: matd — warm session seam（`Establisher`/`NodeConn`）+ per-node 管理 + ホットパスメソッド

**Files:**
- Modify: `crates/matd/src/native.rs`

**Interfaces:**
- Consumes: `mat_controller::case::establish`（Task 1）; `mat_controller::dnssd::{resolve_operational,
  ResolvedNode}`; `mat_controller::im::{self, CLUSTER_ON_OFF, ATTR_ON_OFF, CMD_ON_OFF_ON,
  CMD_ON_OFF_OFF, CLUSTER_COLOR_CONTROL, CMD_MOVE_TO_HUE_AND_SATURATION,
  CMD_MOVE_TO_COLOR_TEMPERATURE, encode_move_to_hue_and_saturation_fields,
  encode_move_to_color_temperature_fields, ImValue}`; `mat_controller::fabric::compressed_fabric_id`.
- Produces（server が呼ぶ公開 API）:
  - `impl NativeBackend`:
    - `pub async fn read_onoff(&self, node_id: u64, endpoint: u16) -> Result<bool, MatError>`
    - `pub async fn on(&self, node_id: u64, endpoint: u16) -> Result<(), MatError>`
    - `pub async fn off(&self, node_id: u64, endpoint: u16) -> Result<(), MatError>`
    - `pub async fn color(&self, node_id: u64, endpoint: u16, hue_raw: u8, saturation_raw: u8, transition: u16) -> Result<(), MatError>`
    - `pub async fn color_temp(&self, node_id: u64, endpoint: u16, mireds: u16, transition: u16) -> Result<(), MatError>`
- 内部 seam（`pub(crate)`、テストで差し替え可能）:
  - `#[async_trait] trait NodeConn: Send { async fn read_onoff(&mut self, endpoint: u16)
    -> Result<bool, MatError>; async fn invoke(&mut self, endpoint: u16, cluster: u32,
    command: u32, fields: Option<Vec<u8>>) -> Result<(), MatError>; }`
  - `#[async_trait] trait Establisher: Send + Sync { async fn establish(&self, node_id:
    u64) -> Result<Box<dyn NodeConn>, MatError>; }`

**設計の要点**: `NativeBackend` は `establisher: Box<dyn Establisher>` と
`sessions: Mutex<HashMap<u64, Arc<Mutex<Option<Box<dyn NodeConn>>>>>>` を持つ。外側
Mutex は per-node slot を get-or-insert する短時間のみ保持し即解放（ノード間並行）。
往復は per-node 内側 Mutex を保持（同一ノード直列）。送信が `ErrorKind::Timeout`
（MRP 再送尽き=session が死んでいる兆候）で失敗したら slot を破棄し1回だけ再確立して
再送する。他 kind（device_rejected 等、コマンドは届いた）は再送しない。

- [ ] **Step 1: warm 再利用と失敗時再確立のテストを書く（fake seam）**

`crates/matd/src/native.rs` の `#[cfg(test)] mod tests` に追加。まず fake を定義:

```rust
    use std::sync::atomic::{AtomicUsize, Ordering};
    use async_trait::async_trait;

    /// 送信 n 回目に Timeout を返すよう設定できる fake セッション。
    struct FakeConn {
        fail_first_send: bool,
        sent: AtomicUsize,
    }

    #[async_trait]
    impl NodeConn for FakeConn {
        async fn read_onoff(&mut self, _endpoint: u16) -> Result<bool, MatError> {
            let n = self.sent.fetch_add(1, Ordering::SeqCst);
            if self.fail_first_send && n == 0 {
                return Err(MatError::new(ErrorKind::Timeout, "fake MRP exhausted"));
            }
            Ok(true)
        }
        async fn invoke(
            &mut self,
            _endpoint: u16,
            _cluster: u32,
            _command: u32,
            _fields: Option<Vec<u8>>,
        ) -> Result<(), MatError> {
            Ok(())
        }
    }

    /// establish 呼び出し回数を数える fake。`fail_first_send` を確立する Conn に伝える。
    struct FakeEstablisher {
        calls: AtomicUsize,
        fail_first_send: bool,
    }

    #[async_trait]
    impl Establisher for FakeEstablisher {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::<FakeConn>::new(FakeConn {
                // 2 回目の確立（=再確立）では成功させる。
                fail_first_send: self.fail_first_send && n == 0,
                sent: AtomicUsize::new(0),
            });
            Ok(Box::new(FakeConn {
                fail_first_send: self.fail_first_send && n == 0,
                sent: AtomicUsize::new(0),
            }))
        }
    }

    fn backend_with(est: FakeEstablisher) -> NativeBackend {
        NativeBackend::with_establisher(Box::new(est))
    }

    #[tokio::test]
    async fn reuses_warm_session_for_same_node() {
        let backend = backend_with(FakeEstablisher {
            calls: AtomicUsize::new(0),
            fail_first_send: false,
        });
        backend.read_onoff(0x1234, 1).await.unwrap();
        backend.read_onoff(0x1234, 1).await.unwrap();
        // 2 回のコマンドで establish は 1 回だけ（warm 再利用）。
        assert_eq!(backend.establish_calls(), 1);
    }

    #[tokio::test]
    async fn re_establishes_once_on_send_timeout() {
        let backend = backend_with(FakeEstablisher {
            calls: AtomicUsize::new(0),
            fail_first_send: true,
        });
        // 1 回目の send が Timeout → slot 破棄 → 再確立 → 再送成功。
        let v = backend.read_onoff(0x1234, 1).await.unwrap();
        assert!(v);
        assert_eq!(backend.establish_calls(), 2);
    }
```

fake の重複 `Box::new` は Step 3 実装後に整理する（今は「establish が Conn を返す」
形さえ合っていればよい）。テスト用アクセサ `with_establisher` / `establish_calls` は
Step 3 で `pub(crate)` 実装する。

- [ ] **Step 2: テストが未実装 seam で落ちるのを確認**

Run: `cargo test -p matd --lib native::tests::reuses_warm_session 2>&1 | tail -15`
Expected: FAIL（`NodeConn` / `Establisher` / `with_establisher` / `establish_calls`
未定義でコンパイルエラー）。

- [ ] **Step 3: seam トレイトと per-node 管理・ホットパスメソッドを実装**

`crates/matd/src/native.rs` の import 群に追加:

```rust
use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::Mutex;

use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::compressed_fabric_id;
use mat_controller::im::{
    self, ATTR_ON_OFF, CLUSTER_COLOR_CONTROL, CLUSTER_ON_OFF, CMD_MOVE_TO_COLOR_TEMPERATURE,
    CMD_MOVE_TO_HUE_AND_SATURATION, CMD_ON_OFF_OFF, CMD_ON_OFF_ON, ImValue,
};
use mat_controller::{case, dnssd};
use std::net::SocketAddr;
use std::time::Duration;
```

seam トレイト:

```rust
/// warm な per-node セッションが提供する操作（実 CASE session or テスト fake）。
#[async_trait]
pub(crate) trait NodeConn: Send {
    async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError>;
    async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<(), MatError>;
}

/// ノード宛の warm セッションを新規確立する手段（実 = mDNS+CASE、テスト = fake）。
#[async_trait]
pub(crate) trait Establisher: Send + Sync {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError>;
}
```

`NativeBackend` を seam を持つ形へ差し替え（Task 3 のフィールドは実 Establisher が
所有するので `NativeBackend` 本体からは外し、`establisher` へ移す）:

```rust
pub struct NativeBackend {
    establisher: Box<dyn Establisher>,
    sessions: Mutex<HashMap<u64, Arc<Mutex<Option<Box<dyn NodeConn>>>>>>,
}

/// mDNS 解決 timeout。SII が来ない場合でも過度に待たない上限。
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(8);

impl NativeBackend {
    /// テスト用: 任意の Establisher を注入する。
    pub(crate) fn with_establisher(establisher: Box<dyn Establisher>) -> Self {
        Self {
            establisher,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// この node の per-node slot（`Arc<Mutex<Option<..>>>`）を得る。外側ロックは
    /// slot 取得の間だけ保持して即解放する（ノード間の並行性を保つ）。
    async fn slot(&self, node_id: u64) -> Arc<Mutex<Option<Box<dyn NodeConn>>>> {
        let mut map = self.sessions.lock().await;
        Arc::clone(map.entry(node_id).or_insert_with(|| Arc::new(Mutex::new(None))))
    }

    /// warm セッションで `op` を実行する。slot が空なら確立。送信が Timeout
    /// （MRP 尽き=session が死んでいる兆候）なら slot を捨てて1回だけ再確立し再送する。
    /// device_rejected 等（コマンドは届いた）は再送しない。
    async fn with_session<F, Fut, T>(&self, node_id: u64, op: F) -> Result<T, MatError>
    where
        F: Fn(&mut Box<dyn NodeConn>) -> Fut,
        Fut: std::future::Future<Output = Result<T, MatError>>,
    {
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            *guard = Some(self.establisher.establish(node_id).await?);
        }
        let result = op(guard.as_mut().expect("established above")).await;
        match result {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Timeout => {
                // session が死んだ疑い。捨てて1回だけ再確立→再送。
                tracing::info!(node_id, "native session send timed out; re-establishing once");
                *guard = None;
                *guard = Some(self.establisher.establish(node_id).await?);
                op(guard.as_mut().expect("re-established")).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn read_onoff(&self, node_id: u64, endpoint: u16) -> Result<bool, MatError> {
        self.with_session(node_id, |c| c.read_onoff(endpoint)).await
    }

    pub async fn on(&self, node_id: u64, endpoint: u16) -> Result<(), MatError> {
        self.with_session(node_id, |c| c.invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None))
            .await
    }

    pub async fn off(&self, node_id: u64, endpoint: u16) -> Result<(), MatError> {
        self.with_session(node_id, |c| c.invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_OFF, None))
            .await
    }

    pub async fn color(
        &self,
        node_id: u64,
        endpoint: u16,
        hue_raw: u8,
        saturation_raw: u8,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields = im::encode_move_to_hue_and_saturation_fields(hue_raw, saturation_raw, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_COLOR_CONTROL,
                CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields.clone()),
            )
        })
        .await
    }

    pub async fn color_temp(
        &self,
        node_id: u64,
        endpoint: u16,
        mireds: u16,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields = im::encode_move_to_color_temperature_fields(mireds, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_COLOR_CONTROL,
                CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields.clone()),
            )
        })
        .await
    }

    #[cfg(test)]
    fn establish_calls(&self) -> usize {
        // テスト専用: FakeEstablisher の呼び出し回数を読む。
        self.establisher.debug_calls()
    }
}
```

テスト用アクセサのため seam に隠しメソッドを足すのは避け、代わりに Step 1 の
`establish_calls()` を「fake が持つカウンタを直接読む」形にする。Step 1 のテストを次に
合わせて修正する（`backend.establish_calls()` → establisher を外から数える）:

Step 1 の各テストで establisher を先に `Arc` で持ち、カウンタを直接検証する形へ変更:

```rust
    #[tokio::test]
    async fn reuses_warm_session_for_same_node() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher { calls: std::sync::Arc::clone(&calls), fail_first_send: false };
        let backend = NativeBackend::with_establisher(Box::new(est));
        backend.read_onoff(0x1234, 1).await.unwrap();
        backend.read_onoff(0x1234, 1).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn re_establishes_once_on_send_timeout() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher { calls: std::sync::Arc::clone(&calls), fail_first_send: true };
        let backend = NativeBackend::with_establisher(Box::new(est));
        let v = backend.read_onoff(0x1234, 1).await.unwrap();
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
```

これに合わせ `FakeEstablisher.calls` を `Arc<AtomicUsize>` にし、`establish_calls()` /
`debug_calls()` は削除する（余計な seam を作らない）。`FakeEstablisher::establish` の
重複 `Box::new` は 1 つに整理する。

- [ ] **Step 4: 実 Establisher（mDNS+CASE）と実 NodeConn（SecureSession ラッパ）を実装**

`crates/matd/src/native.rs` に、Task 3 の `build` が構築する実体を追加:

```rust
/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。
struct CaseEstablisher {
    creds: Arc<FabricCredentials>,
    transport: Arc<UdpTransport>,
    scope_id: u32,
}

#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = dnssd::resolve_operational(self.scope_id, &cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| MatError::new(ErrorKind::Unreachable, format!("native: mDNS resolve node {node_id}: {e}")))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let mut last: Option<MatError> = None;
        for peer in peers {
            match case::establish(Arc::clone(&self.transport), peer, &self.creds, node_id, &mrp).await {
                Ok(session) => {
                    return Ok(Box::new(SessionConn { session, mrp }));
                }
                Err(e) => {
                    last = Some(MatError::new(ErrorKind::SessionFailed, format!("native: CASE via {peer}: {e}")));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            MatError::new(ErrorKind::Unreachable, format!("native: no addresses resolved for node {node_id}"))
        }))
    }
}

/// 実セッション: SecureSession + そのノードの MRP 設定。
struct SessionConn {
    session: mat_controller::session::SecureSession,
    mrp: MrpConfig,
}

#[async_trait]
impl NodeConn for SessionConn {
    async fn read_onoff(&mut self, endpoint: u16) -> Result<bool, MatError> {
        match self
            .session
            .read_attribute(endpoint, CLUSTER_ON_OFF, ATTR_ON_OFF, &self.mrp)
            .await
            .map_err(map_session_err)?
        {
            ImValue::Bool(b) => Ok(b),
            other => Err(MatError::parse_error(format!("native: on-off not a bool: {other:?}"))),
        }
    }

    async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<(), MatError> {
        self.session
            .invoke(endpoint, cluster, command, fields.as_deref(), &self.mrp)
            .await
            .map_err(map_session_err)?;
        Ok(())
    }
}

/// SecureSession のエラーを mat の ErrorKind へ写像する（経路によらず分類を揃える）。
fn map_session_err(e: mat_controller::session::SessionError) -> MatError {
    use mat_controller::session::SessionError;
    match e {
        // MRP 再送尽き。session が死んでいる兆候 → 上位が1回だけ再確立を試みる。
        SessionError::Timeout => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        // デバイスがコマンド/読みを IM ステータスで拒否 → コマンドは届いた。
        SessionError::Im(_) => MatError::new(ErrorKind::DeviceRejected, format!("native: {e}")),
        SessionError::Io(_) => MatError::new(ErrorKind::Unreachable, format!("native: {e}")),
        _ => MatError::new(ErrorKind::Other, format!("native: {e}")),
    }
}
```

Task 3 の `build` の戻り値を、seam を持つ `NativeBackend` に合わせて差し替える:

```rust
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        // ... materials / creds / scope_id / transport は Task 3 のまま ...
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
            transport: Arc::new(transport),
            scope_id,
        };
        Ok(Self::with_establisher(Box::new(establisher)))
    }
```

（Task 3 で `NativeBackend` が直接持っていた `creds`/`transport`/`scope_id` フィールドは
`CaseEstablisher` へ移り、`NativeBackend` 本体は seam のみ持つ。Task 3 の
`build_fails_cleanly_without_kvs` テストはそのまま通る。）

- [ ] **Step 5: 全 native ユニットテストが通るのを確認**

Run: `cargo test -p matd --lib native 2>&1 | tail -15`
Expected: PASS（`reuses_warm_session_for_same_node` / `re_establishes_once_on_send_timeout`
/ `build_fails_cleanly_without_kvs`）。

- [ ] **Step 6: clippy**

Run: `cargo clippy -p matd --all-targets -- -D warnings 2>&1 | tail -10`
Expected: 警告なし（`fields.clone()` の move クロージャ等でエラーが出たら、`with_session`
のクロージャ境界を調整。`Fn` を `FnMut` に緩める、または fields を `Arc<Vec<u8>>` にして
clone コストを消す。実装が固まるまではここで解消する）。

- [ ] **Step 7: Commit**

```bash
git add crates/matd/src/native.rs
git commit -m "feat(matd): native warm session mgmt + hotpath methods (M4)

NodeConn/Establisher async-trait seam lets the per-node warm-session
state machine (reuse across commands, drop+re-establish once on MRP
timeout) be unit-tested without real CASE. Real establisher does
mDNS+CASE; SessionConn wraps SecureSession. read_onoff/on/off/color/
color_temp map SessionError to mat ErrorKind consistently.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 5: matd — `server::run_op` の native/chip-tool 分岐 + 応答 body 共用

**Files:**
- Modify: `crates/matd/src/server.rs`

**Interfaces:**
- Consumes: `crate::native::NativeBackend`（Task 4 の公開メソッド）; 既存 `Op`（protocol.rs）;
  既存 `simple_op` の body 構築ロジック。
- Produces:
  - `serve` / `handle_conn` / `dispatch` / `run_op` が `native: Option<Arc<NativeBackend>>`
    を受け取る形に拡張。
  - `pub(crate) fn is_native_hotpath(op: &Op) -> bool`（routing 判定、ユニットテスト対象）。

- [ ] **Step 1: routing 判定のユニットテストを書く**

`crates/matd/src/server.rs` の `#[cfg(test)] mod tests` に追加:

```rust
    use crate::protocol::Op;

    #[test]
    fn hotpath_routing_selects_native_ops() {
        // native で処理するホットパス。
        assert!(is_native_hotpath(&Op::On { node_id: 1, endpoint: 1 }));
        assert!(is_native_hotpath(&Op::Off { node_id: 1, endpoint: 1 }));
        assert!(is_native_hotpath(&Op::ColorTemp { node_id: 1, endpoint: 1, mireds: 370, kelvin: 2700, transition: 0 }));
        assert!(is_native_hotpath(&Op::Color { node_id: 1, endpoint: 1, hue_raw: 0, saturation_raw: 254, hue: 0, saturation: 100, name: None, rgb: None, transition: 0 }));
        // onoff on-off の read だけ native。
        assert!(is_native_hotpath(&Op::Read { node_id: 1, endpoint: 1, cluster: "onoff".into(), attribute: "on-off".into() }));
    }

    #[test]
    fn hotpath_routing_leaves_others_to_chip_tool() {
        // 別 cluster/attr の read は chip-tool へ。
        assert!(!is_native_hotpath(&Op::Read { node_id: 1, endpoint: 1, cluster: "levelcontrol".into(), attribute: "current-level".into() }));
        assert!(!is_native_hotpath(&Op::Write { node_id: 1, endpoint: 1, cluster: "onoff".into(), attribute: "on-off".into(), value: "1".into() }));
        assert!(!is_native_hotpath(&Op::Describe { node_id: 1 }));
        assert!(!is_native_hotpath(&Op::Invoke { node_id: 1, endpoint: 1, cluster: "identify".into(), command: "identify".into(), args: vec![] }));
        assert!(!is_native_hotpath(&Op::GroupInvoke { group_id: 1, cluster: "onoff".into(), command: "on".into(), args: vec![], endpoint: 1 }));
        assert!(!is_native_hotpath(&Op::Ping));
    }
```

- [ ] **Step 2: テストが未定義関数で落ちるのを確認**

Run: `cargo test -p matd --lib server::tests::hotpath_routing 2>&1 | tail -10`
Expected: FAIL（`is_native_hotpath` not found）。

- [ ] **Step 3: `is_native_hotpath` を実装**

`crates/matd/src/server.rs` に追加:

```rust
/// この op を native warm session で処理するか（ホットパス）。それ以外は
/// chip-tool ws にフォールバックする（M4 スコープ）。
pub(crate) fn is_native_hotpath(op: &Op) -> bool {
    match op {
        Op::On { .. } | Op::Off { .. } | Op::Color { .. } | Op::ColorTemp { .. } => true,
        // read は onoff on-off のみ native（汎用 attr 名→ID テーブルは未実装）。
        Op::Read { cluster, attribute, .. } => cluster == "onoff" && attribute == "on-off",
        _ => false,
    }
}
```

- [ ] **Step 4: 判定テストが通るのを確認**

Run: `cargo test -p matd --lib server::tests::hotpath_routing 2>&1 | tail -10`
Expected: PASS。

- [ ] **Step 5: 応答 body ビルダを純関数として抽出**

`server.rs` の `simple_op` が組み立てている成功 body を、op から作る純関数に切り出す。
`simple_op` 内の各 `Op::X => json!({...})` のうち **native 対象**（On/Off/Color/ColorTemp）と
onoff Read の body を関数へ移す:

```rust
/// native/chip-tool どちらの経路でも使う、ホットパス op の成功 body（timestamp 抜き）。
fn hotpath_success_body(op: &Op, read_value: Option<Value>) -> Value {
    match op {
        Op::On { node_id, endpoint } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "onoff", "command": "on", "status": "success",
        }),
        Op::Off { node_id, endpoint } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "onoff", "command": "off", "status": "success",
        }),
        Op::ColorTemp { node_id, endpoint, mireds, kelvin, transition } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": "colorcontrol", "command": "move-to-color-temperature",
            "kelvin": kelvin, "mireds": mireds, "transition": transition,
            "status": "success",
        }),
        Op::Color { node_id, endpoint, hue_raw, saturation_raw, hue, saturation, name, rgb, transition } => {
            let mut body = json!({
                "node_id": node_id, "endpoint": endpoint,
                "cluster": "colorcontrol", "command": "move-to-hue-and-saturation",
                "hue": hue, "saturation": saturation,
                "hue_raw": hue_raw, "saturation_raw": saturation_raw,
                "transition": transition, "status": "success",
            });
            if let Some(n) = name { body["name"] = json!(n); }
            if let Some(r) = rgb { body["rgb"] = json!(r); }
            body
        }
        Op::Read { node_id, endpoint, cluster, attribute } => json!({
            "node_id": node_id, "endpoint": endpoint,
            "cluster": cluster, "attribute": attribute,
            "value": read_value.unwrap_or(Value::Null),
        }),
        _ => unreachable!("hotpath_success_body called with non-hotpath op"),
    }
}
```

`simple_op` の該当アームは `hotpath_success_body(op, None)`（Read は
`read_value(&result)` を渡す）を呼ぶ形に置き換え、重複 json を消す（DRY）。
Write/Invoke など非ホットパスのアームは `simple_op` に残す。

- [ ] **Step 6: `run_op` に native 分岐を通す**

`run_op` のシグネチャに native を追加し、ホットパスなら native、失敗フォールバックせず
そのまま結果を返す（native が有効な時のみ native へ。無効時は従来どおり chip-tool）:

```rust
async fn run_op(
    op: &Op,
    backend: &ChipToolBackend,
    native: Option<&NativeBackend>,
    store_path: &Path,
) -> Result<Value, MatError> {
    // native が有効かつホットパスなら native 経路。
    if let Some(native) = native {
        if is_native_hotpath(op) {
            return native_op(op, native, store_path).await;
        }
    }
    match op {
        Op::Ping => Ok(json!({ "pong": true })),
        // ... 既存の match そのまま ...
    }
}

/// native ホットパス op を warm session で実行し、成功 body を組む。
async fn native_op(op: &Op, native: &NativeBackend, store_path: &Path) -> Result<Value, MatError> {
    // commission 済みか毎回 KVS で確認（chip-tool 経路と同じ挙動）。
    if let Some(node_id) = op.node_id() {
        require_node(store_path, node_id)?;
    }
    match op {
        Op::On { node_id, endpoint } => {
            native.on(*node_id, *endpoint).await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::Off { node_id, endpoint } => {
            native.off(*node_id, *endpoint).await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::Color { node_id, endpoint, hue_raw, saturation_raw, transition, .. } => {
            native.color(*node_id, *endpoint, *hue_raw, *saturation_raw, *transition).await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::ColorTemp { node_id, endpoint, mireds, transition, .. } => {
            native.color_temp(*node_id, *endpoint, *mireds, *transition).await?;
            Ok(hotpath_success_body(op, None))
        }
        Op::Read { node_id, endpoint, .. } => {
            let v = native.read_onoff(*node_id, *endpoint).await?;
            Ok(hotpath_success_body(op, Some(Value::Bool(v))))
        }
        _ => unreachable!("native_op called with non-hotpath op"),
    }
}
```

- [ ] **Step 7: `serve`/`handle_conn`/`dispatch` に native を通す**

`serve` の署名に `native: Option<Arc<NativeBackend>>` を足し、`handle_conn` →
`dispatch` → `run_op` へ `Option<&NativeBackend>`（`native.as_deref()`）を伝播する。
`serve` 冒頭で有効/無効をログ:

```rust
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    backend: Arc<ChipToolBackend>,
    native: Option<Arc<NativeBackend>>,
) -> std::io::Result<()> {
    tracing::info!(native = native.is_some(), "matd backends");
    // ... 既存 ...
```

`tokio::spawn` する各接続へ `native` を `Option<Arc<..>>` の clone で渡す。
`dispatch(&line, &backend, native.as_deref(), &store_path)` の形。

- [ ] **Step 8: 全 server テスト + build を確認**

Run: `cargo test -p matd --lib server 2>&1 | tail -15 && cargo build -p matd 2>&1 | tail -5`
Expected: PASS（既存 `ensure_ok_*` テスト + 新 routing テスト）、build 成功。
`main.rs` は次タスクで直すため、この時点で `serve` の呼び出し側が壊れてコンパイルエラーに
なるなら Task 6 まで一時的に `None` を渡す修正を `main.rs` に入れてよい（Task 6 で本実装）。

- [ ] **Step 9: Commit**

```bash
git add crates/matd/src/server.rs
git commit -m "feat(matd): route hotpath ops to NativeBackend (M4)

run_op sends on/off/color/color-temp and onoff on-off read to the native
warm-session backend when enabled; everything else stays on chip-tool ws.
Extract hotpath_success_body so both paths emit the identical mat schema.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 6: matd — `ChipToolBackend` の lazy spawn 化

**Files:**
- Modify: `crates/matd/src/backend.rs`

**Interfaces:**
- Produces: `ChipToolBackend::spawn` / `connect` が起動時に ws を確立せず（eager
  `ensure_connected` を外す）、初回 `run_cmdline` で遅延確立する。既存の
  `run_cmdline` / `reap_if_idle` / `keepalive_tick` / `child_pid` / `ws_connected` の
  シグネチャは不変。

- [ ] **Step 1: lazy spawn のテストを書く**

`crates/matd/src/backend.rs` の `#[cfg(test)] mod tests` に追加:

```rust
    #[tokio::test]
    async fn connect_mode_does_not_dial_until_first_command() {
        // Connect モードで、繋ぐ相手が居なくても構築は成功する（遅延確立）。
        // 以前は new() が即接続して失敗していた。
        let backend = ChipToolBackend::connect(59999, Duration::from_secs(300)).await;
        assert!(backend.is_ok(), "lazy connect must not dial at construction");
        assert!(!backend.unwrap().ws_connected().await, "no ws until first command");
    }
```

- [ ] **Step 2: テストが現状の eager 接続で落ちるのを確認**

Run: `cargo test -p matd --lib backend::tests::connect_mode_does_not_dial 2>&1 | tail -10`
Expected: FAIL（現状 `new` が `ensure_connected` を呼び、ポート 59999 へ接続できず
`connect` が `Err` を返す）。

- [ ] **Step 3: 起動時の eager 接続を外す**

`crates/matd/src/backend.rs` の `new`（95〜111 行目付近）から早期接続を削除:

```rust
    async fn new(mode: Mode, idle: Duration) -> Result<Self, MatError> {
        Ok(ChipToolBackend {
            mode,
            idle,
            conn: Mutex::new(Conn {
                ws: None,
                child: None,
                last_used: Instant::now(),
                failures: 0,
            }),
        })
    }
```

（doc コメントの「起動時に一度確立してエラーを早期検出する」旨を「初回コマンドで遅延
確立する」に更新。`run_cmdline` は既に冒頭で `ensure_connected` を呼ぶため、遅延確立は
そのまま機能する。）

- [ ] **Step 4: テストが通るのを確認 + 既存 backend テスト回帰なし**

Run: `cargo test -p matd --lib backend 2>&1 | tail -15`
Expected: PASS（新テスト + 既存 `drop_logs_*` / `exchange_classifies_*`）。

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/backend.rs
git commit -m "refactor(matd): lazy-spawn chip-tool on first fallback command (M4)

Drop the eager connect at construction so a native-hotpath-only workload
never spawns chip-tool. First fallback/group op establishes it via the
existing ensure_connected path. Enables running matd without chip-tool
present (native-first, toward removing chip-tool).

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 7: matd — CLI フラグ + 両 backend の配線 + 起動ログ

**Files:**
- Modify: `crates/matd/src/main.rs`

**Interfaces:**
- Consumes: `NativeBackend::build`（Task 3/4）; `NativeConfig`; `server::serve`（Task 5 の
  新シグネチャ）。
- Produces: matd 起動時に `MAT_MATD_IFACE` が設定されていれば `NativeBackend` を構築し、
  `serve` に `Some(Arc<NativeBackend>)` を渡す。未設定 or 構築失敗なら `None`（全 op
  chip-tool へ、現行挙動）でフォールバック。CLI フラグ `--iface` / `--fabric-index` /
  `--issuer-index`（それぞれ env `MAT_MATD_IFACE` / `MAT_MATD_FABRIC_INDEX` /
  `MAT_MATD_ISSUER_INDEX` にフォールバック）。

- [ ] **Step 1: CLI フラグを追加**

`crates/matd/src/main.rs` の `struct Cli` に追加:

```rust
    /// native warm session に使う Thread mesh の iface 名。未指定なら native を無効化し
    /// 全 op を chip-tool へ回す（安全フォールバック）。
    #[arg(long, env = "MAT_MATD_IFACE")]
    iface: Option<String>,

    /// KVS fabric テーブルの index（jarvis 本番は 2、alpha は 1）。
    #[arg(long, env = "MAT_MATD_FABRIC_INDEX", default_value_t = 1)]
    fabric_index: u8,

    /// CA issuer index。
    #[arg(long, env = "MAT_MATD_ISSUER_INDEX", default_value_t = 0)]
    issuer_index: u8,
```

- [ ] **Step 2: `serve_daemon` で native を構築して配線**

`serve_daemon`（97〜121 行目付近）の backend 構築後、native を組み立てて `serve` へ渡す:

```rust
    let idle = std::time::Duration::from_secs(cli.idle_timeout);
    let backend = if cli.connect {
        ChipToolBackend::connect(cli.port, idle).await?
    } else {
        ChipToolBackend::spawn(&store_path, cli.port, idle).await?
    };

    // native warm session バックエンド（iface 指定時のみ）。構築失敗は致命にせず、
    // chip-tool フォールバックへ落とす（native が実機でコケても matd は無停止）。
    let native = match &cli.iface {
        Some(iface) => {
            let cfg = matd::native::NativeConfig {
                store: store_path.clone(),
                iface: iface.clone(),
                fabric_index: cli.fabric_index,
                issuer_index: cli.issuer_index,
            };
            match matd::native::NativeBackend::build(&cfg).await {
                Ok(b) => {
                    tracing::info!(%iface, fabric_index = cli.fabric_index, "native backend enabled");
                    Some(Arc::new(b))
                }
                Err(e) => {
                    tracing::warn!(error = %e.detail, "native backend build failed; falling back to chip-tool for all ops");
                    None
                }
            }
        }
        None => {
            tracing::info!("MAT_MATD_IFACE unset; native backend disabled (chip-tool only)");
            None
        }
    };

    server::serve(&socket, store_path, Arc::new(backend), native)
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("socket server failed: {e}")))
```

（`store_path` を native と `serve` で使うため、native 構築時に `store_path.clone()` を
渡す。`serve` は `store_path` を move で取るので順序に注意。）

- [ ] **Step 3: ビルド + 全テストが通るのを確認**

Run: `cargo build -p matd 2>&1 | tail -5 && cargo test -p matd 2>&1 | tail -15`
Expected: PASS。

- [ ] **Step 4: `task check`（CI 相当）を全体で通す**

Run: `task check 2>&1 | tail -15`
Expected: fmt:check + clippy(-D warnings) + 全 crate test が緑。

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/main.rs
git commit -m "feat(matd): wire NativeBackend with --iface flag (M4)

matd builds the native warm-session backend when MAT_MATD_IFACE is set
(with --fabric-index/--issuer-index), else disables native and runs all
ops through chip-tool. Native build failure is non-fatal: log and fall
back, so matd stays up if native breaks on real hardware.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 8: 実機受け入れ E2E ハーネス（jarvis）

**Files:**
- Create: `crates/mat-controller/tests/live_matd_native.rs`（socket クライアント: matd へ
  リクエストを送り応答を検証）
- Create: `scripts/e2e-m4.sh`
- Modify: `Taskfile.yml`（`e2e:m4` タスク追加）

**Interfaces:**
- Consumes: 稼働中の matd（native 有効）への unix socket。protocol.rs の JSON リクエスト形。

このタスクは実機依存（`#[ignore]`、CI では走らない）。CI は Task 7 の `task check` で
担保済み。

- [ ] **Step 1: socket クライアントのライブテストを書く**

`crates/mat-controller/tests/live_matd_native.rs` を作成。matd の unix socket へ
newline-delimited JSON を送り、応答を検証する。ホットパスの往復と warm 再利用
（2 回目が速い）、フォールバック（describe）を確認:

```rust
//! Live E2E (M4): drive a running native-enabled matd over its unix socket.
//! Verifies hotpath ops go through the native warm session (on/off/color/
//! color-temp/onoff read), a second same-node command reuses the session
//! (faster), and describe still works via chip-tool fallback.
//! Run via `task e2e:m4`. Not in CI.
//!
//! Required env: MAT_E2E_SOCKET (matd socket path), MAT_E2E_NODE_ID,
//! MAT_E2E_ENDPOINT (default 1).

use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn env_u64(name: &str) -> u64 {
    let s = std::env::var(name).unwrap_or_else(|_| panic!("{name} required"));
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex id"),
        None => s.parse().expect("decimal id"),
    }
}

async fn request(socket: &str, line: &str) -> serde_json::Value {
    let stream = UnixStream::connect(socket).await.expect("connect matd socket");
    let (rd, mut wr) = stream.into_split();
    wr.write_all(line.as_bytes()).await.unwrap();
    wr.write_all(b"\n").await.unwrap();
    let mut lines = BufReader::new(rd).lines();
    let resp = lines.next_line().await.unwrap().expect("response line");
    serde_json::from_str(&resp).expect("json response")
}

fn assert_ok(v: &serde_json::Value, ctx: &str) {
    assert!(v.get("error").is_none(), "{ctx}: error response: {v}");
}

#[tokio::test]
#[ignore = "requires a running native-enabled matd + a commissioned device (task e2e:m4)"]
async fn matd_native_hotpath_roundtrip() {
    let socket = std::env::var("MAT_E2E_SOCKET").expect("MAT_E2E_SOCKET required");
    let node = env_u64("MAT_E2E_NODE_ID");
    let ep: u16 = std::env::var("MAT_E2E_ENDPOINT").ok().and_then(|s| s.parse().ok()).unwrap_or(1);

    // 初回 on（mDNS+CASE を含む） → 応答の所要時間を測る。
    let on = format!(r#"{{"op":"on","node_id":{node},"endpoint":{ep}}}"#);
    let t0 = Instant::now();
    let r = request(&socket, &on).await;
    let cold = t0.elapsed();
    assert_ok(&r, "on (cold)");
    assert_eq!(r["command"], "on");

    // onoff read が on を反映。
    let read = format!(r#"{{"op":"read","node_id":{node},"endpoint":{ep},"cluster":"onoff","attribute":"on-off"}}"#);
    let r = request(&socket, &read).await;
    assert_ok(&r, "read on-off");
    assert_eq!(r["value"], serde_json::json!(true), "on-off should be true after on");

    // 2 回目の on は warm セッション再利用で速いはず（mDNS+CASE を払わない）。
    let t1 = Instant::now();
    let r = request(&socket, &on).await;
    let warm = t1.elapsed();
    assert_ok(&r, "on (warm)");
    eprintln!("cold {cold:?} vs warm {warm:?}");
    assert!(warm < cold, "warm command must be faster than the cold one (session reuse)");
    assert!(warm < Duration::from_millis(500), "warm command should be sub-500ms, got {warm:?}");

    // 色・色温度のホットパス。
    let color = format!(r#"{{"op":"color","node_id":{node},"endpoint":{ep},"hue_raw":180,"saturation_raw":200,"hue":254,"saturation":78,"transition":0}}"#);
    assert_ok(&request(&socket, &color).await, "color");
    let ctemp = format!(r#"{{"op":"color_temp","node_id":{node},"endpoint":{ep},"mireds":370,"kelvin":2700,"transition":0}}"#);
    assert_ok(&request(&socket, &ctemp).await, "color_temp");

    // フォールバック: describe は chip-tool 経由で従来どおり動く。
    let describe = format!(r#"{{"op":"describe","node_id":{node}}}"#);
    let r = request(&socket, &describe).await;
    assert_ok(&r, "describe (chip-tool fallback)");
    assert!(r.get("endpoints").is_some(), "describe returns endpoints");

    // 後始末: off に戻す。
    let off = format!(r#"{{"op":"off","node_id":{node},"endpoint":{ep}}}"#);
    assert_ok(&request(&socket, &off).await, "off");
}
```

- [ ] **Step 2: コンパイルのみ確認（実機なし）**

Run: `cargo test -p mat-controller --test live_matd_native --no-run 2>&1 | tail -5`
Expected: コンパイル成功（`#[ignore]` で実行されない）。

- [ ] **Step 3: E2E スクリプトを書く**

`scripts/e2e-m4.sh` を作成（既存 `scripts/e2e-m3.sh` の構成を踏襲。クロスビルド →
jarvis へ matd と live テストバイナリを転送 → matd を native 有効で起動 → live テスト
実行 → matd 停止）。実ホスト・実 node id は env 必須でハードコードしない:

```bash
#!/usr/bin/env bash
# M4 実機 E2E: native 有効 matd を jarvis で起動し、socket 経由でホットパス往復を検証。
# 必須 env: MAT_E2E_HOST（ssh 先）, MAT_E2E_NODE_ID, MAT_E2E_IFACE。
# 任意: MAT_E2E_ENDPOINT(1), MAT_E2E_FABRIC_INDEX(2), MAT_E2E_STORE(~/.config/mat),
#       MAT_E2E_SOCKET(/tmp/matd-m4.sock)。
set -euo pipefail

: "${MAT_E2E_HOST:?set MAT_E2E_HOST (ssh target)}"
: "${MAT_E2E_NODE_ID:?set MAT_E2E_NODE_ID}"
: "${MAT_E2E_IFACE:?set MAT_E2E_IFACE (thread mesh iface on the host)}"
ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
STORE="${MAT_E2E_STORE:-\$HOME/.config/mat}"
SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-m4.sock}"
TARGET=aarch64-unknown-linux-musl

echo "== cross-building matd + live test for $TARGET =="
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p matd
TESTBIN=$(cargo test -p mat-controller --test live_matd_native --release --target "$TARGET" --no-run --message-format=json \
  | jq -r 'select(.profile.test == true) | .executable' | tail -1)

echo "== transferring to $MAT_E2E_HOST =="
scp "target/$TARGET/release/matd" "$MAT_E2E_HOST:/tmp/matd-m4"
scp "$TESTBIN" "$MAT_E2E_HOST:/tmp/live_matd_native"

echo "== starting native matd on host (socket $SOCKET) =="
# 既存 systemd の chip-tool matd は止めない。別 socket・別 ws ポートで起動。
ssh "$MAT_E2E_HOST" "MAT_MATD_IFACE='$MAT_E2E_IFACE' RUST_LOG=info \
  /tmp/matd-m4 --socket '$SOCKET' --port 9110 --store '$STORE' --fabric-index '$FABRIC_INDEX' \
  >/tmp/matd-m4.log 2>&1 & echo \$! > /tmp/matd-m4.pid; sleep 2"

cleanup() {
  ssh "$MAT_E2E_HOST" "kill \$(cat /tmp/matd-m4.pid) 2>/dev/null; rm -f '$SOCKET' /tmp/matd-m4.pid" || true
}
trap cleanup EXIT

echo "== running live E2E =="
ssh "$MAT_E2E_HOST" "MAT_E2E_SOCKET='$SOCKET' MAT_E2E_NODE_ID='$MAT_E2E_NODE_ID' \
  MAT_E2E_ENDPOINT='$ENDPOINT' /tmp/live_matd_native --ignored --nocapture matd_native_hotpath_roundtrip"

echo "== M4 live E2E PASSED =="
```

`chmod +x scripts/e2e-m4.sh`。

- [ ] **Step 4: Taskfile に `e2e:m4` を追加**

`Taskfile.yml` の e2e タスク群（`e2e:m3` の隣）へ:

```yaml
  e2e:m4:
    desc: "M4 実機 E2E（native 有効 matd を jarvis で起動し socket 往復。要 MAT_E2E_HOST/NODE_ID/IFACE）"
    cmds:
      - bash scripts/e2e-m4.sh
```

- [ ] **Step 5: スクリプトの構文チェック（実行はしない）**

Run: `bash -n scripts/e2e-m4.sh && echo OK`
Expected: `OK`（構文エラーなし。実行はユーザーが実機環境で行う）。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/tests/live_matd_native.rs scripts/e2e-m4.sh Taskfile.yml
git commit -m "test(matd): M4 live E2E harness — native matd over unix socket

live_matd_native drives a running native-enabled matd: hotpath on/off/
color/color-temp/onoff read round-trip, warm-session reuse (2nd command
faster + sub-500ms), and describe via chip-tool fallback. scripts/e2e-m4.sh
cross-builds, transfers, starts matd on a separate socket/port (leaving the
production chip-tool matd untouched), runs the test, tears down. Not in CI.

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Task 9: ドキュメント更新（ARCHITECTURE / CLAUDE / README）

**Files:**
- Modify: `ARCHITECTURE.md`（Phase 5 節に M4 完了を反映）
- Modify: `README.md`（matd の native 有効化 env / フラグを記載）

- [ ] **Step 1: ARCHITECTURE.md の Phase 5 節を更新**

`ARCHITECTURE.md` の Phase 5 節（M3 完了記述のあたり、`grep -n "M4" ARCHITECTURE.md`）で
「M4 = matd の adapter を新 crate に in-process 差し替え」を **実装済み**として記す。
主要点: ホットパス（on/off・色・色温度・onoff read）は native warm CASE、その他は
chip-tool フォールバック、`MAT_MATD_IFACE` で有効化、group は M5 まで chip-tool。

- [ ] **Step 2: README に native 有効化を記載**

`README.md` の matd 運用セクションへ、以下を追記:

```markdown
### matd の native バックエンド（Phase 5 M4）

`MAT_MATD_IFACE=<thread mesh iface>`（または `matd --iface <name>`）を与えると、
matd はホットパス（on/off・色・色温度・onoff on-off read）を組み込みの Matter
コントローラ（`mat-controller`）の warm CASE セッションで処理する。未指定なら
従来どおり全 op を chip-tool interactive server で処理する。fabric テーブルの
index が 1 でない環境（例: 本番相乗り）は `--fabric-index <n>` を指定する。
group 系・write・describe・任意 cluster の read/invoke は引き続き chip-tool 経由。
```

- [ ] **Step 3: `task check` で最終確認（ドキュメントのみ変更だが CI 緑を確認）**

Run: `task check 2>&1 | tail -8`
Expected: 緑。

- [ ] **Step 4: Commit**

```bash
git add ARCHITECTURE.md README.md
git commit -m "docs: reflect M4 (matd native adapter) in ARCHITECTURE/README

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:**
- 決定 1（`Arc<UdpTransport>` 化）→ Task 1。
- 決定 2（`NativeBackend`、per-node warm、無期限保持・失敗時再確立、mat スキーマ応答）→
  Task 3（構築/資格情報）+ Task 4（session 管理/メソッド）+ Task 5（応答 body 共用）。
- 決定 3（`run_op` 分岐、native 適用条件、Read の onoff 限定、エラー写像）→ Task 5 +
  Task 4（`map_session_err`）。
- 決定 4（lazy spawn）→ Task 6。
- 決定 5（move-to-color-temperature）→ Task 2。
- スコープ表の各モジュール → 対応 Task あり（main.rs=Task 7、E2E=Task 8、docs=Task 9）。
- CI 受け入れ 1（ループバック回帰）→ Task 1 Step 6。受け入れ 2（color-temp エンコーダ）→
  Task 2。受け入れ 3（session 再利用/再確立/routing）→ Task 4 + Task 5。受け入れ 4
  （native 無効時 chip-tool）→ Task 7 の `None` 経路（routing テストと main の分岐で担保）。
- 実機受け入れ 5〜9 → Task 8 の live テスト。
- 非ゴール（write/describe の native 化、group native 化、commissioning）→ 各 Task で
  スコープ外を明示（Task 5 の routing が chip-tool へ回す）。

**2. Placeholder scan:** プレースホルダなし。各コード手順に実体あり。Task 8 は実機依存の
ため `#[ignore]`/構文チェックのみ CI 実行（意図的、明記済み）。

**3. Type consistency:**
- `case::establish(Arc<UdpTransport>, ...)`（Task 1 定義 → Task 4 使用）一致。
- `SecureSession`（ライフタイム無し、Task 1 → Task 4 の `SessionConn` フィールド）一致。
- `encode_move_to_color_temperature_fields(u16, u16)`（Task 2 定義 → Task 4 使用）一致。
- `NativeBackend::{build, with_establisher, read_onoff, on, off, color, color_temp}`
  （Task 3/4 定義 → Task 5/7 使用）一致。
- `is_native_hotpath` / `hotpath_success_body` / `native_op`（Task 5 内で定義・使用）一致。
- `NativeConfig{store,iface,fabric_index,issuer_index}`（Task 3 定義 → Task 7 使用）一致。
- `server::serve(.., native: Option<Arc<NativeBackend>>)`（Task 5 拡張 → Task 7 呼び出し）一致。

**リスク注記（実装者向け）**: Task 4 の `with_session` のクロージャ `F: Fn(&mut Box<dyn
NodeConn>) -> Fut` が借用と `.await` をまたぐ点、`async-trait` 越しの `Send` 境界で
clippy/型エラーが出やすい。出た場合は (a) クロージャを `FnMut` に緩める、(b) `color`/
`color_temp` の `fields` を `Arc<Vec<u8>>` にして move-clone を避ける、(c) 最悪 `with_session`
をインライン展開して各メソッドに直書きする、のいずれかで解消する（挙動は不変）。
