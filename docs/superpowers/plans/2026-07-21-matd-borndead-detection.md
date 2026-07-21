# matd born-dead 購読の高速検知 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd の常駐購読が born-dead（確立後デバイス完全無音）に陥ったとき、op 相関で数秒〜グレース 10s、受動でも max_interval+30s で検知して再購読する。

**Architecture:** `mat-controller::session` に無音専用の `SessionError::Silence` を分離し、`mat-native::SubscribeConn::next_report` を「無音 = `Ok(None)`」契約に変更。matd 側は新設の共有ヘルス表 `SubHealth`（server op 経路が書き、pump が読む）と、`next_report` の 5 秒スライス化で op 相関検知を実現する。ワイヤ上の購読交渉パラメータ・mat CLI・JSON スキーマは不変。

**Tech Stack:** Rust (tokio, tracing), 既存 FakeEstablisher/FakeSubConn テストハーネス, `tokio::test(start_paused)` の仮想時計。

**Spec:** `docs/superpowers/specs/2026-07-21-matd-borndead-detection-design.md`

## Global Constraints

- コミット前に必ず `task check`（fmt:check + clippy -D warnings + test）を通す。
- stdout は純粋 JSON のみ / 診断は stderr の `tracing`（CLAUDE.md 設計ルール）。
- 購読交渉パラメータは不変: `SUBSCRIBE_MIN_INTERVAL_FLOOR_S=0` / `SUBSCRIBE_MAX_INTERVAL_CEILING_S=300` / `SUBSCRIBE_KEEP_SUBSCRIPTIONS=false`。
- README / ARCHITECTURE は変更しない（matd 内部挙動で契約ではない）。
- exit code 契約不変（`SessionError::Silence` も MatError では kind=Timeout のまま）。
- コミットメッセージは日本語 conventional commits。末尾に以下を付ける:
  ```
  Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01D1nMZo3ak5uZh4ibnmTrpR
  ```

---

### Task 1: `SessionError::Silence` の分離（mat-controller）

購読の無音 deadline 切れと MRP 送信 ack 切れが同じ `SessionError::Timeout`（Display: `no acknowledgement within MRP retry budget`）に潰れており、pump 終了ログの切り分けを壊している。無音専用 variant を分離する。

**Files:**
- Modify: `crates/mat-controller/src/session.rs:68-90`（enum + Display）
- Modify: `crates/mat-controller/src/session.rs:1061-1114`（`next_subscription_report` の 2 箇所の Timeout 返却）
- Test: `crates/mat-controller/src/session.rs`（tests mod 内の既存テスト `next_subscription_report_times_out_on_silence` を更新）

**Interfaces:**
- Produces: `SessionError::Silence`（unit variant）。Display は `no device-initiated message within the subscription deadline`。`next_subscription_report` は無音 deadline 切れでこれを返す（MRP 送信系の `SessionError::Timeout` は従来どおり）。Task 2 が `SubscriptionSession::next_report` でこれを `Ok(None)` に写像する。

- [ ] **Step 1: 既存テストを Silence 期待に変える（failing test）**

`crates/mat-controller/src/session.rs` の既存テスト（2711 行付近）を変更:

```rust
    /// 無音は Silence（上位=matd が購読死亡と判定して再購読する）。MRP 送信
    /// ack 切れの Timeout とは別 variant（pump 終了ログの切り分けに必須）。
    #[tokio::test]
    async fn next_subscription_report_times_out_on_silence() {
        let (mut s, _dev) = reliable_session_pair();
        assert!(matches!(
            s.next_subscription_report(Duration::from_millis(100), &fast_cfg())
                .await,
            Err(SessionError::Silence)
        ));
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller next_subscription_report_times_out_on_silence`
Expected: コンパイルエラー（`Silence` variant 未定義）

- [ ] **Step 3: variant 追加 + `next_subscription_report` の返却変更**

`session.rs:68` の enum に variant 追加:

```rust
#[derive(Debug)]
pub enum SessionError {
    Timeout,
    /// 購読 pump の無音 deadline 切れ（受信ゼロ）。MRP 送信 ack 切れの
    /// `Timeout` とはログ上の意味が全く違うため分離（born-dead 切り分け）。
    Silence,
    Io(std::io::Error),
    Message(MessageError),
    Crypto(CryptoError),
    Im(crate::im::ImError),
    UnexpectedOpcode(u8),
}
```

Display（`session.rs:80` 付近）に追加:

```rust
            SessionError::Silence => {
                write!(f, "no device-initiated message within the subscription deadline")
            }
```

`next_subscription_report`（`session.rs:1071-1082`）の 2 箇所を変更:

```rust
                if remaining.is_zero() {
                    return Err(SessionError::Silence);
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    return Err(SessionError::Silence);
                };
```

（メソッドの doc コメント `1058-1060` も「`timeout` 無音は `SessionError::Silence`」に直す。）

- [ ] **Step 4: mat-native の `map_session_err` に Silence を明示写像**

`crates/mat-native/src/lib.rs:657` の `map_session_err` に arm 追加（`_ => Other` に落とさない — 無音は再確立対象の Timeout kind）:

```rust
        // 購読の無音 deadline 切れ。通常は SubscriptionSession::next_report が
        // Ok(None) に写像するのでここへは来ないが、防御的に Timeout kind へ。
        SessionError::Silence => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
```

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-controller next_subscription_report && cargo build -p mat-native`
Expected: PASS（3 テスト: reports_and_keepalive / times_out_on_silence / 他購読系）+ ビルド成功

- [ ] **Step 6: Commit**

```bash
git add crates/mat-controller/src/session.rs crates/mat-native/src/lib.rs
git commit -m "fix(mat-controller): 購読無音を SessionError::Silence に分離

無音 deadline 切れが MRP 送信 ack 切れと同じ Timeout/同じ文言に潰れて
born-dead の切り分けを壊していた（2026-07-21 夜の実機調査）。"
```

---

### Task 2: `SubscribeConn::next_report` を「無音 = Ok(None)」契約へ（mat-native + matd 追従）

pump が「スライス無音（続行）」と「セッションエラー（pump 終了）」を文字列比較なしで区別できるよう、trait の戻り値を `Result<Option<ReportDataMessage>, MatError>` に変える。あわせて FakeSubConn の live キューを FakeEstablisher と共有の `Arc<Mutex<VecDeque>>` にして、テストが確立後に live report を注入できるようにする（Task 4 のテストが使う）。

**Files:**
- Modify: `crates/mat-native/src/lib.rs:174-178`（trait 定義）、`crates/mat-native/src/lib.rs:528-537`（SubscriptionSession impl）、`crates/mat-native/src/lib.rs` tests の `fake_establisher_serves_scripted_subscription`
- Modify: `crates/mat-native/src/test_support.rs:20-89`（FakeSubConn）、`test_support.rs:256-297`（FakeEstablisher）
- Modify: `crates/matd/src/subscription.rs:275-291`（pump の match を暫定追従 — 挙動は従来どおり）

**Interfaces:**
- Consumes: Task 1 の `SessionError::Silence`。
- Produces: `async fn next_report(&mut self, timeout: Duration) -> Result<Option<mat_controller::im::ReportDataMessage>, MatError>`（`Ok(None)` = timeout 内無音）。`FakeSubConn.live: Arc<Mutex<VecDeque<ReportDataMessage>>>`、`FakeEstablisher.sub_live: Arc<Mutex<VecDeque<ReportDataMessage>>>`（establish した全 conn と共有）。

- [ ] **Step 1: mat-native のテストを新契約に変える（failing test）**

`crates/mat-native/src/lib.rs` tests の `fake_establisher_serves_scripted_subscription` 末尾を変更:

```rust
        // scripted report が尽きたら next_report は timeout まで待って Ok(None)（無音）。
        let silent = conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(silent.is_none());
        // 共有 live キューに積めば次の next_report が払い出す。
        est.sub_live
            .lock()
            .unwrap()
            .push_back(crate::test_support::onoff_report(1, false));
        let msg = conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap()
            .expect("live report");
        assert_eq!(msg.reports.len(), 1);
        let _ = FakeSubConn::default(); // 型が公開されていること
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-native fake_establisher_serves_scripted_subscription`
Expected: コンパイルエラー（戻り値型 / `sub_live` 未定義）

- [ ] **Step 3: trait・実装・fake を変更**

`crates/mat-native/src/lib.rs:172-178` の trait を変更:

```rust
    /// 次のデバイス発 report を待つ（keep-alive は reports 空の Some で返る）。
    /// `timeout` 内無音は `Ok(None)` — エラーではない（pump がスライスで刻んで
    /// 死活判定するための契約）。`Err` はセッション異常のみ。
    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<mat_controller::im::ReportDataMessage>, MatError>;
```

`SubscriptionSession` impl（`lib.rs:528-537`）:

```rust
    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<mat_controller::im::ReportDataMessage>, MatError> {
        match self.session.next_subscription_report(timeout, &self.mrp).await {
            Ok(msg) => Ok(Some(msg)),
            Err(mat_controller::session::SessionError::Silence) => Ok(None),
            Err(e) => Err(map_session_err(e)),
        }
    }
```

`crates/mat-native/src/test_support.rs` の FakeSubConn（live を共有キュー化、Timeout エラーを廃止）:

```rust
/// 購読 fake。`priming` は subscribe_wildcard が返す priming チャンク、`live` は
/// next_report が 1 呼び出し 1 通で払い出す共有キュー（FakeEstablisher.sub_live
/// と同一 — テストが確立後に注入できる）。尽きたら timeout まで待って Ok(None)
/// （実セッションの無音と同じ形）。
pub struct FakeSubConn {
    pub max_interval_s: u16,
    pub priming: Vec<mat_controller::im::ReportDataMessage>,
    pub live: std::sync::Arc<
        std::sync::Mutex<std::collections::VecDeque<mat_controller::im::ReportDataMessage>>,
    >,
    /// subscribe_wildcard が受けた clusters の記録先（FakeEstablisher と共有）。
    pub seen_clusters: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
}
```

`Default for FakeSubConn` の `live` は `std::sync::Arc::default()`。`next_report`:

```rust
    async fn next_report(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<mat_controller::im::ReportDataMessage>, MatError> {
        if let Some(r) = self.live.lock().unwrap().pop_front() {
            return Ok(Some(r));
        }
        tokio::time::sleep(timeout).await;
        Ok(self.live.lock().unwrap().pop_front())
    }
```

`FakeEstablisher` に共有キューを追加:

```rust
pub struct FakeEstablisher {
    pub calls: std::sync::Arc<AtomicUsize>,
    pub fail_first_send: bool,
    pub fail_kind: ErrorKind,
    /// 直近の establish_subscription が返した FakeSubConn の seen_clusters と
    /// 共有される記録先（matd の manager テストが検証に使う）。
    pub sub_clusters: std::sync::Arc<std::sync::Mutex<Vec<u32>>>,
    /// 全 FakeSubConn と共有する live キュー（テストが確立後に report を注入する）。
    pub sub_live: std::sync::Arc<
        std::sync::Mutex<std::collections::VecDeque<mat_controller::im::ReportDataMessage>>,
    >,
}
```

`Default` に `sub_live: std::sync::Arc::default(),` を足し、`establish_subscription` を:

```rust
        Ok(Box::new(FakeSubConn {
            seen_clusters: std::sync::Arc::clone(&self.sub_clusters),
            live: std::sync::Arc::clone(&self.sub_live),
            ..Default::default()
        }))
```

Step 1 のテストは `est` を move せず借用で `establish_subscription` を呼んでいる（既存コードのまま）ので `est.sub_live` に触れる。

- [ ] **Step 4: matd pump を暫定追従（挙動不変）**

`crates/matd/src/subscription.rs:275-290` の match を 3 arm に:

```rust
    loop {
        match conn.next_report(deadline).await {
            Ok(Some(msg)) => {
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(ev);
                }
                // keep-alive（reports 空）も無音 deadline をリセットするだけで良い。
            }
            Ok(None) => {
                // 無音 deadline 切れ → 再購読（Task 4 で born-dead/op 相関の
                // 判定に置き換わる暫定形）。
                tracing::info!(node_id, "report pump ended (silence)");
                return Ok(());
            }
            Err(e) => {
                // セッションエラー → 再購読。何で死んだかは切り分けに必須なので
                // 詳細を残す（直後に caller が「subscription lost」を出す）。
                tracing::info!(node_id, kind = ?e.kind, detail = %e.detail, "report pump ended");
                return Ok(());
            }
        }
    }
```

- [ ] **Step 5: 全テスト確認**

Run: `task test`
Expected: PASS（mat-native / matd の既存購読テスト含む全緑）

- [ ] **Step 6: Commit**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/test_support.rs crates/matd/src/subscription.rs
git commit -m "refactor(mat-native): SubscribeConn::next_report を無音=Ok(None) 契約へ

pump が無音スライスとセッションエラーを型で区別できるようにする
（born-dead 検知の前提）。FakeSubConn の live は FakeEstablisher と
共有キュー化し、確立後のテスト注入を可能に。"
```

---

### Task 3: `SubHealth` と pump 判定の純関数（matd）

server op 経路（書き手）と購読 pump（読み手）が共有する op 相関ヘルス表と、pump 終了判定の純関数を `crates/matd/src/subscription.rs` に足す。この Task では配線しない（Task 4/5 が使う）。

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`use std::collections::HashMap;` / `use std::sync::Mutex;` を追加し、既存 const 群の下に追記）
- Test: 同ファイル tests mod

**Interfaces:**
- Produces:
  - `pub struct SubHealth`、`SubHealth::new(clusters: Option<Vec<u32>>) -> Self`
  - `pub fn note_op(&self, node_id: u64, cluster: u32)`（cluster が購読対象のときだけ pending を打つ。`clusters` 空 = full wildcard = 全対象）
  - `pub fn clear_pending(&self, node_id: u64)`
  - `pub fn pending_elapsed(&self, node_id: u64) -> Option<std::time::Duration>`
  - `pub(crate) fn silence_deadline(max_interval_s: u16) -> Duration`
  - `pub(crate) enum PumpEnd { OpGrace { since_op: Duration }, BornDeadSilence, Silence }`
  - `pub(crate) fn pump_verdict(proven: bool, since_last_msg: Duration, deadline: Duration, pending_op: Option<Duration>) -> Option<PumpEnd>`
  - 定数 `PUMP_SLICE = 5s` / `OP_GRACE = 10s` / `SILENCE_SLACK = 30s`（`DEATH_FACTOR` はこの Task では残し、Task 4 で削除）

- [ ] **Step 1: failing tests を書く**

`crates/matd/src/subscription.rs` の tests mod に追加:

```rust
    #[test]
    fn silence_deadline_is_max_interval_plus_slack() {
        assert_eq!(silence_deadline(300), Duration::from_secs(330));
        assert_eq!(silence_deadline(60), Duration::from_secs(90));
        // 極端に小さくても常識的な下限（5s）を割らない。
        assert!(silence_deadline(0) >= Duration::from_secs(5));
    }

    #[test]
    fn pump_verdict_prioritizes_op_grace_then_silence() {
        let dl = Duration::from_secs(330);
        // 平常: 何も返さない。
        assert!(pump_verdict(true, Duration::from_secs(10), dl, None).is_none());
        // op から OP_GRACE 未満はまだ待つ。
        assert!(pump_verdict(true, Duration::from_secs(10), dl, Some(Duration::from_secs(9))).is_none());
        // op から OP_GRACE 経過でデバイス発ゼロ → op 相関死。
        assert!(matches!(
            pump_verdict(true, Duration::from_secs(15), dl, Some(Duration::from_secs(10))),
            Some(PumpEnd::OpGrace { .. })
        ));
        // 無音 deadline 超過: 生存実績なし → born-dead、あり → 通常無音死。
        assert!(matches!(
            pump_verdict(false, Duration::from_secs(330), dl, None),
            Some(PumpEnd::BornDeadSilence)
        ));
        assert!(matches!(
            pump_verdict(true, Duration::from_secs(330), dl, None),
            Some(PumpEnd::Silence)
        ));
    }

    #[tokio::test]
    async fn sub_health_notes_and_clears_pending_respecting_clusters() {
        // 絞り込み無し = 全 cluster が対象。
        let h = SubHealth::new(None);
        assert!(h.pending_elapsed(5).is_none());
        h.note_op(5, 0x0006);
        assert!(h.pending_elapsed(5).is_some());
        h.clear_pending(5);
        assert!(h.pending_elapsed(5).is_none());
        // 絞り込みあり: 対象外 cluster の op は無視。
        let h = SubHealth::new(Some(vec![0x0402]));
        h.note_op(5, 0x0006);
        assert!(h.pending_elapsed(5).is_none());
        h.note_op(5, 0x0402);
        assert!(h.pending_elapsed(5).is_some());
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p matd subscription`
Expected: コンパイルエラー（`SubHealth` / `pump_verdict` / `silence_deadline` 未定義）

- [ ] **Step 3: 実装**

`subscription.rs` の const 群（`BACKOFF_MAX` の下）に追記:

```rust
/// pump の受信待ち 1 スライス。op 相関検知（SubHealth）をこの周期で確認する。
/// `next_report` は recv → screen → StatusResponse の多段 await で cancel-safe
/// でないため、`select!` ではなくスライスで刻む（spec §1）。
const PUMP_SLICE: Duration = Duration::from_secs(5);
/// 状態変更 op 成功からデバイス発メッセージ皆無をこの時間まで許す（spec §1）。
const OP_GRACE: Duration = Duration::from_secs(10);
/// 無音 deadline: デバイス選択 max_interval + この slack。デバイスは
/// max_interval までに必ず report か keep-alive を送る義務があり、slack は
/// MRP 再送とジッタの余裕（旧 DEATH_FACTOR 1.5 = 450s を置換、spec §2）。
const SILENCE_SLACK: Duration = Duration::from_secs(30);

/// 無音 deadline の計算（純関数）。
pub(crate) fn silence_deadline(max_interval_s: u16) -> Duration {
    (Duration::from_secs(u64::from(max_interval_s)) + SILENCE_SLACK)
        .max(Duration::from_secs(5))
}

/// pump 終了理由（純関数 `pump_verdict` の出力 — ログ文言の出し分けに使う）。
#[derive(Debug, PartialEq)]
pub(crate) enum PumpEnd {
    /// 状態変更 op から OP_GRACE 経過してもデバイス発ゼロ（op 相関の born-dead 検知）。
    OpGrace { since_op: Duration },
    /// 確立以降デバイス発ゼロのまま無音 deadline 超過（born-dead）。
    BornDeadSilence,
    /// 生存実績のあと無音 deadline 超過（通常の購読死）。
    Silence,
}

/// pump を殺すべきか判定する（純関数 — 時計は pump が持つ）。
/// op 相関を無音 deadline より先に評価する（そちらが常に早く満ちるため）。
pub(crate) fn pump_verdict(
    proven: bool,
    since_last_msg: Duration,
    deadline: Duration,
    pending_op: Option<Duration>,
) -> Option<PumpEnd> {
    if let Some(since_op) = pending_op {
        if since_op >= OP_GRACE {
            return Some(PumpEnd::OpGrace { since_op });
        }
    }
    if since_last_msg >= deadline {
        return Some(if proven {
            PumpEnd::Silence
        } else {
            PumpEnd::BornDeadSilence
        });
    }
    None
}

/// op 相関ヘルス表: server op 経路（書き手）と購読 pump（読み手）の共有状態。
/// 「状態変更 op が success したのにデバイス発メッセージが来ない」= レポート
/// 経路死の証拠、を pending として持つ。ephemeral なランタイム状態のみ
/// （設計ルール4の永続状態には該当しない）。
pub struct SubHealth {
    /// 購読対象クラスタ集合（subscriptions.toml 由来。空 = full wildcard = 全対象）。
    clusters: Vec<u32>,
    /// node_id → 未消化の状態変更 op の時刻。
    pending: Mutex<HashMap<u64, tokio::time::Instant>>,
}

impl SubHealth {
    pub fn new(clusters: Option<Vec<u32>>) -> Self {
        Self {
            clusters: clusters.unwrap_or_default(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// 状態変更 op が success した。cluster が購読対象なら pending を打つ。
    pub fn note_op(&self, node_id: u64, cluster: u32) {
        if !self.clusters.is_empty() && !self.clusters.contains(&cluster) {
            return;
        }
        self.pending
            .lock()
            .unwrap()
            .insert(node_id, tokio::time::Instant::now());
    }

    /// デバイス発メッセージ（keep-alive 含む）や priming を受けた — pending 解除。
    pub fn clear_pending(&self, node_id: u64) {
        self.pending.lock().unwrap().remove(&node_id);
    }

    /// 未消化 op からの経過時間（無ければ None）。
    pub fn pending_elapsed(&self, node_id: u64) -> Option<Duration> {
        self.pending
            .lock()
            .unwrap()
            .get(&node_id)
            .map(|t| t.elapsed())
    }
}
```

ファイル先頭の use に `use std::collections::HashMap;` / `use std::sync::Mutex;` を追加。

- [ ] **Step 4: テスト確認**

Run: `cargo test -p matd subscription`
Expected: PASS（新 3 テスト + 既存全部。未使用警告が clippy で出る場合は `#[allow(dead_code)]` を一時付与せず、Task 4 で配線されるまでは `pub` 面のため出ない想定 — 出たら Task 4 と同コミットにせず `pub(crate)` 面の未使用のみ `cargo clippy -p matd` で確認して調整）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): SubHealth と pump 死活判定の純関数を追加

op 相関の born-dead 検知（spec §1）と無音 deadline 330s（spec §2）の
判定部。配線は次コミット。"
```

---

### Task 4: pump のスライス化と born-dead / op 相関検知の配線（matd）

`run_subscription_once` の pump を 5 秒スライスで刻み、`SubHealth` と `pump_verdict` を配線する。`DEATH_FACTOR` を削除し、死因別ログを出す。

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`DEATH_FACTOR` 削除、`spawn_subscription_manager` / `node_subscription_loop` / `run_subscription_once` に `Arc<SubHealth>` を配線、pump ループ書き換え、既存 manager テスト 2 件の引数追従）
- Modify: `crates/matd/src/main.rs:203-229`（`SubHealth` 生成と受け渡し）
- Test: `crates/matd/src/subscription.rs` tests mod

**Interfaces:**
- Consumes: Task 2 の `next_report -> Ok(Option<_>)`、Task 3 の `SubHealth` / `pump_verdict` / `silence_deadline` / `PUMP_SLICE`。
- Produces: `pub fn spawn_subscription_manager(native, store_path, events, clusters: Option<Vec<u32>>, health: Arc<SubHealth>) -> Vec<JoinHandle<()>>`（第 5 引数追加）。Task 5 の server 配線は同じ `Arc<SubHealth>` を `note_op` 側から使う。

- [ ] **Step 1: failing tests を書く**

`crates/matd/src/subscription.rs` tests mod に追加（既存 2 つの manager テストの `spawn_subscription_manager` 呼び出しにも第 5 引数 `std::sync::Arc::new(SubHealth::new(None))` を足す）:

```rust
    /// op 相関検知: 確立後に note_op して沈黙させると、無音 deadline (90s) を
    /// 待たず grace+backoff 内（<40s）に再購読 = 2 回目の priming が届く。
    #[tokio::test(start_paused = true)]
    async fn op_grace_triggers_fast_resubscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();
        let native =
            crate::native::NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(64);
        let health = std::sync::Arc::new(SubHealth::new(None));
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            None,
            std::sync::Arc::clone(&health),
        );
        // 1 回目の priming（確立）。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // 状態変更 op（デバイス発は来ない = born-dead 相当）。
        let t0 = tokio::time::Instant::now();
        health.note_op(5, 0x0006);
        // grace(10s) + backoff(5s) + スライス誤差内に再購読の priming が届く。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(40), rx.recv())
            .await
            .expect("re-priming after op-grace")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(elapsed >= Duration::from_secs(10), "grace より早く殺さない: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(40), "無音 deadline (90s) を待っていないこと: {elapsed:?}");
    }

    /// live report（keep-alive 相当含む）が届けば pending は解除され、
    /// 無音 deadline 前に再購読は起きない。
    #[tokio::test(start_paused = true)]
    async fn live_report_clears_pending_without_resubscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();
        let est = FakeEstablisher::default();
        let live = std::sync::Arc::clone(&est.sub_live);
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(64);
        let health = std::sync::Arc::new(SubHealth::new(None));
        let _handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            None,
            std::sync::Arc::clone(&health),
        );
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        // op → 直後に live report が届く（健全経路）。
        health.note_op(5, 0x0006);
        live.lock().unwrap().push_back(onoff_report(1, false));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming);
        assert!(health.pending_elapsed(5).is_none(), "受信で pending 解除");
        // 無音 deadline (90s) 未満の 80s の間、再購読（= 追加イベント）は起きない。
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(80), rx.recv())
                .await
                .is_err(),
            "健全な購読を殺していないこと"
        );
    }
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p matd subscription`
Expected: コンパイルエラー（`spawn_subscription_manager` の引数数不一致）

- [ ] **Step 3: 配線を実装**

`spawn_subscription_manager` に第 5 引数 `health: Arc<SubHealth>` を追加し、タスク生成で `Arc::clone(&health)` を `node_subscription_loop(node_id, native, events, clusters, health)` に渡す。`node_subscription_loop` も同様に受けて `run_subscription_once(node_id, backend, &events, &clusters, &health, down_since, failures)` へ。

`run_subscription_once` の確立後〜pump を書き換え:

```rust
    let mut conn = backend.establish_subscription(node_id).await?;
    let (info, priming) = conn.subscribe_wildcard(clusters).await?;
    tracing::info!(
        node_id,
        subscription_id = info.subscription_id,
        max_interval_s = info.max_interval_s,
        down_s = down_since.elapsed().as_secs(),
        attempts = prior_failures + 1,
        "subscription established"
    );
    // priming は現在状態の全量 — down 中の op はここで配信されるので pending 解除。
    health.clear_pending(node_id);
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            let _ = events.send(ev); // 受信者ゼロは正常（listen 接続なし）
        }
    }
    let deadline = silence_deadline(info.max_interval_s);
    tracing::debug!(node_id, deadline_s = deadline.as_secs(), "report pump running");
    // 確立以降デバイス発を 1 度でも受けたか（born-dead 判定）。
    let mut proven = false;
    let mut last_msg = tokio::time::Instant::now();
    loop {
        if let Some(end) = pump_verdict(
            proven,
            last_msg.elapsed(),
            deadline,
            health.pending_elapsed(node_id),
        ) {
            // 再購読直後に同じ pending で即再発火しないよう先に消す。
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
                    "report pump ended (silence past deadline)"
                ),
            }
            return Ok(());
        }
        let remaining = deadline.saturating_sub(last_msg.elapsed());
        let slice = PUMP_SLICE.min(remaining);
        match conn.next_report(slice).await {
            Ok(Some(msg)) => {
                proven = true;
                last_msg = tokio::time::Instant::now();
                health.clear_pending(node_id);
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(ev);
                }
                // keep-alive（reports 空）も受信 = 経路生存の証明として扱う。
            }
            Ok(None) => {
                // スライス無音 — 次周回の pump_verdict で判定する。
            }
            Err(e) => {
                // セッションエラー → 再購読。何で死んだかは切り分けに必須なので
                // 詳細を残す（直後に caller が「subscription lost」を出す）。
                health.clear_pending(node_id);
                tracing::info!(node_id, kind = ?e.kind, detail = %e.detail, "report pump ended");
                return Ok(());
            }
        }
    }
```

`DEATH_FACTOR` 定数と旧 deadline 計算（`Duration::from_secs_f64(...)`）は削除する。ファイル冒頭のモジュール doc コメント（5 行目「失敗・死亡時は指数 backoff」付近）に「op 相関 + 無音 deadline = max_interval+30s の死活判定（spec 2026-07-21-matd-borndead-detection）」を 1 行追記。

`crates/matd/src/main.rs` の配線（205 行付近、`sub_clusters` 読み込みの後）:

```rust
    // op 相関ヘルス表: server（note_op）と購読 pump（判定）の共有。
    let sub_health = std::sync::Arc::new(matd::subscription::SubHealth::new(sub_clusters.clone()));
    let _sub_handles = matd::subscription::spawn_subscription_manager(
        std::sync::Arc::clone(&native),
        store_path.clone(),
        events_tx.clone(),
        sub_clusters,
        std::sync::Arc::clone(&sub_health),
    );
```

（`sub_health` は `spawn_subscription_manager` へ渡した時点で使用済みなので未使用警告は出ない。Task 5 で `server::serve` にも渡す。）

- [ ] **Step 4: テスト確認**

Run: `cargo test -p matd && cargo clippy -p matd -- -D warnings`
Expected: PASS（新 2 テスト含む全緑、clippy 警告ゼロ）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs crates/matd/src/main.rs
git commit -m "feat(matd): 購読 pump をスライス化し born-dead / op 相関検知を配線

無音 deadline を max_interval×1.5(450s) → +30s(330s) に短縮（spec §2）、
op 相関はグレース 10s で検知（spec §1）。死因別ログで born-dead を明示
（spec §3）。"
```

---

### Task 5: server op 経路の `note_op` 配線（matd）

状態変更 op が success したら `SubHealth::note_op` を打つ。`Arc<SubHealth>` を `serve` → `handle_conn` → `dispatch` → `run_op` に通す。

**Files:**
- Modify: `crates/matd/src/server.rs`（`serve` / `handle_conn` / `dispatch` / `run_op` のシグネチャ、`op_report_expectation` 新設）
- Modify: `crates/matd/src/main.rs:231`（`serve` 呼び出しに `sub_health` を渡す）
- Test: `crates/matd/src/server.rs` tests mod

**Interfaces:**
- Consumes: Task 3 の `SubHealth`（`note_op` / `pending_elapsed`）、Task 4 で main.rs に生成済みの `sub_health`。
- Produces: `fn op_report_expectation(op: &Op) -> Option<(u64, u32)>`（server.rs 内 private）。`pub async fn serve(socket, store_path, native, events, health: Arc<SubHealth>)`。

- [ ] **Step 1: failing tests を書く**

`crates/matd/src/server.rs` tests mod に追加（use に `crate::subscription::SubHealth` と `mat_native::test_support::FakeEstablisher` が必要）:

```rust
    /// 状態変更 op の success が SubHealth に pending を打つ（read は打たない）。
    #[tokio::test]
    async fn run_op_success_marks_pending_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-21T00:00:00+09:00".into(),
            })
            .unwrap();
        let native = crate::native::NativeBackend::with_establisher(Box::new(
            FakeEstablisher::default(),
        ));
        let state = NativeState::Ready(Box::new(native));
        let health = std::sync::Arc::new(SubHealth::new(None));

        // off (onoff=0x0006) の success → pending。
        let body = run_op(
            &Op::Off { node_id: 5, endpoint: 1 },
            &state,
            dir.path(),
            &health,
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "success");
        assert!(health.pending_elapsed(5).is_some());

        // read は状態を変えないので pending を打たない。
        health.clear_pending(5);
        let _ = run_op(
            &Op::Read {
                node_id: 5,
                endpoint: 1,
                cluster: "onoff".into(),
                attribute: "on-off".into(),
            },
            &state,
            dir.path(),
            &health,
        )
        .await
        .unwrap();
        assert!(health.pending_elapsed(5).is_none());
    }

    #[test]
    fn op_report_expectation_maps_state_changing_ops() {
        assert_eq!(
            op_report_expectation(&Op::On { node_id: 5, endpoint: 1 }),
            Some((5, 0x0006))
        );
        assert_eq!(
            op_report_expectation(&Op::Level {
                node_id: 5,
                endpoint: 1,
                level: 128,
                transition: None,
            }),
            Some((5, 0x0008))
        );
        assert_eq!(
            op_report_expectation(&Op::Read {
                node_id: 5,
                endpoint: 1,
                cluster: "onoff".into(),
                attribute: "on-off".into(),
            }),
            None
        );
        // Write は cluster 名を解決して返す。
        assert_eq!(
            op_report_expectation(&Op::Write {
                node_id: 5,
                endpoint: 1,
                cluster: "onoff".into(),
                attribute: "on-time".into(),
                value: serde_json::json!(0),
            }),
            Some((5, 0x0006))
        );
    }
```

注意: `Op::Level` / `Op::Color` / `Op::ColorTemp` / `Op::Write` のフィールド名・型は `crates/matd/src/protocol.rs` の定義が正 — テストコードのフィールドが合わなければ protocol.rs に合わせて直す（`native_op` の match（server.rs:509-630 付近）が使っている形が実例）。

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p matd server`
Expected: コンパイルエラー（`run_op` 引数数 / `op_report_expectation` 未定義）

- [ ] **Step 3: 実装**

server.rs に純関数を追加（`is_native_hotpath` の近く）:

```rust
/// 状態変更 op → (node_id, 変化が現れる cluster)。op 相関の born-dead 検知
/// （SubHealth::note_op）の根拠。Read/Describe は状態を変えないので None。
/// Group 系はノード特定不能、listen/管理系は対象外で None。
fn op_report_expectation(op: &Op) -> Option<(u64, u32)> {
    match op {
        Op::On { node_id, .. } | Op::Off { node_id, .. } => Some((*node_id, 0x0006)),
        Op::Level { node_id, .. } => Some((*node_id, 0x0008)),
        Op::Color { node_id, .. } | Op::ColorTemp { node_id, .. } => Some((*node_id, 0x0300)),
        Op::Write { node_id, cluster, .. } | Op::Invoke { node_id, cluster, .. } => {
            mat_core::ids::resolve_cluster(cluster).map(|c| (*node_id, c))
        }
        _ => None,
    }
}
```

`run_op` のシグネチャに `health: &crate::subscription::SubHealth` を追加し、hotpath 分岐（server.rs:341-343）を:

```rust
    if is_native_hotpath(op) {
        let result = native_op(op, native, store_path).await;
        if result.is_ok() {
            if let Some((node_id, cluster)) = op_report_expectation(op) {
                health.note_op(node_id, cluster);
            }
        }
        return result;
    }
```

`dispatch` / `handle_conn` / `serve` に `health: Arc<crate::subscription::SubHealth>`（dispatch/run_op は `&SubHealth`）を貫通させる。`handle_conn` の spawn 箇所（server.rs:74-79）で `let health = Arc::clone(&health);` を足して move する。`serve` のシグネチャ:

```rust
pub async fn serve(
    socket_path: &Path,
    store_path: PathBuf,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
    health: Arc<crate::subscription::SubHealth>,
) -> std::io::Result<()> {
```

`main.rs:231` を追従:

```rust
    server::serve(&socket, store_path, native, events_tx, sub_health)
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("socket server failed: {e}")))
```

（Task 4 で入れた一時的な `let _ = &sub_health;` があれば除去。server.rs 内に `dispatch`/`run_op` を呼ぶ既存テストがあれば `&SubHealth::new(None)` を渡す形に追従させる。）

- [ ] **Step 4: テスト確認**

Run: `cargo test -p matd && cargo clippy -p matd -- -D warnings`
Expected: PASS（新 2 テスト含む全緑）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/server.rs crates/matd/src/main.rs
git commit -m "feat(matd): 状態変更 op の success で SubHealth::note_op を配線

op 相関 born-dead 検知の書き手側（spec §1）。対象は on/off/level/color/
color-temp/write/invoke の success のみ（read/describe/group は対象外）。"
```

---

### Task 6: 全体検証とリリース準備（0.28.0）

**Files:**
- Modify: `Cargo.toml:6`（workspace version）、`Cargo.lock`（ビルドで追従）

**Interfaces:**
- Consumes: Task 1-5 の全変更。

- [ ] **Step 1: CI 相当を全部通す**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / test 全部 PASS

- [ ] **Step 2: バージョンを 0.28.0 に**

`Cargo.toml:6` を `version = "0.28.0"` に変更し、`cargo build -p matd -p mat` で Cargo.lock を追従させる。

- [ ] **Step 3: 最終確認と Commit**

Run: `task check`
Expected: PASS

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(release): 0.28.0 — matd born-dead 購読の高速検知"
```

---

## 実機受け入れ基準（実装完了後・デプロイして検証 — plan 外の手作業）

spec §4 のとおり:
1. born-dead 時間帯（夜）に node6 で `mat off` → `mat listen` が priming 経由で 60s 内に受信・exit 0。matd ログに `report pump ended (op-correlated...)` → `subscription established` の連鎖。
2. 落ち着いた時間帯の通常 listen E2E 再確認(born-alive 経路の無回帰、~数百 ms 配信)。
3. 観測性: 無音死ログが born-dead / silence / セッションエラーで区別されて出る。
