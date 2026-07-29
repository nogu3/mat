# op 予算（deadline）の伝播と執行 実装計画 — Issue #16

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat → matd に op 予算（deadline）を伝播し、matd が予算内に構造化 timeout を返し、クライアント切断で進行中 op をキャンセルする（Issue #16、1.10.0）。

**Architecture:** socket protocol に相対予算 `deadline_ms` を追加。matd の `with_session` が予算を執行（予算超過 = slot 破棄 + `timeout`、再確立+再送は残り予算 ≥ 10s のときのみ）。`handle_conn` は `select!` でクライアント切断を検出し op を drop。mat は グローバル `--op-timeout-ms`（既定 60s）で両経路に同じ予算を適用する。

**Tech Stack:** Rust / tokio（`tokio::time::timeout`, `select!`）/ serde / clap。テストは既存の `FakeEstablisher`（`mat-native::test_support`）と `crates/matd/tests/integration.rs` の足場を拡張。

**Spec:** `docs/superpowers/specs/2026-07-29-op-deadline-budget-design.md`

## Global Constraints

- ブランチ: `feat/op-deadline-budget`（main から作成、実装コミットは全てここ）
- stdout は純粋な構造化 JSON のみ。診断は stderr の `tracing`（CLAUDE.md 出力規約）
- プロトコルコードは backend クレートのみ（`mat` / `matd` のコマンド層に TLV/CASE を書かない）
- 予算超過のエラーは既存の `ErrorKind::Timeout`（exit 3）。新 kind は追加しない
- deadline は**相対 ms**（絶対時刻は使わない — 2 プロセス間の時計合意を仮定しない）
- deadline 適用対象は**単一ノード op のみ**: read / write / invoke / on / off / color / color_temp / level / describe。group 系 / provision / bump / listen / ping / shutdown / open-window / diag は対象外
- `deadline_ms` のセマンティクス: **`Some(0)` = 明示無制限、`Some(n)` = n ms、無指定（旧クライアント）= matd 既定 60s**
- 各タスク末尾で `task check`（fmt:check + clippy -D warnings + test）を通してからコミット
- コメント・ログは既存コードの日本語スタイルに合わせる
- **マージ前に jarvis 実機 E2E 必須**（ユーザー明示ルール）— 本計画のタスクには含めない（finishing 段階で実施）

---

### Task 1: 予算成分の pub 化と釘打ちテスト（mat-controller / mat-native）

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs`（`total_budget` を pub 化、~:56）
- Modify: `crates/mat-controller/src/session.rs`（`IM_RECV_TIMEOUT` pub 化 :27、`worst_case_send_budget()` 追加）
- Modify: `crates/mat-controller/src/case.rs`（`RECV_TIMEOUT` pub 化 :38）
- Modify: `crates/mat-native/src/lib.rs`（`CACHE_MISS_TIMEOUT` pub 化 :265）
- Test: `crates/mat-controller/src/session.rs`（mod tests に釘打ちテスト追加）

**Interfaces:**
- Consumes: 既存の `MrpConfig::default()`（initial 300ms / retries 4 / backoff 1.6）
- Produces:
  - `pub fn exchange::total_budget(cfg: &MrpConfig) -> Duration`
  - `pub const session::IM_RECV_TIMEOUT: Duration`（10s）
  - `pub fn session::worst_case_send_budget() -> Duration`（= total_budget(default) + IM_RECV_TIMEOUT ≈ 14.74s）
  - `pub const case::RECV_TIMEOUT: Duration`（10s）
  - `pub const mat_native::CACHE_MISS_TIMEOUT`（35s、lib.rs の定義位置で pub 化。re-export は不要）

- [ ] **Step 1: 釘打ちテストを書く（失敗確認用）**

`crates/mat-controller/src/session.rs` の `#[cfg(test)] mod tests` に追加:

```rust
/// Issue #16: op 予算の設計根拠になる成分値を釘打ちする。上流の MRP/IM 既定値を
/// 変えるとここが割れ、matd の RETRY_MIN_BUDGET / mat の --op-timeout-ms 既定の
/// 再検討が強制される。
#[test]
fn budget_components_are_pinned() {
    let mrp = crate::exchange::total_budget(&crate::exchange::MrpConfig::default());
    // 300 + 480 + 768 + 1228.8 + 1966.08 = 4742.88ms（5 送信の間隔総和）
    assert_eq!(mrp.as_millis(), 4742);
    assert_eq!(IM_RECV_TIMEOUT.as_secs(), 10);
    // 単一 op 送信の最悪 = MRP 総和 + IM 応答待ち
    assert_eq!(worst_case_send_budget().as_millis(), 14742);
    assert_eq!(crate::case::RECV_TIMEOUT.as_secs(), 10);
}
```

注意: `total_budget` の実装（`exchange.rs:56-62` 付近、`for _ in 0..=cfg.max_retries` で interval を加算）を読み、総和が 4742.88ms（`as_millis()` 切り捨てで 4742）になることを確認してから書く。実装と 1ms 単位でずれる場合はこのテストの期待値を実測値に合わせる（釘打ちの目的は「変化に気づく」こと）。

- [ ] **Step 2: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test -p mat-controller budget_components_are_pinned 2>&1 | head -20`
Expected: FAIL（`total_budget` が private / `worst_case_send_budget` 未定義）

- [ ] **Step 3: pub 化と worst_case_send_budget を実装**

`exchange.rs`: `fn total_budget` → `pub fn total_budget`（doc コメント追加: 「MRP 再送が尽きるまでの待ち時間総和。op 予算設計（Issue #16）の成分」）。

`session.rs`: `const IM_RECV_TIMEOUT` → `pub const IM_RECV_TIMEOUT`（doc: 「ack に応答が piggyback しなかった場合の IM 応答待ち。op 予算設計の成分」）。同ファイルに追加:

```rust
/// 単一 op 送信の最悪所要（MRP 再送総和 + IM 応答待ち ≈ 14.74s）。
/// matd の呼び出し側予算（Issue #16）はこの値を前提に設計する。
pub fn worst_case_send_budget() -> Duration {
    crate::exchange::total_budget(&crate::exchange::MrpConfig::default()) + IM_RECV_TIMEOUT
}
```

`case.rs`: `const RECV_TIMEOUT` → `pub const RECV_TIMEOUT`（doc に「CASE ハンドシェイク各往復の応答待ち。op 予算設計の成分」を追記）。

`mat-native/src/lib.rs`: `const CACHE_MISS_TIMEOUT` → `pub const CACHE_MISS_TIMEOUT`（既存 doc コメントは維持し「op 予算設計の成分（Issue #16）」を追記）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller budget_components_are_pinned`
Expected: PASS

- [ ] **Step 5: task check + コミット**

```bash
task check
git add -A crates/mat-controller crates/mat-native
git commit -m "refactor(controller): op 予算成分を pub 化し合計を釘打ち — Issue #16 下地"
```

---

### Task 2: socket protocol に deadline_ms を追加（matd/protocol.rs）

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`Request` 構造体 :11-18、mod tests）

**Interfaces:**
- Produces: `Request.deadline_ms: Option<u64>`（`#[serde(default)]`）。後続 Task 5 が `dispatch` で消費する。

- [ ] **Step 1: 失敗するテストを書く**

`protocol.rs` の mod tests に追加:

```rust
#[test]
fn deadline_ms_parses_and_defaults_to_none() {
    // 新 mat からの deadline 付きリクエスト。
    let r = parse(
        r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off","deadline_ms":15000}"#,
    );
    assert_eq!(r.deadline_ms, Some(15000));
    // 旧 mat（フィールド無し）は None。
    let r = parse(r#"{"op":"ping"}"#);
    assert_eq!(r.deadline_ms, None);
    // 明示 0（無制限）も値として通る。
    let r = parse(r#"{"op":"on","node_id":3,"endpoint":1,"deadline_ms":0}"#);
    assert_eq!(r.deadline_ms, Some(0));
}

#[test]
fn unknown_top_level_fields_are_tolerated() {
    // 前方互換の釘打ち: 未知フィールドは無視される（新 mat → 旧 matd で
    // deadline_ms が未知でも parse_error にならないことの一般形）。
    let r = parse(
        r#"{"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off","future_field":42}"#,
    );
    assert!(matches!(r.op, Op::Read { .. }));
}
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p matd deadline_ms 2>&1 | head -10`
Expected: FAIL（`deadline_ms` フィールド未定義のコンパイルエラー）

- [ ] **Step 3: フィールド追加**

`Request` に追加（`id` と `op` の間）:

```rust
    /// クライアントの op 予算（相対 ms）。単一ノード op のみに適用される。
    /// `Some(0)` = 明示無制限、無指定（旧クライアント）= matd 既定 60s
    /// （server::DEFAULT_OP_BUDGET）。Issue #16。
    #[serde(default)]
    pub deadline_ms: Option<u64>,
```

- [ ] **Step 4: テスト確認**

Run: `cargo test -p matd --lib protocol`
Expected: PASS（既存テスト含め全通過）

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/matd/src/protocol.rs
git commit -m "feat(matd): socket protocol に deadline_ms — op 予算の伝播（Issue #16）"
```

---

### Task 3: FakeConn / FakeEstablisher に遅延注入（mat-native/test_support.rs）

**Files:**
- Modify: `crates/mat-native/src/test_support.rs`（`FakeConn` :138-165、`FakeEstablisher` :296-344）

**Interfaces:**
- Consumes: 既存の `FakeConn` / `FakeEstablisher`（`Default` + struct update 構文で既存テスト多数が構築）
- Produces:
  - `FakeConn.delay: Option<std::time::Duration>` — `read_onoff` / `write_tlv` / `invoke` / `read_json` / `read_cluster` の冒頭で `tokio::time::sleep(delay)` する
  - `FakeEstablisher.conn_delay: Option<Duration>` — 払い出す `FakeConn.delay` に伝播
  - `FakeEstablisher.establish_delay: Option<Duration>` — `establish()` 冒頭で sleep

- [ ] **Step 1: 失敗するテストを書く**

`test_support.rs` 末尾（または既存 tests mod）に追加:

```rust
#[cfg(test)]
mod delay_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn conn_delay_delays_send_ops() {
        let est = FakeEstablisher {
            conn_delay: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let mut conn = est.establish(1).await.unwrap();
        let started = std::time::Instant::now();
        conn.read_onoff(1).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn establish_delay_delays_establish() {
        let est = FakeEstablisher {
            establish_delay: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        est.establish(1).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(50));
    }
}
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat-native delay 2>&1 | head -10`
Expected: FAIL（フィールド未定義）

- [ ] **Step 3: 実装**

`FakeConn` にフィールド追加 + `Default` に `delay: None`:

```rust
    /// 送信系メソッド冒頭の遅延（deadline 執行テスト用、Issue #16）。None = 遅延なし。
    pub delay: Option<std::time::Duration>,
```

`NodeConn` impl の `read_onoff` / `invoke` / `read_json` / `read_cluster` / `write_tlv` それぞれの冒頭に:

```rust
        if let Some(d) = self.delay {
            tokio::time::sleep(d).await;
        }
```

`FakeEstablisher` にフィールド追加 + `Default` に `None` × 2:

```rust
    /// 払い出す FakeConn の送信遅延（deadline 執行テスト用、Issue #16）。
    pub conn_delay: Option<std::time::Duration>,
    /// establish 自体の遅延（establish フェーズの deadline 執行テスト用）。
    pub establish_delay: Option<std::time::Duration>,
```

`establish()` を修正:

```rust
    async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        if let Some(d) = self.establish_delay {
            tokio::time::sleep(d).await;
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeConn {
            fail_first_send: self.fail_first_send && n == 0,
            fail_kind: self.fail_kind,
            delay: self.conn_delay,
            ..Default::default()
        }))
    }
```

注意: `establish_delay` の sleep は `calls` カウントより**前**（deadline でカットされた establish を「呼ばれなかった」と数えられるように）。

- [ ] **Step 4: テスト確認**

Run: `cargo test -p mat-native`
Expected: PASS（既存テスト含め全通過 — 既存の struct update 構文構築は `..Default::default()` で新フィールドを埋めるため無変更で通る）

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/mat-native/src/test_support.rs
git commit -m "test(mat-native): FakeConn/FakeEstablisher に遅延注入 — deadline テスト足場"
```

---

### Task 4: with_session の deadline 執行（matd/native.rs）

**Files:**
- Modify: `crates/matd/src/native.rs`（`with_session` :124-172、全単一ノード pub メソッド、mod tests）
- Modify: `crates/matd/src/server.rs`（`native_op` :800 以降の backend 呼び出しに `None` を機械的に渡す — 実際の deadline 配線は Task 5）

**Interfaces:**
- Consumes: Task 1 の `mat_controller::session::worst_case_send_budget`（RETRY_MIN_BUDGET の根拠コメント用）、Task 3 の遅延注入 fake
- Produces:
  - `with_session(&self, node_id: u64, deadline: Option<std::time::Instant>, op: F)`（private）
  - 単一ノード pub メソッドの新シグネチャ（Task 5 の server が使う）:
    - `read_onoff(&self, node_id, endpoint, deadline: Option<Instant>) -> Result<bool, MatError>`
    - `on(&self, node_id, endpoint, deadline) -> Result<(), MatError>` / `off(同)`
    - `color(&self, node_id, endpoint, hue_raw, saturation_raw, transition, deadline)`
    - `color_temp(&self, node_id, endpoint, mireds, transition, deadline)`
    - `level(&self, node_id, endpoint, level, transition, deadline)`
    - `read_json(&self, node_id, endpoint, cluster, attribute, deadline) -> Result<Value, MatError>`
    - `write_tlv(&self, node_id, endpoint, cluster, attribute, data_tlv, timed, deadline)`
    - `invoke_generic(&self, node_id, endpoint, cluster, command, fields, timed, deadline)`
    - `describe(&self, node_id, deadline) -> Result<Vec<(u16, Vec<u64>)>, MatError>`
  - `provision_node` はシグネチャ不変（内部で `with_session(node_id, None, ..)` — provision は deadline 対象外）
  - `pub(crate) const RETRY_MIN_BUDGET: Duration = Duration::from_secs(10);`

- [ ] **Step 1: 失敗するテストを書く**

`native.rs` の mod tests に追加（既存テストの `read_onoff(0x1234, 1)` 呼び出しはこの時点でコンパイルエラーになるため、Step 3 で `, None` を付けて回す）:

```rust
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn deadline_cuts_send_and_drops_slot() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            conn_delay: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        let deadline = Some(Instant::now() + Duration::from_millis(50));
        let err = backend
            .read_onoff(0x1234, 1, deadline)
            .await
            .expect_err("deadline must cut the slow send");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.detail.contains("in send"), "detail: {}", err.detail);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // slot は破棄済み: 無制限の次 op は再確立してから成功する。
        backend.read_onoff(0x1234, 1, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deadline_cuts_establish_phase() {
        let est = FakeEstablisher {
            establish_delay: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        let deadline = Some(Instant::now() + Duration::from_millis(50));
        let err = backend
            .read_onoff(0x1234, 1, deadline)
            .await
            .expect_err("deadline must cut the slow establish");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.detail.contains("in establish"), "detail: {}", err.detail);
    }

    #[tokio::test]
    async fn insufficient_budget_skips_re_establish() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // fake の失敗は即時なので、残り予算 ≈ 5s < RETRY_MIN_BUDGET(10s)。
        let deadline = Some(Instant::now() + Duration::from_secs(5));
        let err = backend
            .read_onoff(0x1234, 1, deadline)
            .await
            .expect_err("timeout must surface when retry is skipped");
        assert_eq!(err.kind, ErrorKind::Timeout);
        // 再確立していない: establish は初回の 1 回だけ。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // slot は破棄済み（MRP 尽きの session は持ち越さない）。
        backend.read_onoff(0x1234, 1, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn sufficient_budget_still_re_establishes() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 残り予算 ≈ 60s ≥ RETRY_MIN_BUDGET → 従来どおり再確立+再送。
        let deadline = Some(Instant::now() + Duration::from_secs(60));
        let v = backend.read_onoff(0x1234, 1, deadline).await.unwrap();
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p matd --lib 2>&1 | head -20`
Expected: FAIL（コンパイルエラー: シグネチャ不一致）

- [ ] **Step 3: with_session と pub メソッドを実装**

`native.rs` 冒頭に use 追加: `use std::time::{Duration, Instant};`

定数とヘルパ（`NodeSlot` 定義の近く）:

```rust
/// Timeout 腕の再確立+再送に最低限必要な残り予算。warm-cache の mDNS 解決 +
/// CASE 往復（典型 ~1s）+ MRP 一巡（`mat_controller::exchange::total_budget`
/// 既定 ≈ 4.74s、`worst_case_send_budget` ≈ 14.74s の内数）+ 応答余裕。
/// これ未満なら再送が成功しても呼び出し側の予算内に応答を返せない
/// （Issue #16: 「1 回だけ再確立して再送」が構造的に無駄だった側）。
pub(crate) const RETRY_MIN_BUDGET: Duration = Duration::from_secs(10);

/// deadline までの残り。None = 無制限。既に過ぎていれば ZERO。
fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

/// future を残り予算で包む。予算超過はフェーズと経過 ms 入りの Timeout。
async fn bounded<T>(
    deadline: Option<Instant>,
    started: Instant,
    phase: &str,
    fut: impl std::future::Future<Output = Result<T, MatError>>,
) -> Result<T, MatError> {
    match remaining(deadline) {
        None => fut.await,
        Some(rem) => match tokio::time::timeout(rem, fut).await {
            Ok(r) => r,
            Err(_) => Err(MatError::new(
                ErrorKind::Timeout,
                format!(
                    "op deadline exceeded after {}ms in {phase}",
                    started.elapsed().as_millis()
                ),
            )),
        },
    }
}
```

`with_session` を置き換え（doc コメントも更新: deadline 執行と予算条件付き再確立を追記）:

```rust
    async fn with_session<F, T>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        op: F,
    ) -> Result<T, MatError>
    where
        F: for<'a> Fn(
            &'a mut Box<dyn NodeConn>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>,
        >,
    {
        let started = Instant::now();
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            // cold: warm セッションが無いので CASE から張る。（既存コメント維持）
            tracing::info!(node_id, "no warm session; establishing");
            *guard = Some(
                bounded(
                    deadline,
                    started,
                    "establish",
                    self.engine.establisher.establish(node_id),
                )
                .await?,
            );
        }
        let result = bounded(
            deadline,
            started,
            "send",
            op(guard.as_mut().expect("established above")),
        )
        .await;
        match result {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Timeout => {
                // MRP 尽き or deadline 超過 — どちらも session は持ち越さない。
                *guard = None;
                // 残り予算が RETRY_MIN_BUDGET 未満なら再確立を撃たない（Issue #16:
                // 呼び出し側の予算内に応答できない再送は最初から無駄）。
                if let Some(rem) = remaining(deadline) {
                    if rem < RETRY_MIN_BUDGET {
                        tracing::info!(
                            node_id,
                            remaining_ms = u64::try_from(rem.as_millis()).unwrap_or(u64::MAX),
                            "skipping re-establish; insufficient budget"
                        );
                        return Err(e);
                    }
                }
                tracing::info!(
                    node_id,
                    "native session send timed out; re-establishing once"
                );
                *guard = Some(
                    bounded(
                        deadline,
                        started,
                        "resend-establish",
                        self.engine.establisher.establish(node_id),
                    )
                    .await?,
                );
                let retried = bounded(
                    deadline,
                    started,
                    "resend",
                    op(guard.as_mut().expect("re-established")),
                )
                .await;
                if let Err(e2) = &retried {
                    // 再送側も slot 衛生を揃える: session 健全なエラー
                    // （DeviceRejected/ParseError）以外は持ち越さない。
                    if !matches!(e2.kind, ErrorKind::DeviceRejected | ErrorKind::ParseError) {
                        *guard = None;
                    }
                }
                retried
            }
            Err(e) if matches!(e.kind, ErrorKind::DeviceRejected | ErrorKind::ParseError) => Err(e),
            Err(e) => {
                // （既存コメント維持）
                tracing::info!(
                    node_id,
                    kind = ?e.kind,
                    "native session error; dropping session for lazy re-establish"
                );
                *guard = None;
                Err(e)
            }
        }
    }
```

pub メソッド群: 各シグネチャ末尾に `deadline: Option<Instant>` を追加し `with_session(node_id, deadline, ..)` へ渡す（Interfaces 節のシグネチャどおり）。`provision_node` は不変で内部を `self.with_session(node_id, None, ..)` にする（doc に「provision は deadline 対象外（spec）」を追記）。

`server.rs`: `native_op` 内の backend 呼び出し全箇所（`native.on(..)` 等）に `, None` を追加（コンパイルを通すだけ。実配線は Task 5）。

既存テスト（`reuses_warm_session_for_same_node` 等）の呼び出しに `, None` を追加。

- [ ] **Step 4: テスト確認**

Run: `cargo test -p matd --lib`
Expected: PASS（新規 4 本 + 既存全部）

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/matd/src/native.rs crates/matd/src/server.rs
git commit -m "feat(matd): with_session の deadline 執行 — 予算超過=slot破棄+timeout、再確立は残り予算条件付き（Issue #16）"
```

---

### Task 5: server の予算適用 — deadline_ms → Instant 配線（matd/server.rs）

**Files:**
- Modify: `crates/matd/src/server.rs`（`dispatch` :413、`run_op` :469、`native_op` :800、unit tests）
- Test: `crates/matd/tests/integration.rs`（deadline 統合テスト + fake 注入ヘルパ）

**Interfaces:**
- Consumes: Task 2 の `Request.deadline_ms`、Task 4 の backend メソッド（`deadline: Option<Instant>` 引数）
- Produces:
  - `const DEFAULT_OP_BUDGET: Duration = Duration::from_secs(60);`
  - `fn op_deadline(deadline_ms: Option<u64>) -> Option<Instant>`（unit テスト対象）
  - `dispatch` → `run_op` → `native_op` に `deadline: Option<Instant>` が通る
  - integration.rs に `start_matd_with_est(store_path, est) -> (PathBuf, JoinHandle)` ヘルパ（Task 6 も使う）

- [ ] **Step 1: unit テストを書く**

`server.rs` の mod tests に追加:

```rust
    #[test]
    fn op_deadline_semantics() {
        // Some(0) = 明示無制限。
        assert!(op_deadline(Some(0)).is_none());
        // Some(n) = 今 + n ms（下限だけ確認 — 実行遅延で厳密比較はしない）。
        let d = op_deadline(Some(5_000)).expect("finite budget");
        assert!(d <= std::time::Instant::now() + std::time::Duration::from_millis(5_000));
        // None（旧クライアント）= 既定 60s。
        let d = op_deadline(None).expect("default budget");
        assert!(d > std::time::Instant::now() + std::time::Duration::from_secs(59));
    }
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p matd --lib op_deadline 2>&1 | head -10`
Expected: FAIL（`op_deadline` 未定義）

- [ ] **Step 3: 実装**

`server.rs` に追加（`SLOW_OP_MS` 等の定数の近く）:

```rust
/// deadline_ms 未指定（旧 mat クライアント）の単一ノード op に適用する既定予算。
/// per-node Mutex の無期限保持を防ぐ受け皿（Issue #16）。
const DEFAULT_OP_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// リクエストの deadline_ms を絶対時刻へ変換する（単一ノード op 用）。
/// `Some(0)` = 明示無制限、`Some(n)` = n ms、`None`（旧クライアント）= 既定 60s。
fn op_deadline(deadline_ms: Option<u64>) -> Option<std::time::Instant> {
    match deadline_ms {
        Some(0) => None,
        Some(n) => Some(std::time::Instant::now() + std::time::Duration::from_millis(n)),
        None => Some(std::time::Instant::now() + DEFAULT_OP_BUDGET),
    }
}
```

`dispatch`: パース成功後に `let deadline = op_deadline(req.deadline_ms);` を計算し、`run_op(&req.op, native, store_path, health, deadline)` へ渡す。

`run_op`: シグネチャに `deadline: Option<std::time::Instant>` を追加。`is_native_hotpath` 分岐で `native_op(op, native, store_path, deadline)` へ渡す。group / provision / bump 分岐は使わない（deadline 対象外）。

`native_op`: シグネチャに `deadline: Option<std::time::Instant>` を追加し、Task 4 で `None` にしていた backend 呼び出し全箇所を `deadline` に置き換える。`server.rs` 内の既存テスト `native_op_invariant_violations_are_typed_errors_not_panics`（:1868 付近）等、`native_op` / `run_op` を直接呼ぶテストには `, None` を追随させる。

- [ ] **Step 4: integration テストを書く**

`crates/matd/tests/integration.rs` に、既存 `start_matd_with_fake`（:117）を一般化したヘルパと 1 テストを追加（store 準備・roundtrip は既存 `read_write_invoke_on_ping_and_errors_roundtrip` :135 のパターンをそのまま踏襲する）:

```rust
/// FakeEstablisher を注入して matd を起動する（deadline / 切断キャンセルのテスト用）。
async fn start_matd_with_est(
    store_path: PathBuf,
    est: FakeEstablisher,
) -> (PathBuf, tokio::task::JoinHandle<()>) {
    // 実装は start_matd_with_fake と同一で、FakeEstablisher::default() の代わりに
    // 引数の est を使う。start_matd_with_fake は本ヘルパの薄い wrapper に変える。
    ...
}

#[tokio::test]
async fn deadline_cuts_slow_op_with_structured_timeout() {
    // conn_delay 300ms の fake に deadline_ms=50 の read を送る。
    // （store 準備は read_write_invoke_on_ping_and_errors_roundtrip と同じ手順）
    let est = FakeEstablisher {
        conn_delay: Some(std::time::Duration::from_millis(300)),
        ..Default::default()
    };
    let (socket, _h) = start_matd_with_est(store, est).await;
    let resps = roundtrip(
        &socket,
        &[
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off","deadline_ms":50}),
            // 明示無制限（0）は遅くても成功する。
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off","deadline_ms":0}),
            // 未指定（旧クライアント）は既定 60s → 成功する。
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"}),
        ],
    )
    .await;
    assert_eq!(resps[0]["error"]["kind"], "timeout", "resp: {}", resps[0]);
    assert!(resps[0]["error"]["detail"].as_str().unwrap().contains("deadline"));
    assert_eq!(resps[1]["value"], json!(true), "resp: {}", resps[1]);
    assert_eq!(resps[2]["value"], json!(true), "resp: {}", resps[2]);
}
```

注意: roundtrip ヘルパ（:42）が「全リクエスト送信 → 全応答読み」型なら逐次処理でそのまま使える。1 接続 1 リクエスト型なら 3 回に分ける — 既存実装に従う。

- [ ] **Step 5: テスト確認**

Run: `cargo test -p matd`
Expected: PASS（unit + integration 全部）

- [ ] **Step 6: task check + コミット**

```bash
task check
git add crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): 単一ノード op に予算適用 — deadline_ms/既定60s を with_session へ配線（Issue #16）"
```

---

### Task 6: クライアント切断で進行中 op をキャンセル（matd/server.rs + native.rs）

**Files:**
- Modify: `crates/matd/src/server.rs`（`handle_conn` :108-181）
- Modify: `crates/matd/src/native.rs`（`drop_session` 追加）
- Test: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 5 の `start_matd_with_est`、Task 3 の `conn_delay`
- Produces:
  - `NativeBackend::drop_session(&self, node_id: u64)`（pub async、戻り値なし）
  - `handle_conn` の select! 化（外部インターフェース不変）

- [ ] **Step 1: integration テストを書く**

```rust
#[tokio::test]
async fn client_disconnect_aborts_op_and_drops_slot() {
    // store 準備は read_write_invoke_on_ping_and_errors_roundtrip と同じ手順。
    use tokio::io::AsyncWriteExt;
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let est = FakeEstablisher {
        calls: std::sync::Arc::clone(&calls),
        conn_delay: Some(std::time::Duration::from_millis(500)),
        ..Default::default()
    };
    let (socket, _h) = start_matd_with_est(store, est).await;

    // conn A: read を送って 100ms で切断（mando が mat を kill する形の再現）。
    let mut a = tokio::net::UnixStream::connect(&socket).await.unwrap();
    a.write_all(
        b"{\"op\":\"read\",\"node_id\":1,\"endpoint\":1,\"cluster\":\"onoff\",\"attribute\":\"on-off\"}\n",
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(a);
    // abort 処理（future drop + slot 破棄）が走る猶予。
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // conn B: 同一ノードへ read。キャンセルが効いていれば
    // (a) Mutex 待ちせず進み、(b) slot 破棄済みなので establish は 2 回目になる。
    let resps = roundtrip(
        &socket,
        &[json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"})],
    )
    .await;
    assert_eq!(resps[0]["value"], json!(true), "resp: {}", resps[0]);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "conn A の establish + slot 破棄後の conn B 再確立で 2 回のはず"
    );
}

#[tokio::test]
async fn pipelined_second_request_is_buffered_not_lost() {
    // 1 接続に 2 リクエストを一気に書いても、両方に順番どおり応答が返る
    // （select! 化で追加行がバッファされることの釘打ち）。
    let (socket, _h) = start_matd_with_fake(store).await;
    let resps = roundtrip(
        &socket,
        &[
            json!({"id":1,"op":"ping"}),
            json!({"id":2,"op":"ping"}),
        ],
    )
    .await;
    assert_eq!(resps[0]["id"], json!(1));
    assert_eq!(resps[1]["id"], json!(2));
}
```

注意: `roundtrip` が元々 2 リクエストを一括送信する実装ならば 2 本目のテストは既存挙動の釘打ちとしてそのまま通る。1 行ずつ往復する実装なら、一括書き込みする専用のヘルパをこのテスト内に書く。

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p matd --test integration client_disconnect 2>&1 | tail -5`
Expected: FAIL（キャンセル未実装 — conn A の op が 500ms 走り切って warm slot が残るため establish 回数が 1 のまま、または B の応答が遅延）

- [ ] **Step 3: drop_session と handle_conn select! 化を実装**

`native.rs` に追加:

```rust
    /// クライアント切断で進行中 op が破棄された後の始末。中途 exchange の
    /// session を次 op に持ち越さないよう slot を破棄する（次回 lazy 再確立）。
    /// op の future は drop 済みなので内側ロックはすぐ取れる。
    pub async fn drop_session(&self, node_id: u64) {
        let slot = self.slot(node_id).await;
        *slot.lock().await = None;
    }
```

`server.rs` の `handle_conn` ループを置き換え（listen 先取り部は無変更）:

```rust
    let mut pending_line: Option<String> = None;
    loop {
        let line = match pending_line.take() {
            Some(l) => l,
            None => match lines.next_line().await? {
                Some(l) => l,
                None => break,
            },
        };
        if line.trim().is_empty() {
            continue;
        }
        // （listen 先取り if let はここに既存のまま）

        let started = std::time::Instant::now();
        // ブロックスコープで dispatch future の寿命を区切る: ClientGone で break
        // した時点ではまだ future が per-node Mutex を握っている可能性があり、
        // その状態で abort_op（slot 破棄の lock().await）を呼ぶとデッドロック
        // する。ブロックを抜けて future を drop してから後始末する。
        let turn = {
            let dispatch_fut = dispatch(&line, &native, &store_path, &health);
            tokio::pin!(dispatch_fut);
            loop {
                tokio::select! {
                    res = &mut dispatch_fut => break OpTurn::Done(res),
                    // op 実行中の追加行は 1 行だけバッファ（逐次セマンティクス維持）。
                    // バッファ済みなら次の行は読まない（取りこぼし防止）。
                    next = lines.next_line(), if pending_line.is_none() => match next {
                        Ok(Some(l)) => pending_line = Some(l),
                        // クライアント切断: op を破棄する。future drop で per-node
                        // Mutex が解放され、後続 op の head-of-line blocking が
                        // 消える（Issue #16）。応答は書かない（相手がいない）。
                        _ => break OpTurn::ClientGone,
                    },
                }
            }
        }; // ← ここで dispatch future が drop され、Mutex が解放される
        let (response, is_shutdown) = match turn {
            OpTurn::Done(res) => res,
            OpTurn::ClientGone => {
                abort_op(&line, &native, started).await;
                return Ok(());
            }
        };
        // （応答書き込み・flush・shutdown 分岐は既存のまま）
    }
```

`handle_conn` の近くに補助 enum を追加:

```rust
/// 1 op の帰結: 応答あり（通常）か、クライアント切断で放棄したか。
enum OpTurn {
    Done((Value, bool)),
    ClientGone,
}
```

`abort_op` を追加:

```rust
/// クライアント切断で放棄された op の後始末: 観測ログ + 単一ノード op なら
/// slot 破棄（drop された op future が session を中途 exchange のまま残しうる）。
/// `line` の再パースは切断時のみのコストで、通常経路には乗らない。
async fn abort_op(line: &str, native: &NativeState, started: std::time::Instant) {
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (op_name, node_id) = match serde_json::from_str::<Request>(line) {
        Ok(req) => (req.op.name(), req.op.node_id()),
        Err(_) => ("unknown", None),
    };
    tracing::warn!(op = op_name, node_id, elapsed_ms, "op aborted (client disconnected)");
    if let (Some(node_id), NativeState::Ready(b)) = (node_id, native) {
        b.drop_session(node_id).await;
    }
}
```

- [ ] **Step 4: テスト確認**

Run: `cargo test -p matd`
Expected: PASS（新規 2 本 + 既存全部。listen のテストが無変更で通ることも確認）

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/matd/src/server.rs crates/matd/src/native.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): クライアント切断で進行中 op をキャンセル — future drop + slot 破棄（Issue #16）"
```

---

### Task 7: mat CLI の --op-timeout-ms と matd 経路（cli.rs / matd_client.rs / main.rs）

**Files:**
- Modify: `crates/mat/src/cli.rs`（`Cli` グローバル引数 :20-69）
- Modify: `crates/mat/src/matd_client.rs`（`dispatch` :98、`dispatch_auto` :139、`exchange_on_stream` :428、unit tests）
- Modify: `crates/mat/src/main.rs`（呼び出し配線 :116-121）

**Interfaces:**
- Consumes: Task 2 の deadline_ms セマンティクス（`0` = 明示無制限）
- Produces:
  - `Cli.op_timeout_ms: u64`（グローバル、env `MAT_OP_TIMEOUT_MS`、既定 60_000）
  - `matd_client::dispatch(sockets, command, op_timeout_ms: u64)` / `dispatch_auto(同)`
  - `fn attach_deadline(op: &mut Value, op_timeout_ms: u64) -> Option<Duration>`（private、unit テスト対象）
  - `exchange_on_stream(stream, op, read_timeout: Option<Duration>)`（read timeout 超過 = `ErrorKind::Timeout`）
  - `const CLIENT_SLACK: Duration = Duration::from_secs(2);`

- [ ] **Step 1: 失敗する unit テストを書く**

`matd_client.rs` の mod tests に追加:

```rust
    #[test]
    fn attach_deadline_only_for_single_node_ops() {
        // 単一ノード op（top-level node_id あり）: deadline_ms が付き read timeout が返る。
        let mut op = json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"});
        let rt = attach_deadline(&mut op, 15_000);
        assert_eq!(op["deadline_ms"], json!(15_000));
        assert_eq!(rt, Some(std::time::Duration::from_millis(15_000) + CLIENT_SLACK));

        // 0 = 明示無制限: フィールドは付く（matd の既定 60s を止める）が read timeout なし。
        let mut op = json!({"op":"on","node_id":3,"endpoint":1});
        let rt = attach_deadline(&mut op, 0);
        assert_eq!(op["deadline_ms"], json!(0));
        assert_eq!(rt, None);

        // group 系（node_id なし）: 無変更・read timeout なし。
        let mut op = json!({"op":"group_invoke","group_id":10,"cluster":"onoff","command":"on","endpoint":1});
        let rt = attach_deadline(&mut op, 15_000);
        assert!(op.get("deadline_ms").is_none());
        assert_eq!(rt, None);

        // ping / group_bump も対象外。
        let mut op = json!({"op":"ping"});
        assert!(attach_deadline(&mut op, 15_000).is_none());
        assert!(op.get("deadline_ms").is_none());
    }

    #[test]
    fn exchange_read_timeout_maps_to_timeout_kind() {
        // 応答しないサーバ相手に read timeout → ErrorKind::Timeout（exit 3）。
        let (client, _server) = UnixStream::pair().unwrap();
        let err = exchange_on_stream(
            client,
            &json!({"op":"ping"}),
            Some(std::time::Duration::from_millis(100)),
        )
        .expect_err("must time out");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.detail.contains("may have been executed"), "detail: {}", err.detail);
    }
```

既存の `exchange_on_stream` テスト（:1040, :1066 付近）は第 3 引数 `None` を追加して維持する。

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat --lib matd_client 2>&1 | head -10`
Expected: FAIL（`attach_deadline` / `CLIENT_SLACK` 未定義、シグネチャ不一致）

- [ ] **Step 3: 実装**

`cli.rs` の `Cli` にグローバル引数を追加（`issuer_index` の後）:

```rust
    /// 単一ノード op（read/write/invoke/on/off/color/color-temp/level/describe）
    /// の予算 ms。matd 経路では deadline としてリクエストに載り、matd が予算内に
    /// 構造化 timeout（exit 3）を返す。直経路では op 全体を同じ予算で打ち切る。
    /// 0 = 無制限。`mat listen` の `--timeout-ms`（ストリーム受信予算）とは別物。
    #[arg(
        long = "op-timeout-ms",
        global = true,
        env = "MAT_OP_TIMEOUT_MS",
        default_value_t = 60_000,
        value_name = "MS"
    )]
    pub op_timeout_ms: u64,
```

`matd_client.rs`:

```rust
use std::time::Duration;

/// matd の構造化エラーを待つ read timeout の余裕。matd は予算ちょうどで構造化
/// timeout を返すので、こちらは予算 + slack まで待って必ず先に受け取る。
/// slack を使い切る（= matd が予算内に応答しない）のは旧 matd か matd 停止。
const CLIENT_SLACK: Duration = Duration::from_secs(2);

/// 単一ノード op（top-level に node_id を持つ op JSON）へ deadline_ms を付与し、
/// 適用時の read timeout を返す。非対象 op は無変更・read timeout なし。
/// 0 = 明示無制限（matd 既定 60s の適用を止める）— read timeout も掛けない。
fn attach_deadline(op: &mut Value, op_timeout_ms: u64) -> Option<Duration> {
    let Value::Object(map) = op else { return None };
    if !map.contains_key("node_id") {
        return None;
    }
    map.insert("deadline_ms".into(), json!(op_timeout_ms));
    (op_timeout_ms > 0).then(|| Duration::from_millis(op_timeout_ms) + CLIENT_SLACK)
}
```

`dispatch` / `dispatch_auto`: シグネチャに `op_timeout_ms: u64` を追加。`to_op` 成功後に

```rust
    let mut op = op;
    let read_timeout = attach_deadline(&mut op, op_timeout_ms);
```

とし、`exchange_on_stream(stream, &op, read_timeout)` を呼ぶ。

`exchange_on_stream`: 第 3 引数 `read_timeout: Option<Duration>` を追加。`write_all` の後に

```rust
    if let Some(t) = read_timeout {
        stream
            .set_read_timeout(Some(t))
            .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to set read timeout: {e}")))?;
    }
```

read エラーの map を分岐（timeout は専用 kind — mando が kill ではなく exit 3 を受け取れる）:

```rust
    let n = reader.read_line(&mut resp).map_err(|e| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            MatError::new(
                ErrorKind::Timeout,
                format!("no response from matd within the op budget: {e}; the request may have been executed"),
            )
        } else {
            MatError::new(
                ErrorKind::MatdUnavailable,
                format!("failed to read response from matd: {e}; the request may have been executed"),
            )
        }
    })?;
```

`main.rs`: `args.op_timeout_ms` を束縛し（`args.command` の partial move より前に読む必要はない — u64 は Copy）、`matd_client::dispatch(&sockets, &command, args.op_timeout_ms)` / `dispatch_auto(&sockets, &command, args.op_timeout_ms)` へ渡す。`dispatch_listen` は無変更。

- [ ] **Step 4: テスト確認**

Run: `cargo test -p mat`
Expected: PASS

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/mat/src/cli.rs crates/mat/src/matd_client.rs crates/mat/src/main.rs
git commit -m "feat(mat): --op-timeout-ms — matd 経路へ deadline_ms 伝播 + read timeout で exit 3（Issue #16）"
```

---

### Task 8: mat 直経路の予算適用（native_direct.rs）

**Files:**
- Modify: `crates/mat/src/native_direct.rs`（`NativeOp` に `budget_applies`、`run` :579、`execute` :672）
- Modify: `crates/mat/src/main.rs`（`native_direct::run` 呼び出し :147）

**Interfaces:**
- Consumes: Task 7 の `Cli.op_timeout_ms`
- Produces:
  - `NativeOp::budget_applies(&self) -> bool`（unit テスト対象）
  - `run(command, store_path, cfg, op_timeout_ms: u64)` / `execute(op, store_path, cfg, op_timeout_ms: u64)`

- [ ] **Step 1: 失敗する unit テストを書く**

`native_direct.rs` の mod tests に追加:

```rust
    #[test]
    fn budget_applies_only_to_single_node_hotpath_ops() {
        // 対象: 単一ノードの hotpath op（spec の適用範囲）。
        assert!(NativeOp::On { node_id: 1, endpoint: 1 }.budget_applies());
        assert!(NativeOp::ReadOnOff { node_id: 1, endpoint: 1 }.budget_applies());
        assert!(NativeOp::Describe { node_id: 1 }.budget_applies());
        // 対象外: open-window（commission フロー）・group 系・bump。
        // （variant のフィールドは実定義に合わせて構築する）
        assert!(!NativeOp::GroupBump.budget_applies());
    }
```

注意: `NativeOp` の variant フィールド（`Describe` / `OpenWindow` / `GroupOnOff` 等）は `native_direct.rs:35-` の実定義を見て正しく構築する。`Describe` にフィールドが複数ある場合はそれに従う。

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat --lib budget_applies 2>&1 | head -10`
Expected: FAIL（`budget_applies` 未定義）

- [ ] **Step 3: 実装**

```rust
impl NativeOp {
    /// `--op-timeout-ms` の適用対象か（spec: 単一ノードの read/write/invoke/
    /// on/off/color 系/level/describe のみ）。open-window / diag thread /
    /// group 系 / provision / grant / bump は対象外。
    fn budget_applies(&self) -> bool {
        matches!(
            self,
            NativeOp::On { .. }
                | NativeOp::Off { .. }
                | NativeOp::ReadOnOff { .. }
                | NativeOp::Color { .. }
                | NativeOp::ColorTemp { .. }
                | NativeOp::Level { .. }
                | NativeOp::ReadAttr { .. }
                | NativeOp::WriteAttr { .. }
                | NativeOp::InvokeGeneric { .. }
                | NativeOp::Describe { .. }
        )
    }
}
```

`execute` にパラメータ `op_timeout_ms: u64` を追加し、`rt.block_on` 内を:

```rust
    rt.block_on(async {
        let native_cfg = NativeConfig { /* 既存のまま */ };
        let engine = Engine::build(&native_cfg)
            .await
            .map_err(map_engine_build_error)?;
        if op.budget_applies() && op_timeout_ms > 0 {
            // 直経路にも matd 経路と同じ予算セマンティクス（exit 3）。
            match tokio::time::timeout(
                std::time::Duration::from_millis(op_timeout_ms),
                run_op(&engine, op),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(MatError::new(
                    mat_core::error::ErrorKind::Timeout,
                    format!("op deadline exceeded after {op_timeout_ms}ms (direct path)"),
                )),
            }
        } else {
            run_op(&engine, op).await
        }
    })
```

`run` にも `op_timeout_ms: u64` を追加して `execute` へ渡す。`main.rs` の呼び出しを `native_direct::run(&command, &store_path, cfg, args.op_timeout_ms)` に更新。

- [ ] **Step 4: テスト確認**

Run: `cargo test -p mat`
Expected: PASS

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/mat/src/native_direct.rs crates/mat/src/main.rs
git commit -m "feat(mat): 直経路にも --op-timeout-ms 適用 — 単一ノード op を予算で打ち切り exit 3（Issue #16）"
```

---

### Task 9: ドキュメントとバージョン 1.10.0

**Files:**
- Modify: `Cargo.toml`（workspace.package.version `1.9.0` → `1.10.0`）
- Modify: `README.md`（`--op-timeout-ms`、timeout の説明）
- Modify: `ARCHITECTURE.md`（matd 節に op 予算・切断キャンセルの記録）

**Interfaces:**
- Consumes: Task 1-8 の確定挙動
- Produces: リリース可能な 1.10.0（実機 E2E は finishing 段階）

- [ ] **Step 1: バージョン更新**

`Cargo.toml` の `[workspace.package] version = "1.9.0"` → `"1.10.0"`。
`cargo build -p mat 2>&1 | tail -2` で Cargo.lock を追随させる。

- [ ] **Step 2: README 更新**

- グローバルオプションの説明がある節（`--matd` / `MAT_IFACE` 等が載っている場所）に `--op-timeout-ms`（env `MAT_OP_TIMEOUT_MS`、既定 60000、0 = 無制限、対象 = 単一ノード op、listen の `--timeout-ms` とは別物）を追記
- "Errors and exit codes" の `timeout` 行に「op 予算超過（`--op-timeout-ms` / matd 既定 60s）を含む」旨を追記
- matd のプロトコル説明があれば `deadline_ms`（任意、相対 ms、0 = 無制限、未指定 = matd 既定 60s）を追記

- [ ] **Step 3: ARCHITECTURE 更新**

matd 節（warm session の説明がある場所）に 3-4 行で追記: 単一ノード op は予算付き（`deadline_ms` 伝播、未指定 60s）で、予算超過は slot 破棄 + 構造化 timeout。再確立+再送は残り予算 ≥ 10s のときのみ。クライアント切断は進行中 op を drop し per-node Mutex を即解放。spec へのリンク（`docs/superpowers/specs/2026-07-29-op-deadline-budget-design.md`）。

- [ ] **Step 4: 全体検証 + コミット**

```bash
task check
git add Cargo.toml Cargo.lock README.md ARCHITECTURE.md
git commit -m "chore: 1.10.0（op 予算 deadline の伝播と執行 — Issue #16）+ README/ARCHITECTURE"
```

---

## 完了後（本計画の外）

1. superpowers:requesting-code-review でレビュー
2. **jarvis 実機 E2E（マージ前必須）**: 隔離 matd 方式（`*.new` バイナリ、本番未置換）
   - `mat read --op-timeout-ms 3000` が不達ノード相当で ~3s+2s 以内に exit 3
   - 旧 mat（1.9.0）→ 新 matd の通常 op 成功（deadline_ms 無し = 既定 60s）
   - 新 mat → 新 matd の通常 op 成功
3. superpowers:finishing-a-development-branch（merge → デプロイは despliegue skill）
4. nogu3/mando に「`mat` 呼び出しへ `--op-timeout-ms 13000` を渡す」issue を起票し Issue #16 にリンク、その後 #16 クローズ
