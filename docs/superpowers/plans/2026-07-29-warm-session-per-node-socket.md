# warm session の UDP ソケットをノードごとに分離 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `CaseEstablisher::establish` をノードごとの専用 UDP ソケットに切り替え、並行 op が互いの応答を黙って破棄する問題（安定性監査 Tier 1 #3）を解消する。

**Architecture:** 購読側 `establish_subscription` が既に実装している「ノードごとに専用 `UdpTransport` + 専用 CASE + local port の info ログ」の規律を op 側 `establish` にも適用する。共有 transport は group multicast 送信（`GroupCtx`）専用として残る。回帰テストは、既存のループバック CASE 応答器（`case_self_handshake.rs`）を mat-controller の feature-gated `test_support` モジュールへ抽出し、mat-native のユニットテストから並行 2 応答器で釘打つ。

**Tech Stack:** Rust / tokio / 既存の mat-controller CASE 応答器足場。新規依存なし。

**Spec:** `docs/superpowers/specs/2026-07-29-warm-session-per-node-socket-design.md`

## Global Constraints

- ブランチ: `feat/audit3-per-node-op-socket`（作成済み、spec コミット済み）。
- バージョン: 1.8.0（workspace `Cargo.toml` の `version`、Task 3 で上げる）。
- コミット前に `task check`（fmt:check + clippy -D warnings + test）合格が必須。
- commissioning / PASE / BLE / group 送信 / 購読の各経路は挙動不変。matd 側
  （`crates/matd/`）は一切変更しない。
- 新規 cargo 依存の追加禁止（feature 追加と dev-dependencies の feature 指定のみ可）。
- 実機 E2E（Task 4）はメインセッション（オーケストレータ）が実施する。サブエージェント
  は Task 4 を実行しないこと。

---

### Task 1: mat-controller — CASE 応答器足場を feature-gated `test_support` へ抽出

**Files:**
- Create: `crates/mat-controller/src/test_support.rs`
- Modify: `crates/mat-controller/src/lib.rs`（module 宣言 1 行追加）
- Modify: `crates/mat-controller/Cargo.toml`（feature + self dev-dependency）
- Modify: `crates/mat-controller/tests/case_self_handshake.rs`（抽出した分を削除し import に置換）

**Interfaces:**
- Consumes: 既存の `tests/case_self_handshake.rs` の応答器実装（テスト関数以外の全て）。
- Produces（Task 2 が使う公開 API — `mat_controller::test_support::` 配下）:
  - `pub const IPK: [u8; 16]`
  - `pub const INITIATOR_NODE_ID: u64`
  - `pub const NODE01_NOC: &[u8]` / `pub const NODE01_PRIV: &[u8]` /
    `pub const ICA01: &[u8]` / `pub const ROOT01_CHIP: &[u8]` /
    `pub const ROOT01_PRIV: &[u8]`
  - `pub fn fast_cfg() -> MrpConfig`
  - `pub async fn responder_task(transport: UdpTransport, initiator_node_id: u64, responder_node_id: u64, noc_tlv: Vec<u8>, icac_tlv: Vec<u8>, op_priv: [u8; 32], root_tlv: Vec<u8>) -> SocketAddr`
    — **戻り値が新規**: 応答器が観測した initiator のソースアドレス（Task 2 の
    「ソケットが分かれている」assert の観測点）。それ以外のシグネチャは現行の
    `responder_task` と同一。

- [ ] **Step 1: feature と self dev-dependency を追加**

`crates/mat-controller/Cargo.toml` の**既存 `[features]` セクション**（`ble` がある）に
1 エントリ追記する（セクション見出しを重複させない）:

```toml
# テスト専用 CASE 応答器（test_support モジュール）。プロダクションビルドには
# 入れない — dev-dependencies 経由でテストビルドのみ有効化する。
test-responder = []
```

同ファイルの `[dev-dependencies]` に self 依存を追加（自クレートの統合テストで
feature を有効化する cargo の標準トリック。dev-dependency の循環は cargo が許容する）:

```toml
mat-controller = { path = ".", features = ["test-responder"] }
```

- [ ] **Step 2: `test_support.rs` へ応答器を抽出**

`crates/mat-controller/tests/case_self_handshake.rs` から **`#[tokio::test] async fn case_establishes_and_reads_over_loopback` 以外の全て**（冒頭コメント、`use` 群、定数群（`OPCODE_*` / `INFO_*` / `*_NONCE` / `PROTO_SECURE_CHANNEL` / `IPK` / `INITIATOR_NODE_ID` / fixtures の `include_bytes!` 定数）、`fast_cfg`、crypto ヘルパ（`hkdf16`/`hkdf48`/`sha256`/`random_secret`/`eph_pub`/`ecdh`/`encode_tbs`）、フレーミングヘルパ（`build_unsecured`/`recv_dg`/`decode_unsecured`）、パーサ（`Sigma1` 構造体/`parse_sigma1`/`parse_sigma3_encrypted`/`parse_tbe`）、`report_data_false_suppressed`、`responder_task`）を `crates/mat-controller/src/test_support.rs` に移動する。機械的な移動 + 以下の調整のみ（ロジック変更禁止）:

1. `use mat_controller::...` → `use crate::...` に書き換え（`mat_controller::crypto::encrypt_payload` などの完全修飾参照も `crate::` へ）。
2. `include_bytes!` のパスを `tests/fixtures/...` 起点から `../tests/fixtures/...` に変更（fixtures ファイル自体は移動しない）:

```rust
pub const NODE01_NOC: &[u8] = include_bytes!("../tests/fixtures/node01_01_chip.bin");
pub const NODE01_PRIV: &[u8] = include_bytes!("../tests/fixtures/node01_01_privkey.bin");
pub const ICA01: &[u8] = include_bytes!("../tests/fixtures/ica01_chip.bin");
pub const ROOT01_CHIP: &[u8] = include_bytes!("../tests/fixtures/root01_chip.bin");
pub const ROOT01_PRIV: &[u8] = include_bytes!("../tests/fixtures/root01_privkey.bin");
```

3. Interfaces 節に挙げた項目を `pub` にする（ヘルパ・パーサ・`Sigma1` は private のまま）。
4. `responder_task` の戻り値を `-> SocketAddr` にし、関数末尾（ReportData 送信後）に `initiator_addr` を返す行を足す:

```rust
    transport
        .send_to(&report_dg, initiator_addr)
        .await
        .expect("send report data");
    initiator_addr
}
```

5. モジュール冒頭に doc コメントを付ける（現行テストの冒頭コメントの応答器説明部を流用し、「テスト専用・`test-responder` feature 限定・プロダクション非搭載」を明記）。

`crates/mat-controller/src/lib.rs` に module 宣言を追加:

```rust
#[cfg(feature = "test-responder")]
#[doc(hidden)]
pub mod test_support;
```

- [ ] **Step 3: `case_self_handshake.rs` を抽出後の足場で書き直す**

テストファイルには `#[tokio::test]` 関数 1 本と最小限の `use` だけ残す。テスト本体のロジックは不変で、参照先を `mat_controller::test_support::{...}` に切り替える:

```rust
use mat_controller::test_support::{
    fast_cfg, responder_task, ICA01, INITIATOR_NODE_ID, IPK, NODE01_NOC, NODE01_PRIV,
    ROOT01_CHIP, ROOT01_PRIV,
};
```

`responder.await` の戻り値型が `SocketAddr` になるので、initiator の local addr と
一致することを assert に昇格させる（応答器観測点の健全性確認 — Task 2 の
port assert と同じ観測系）。initiator transport の構築を `Arc<UdpTransport>` に
束縛してから包む形に変え、`local_addr()`（既存 pub API）を先に取る:

```rust
    let initiator_udp = Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let initiator_local = initiator_udp.local_addr().unwrap();
    let initiator_transport = Arc::new(Transport::Udp(Arc::clone(&initiator_udp)));
    // ...（establish + read_attribute は現行のまま）...
    let observed = responder.await.expect("responder task panicked");
    assert_eq!(observed, initiator_local, "responder saw the initiator's socket");
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller --test case_self_handshake`
Expected: PASS（1 test）。これで self dev-dependency の feature 有効化も検証される。

Run: `cargo test -p mat-controller`
Expected: PASS（全既存テスト無風）。

- [ ] **Step 5: fmt + clippy**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: 警告ゼロ。

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller
git commit -m "test(mat-controller): CASE応答器足場を test-responder feature の test_support へ抽出"
```

---

### Task 2: mat-native — establish() をノード専用ソケット化 + 並行 2 応答器テスト

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`CaseEstablisher` 構造体 :396-401、`establish` :405-442、`Engine::build` :344-376、`#[cfg(test)]` テスト追加）
- Modify: `crates/mat-native/Cargo.toml`（dev-dependencies に feature 付き mat-controller）

**Interfaces:**
- Consumes: Task 1 の `mat_controller::test_support`（`responder_task` が `SocketAddr` を返す）。
- Produces: `CaseEstablisher` は `creds` / `scope_id` / `resolver` の 3 フィールドに
  なる（`transport` フィールド削除）。`Establisher` trait / `NodeConn` / matd から見た
  挙動は不変。

- [ ] **Step 1: dev-dependencies を追加**

`crates/mat-native/Cargo.toml` の `[dev-dependencies]` に追加（mat-controller は
normal 依存に既にあるが、テストビルドでのみ `test-responder` feature を足すため
dev 側にも書く）:

```toml
mat-controller = { workspace = true, features = ["test-responder"] }
```

- [ ] **Step 2: 失敗するテストを書く**

`crates/mat-native/src/lib.rs` の既存 `#[cfg(test)]` テスト群の隣に追加。
**変更後の** `CaseEstablisher`（`transport` フィールド無し）を前提に書く:

```rust
#[cfg(test)]
mod dedicated_op_socket_tests {
    use super::*;
    use mat_controller::cert::MatterCert;
    use mat_controller::kvs::SelfIssueMaterials;
    use mat_controller::test_support as case_ts;
    use mat_controller::transport::UdpTransport;
    use std::net::Ipv6Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 呼び出し順に固定ポートを払い出す fake resolver。2 応答器が同一
    /// fixture 識別（同一 node_id）なので、どちらの establish がどちらの
    /// 応答器に着いても対称で問題ない。
    struct FixedPortResolver {
        ports: Vec<u16>,
        next: AtomicUsize,
    }

    #[async_trait]
    impl Resolver for FixedPortResolver {
        async fn resolve(
            &self,
            _scope_id: u32,
            _cfid: [u8; 8],
            _node_id: u64,
            _timeout: Duration,
        ) -> Result<dnssd::ResolvedNode, dnssd::DnssdError> {
            let i = self.next.fetch_add(1, Ordering::SeqCst);
            Ok(dnssd::ResolvedNode {
                port: self.ports[i],
                addresses: vec![Ipv6Addr::LOCALHOST],
                session_idle_interval_ms: Some(50),
                session_active_interval_ms: Some(50),
            })
        }
    }

    /// 監査#3 の釘打ち: 異なるノードへの並行 op が互いの応答を吸わない。
    /// ループバックに CASE 応答器を 2 つ立て、並行 establish + read が両方
    /// 成功し、応答器の観測した initiator ソースポートが異なる（= ノード
    /// ごとの専用ソケット）ことを assert する。共有ソケットに退行すると
    /// ポートが一致して確実に落ちる。
    #[tokio::test]
    async fn concurrent_establishes_use_dedicated_sockets() {
        let noc = MatterCert::parse(case_ts::NODE01_NOC).expect("parse fixture NOC");
        let responder_node_id = noc.node_id().expect("node id");
        let fabric_id = noc.fabric_id().expect("fabric id");
        let op_priv: [u8; 32] = case_ts::NODE01_PRIV.try_into().unwrap();

        // 応答器 2 つ（同一識別・別ポート）。
        let mut handles = Vec::new();
        let mut ports = Vec::new();
        for _ in 0..2 {
            let t = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap();
            ports.push(t.local_addr().unwrap().port());
            handles.push(tokio::spawn(case_ts::responder_task(
                t,
                case_ts::INITIATOR_NODE_ID,
                responder_node_id,
                case_ts::NODE01_NOC.to_vec(),
                case_ts::ICA01.to_vec(),
                op_priv,
                case_ts::ROOT01_CHIP.to_vec(),
            )));
        }

        let materials = SelfIssueMaterials {
            rcac: case_ts::ROOT01_CHIP.to_vec(),
            root_private_key: case_ts::ROOT01_PRIV.try_into().unwrap(),
            ipk_operational: case_ts::IPK,
            node_id: case_ts::INITIATOR_NODE_ID,
            fabric_id,
        };
        let creds = FabricCredentials::from_self_issued(materials).expect("creds");
        let est = CaseEstablisher {
            creds: Arc::new(creds),
            scope_id: 0,
            resolver: Arc::new(FixedPortResolver {
                ports,
                next: AtomicUsize::new(0),
            }),
        };

        let (a, b) = tokio::join!(
            est.establish(responder_node_id),
            est.establish(responder_node_id)
        );
        let mut a = a.expect("establish 1");
        let mut b = b.expect("establish 2");
        let (ra, rb) = tokio::join!(a.read_onoff(1), b.read_onoff(1));
        // 応答器は on-off=false を返す（clippy: bool_assert_comparison を避け assert! で）。
        assert!(!ra.expect("read 1"));
        assert!(!rb.expect("read 2"));

        let sa = handles.pop().unwrap().await.expect("responder 2");
        let sb = handles.pop().unwrap().await.expect("responder 1");
        assert_ne!(
            sa.port(),
            sb.port(),
            "op sockets must be dedicated per establish (audit #3)"
        );
    }
}
```

- [ ] **Step 3: テストが失敗することを確認**

Run: `cargo test -p mat-native concurrent_establishes_use_dedicated_sockets`
Expected: **コンパイルエラー**（現行 `CaseEstablisher` には `transport` フィールドが
必須で、テストの構築式にフィールドが足りない）。

- [ ] **Step 4: 実装 — establish のノード専用ソケット化**

`crates/mat-native/src/lib.rs` を 3 箇所変更する。

(1) `CaseEstablisher` から `transport` フィールドを削除:

```rust
/// 実確立器: 保持した資格情報で mDNS 解決 → CASE。op セッションのソケットは
/// ノードごとに専用（監査#3）— 共有ソケットは group multicast 送信のみ。
struct CaseEstablisher {
    creds: Arc<FabricCredentials>,
    scope_id: u32,
    resolver: Arc<dyn Resolver>,
}
```

(2) `establish` の冒頭で専用ソケットを bind し、CASE 成功時に info ログ
（`establish_subscription` :450-478 と同じ形。既存の resolve / peer ループ /
エラー写像は不変）:

```rust
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        // op 専用ソケット: 共有ソケットでは並行 op が他ノード宛の応答を
        // recv して screen で捨てる（監査#3）。購読側と同じ規律で
        // ノードごとに専用 UdpTransport + 専用 CASE を確立する。
        let transport = UdpTransport::bind().await.map_err(|e| {
            MatError::new(ErrorKind::Other, format!("native: bind op udp: {e}"))
        })?;
        // local port は実機切り分け（ss -uanp / tcpdump 突合)の鍵なので
        // 確立ごとに可視化する（購読側の同名ログと対）。
        let local = transport.local_addr().ok();
        let transport = Arc::new(Transport::Udp(Arc::new(transport)));
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let mut last: Option<MatError> = None;
        for peer in peers {
            match case::establish(Arc::clone(&transport), peer, &self.creds, node_id, &mrp).await
            {
                Ok(session) => {
                    tracing::info!(
                        node_id,
                        local = %local.map(|a| a.to_string()).unwrap_or_default(),
                        %peer,
                        "op transport bound (dedicated socket + CASE)"
                    );
                    return Ok(Box::new(SessionConn { session, mrp }));
                }
                Err(e) => {
                    last = Some(MatError::new(
                        ErrorKind::SessionFailed,
                        format!("native: CASE via {peer}: {e}"),
                    ));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            MatError::new(
                ErrorKind::Unreachable,
                format!("native: no addresses resolved for node {node_id}"),
            )
        }))
    }
```

(3) `Engine::build_with_resolver` の `CaseEstablisher` 構築から transport を外す。
`build` の `UdpTransport::bind()` と `Arc::new(transport)` は **GroupCtx 用に残す**。
:368-370 の「CaseEstablisher は Arc<Transport> を取る一方…」コメントは実態に
合わせて書き換える:

```rust
        let transport = Arc::new(transport);
        let group = group::GroupCtx {
            main_ini,
            counter_path: cfg.store.join("native_group_counter"),
            fabric_index: cfg.fabric_index,
            fabric_id,
            node_id,
            scope_id,
            dest_port: MATTER_PORT,
            transport: Arc::clone(&transport),
            sender: tokio::sync::Mutex::new(None),
        };
        // build が bind する共有 UdpTransport は group multicast 送信専用。
        // op / 購読の unicast セッションはノードごとに専用ソケットを bind する
        // （監査#3 / 購読 spec）。
        let establisher = CaseEstablisher {
            creds: Arc::new(creds),
            scope_id,
            resolver,
        };
```

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-native concurrent_establishes_use_dedicated_sockets`
Expected: PASS

- [ ] **Step 6: 既存テストの無風確認**

Run: `cargo test`
Expected: 全クレート PASS（op の意味論は不変。FakeConn / バイナリ統合テストに
修正が要らないこと自体が受け入れ基準）。

- [ ] **Step 7: fmt + clippy**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: 警告ゼロ。

- [ ] **Step 8: Commit**

```bash
git add crates/mat-native
git commit -m "feat(mat-native): op セッションのUDPソケットをノードごとに専用化（監査#3）"
```

---

### Task 3: バージョン 1.8.0 + 最終チェック

**Files:**
- Modify: `Cargo.toml`（workspace.package の `version = "1.7.0"` → `"1.8.0"`）
- Modify: `Cargo.lock`（ビルドで自動追随）

**Interfaces:**
- Consumes: Task 1-2 の全変更。
- Produces: リリース可能な 1.8.0（実機 E2E 待ち状態）。

- [ ] **Step 1: バージョンを上げる**

`Cargo.toml` の `[workspace.package]` セクション:

```toml
version = "1.8.0"
```

- [ ] **Step 2: CI 相当の全チェック**

Run: `task check`
Expected: fmt:check / clippy / test すべて合格。`Cargo.lock` が 1.8.0 に追随して
更新されることを `git status` で確認。

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.8.0（op ソケットのノード別分離 — 監査#3）"
```

---

### Task 4: 実機 E2E（メインセッション実施 — サブエージェント対象外）

マージ前必須（ユーザー方針: mat の変更は main マージ前に jarvis 実機 E2E）。
jarvis 上の隔離 matd 方式（別 socket + store コピー、本番 matd は触らない。
過去の隔離手順は `scripts/e2e-m4.sh` と memory `jarvis-matd-deploy` 参照）。

- [ ] **Step 1: aarch64 ビルドと転送**

`task dist:arm64` で `dist/arm64/{mat,matd}` を作り、jarvis へ `*.new` として scp
（本番バイナリは置換しない）。

- [ ] **Step 2: 隔離 matd 起動**

store コピー（台帳 2 ノード以上）+ 別 socket で `matd.new` を起動し、起動ログと
購読確立を確認。

- [ ] **Step 3: 並行 op の交差確認**

2 ノードへの read を同時発行:

```bash
MAT_MATD_SOCKET=<隔離socket> ./mat.new read <node_a> 1 onoff on-off &
MAT_MATD_SOCKET=<隔離socket> ./mat.new read <node_b> 1 onoff on-off &
wait
```

Expected: 両方 exit 0 + JSON。matd ログに `op transport bound (dedicated socket + CASE)`
がノードごとに local port 付きで出る。

- [ ] **Step 4: ソケット分離の観測**

`ss -uanp | grep matd` で、warm session 数 + 購読数 + group 1 本に一致する UDP
ソケット数を確認。数分アイドル後に再確認し、fd リークがないこと。

- [ ] **Step 5: スモーク**

on/off/level の通常 op と `mat listen`（購読経路無風）を隔離環境で 1 周。
合格したら隔離 matd を停止・掃除。

---

## 完了後

`superpowers:finishing-a-development-branch` で main マージ → `despliegue` スキルで
jarvis 本番デプロイ（1.8.0）→ memory 更新（監査バックログ #3 完了、
jarvis-matd-deploy のバージョン記録）。
