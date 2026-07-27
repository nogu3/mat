# 購読の無音 probe + レポート破棄修正 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 無音 deadline での購読 teardown を probe（キャップ付き延長）で救済・計測し、backoff 上限を 60s に縮め、購読 pump のレポート破棄 2 経路を塞ぐ（issue #15 / 安定性監査 #1、1.6.0）。

**Architecture:** matd の購読 pump（`run_subscription_once`）は `pump_verdict` が `Silence` を返したとき、teardown 前に `SubscribeConn::probe()`（購読と同じ CASE セッション上の軽い read）を撃つ。成功なら deadline 再武装（連続 2 回まで）、失敗/キャップ到達で従来どおり teardown。mat-controller の `next_subscription_report` はデコード済みレポートを `respond_status` 失敗で道連れにせず deferred error として持ち越し、`respond_status` の ack 待ちループは続きチャンクを `peer_initiated` へ待避する。

**Tech Stack:** Rust (tokio, async-trait, tracing)。テストは既存足場（`FakeEstablisher`/`FakeSubConn` + `start_paused` 時計制御、session.rs の UDP ペア）を拡張。

**Spec:** `docs/superpowers/specs/2026-07-27-subscription-silence-probe-report-loss-design.md`

## Global Constraints

- バージョンは workspace `Cargo.toml` の `version = "1.5.0"` → `"1.6.0"`（Task 6 まで変更しない）
- コミット前に `task check`（fmt:check + clippy -D warnings + test）を通す
- コメント・ログ文言は既存ファイルの流儀に従う（コメントは日本語、tracing ログは英語）
- stdout 純 JSON / stderr tracing の設計ルールに変更なし（今回のログは全て stderr tracing）
- 実 node_id・実 IP をコード/コミットメッセージに書かない（サンプルは node 5 / 192.0.2.0/24）
- 各タスク完了ごとにコミット（メッセージは `git log` の既存スタイル: `fix(scope): 日本語要約` / `test(scope): ...` / `feat(scope): ...`）

---

### Task 1: session.rs 経路A — デコード済みレポートを respond_status 失敗で道連れにしない（deferred error）

**Files:**
- Modify: `crates/mat-controller/src/session.rs`（`SecureSession` 構造体 ~`:122-139`、`next_subscription_report` ~`:1070-1123`、tests 末尾）

**Interfaces:**
- Consumes: 既存の `SecureSession::next_subscription_report(&mut self, timeout: Duration, cfg: &MrpConfig) -> Result<ReportDataMessage, SessionError>`、tests の `bind_local()` / `device_datagram(...)` / `open_from_controller(...)` / `subscription_report_payload(sub_id, value, more)` / `fast_cfg()` ヘルパ
- Produces: `SecureSession` に private フィールド `deferred_sub_err: Option<SessionError>` を追加。外部シグネチャ変更なし（挙動のみ変更: respond_status 失敗時もレポートを `Ok` で返し、次回呼び出しが `Err` を返す）

- [ ] **Step 1: 失敗するテストを書く**

`session.rs` の `mod tests` 末尾（`next_subscription_report_drains_buffered_report_before_reading_socket` の後）に追加。デバイスが ReportData を送るが StatusResponse への ack を一切返さない → 現行実装は `respond_status` の Timeout が `?` で伝播しレポートごと `Err` になる。新契約: 1 回目はデコード済みレポートが `Ok` で返り、2 回目の呼び出しが deferred の `Err(Timeout)` を返す。

```rust
    /// 監査#1 経路A: 購読 report への StatusResponse が ack されず MRP 予算を
    /// 使い切っても、デコード済み report は道連れにしない。1 回目の呼び出しは
    /// Ok(report) を返し、失敗は deferred error として 2 回目の呼び出しで返る
    /// （read 経路の best-effort と違い、セッション死の即時検知は保つ）。
    #[tokio::test]
    async fn respond_status_failure_defers_error_and_still_delivers_report() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = match &*transport {
            Transport::Udp(u) => u.local_addr().unwrap(),
            Transport::Reliable(_) => unreachable!(),
        };
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        // デバイス: report を送るが、以後一切 ack しない（無応答デバイス）。
        let dev = tokio::spawn(async move {
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 700,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x7777,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let d = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(9, true, false),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&d, local).await.unwrap();
            // StatusResponse（+再送）を受けるが ack は返さない。
            let mut buf = [0u8; MAX_DATAGRAM];
            while tokio::time::timeout(Duration::from_secs(1), device.recv_from(&mut buf))
                .await
                .is_ok()
            {}
        });

        // 1 回目: respond_status は MRP 予算切れで失敗するが、report は返る。
        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .expect("decoded report must survive a failed status response");
        assert_eq!(rd.subscription_id, Some(9));

        // 2 回目: 持ち越された Timeout が返る（セッション死の即時検知）。
        let err = s
            .next_subscription_report(Duration::from_millis(100), &fast_cfg())
            .await
            .unwrap_err();
        assert!(matches!(err, SessionError::Timeout), "deferred: {err:?}");
        dev.await.unwrap();
    }
```

補足: `Transport` 列挙から local addr を取る形は同ファイルの既存 UDP テスト（`udp_device_initiated_report_is_acked_and_delivered` 周辺）を参照し、そこで使われている実際のパターンに合わせること（`Transport::Udp(Arc<UdpTransport>)` の形はテスト冒頭の使用例で確認できる）。

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller respond_status_failure_defers_error -- --nocapture`
Expected: FAIL — 1 回目の `next_subscription_report` が `Err(Timeout)` を返すため `expect` でパニック。

- [ ] **Step 3: 実装**

`SecureSession` 構造体にフィールド追加（`last_rx` の後）:

```rust
    /// 購読 pump: デコード済み report を返した後に respond_status が失敗した
    /// ときの持ち越しエラー。次の next_subscription_report 呼び出しが返す
    /// （report を道連れにせず、セッション死の即時検知も保つ — 監査#1 経路A）。
    deferred_sub_err: Option<SessionError>,
```

`SecureSession::new` の初期化に `deferred_sub_err: None,` を追加。

`next_subscription_report` の先頭（`let msg = if let Some(m) = ...` の前）に:

```rust
        if let Some(e) = self.deferred_sub_err.take() {
            return Err(e);
        }
```

末尾の応答部を差し替え:

```rust
        if !rd.suppress_response {
            self.respond_status(msg.proto.exchange_id, 0, cfg).await?;
        }
        Ok(rd)
```

↓

```rust
        if !rd.suppress_response {
            if let Err(e) = self.respond_status(msg.proto.exchange_id, 0, cfg).await {
                // デコード済み report を道連れにしない: report は届け、失敗は
                // 次回呼び出しへ持ち越す（pump は 5s スライスで即座に気づく）。
                tracing::debug!(error = %e, "sub pump: status response failed; delivering report, deferring error");
                self.deferred_sub_err = Some(e);
            }
        }
        Ok(rd)
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller respond_status_failure_defers_error`
Expected: PASS。既存の session テストも全て PASS: `cargo test -p mat-controller`

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/session.rs
git commit -m "fix(controller): 購読reportをStatusResponse失敗で道連れにしない（監査#1 経路A）"
```

---

### Task 2: session.rs 経路B — respond_status の ack 待ち中に届いた続きチャンクを待避する

**Files:**
- Modify: `crates/mat-controller/src/session.rs`（`respond_status` の ack 待ちループ ~`:937-967`、tests 末尾）

**Interfaces:**
- Consumes: Task 1 と同じテストヘルパ群 + `SecureSession::peer_initiated`（private、tests はサブモジュールなので直接読める）
- Produces: 外部シグネチャ変更なし。挙動のみ変更: `respond_status` が ack 待ち中に受けた IM ReportData を `peer_initiated` へ待避（ack 照合結果に関わらず）

- [ ] **Step 1: 失敗するテストを書く**

デバイスが chunk A（more_chunks=true）→ こちらの StatusResponse に **ReportData chunk B で piggyback ack** を返すシナリオ。現行実装は chunk B を ack 照合の副産物として破棄する。新契約: chunk B は `peer_initiated` へ待避され、次の `next_subscription_report` が配信する。

```rust
    /// 監査#1 経路B: respond_status の ack 待ち中に届いた続きチャンク
    /// （StatusResponse への ack を piggyback した ReportData）は破棄せず
    /// peer_initiated へ待避し、次の next_subscription_report が配信する。
    #[tokio::test]
    async fn report_chunk_arriving_during_status_ack_wait_is_not_lost() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = match &*transport {
            Transport::Udp(u) => u.local_addr().unwrap(),
            Transport::Reliable(_) => unreachable!(),
        };
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            // chunk A（more_chunks=true、needs_ack）。
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 800,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x8888,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            let d = seal_message(
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(11, true, true),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&d, local).await.unwrap();
            // StatusResponse を待ち、chunk B（piggyback ack、needs_ack）で応える。
            let mut buf = [0u8; MAX_DATAGRAM];
            loop {
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode != crate::im::OPCODE_STATUS_RESPONSE {
                    continue; // standalone ack 等は読み飛ばす
                }
                let header2 = MessageHeader {
                    session_id: LOCAL_SID,
                    security_flags: 0,
                    message_counter: 801,
                    source_node_id: None,
                    destination: Destination::None,
                };
                let proto2 = ProtocolHeader {
                    initiator: true,
                    needs_ack: true,
                    acked_counter: Some(h.message_counter),
                    opcode: crate::im::OPCODE_REPORT_DATA,
                    exchange_id: 0x8888,
                    protocol_id: crate::im::PROTOCOL_ID_IM,
                    vendor_id: None,
                };
                let d2 = seal_message(
                    &R2I,
                    &header2,
                    &proto2,
                    &subscription_report_payload(12, false, false),
                    DEV_NODE,
                )
                .unwrap();
                device.send_to(&d2, from).await.unwrap();
                break;
            }
            // chunk B への StatusResponse に standalone ack を返す（2 回目の
            // next_subscription_report を完走させる）。
            loop {
                let Ok(Ok((n, from))) =
                    tokio::time::timeout(Duration::from_secs(2), device.recv_from(&mut buf)).await
                else {
                    break;
                };
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode == crate::im::OPCODE_STATUS_RESPONSE {
                    let header3 = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: 802,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let proto3 = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: OPCODE_MRP_STANDALONE_ACK,
                        exchange_id: p.exchange_id,
                        protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                        vendor_id: None,
                    };
                    let d3 = seal_message(&R2I, &header3, &proto3, &[], DEV_NODE).unwrap();
                    device.send_to(&d3, from).await.unwrap();
                    break;
                }
            }
        });

        // 1 回目: chunk A が返り、その StatusResponse は chunk B の piggyback
        // ack で確認される。
        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(11));
        // chunk B は破棄されず待避済み。
        assert_eq!(s.peer_initiated.len(), 1, "chunk B must be stashed");
        // 2 回目: 待避済み chunk B がソケットを読まずに返る。
        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(12));
        dev.await.unwrap();
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-controller report_chunk_arriving_during_status_ack_wait -- --nocapture`
Expected: FAIL — `assert_eq!(s.peer_initiated.len(), 1)` が 0 で落ちる（chunk B が破棄されている）。

- [ ] **Step 3: 実装**

`respond_status` の ack 待ちループ内、`screen_with` が `Some(msg)` を返した後の処理を差し替え:

```rust
                if msg.proto.acked_counter == Some(our_counter) {
                    return Ok(());
                }
```

↓

```rust
                let acked = msg.proto.acked_counter == Some(our_counter);
                // ack 待ち中に届いた続きチャンク（device 発 ReportData）は
                // ack 照合の副産物として捨てない — screen_with のフィルタ落ち
                // 待避と同じ規律で peer_initiated へ積み、購読 API が消費する
                // （監査#1 経路B）。
                if msg.proto.protocol_id == im::PROTOCOL_ID_IM
                    && msg.proto.opcode == im::OPCODE_REPORT_DATA
                {
                    if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
                        tracing::warn!("peer-initiated report buffer full; dropping oldest");
                        self.peer_initiated.pop_front();
                    }
                    self.peer_initiated.push_back(msg);
                }
                if acked {
                    return Ok(());
                }
```

注意: `respond_status` 冒頭は `use crate::im;` 済みなので `im::` で参照できる。`msg` は待避で move するため `acked` の評価を先に行う。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller`
Expected: 新テスト含め全 PASS（`respond_status_retransmits_fast_after_recent_peer_rx` 等の既存 respond_status テストが無回帰であること）。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/session.rs
git commit -m "fix(controller): ack待ち中の続きチャンクをpeer_initiatedへ待避（監査#1 経路B）"
```

---

### Task 3: SubscribeConn::probe の追加（mat-controller 定数 + mat-native 実装 + fake）

**Files:**
- Modify: `crates/mat-controller/src/im.rs`（定数 2 つ追加、`CLUSTER_ON_OFF` 定義群 ~`:21` の並び）
- Modify: `crates/mat-native/src/lib.rs`（`SubscribeConn` trait ~`:164-179`、`SubscriptionSession` impl ~`:503-543`）
- Modify: `crates/mat-native/src/test_support.rs`（`FakeSubConn` / `FakeEstablisher`）

**Interfaces:**
- Consumes: `SecureSession::read_attribute(endpoint, cluster, attribute, &MrpConfig) -> Result<ImValue, SessionError>`（既存）、`map_session_err`（`mat-native/src/lib.rs:663`）、`take_failure(&AtomicUsize) -> bool`（test_support 既存）
- Produces:
  - `mat_controller::im::CLUSTER_BASIC_INFORMATION: u32 = 0x0028` / `ATTR_DATA_MODEL_REVISION: u32 = 0x0000`
  - trait メソッド `SubscribeConn::probe(&mut self) -> Result<(), MatError>`（デフォルト実装なし — 全実装が明示）
  - `FakeSubConn` に `pub fail_probe: Arc<AtomicUsize>` / `pub probe_calls: Arc<AtomicUsize>`、`FakeEstablisher` に同名フィールド（払い出す FakeSubConn と Arc 共有 — `fail_next_report` と同じ流儀）。Task 5 の matd テストはこの `est.fail_probe` / `est.probe_calls` を使う

- [ ] **Step 1: 失敗するテストを書く**

`mat-native/src/lib.rs` の既存 fake テスト群（`fake_sub_conn_next_report_fails_when_injected_after_establish` ~`:1024` 付近）の隣に追加:

```rust
    /// probe: 既定は成功、fail_probe 注入で残り回数だけ失敗、呼び出しは
    /// probe_calls で数えられる（matd の延長キャップテストの前提となる fake 契約）。
    #[tokio::test]
    async fn fake_sub_conn_probe_succeeds_counts_and_fails_when_injected() {
        use std::sync::atomic::Ordering;
        let est = crate::test_support::FakeEstablisher::default();
        let mut conn = est.establish_subscription(5).await.unwrap();
        assert!(conn.probe().await.is_ok());
        assert_eq!(est.probe_calls.load(Ordering::SeqCst), 1);
        est.fail_probe.store(1, Ordering::SeqCst);
        assert!(conn.probe().await.is_err());
        assert!(conn.probe().await.is_ok(), "失敗は注入した回数だけ");
        assert_eq!(est.probe_calls.load(Ordering::SeqCst), 3);
    }
```

- [ ] **Step 2: コンパイルが落ちることを確認**

Run: `cargo test -p mat-native fake_sub_conn_probe`
Expected: FAIL（コンパイルエラー: `probe` が trait に無い / フィールドが無い）。

- [ ] **Step 3: 実装**

`crates/mat-controller/src/im.rs` の定数群に追加（`CLUSTER_ON_OFF` 群の並びに合わせる）:

```rust
pub const CLUSTER_BASIC_INFORMATION: u32 = 0x0028;
pub const ATTR_DATA_MODEL_REVISION: u32 = 0x0000;
```

`crates/mat-native/src/lib.rs` の `SubscribeConn` trait にメソッド追加:

```rust
    /// 購読セッションの生存確認。無音 deadline 到達時、teardown 前に撃つ軽い
    /// read（endpoint 0 の basicinformation / data-model-revision）。成功 =
    /// セッション（CASE + 経路）は生きている。**購読自体の生存は証明しない**
    /// — デバイス側が購読を畳んでいても成功するため、呼び出し側（matd）は
    /// 連続成功をキャップする（spec 2026-07-27 無音 probe）。
    async fn probe(&mut self) -> Result<(), MatError>;
```

`SubscriptionSession` の impl に追加（`next_report` の後）:

```rust
    async fn probe(&mut self) -> Result<(), MatError> {
        use mat_controller::im::{ATTR_DATA_MODEL_REVISION, CLUSTER_BASIC_INFORMATION};
        self.session
            .read_attribute(0, CLUSTER_BASIC_INFORMATION, ATTR_DATA_MODEL_REVISION, &self.mrp)
            .await
            .map(|_| ())
            .map_err(map_session_err)
    }
```

`test_support.rs` の `FakeSubConn` にフィールド追加:

```rust
    /// 残り回数だけ `probe` を失敗させる（0 = 常に成功）。
    pub fail_probe: std::sync::Arc<AtomicUsize>,
    /// `probe` の呼び出し回数（FakeEstablisher と共有 — キャップ検証用）。
    pub probe_calls: std::sync::Arc<AtomicUsize>,
```

`FakeSubConn::default()` に `fail_probe: std::sync::Arc::default(), probe_calls: std::sync::Arc::default(),` を追加。impl に:

```rust
    async fn probe(&mut self) -> Result<(), MatError> {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.fail_probe) {
            return Err(MatError::new(
                ErrorKind::SessionFailed,
                "fake probe failure",
            ));
        }
        Ok(())
    }
```

`FakeEstablisher` にフィールド追加（doc コメントは `fail_next_report` の流儀に合わせる）:

```rust
    /// 払い出す `FakeSubConn` の `probe` を残り回数だけ失敗させる（0 = 常に成功）。
    pub fail_probe: std::sync::Arc<AtomicUsize>,
    /// 全 FakeSubConn と共有する probe 呼び出しカウンタ。
    pub probe_calls: std::sync::Arc<AtomicUsize>,
```

`FakeEstablisher::default()` と `establish_subscription` の `FakeSubConn` 構築に配線:

```rust
            fail_probe: std::sync::Arc::clone(&self.fail_probe),
            probe_calls: std::sync::Arc::clone(&self.probe_calls),
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native && cargo build --workspace`
Expected: 全 PASS + workspace 全体がコンパイルできる（trait 追加の波及先は SubscriptionSession / FakeSubConn の 2 実装のみのはず — 他に `impl SubscribeConn` があればここで発覚する）。

- [ ] **Step 5: コミット**

```bash
git add crates/mat-controller/src/im.rs crates/mat-native/src/lib.rs crates/mat-native/src/test_support.rs
git commit -m "feat(native): SubscribeConn::probe追加（無音deadline時の生存確認read）"
```

---

### Task 4: matd — should_probe 純関数 + BACKOFF_MAX 60s

**Files:**
- Modify: `crates/matd/src/subscription.rs`（定数 `:24-26`、`PumpEnd` の下に純関数追加、tests の `backoff_doubles_from_5s_capped_at_5min`）

**Interfaces:**
- Consumes: 既存 `PumpEnd` enum、`next_backoff`
- Produces: `pub(crate) const SILENCE_PROBE_MAX: u32 = 2;`、`pub(crate) fn should_probe(end: &PumpEnd, probes_used: u32) -> bool`（Task 5 の pump が使う）

- [ ] **Step 1: 失敗するテストを書く**

`subscription.rs` の tests に追加、既存 backoff テストを更新:

```rust
    #[test]
    fn should_probe_only_for_proven_silence_under_cap() {
        // 生存実績ありの無音だけが probe 対象。キャップ 2 で打ち止め。
        assert!(should_probe(&PumpEnd::Silence, 0));
        assert!(should_probe(&PumpEnd::Silence, 1));
        assert!(!should_probe(&PumpEnd::Silence, 2));
        // born-dead / op 相関は probe しない（probe 成功が何も証明しないため）。
        assert!(!should_probe(&PumpEnd::BornDeadSilence, 0));
        assert!(!should_probe(
            &PumpEnd::OpGrace {
                since_op: Duration::from_secs(10)
            },
            0
        ));
    }
```

既存 `backoff_doubles_from_5s_capped_at_5min` を 60s 上限へ改名・更新:

```rust
    #[test]
    fn backoff_doubles_from_5s_capped_at_60s() {
        use std::time::Duration;
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            next_backoff(Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(40)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_backoff(Duration::from_secs(60)),
            Duration::from_secs(60)
        );
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd should_probe backoff_doubles`
Expected: FAIL（`should_probe` 未定義のコンパイルエラー）。

- [ ] **Step 3: 実装**

定数変更（`:26` の doc コメントも更新）:

```rust
/// 再購読 backoff の初期値 / 上限。上限は当初 300s だったが、リンク回復後に
/// 最大 5 分無試行 = センサーの照明 1 回分不発になるため 60s へ短縮
/// （issue #15、blind 実測 1 日 3.7 時間の主因の一つ）。
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
```

`PumpEnd` 定義の直後に:

```rust
/// 無音 probe の連続延長キャップ。probe はセッション生存しか証明できず、
/// デバイス側が購読を畳んでいても成功する — 無制限延長はゾンビ購読の恒久盲目
/// になるため、デバイス発メッセージ無しの連続成功は 2 回まで（盲目上限
/// ≈ 3×deadline）。実レポート/keep-alive 受信でリセット。
pub(crate) const SILENCE_PROBE_MAX: u32 = 2;

/// 無音 deadline 到達時に probe を撃つべきか（純関数）。生存実績ありの無音
/// （`Silence`）だけが対象 — born-dead は「op 経路は生きてレポート経路だけ
/// 死んでいる」状態なので op 経路と同型の probe が成功しても何も証明しない。
pub(crate) fn should_probe(end: &PumpEnd, probes_used: u32) -> bool {
    matches!(end, PumpEnd::Silence) && probes_used < SILENCE_PROBE_MAX
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 新テスト PASS。統合テスト `establish_failures_climb_backoff_then_recover`（ラダー 5+10+20=35s、上限に届かない）と `backoff_resets_after_successful_establishment`（リセット判定閾値 15s vs 未リセット 40s）は 60s 上限でも成立するので無回帰のはず — 落ちたら期待値を確認。この時点で `should_probe` が未使用のため `dead_code` warning が出る場合は、Task 5 で使用されるまでの一時措置として `#[allow(dead_code)]` は付けず、テストから参照されていることで警告が出ないことを確認する（`pub(crate)` + テスト使用で通常は出ない）。

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): should_probe純関数とBACKOFF_MAX 60s短縮（issue#15）"
```

---

### Task 5: matd — pump へ probe を配線（延長・キャップ・リセット・ログ）

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`run_subscription_once` `:437-525`、tests）

**Interfaces:**
- Consumes: Task 3 の `SubscribeConn::probe` / `FakeEstablisher::{fail_probe, probe_calls}`、Task 4 の `should_probe` / `SILENCE_PROBE_MAX`、既存 `spawn_manager` テスト足場
- Produces: pump の新挙動（外部シグネチャ変更なし）。新ログ行: `silence probe passed; deadline re-armed`（info, fields: node_id, probes）/ `report pump ended (silence past deadline; probe failed)` / `report pump ended (silence past deadline; probe extensions exhausted)`

- [ ] **Step 1: 失敗する統合テストを書く（3 本）**

既存 `silent_subscription_dies_at_deadline_and_resubscribes`（born-dead 経路 = proven なし）は**そのまま残す**（probe は `Silence` のみ対象なので挙動不変 — これ自体が回帰チェック）。tests に追加:

```rust
    /// 無音 probe の延長とキャップ: 生存実績ありの購読が無音になったとき、
    /// probe 成功で deadline (90s) を 2 回まで再武装し、3 回目の deadline で
    /// teardown する（合計 ~270s + backoff 5s で再購読）。
    #[tokio::test(start_paused = true)]
    async fn silence_probe_extends_twice_then_dies() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let probe_calls = Arc::clone(&est.probe_calls);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // 生存実績を作る（proven=true — probe は Silence のみ対象）。
        live.lock().unwrap().push_back(onoff_report(1, false));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming);
        let t0 = tokio::time::Instant::now();

        // 以後完全無音。probe 成功 2 回分（90s×2）は再購読が起きない。
        assert!(
            tokio::time::timeout(Duration::from_secs(260), rx.recv())
                .await
                .is_err(),
            "probe 延長中に再購読してはいけない"
        );
        // 3 回目の deadline (270s) + backoff 5s で再購読の priming が届く。
        let ev = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("exhausted 後の再購読 priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(270),
            "キャップ 2 回分の延長を使い切ること: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(300),
            "キャップ後は次の deadline で死ぬこと: {elapsed:?}"
        );
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            2,
            "probe は延長 2 回分だけ（3 回目は撃たずに teardown）"
        );
    }

    /// probe 失敗は従来どおり deadline で即 teardown（延長しない）。
    #[tokio::test(start_paused = true)]
    async fn silence_probe_failure_tears_down_at_deadline() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        est.fail_probe.store(1, Ordering::SeqCst);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        live.lock().unwrap().push_back(onoff_report(1, false));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming);
        let t0 = tokio::time::Instant::now();

        // probe が失敗するので 1 回目の deadline (90s) + backoff 5s で再購読。
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("probe 失敗後の再購読 priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(90),
            "deadline より早く殺さない: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(120),
            "probe 失敗は延長せず即 teardown: {elapsed:?}"
        );
    }

    /// デバイス発メッセージで probe カウンタがリセットされ、次の無音でも
    /// 再びフルに 2 回延長できる。
    #[tokio::test(start_paused = true)]
    async fn device_message_resets_probe_budget() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let probe_calls = Arc::clone(&est.probe_calls);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        live.lock().unwrap().push_back(onoff_report(1, false));
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();

        // 1 回目の deadline で probe 延長（probes_used=1）を消費させる。
        // 95s 待って probe 1 回分を確実に跨ぐ（イベントは来ない）。
        assert!(
            tokio::time::timeout(Duration::from_secs(95), rx.recv())
                .await
                .is_err()
        );
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1, "延長 1 回消費済み");
        // デバイス発メッセージ → カウンタリセット。
        live.lock().unwrap().push_back(onoff_report(1, true));
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("live event 2")
            .unwrap();
        let t0 = tokio::time::Instant::now();

        // リセット後も再びフルに 2 回延長 → 270s±で teardown。
        assert!(
            tokio::time::timeout(Duration::from_secs(260), rx.recv())
                .await
                .is_err(),
            "リセット後の延長 2 回分は再購読しない"
        );
        let ev = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("再購読 priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_secs(270), "{elapsed:?}");
        assert_eq!(probe_calls.load(Ordering::SeqCst), 3, "1 + リセット後 2");
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd silence_probe device_message_resets`
Expected: FAIL — 現行 pump は probe を撃たないため、延長テストは 90s+5s で再購読イベントが届いて `timeout(...).is_err()` の assert が落ちる。

- [ ] **Step 3: 実装**

`run_subscription_once` の pump ループを変更。`let mut proven = false;` の隣に `let mut probes_used: u32 = 0;` を追加し、verdict 分岐を差し替え:

```rust
        if let Some(end) = pump_verdict(
            proven,
            last_msg.elapsed(),
            deadline,
            health.pending_elapsed(node_id),
        ) {
            if should_probe(&end, probes_used) {
                match conn.probe().await {
                    Ok(()) => {
                        // セッションは生きている — teardown せず deadline を
                        // 再武装（連続 SILENCE_PROBE_MAX 回まで）。この行の
                        // 数が「従来なら無駄に殺していた回数」の実測になる。
                        probes_used += 1;
                        last_msg = tokio::time::Instant::now();
                        tracing::info!(
                            node_id,
                            probes = probes_used,
                            "silence probe passed; deadline re-armed"
                        );
                        continue;
                    }
                    Err(e) => {
                        health.clear_pending(node_id);
                        tracing::info!(
                            node_id,
                            silent_s = last_msg.elapsed().as_secs(),
                            kind = ?e.kind,
                            detail = %e.detail,
                            "report pump ended (silence past deadline; probe failed)"
                        );
                        return Ok(());
                    }
                }
            }
            // 再購読直後に同じ pending で即再発火しないよう先に消す
            // （probe 継続時は消さない — op 相関シグナルを保つ）。
            health.clear_pending(node_id);
            match end {
                PumpEnd::OpGrace { since_op } => tracing::info!(
                    node_id,
                    since_op_s = since_op.as_secs(),
                    "report pump ended (op-correlated: no device message after op)"
                ),
                PumpEnd::BornDeadSilence => tracing::info!(
                    node_id,
                    silent_s = last_msg.elapsed().as_secs(),
                    "report pump ended (born-dead: no device message since establishment)"
                ),
                PumpEnd::Silence => tracing::info!(
                    node_id,
                    silent_s = last_msg.elapsed().as_secs(),
                    probes = probes_used,
                    "report pump ended (silence past deadline; probe extensions exhausted)"
                ),
            }
            return Ok(());
        }
```

`Ok(Some(msg))` 受信分岐に probe カウンタリセットを追加:

```rust
            Ok(Some(msg)) => {
                proven = true;
                last_msg = tokio::time::Instant::now();
                probes_used = 0;
                health.clear_pending(node_id);
                ...（既存のまま）
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 新 3 本 + 既存全テスト PASS。特に `silent_subscription_dies_at_deadline_and_resubscribes`（born-dead、probe 対象外なので 90s 死のまま）と `op_grace_triggers_fast_resubscribe` / `changing_op_with_silent_device_triggers_fast_resubscribe`（OpGrace、probe 対象外）が無回帰であること。

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): 無音deadlineでprobe延長（キャップ2）— teardown 73%の救済と偽陽性計測（issue#15）"
```

---

### Task 6: バージョン 1.6.0 + task check

**Files:**
- Modify: `Cargo.toml`（workspace `version = "1.5.0"` → `"1.6.0"`）
- Modify: `Cargo.lock`（ビルドで自動更新）

**Interfaces:**
- Consumes: Task 1〜5 が全てコミット済みであること
- Produces: 1.6.0 のリリース可能な main 候補（マージ前 E2E は Task 7）

- [ ] **Step 1: バージョン更新**

`Cargo.toml` の `[workspace.package]` セクション: `version = "1.5.0"` → `version = "1.6.0"`

- [ ] **Step 2: CI 相当を通す**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / test 全 PASS。`Cargo.lock` が 1.6.0 で更新される。

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.6.0（無音probe+backoff60s+レポート破棄修正）"
```

---

### Task 7: jarvis 実機 E2E（マージ前・本番未置換）→ マージ → デプロイ

このタスクはメインセッション（人間 + AI）で実施する — subagent に委任しない（ssh・実機操作・ユーザー確認を含むため）。手順は確立済みの隔離 matd 方式（メモリ [[jarvis-matd-deploy]] / spec §4）。

- [ ] **Step 1: クロスビルドと転送**

```bash
task dist:arm64
scp dist/arm64/mat jarvis:~/mat.new
scp dist/arm64/matd jarvis:~/matd.new
```

- [ ] **Step 2: 隔離 matd を起動**

jarvis 上で: `~/.config/mat` を `/tmp` 配下へコピーし `nodes.json` を 1 ノード（照明系で可）に絞る → `matd.new --store <コピー> --socket /tmp/<dir>/t.sock` を起動（`MAT_MATD_IFACE` 等は本番 unit の Environment に合わせる）。本番 KVS への書き込みゼロを厳守。

- [ ] **Step 3: E2E 確認項目**

1. 購読確立（`subscription established` ログ）+ `mat.new --matd /tmp/<dir>/t.sock listen` でイベント受信（無回帰）。
2. probe の実機観測: 隔離 matd と本番 matd の追い出し合戦（KeepSubscriptions=false）で無音死を誘発し、`silence probe passed; deadline re-armed` または `probe failed`/`extensions exhausted` のログ形状を journal（またはファイル直書きログ）で確認。
3. teardown ログの新形状 3 種（passed / probe failed / exhausted）に文字化け・フィールド欠けが無いこと。
4. 終了後 `/tmp` の store コピー（認証情報）を必ず削除。

- [ ] **Step 4: マージと push**

E2E 合格をユーザーへ報告し、確認を得てから: ブランチを main へマージ（`git merge --no-ff`）→ `git push`。

- [ ] **Step 5: 本番デプロイ**

despliegue skill の手順どおり: `*.new` を backup（`.bak-1.5.0`）→ `install -m755` → `systemctl --user restart matd`。スモーク: 再起動 ~2 分後に購読確立本数（`ss -uanp` の matd UDP ソケット数）、`journalctl` で WARN/ERROR ゼロ、warm read、`report pump ended` 内訳と `silence probe passed` の初期観測。メモリ（jarvis-matd-deploy / mat-stability-audit-backlog / matd-subscribe-listen）を更新。

---

## Self-Review 記録

- **Spec coverage:** §1 probe+キャップ+ログ 3 種 = Task 3/4/5、§1 backoff 60s = Task 4、§2 経路A = Task 1、§2 経路B = Task 2、§3 テスト 6 本 = Task 1(⑤)/2(⑥)/5(①〜④は 3 本に統合: 延長+キャップ / 失敗 / リセット)、§4 出荷 = Task 6/7。`clear_pending` を teardown 時のみに限る変更 = Task 5 Step 3 のコード（probe 継続パスでは呼ばない）。漏れなし。
- **Placeholder scan:** 全ステップに実コード/実コマンドあり。Task 7 のみ実機手順のため箇条書きだが、参照先（メモリ・spec §4）と具体コマンドを明記済み。
- **Type consistency:** `should_probe(&PumpEnd, u32) -> bool`（Task 4 定義、Task 5 使用）、`SubscribeConn::probe(&mut self) -> Result<(), MatError>`（Task 3 定義、Task 5 使用）、`FakeEstablisher::{fail_probe, probe_calls}: Arc<AtomicUsize>`（Task 3 定義、Task 5 使用）で一致。
