# `matd status` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `matd status` サブコマンドを追加し、購読のノード別ライフサイクル状態とデーモン基本情報を 1 回の socket 往復で JSON として返す（安定性監査 Tier 2 ⑥）。

**Architecture:** socket プロトコルに node 無し admin op `status` を追加。購読ライフサイクル状態は server と購読 pump の既存共有点 `SubHealth` に node_id → `NodeSubStatus` レジストリとして足し、購読ループの既存ログ出力箇所と 1:1 の遷移点で書く。応答は dispatch 内で完結（デバイス・ワイヤに触れない）。

**Tech Stack:** Rust / tokio / serde_json。spec: `docs/superpowers/specs/2026-07-30-matd-status-design.md`

**Branch:** `feat/matd-status`（実行開始時に main から作成）

## Global Constraints

- stdout は純粋 JSON のみ（装飾なし）。`timestamp` は ISO 8601 必須（dispatch が自動付与）。
- 期間系フィールドは全て「今からの経過秒」（`for_s` / `..._ago_s` / `uptime_s` / `backoff_s`）。
- matd 不在時は `{"error":{"kind":"other","detail":"matd not running at ..."}}` で exit 1（`matd_unavailable`/13 は `mat listen` 専用契約 — 広げない）。
- `mat` 側には status を入れない（入り口は matd バイナリのみ）。
- レジストリは ephemeral なプロセス内状態のみ（設計ルール 4 の永続状態に該当しない）。
- 各タスクの最後に `cargo test -p matd` が通ること。最終タスクで `task check`。
- コミットメッセージ末尾に Co-Authored-By / Claude-Session トレーラを付ける（セッション既定）。

---

### Task 1: protocol — `Op::Status`

**Files:**
- Modify: `crates/matd/src/protocol.rs`

**Interfaces:**
- Produces: `Op::Status`（フィールド無し variant）、`op.name() == "status"`、`node_id()/group_id()/endpoint()/log_path()` は全て `None`。Task 4 の dispatch がこの variant で分岐する。

- [ ] **Step 1: Write the failing test**

`protocol.rs` の `mod tests` 末尾に追加:

```rust
#[test]
fn status_has_no_node_and_matches_wire_tag() {
    // admin op（`matd status` が送る）。native にもデバイスにも触れない。
    let r = parse(r#"{"op":"status"}"#);
    assert!(matches!(r.op, Op::Status));
    assert_eq!(r.op.node_id(), None);
    assert_eq!(r.op.group_id(), None);
    assert_eq!(r.op.endpoint(), None);
    assert_eq!(r.op.log_path(), None);
    assert_eq!(r.op.name(), "status");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p matd status_has_no_node`
Expected: コンパイルエラー（`Op::Status` 未定義）

- [ ] **Step 3: Write minimal implementation**

`enum Op` の `Ping` の直後に追加:

```rust
    /// 購読とデーモンの現況を返す admin op（`matd status` が送る）。
    /// Ping と同じく単一 node は持たず、デバイス・ワイヤには触れない
    /// （dispatch がレジストリ snapshot を JSON 化するだけ）。
    Status,
```

4 つの網羅 match に `Op::Status` を追加（コンパイラが漏れを検出する）:

- `node_id()`: `| Op::Status` を `Op::Ping` の隣の None 群へ
- `name()`: `Op::Status => "status",`
- `group_id()`: None 群へ
- `endpoint()`: None 群へ
- `log_path()`: None 群へ

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p matd status_has_no_node`
Expected: PASS（`cargo test -p matd` 全体も PASS — server.rs 側は catch-all arm があるためこの時点で壊れない）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/protocol.rs
git commit -m "feat(matd): socket protocol に status op — 監査⑥の観測口"
```

---

### Task 2: SubHealth — 購読ライフサイクルレジストリ

**Files:**
- Modify: `crates/matd/src/subscription.rs`

**Interfaces:**
- Consumes: 既存 `SubHealth`（`pending` / `values` / `clusters` フィールド、`pending_elapsed()`）。
- Produces（Task 3 / Task 4 が使う）:
  - `pub(crate) enum NodeSubStatus`（Establishing / Established / Down）
  - `SubHealth::mark_establishing(&self, node_id: u64)`
  - `SubHealth::mark_established(&self, node_id: u64, subscription_id: u32, max_interval_s: u16)`
  - `SubHealth::note_device_msg(&self, node_id: u64)`
  - `SubHealth::mark_down(&self, node_id: u64, since: tokio::time::Instant, attempts: u32, backoff: Duration, last_error: MatError)`
  - `SubHealth::clusters(&self) -> Option<&[u32]>`（空 = wildcard = None）
  - `SubHealth::status_nodes(&self) -> Vec<serde_json::Value>`（node_id 昇順）

- [ ] **Step 1: Write the failing tests**

`subscription.rs` の `mod tests` に追加:

```rust
/// レジストリの遷移と JSON 形（spec の応答スキーマ nodes 配列）。
/// tokio::time::Instant なので start_paused + advance で経過秒を決定化できる。
#[tokio::test(start_paused = true)]
async fn status_nodes_reflects_lifecycle_transitions() {
    use serde_json::json;
    let h = SubHealth::new(None);
    assert!(h.status_nodes().is_empty());

    // establishing: spawn 直後。
    h.mark_establishing(5);
    tokio::time::advance(Duration::from_secs(2)).await;
    let n = h.status_nodes();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["node_id"], 5);
    assert_eq!(n[0]["state"], "establishing");
    assert_eq!(n[0]["for_s"], 2);

    // established: 確立時刻から for_s、受信で last_device_msg_ago_s が縮む。
    h.mark_established(5, 7, 300);
    tokio::time::advance(Duration::from_secs(40)).await;
    h.note_device_msg(5);
    tokio::time::advance(Duration::from_secs(2)).await;
    let n = h.status_nodes();
    assert_eq!(n[0]["state"], "established");
    assert_eq!(n[0]["for_s"], 42);
    assert_eq!(n[0]["subscription_id"], 7);
    assert_eq!(n[0]["max_interval_s"], 300);
    assert_eq!(n[0]["last_device_msg_ago_s"], 2);
    assert_eq!(n[0]["pending_op_ago_s"], serde_json::Value::Null);

    // op 相関 pending が経過秒で載る。
    h.note_op(5, 0x0006);
    tokio::time::advance(Duration::from_secs(3)).await;
    assert_eq!(h.status_nodes()[0]["pending_op_ago_s"], 3);

    // down: attempts / backoff_s / last_error（kind は snake_case 名）。
    h.clear_pending(5);
    h.mark_down(
        5,
        tokio::time::Instant::now(),
        3,
        Duration::from_secs(20),
        mat_core::error::MatError::new(mat_core::error::ErrorKind::Unreachable, "no route"),
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    let n = h.status_nodes();
    assert_eq!(n[0]["state"], "down");
    assert_eq!(n[0]["for_s"], 1);
    assert_eq!(n[0]["attempts"], 3);
    assert_eq!(n[0]["backoff_s"], 20);
    assert_eq!(n[0]["last_error"], json!({"kind": "unreachable", "detail": "no route"}));

    // node_id 昇順の安定出力。
    h.mark_establishing(2);
    let n = h.status_nodes();
    assert_eq!(n[0]["node_id"], 2);
    assert_eq!(n[1]["node_id"], 5);
}

/// clusters(): 空 = full wildcard = None、非空はそのまま。
#[test]
fn clusters_exposes_narrowing_none_for_wildcard() {
    assert!(SubHealth::new(None).clusters().is_none());
    assert_eq!(
        SubHealth::new(Some(vec![0x0006, 0x0406])).clusters(),
        Some(&[0x0006u32, 0x0406][..])
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p matd status_nodes_reflects_lifecycle -- --nocapture` および `cargo test -p matd clusters_exposes_narrowing`
Expected: コンパイルエラー（`mark_establishing` 等未定義）

- [ ] **Step 3: Write the implementation**

`subscription.rs` の import に `use mat_core::error::MatError;` を追加（`ErrorKind` はテスト側でフルパス参照なので本体は MatError のみ）。

`SubHealth` 定義の直前に enum を追加:

```rust
/// 購読ライフサイクル状態（status op が読む）。「ログに出す状態遷移は
/// レジストリにも書く」が規律 — 遷移点は node_subscription_loop /
/// run_subscription_once の既存ログ出力箇所と 1:1。
#[derive(Debug, Clone)]
pub(crate) enum NodeSubStatus {
    /// spawn 直後〜初回確立前のみ。喪失後の再試行中は Down のまま
    /// （attempts が増える — down_since / classify_failure と同じ見方）。
    Establishing { since: tokio::time::Instant },
    /// 購読成立中。last_device_msg はデバイス発メッセージ
    /// （keep-alive 含む）受信のたび更新。
    Established {
        since: tokio::time::Instant,
        subscription_id: u32,
        max_interval_s: u16,
        last_device_msg: tokio::time::Instant,
    },
    /// 確立失敗 or 購読喪失で backoff 中（再確立まで持続）。
    Down {
        since: tokio::time::Instant,
        attempts: u32,
        backoff: Duration,
        last_error: MatError,
    },
}
```

`SubHealth` にフィールドを追加し（doc コメントの役割記述も「op 相関ヘルス表」から「op 相関 + 購読ランタイム状態の共有点」へ広げる）、`new()` で空 init:

```rust
    /// node_id → 購読ライフサイクル状態（status op が読む）。
    status: Mutex<HashMap<u64, NodeSubStatus>>,
```

`impl SubHealth` にメソッドを追加:

```rust
    /// 購読ループ spawn（初回確立前）。
    pub(crate) fn mark_establishing(&self, node_id: u64) {
        self.status.lock().unwrap().insert(
            node_id,
            NodeSubStatus::Establishing {
                since: tokio::time::Instant::now(),
            },
        );
    }

    /// 購読成立（「subscription established」ログと同時に呼ぶ）。
    pub(crate) fn mark_established(&self, node_id: u64, subscription_id: u32, max_interval_s: u16) {
        let now = tokio::time::Instant::now();
        self.status.lock().unwrap().insert(
            node_id,
            NodeSubStatus::Established {
                since: now,
                subscription_id,
                max_interval_s,
                last_device_msg: now,
            },
        );
    }

    /// デバイス発メッセージ受信（keep-alive 含む）。Established のときだけ更新。
    pub(crate) fn note_device_msg(&self, node_id: u64) {
        if let Some(NodeSubStatus::Established {
            last_device_msg, ..
        }) = self.status.lock().unwrap().get_mut(&node_id)
        {
            *last_device_msg = tokio::time::Instant::now();
        }
    }

    /// 確立失敗 or 購読喪失（「subscription lost」/ 失敗ログと同時に呼ぶ）。
    /// since はダウン起点（down_since）、attempts はダウン以降の失敗数。
    pub(crate) fn mark_down(
        &self,
        node_id: u64,
        since: tokio::time::Instant,
        attempts: u32,
        backoff: Duration,
        last_error: MatError,
    ) {
        self.status.lock().unwrap().insert(
            node_id,
            NodeSubStatus::Down {
                since,
                attempts,
                backoff,
                last_error,
            },
        );
    }

    /// 購読対象クラスタ（status 応答用）。空 = full wildcard = None
    /// （subscribe_config は空リストを起動拒否するので混同はない）。
    pub(crate) fn clusters(&self) -> Option<&[u32]> {
        if self.clusters.is_empty() {
            None
        } else {
            Some(&self.clusters)
        }
    }

    /// status 応答の nodes 配列（node_id 昇順の安定出力）。期間は全て
    /// 「今からの経過秒」— 内部時計は tokio::time::Instant で ISO 変換
    /// 不能なため、経過秒が正直な表現（spec）。
    pub(crate) fn status_nodes(&self) -> Vec<serde_json::Value> {
        let status = self.status.lock().unwrap();
        let mut ids: Vec<u64> = status.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
            .map(|id| match &status[&id] {
                NodeSubStatus::Establishing { since } => serde_json::json!({
                    "node_id": id,
                    "state": "establishing",
                    "for_s": since.elapsed().as_secs(),
                }),
                NodeSubStatus::Established {
                    since,
                    subscription_id,
                    max_interval_s,
                    last_device_msg,
                } => serde_json::json!({
                    "node_id": id,
                    "state": "established",
                    "for_s": since.elapsed().as_secs(),
                    "subscription_id": subscription_id,
                    "max_interval_s": max_interval_s,
                    "last_device_msg_ago_s": last_device_msg.elapsed().as_secs(),
                    // 未消化の状態変更 op（op 相関）。通常 null、値が入って
                    // いれば「op 成功後デバイス発ゼロ」を観測中の瞬間。
                    "pending_op_ago_s": self.pending_elapsed(id).map(|d| d.as_secs()),
                }),
                NodeSubStatus::Down {
                    since,
                    attempts,
                    backoff,
                    last_error,
                } => serde_json::json!({
                    "node_id": id,
                    "state": "down",
                    "for_s": since.elapsed().as_secs(),
                    "attempts": attempts,
                    "backoff_s": backoff.as_secs(),
                    "last_error": {
                        "kind": last_error.kind,
                        "detail": last_error.detail,
                    },
                }),
            })
            .collect()
    }
```

注意: `status_nodes` 内の `pending_elapsed` は `status` ロック保持中に `pending` ロックを取る。ロック順は status → pending のこの 1 箇所のみ（他に両方を同時に取る場所は無い）なのでデッドロックしない。

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p matd status_nodes_reflects_lifecycle clusters_exposes_narrowing`
Expected: PASS（`cargo test -p matd` 全体も PASS）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): SubHealth に購読ライフサイクルレジストリ — status op の読み出し元"
```

---

### Task 3: 購読ループの遷移点配線

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`node_subscription_loop` / `run_subscription_once`）

**Interfaces:**
- Consumes: Task 2 の `mark_establishing` / `mark_established` / `note_device_msg` / `mark_down`。
- Produces: `run_subscription_once` の戻り値が `Result<(), MatError>` → `Result<String, MatError>` に変わる（`Ok(reason)` = pump 終了理由の人間可読文字列。呼び手はこれを `Down.last_error` の detail に使う）。外部（server 側）への影響なし。

- [ ] **Step 1: Write the failing test**

`subscription.rs` の `mod tests` に追加（既存 `spawn_manager` 足場を流用。FakeEstablisher は `subscription_id: 1` / `max_interval_s: 60` / `fail_kind: Timeout` を返す）:

```rust
/// manager 経路の統合: established（priming 到達後）→ down（op 相関死 +
/// 確立失敗の継続）→ 再 established をレジストリで追える。
#[tokio::test(start_paused = true)]
async fn status_nodes_tracks_established_down_reestablished() {
    use std::sync::atomic::Ordering;

    let est = FakeEstablisher::default();
    let fail_subscription = Arc::clone(&est.fail_subscription);
    let (mut rx, health, _dir, _handles) = spawn_manager(est, None);

    // priming 到達 = established（subscription_id / max_interval は fake の値）。
    let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
        .await
        .expect("first priming")
        .unwrap();
    assert!(ev.priming);
    let nodes = health.status_nodes();
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0]["node_id"], 5);
    assert_eq!(nodes[0]["state"], "established");
    assert_eq!(nodes[0]["subscription_id"], 1);
    assert_eq!(nodes[0]["max_interval_s"], 60);

    // 以後の確立を失敗させ続けてから op 相関で pump を殺す → down が観測できる。
    fail_subscription.store(1000, Ordering::SeqCst);
    health.note_op(5, 0x0006);
    let mut down = None;
    for _ in 0..300 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let nodes = health.status_nodes();
        if nodes[0]["state"] == "down" {
            down = Some(nodes[0].clone());
            break;
        }
    }
    let down = down.expect("status reaches down");
    // 最初の down は pump 終了理由（op 相関）が last_error に入る。以後の
    // 確立失敗で attempts が増え、last_error は establish 失敗へ置き換わる —
    // どちらを観測するかはタイミング次第なので形だけ釘打ちする。
    assert!(down["for_s"].is_u64());
    assert!(down["attempts"].is_u64());
    assert!(down["backoff_s"].as_u64().unwrap() >= 5);
    assert!(down["last_error"]["kind"].is_string());
    assert!(down["last_error"]["detail"].is_string());

    // 失敗注入を解除 → 再確立で established に戻る。
    fail_subscription.store(0, Ordering::SeqCst);
    let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
        .await
        .expect("re-priming after recovery")
        .unwrap();
    assert!(ev.priming);
    assert_eq!(health.status_nodes()[0]["state"], "established");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p matd status_nodes_tracks_established_down_reestablished`
Expected: FAIL（`status_nodes()` が空 — 遷移点が未配線）

- [ ] **Step 3: Wire the transitions**

`node_subscription_loop` を書き換え（`health.mark_establishing` を loop 前に、
`mark_down` を backoff 計算後に。ログ既存文言は不変）:

```rust
    let mut backoff = Duration::ZERO;
    // ダウン起点（起動 or 購読喪失）とその後の失敗ストリーク。established で
    // リセットされる（run_subscription_once が確立ログにダウン時間を載せる）。
    let mut down_since = tokio::time::Instant::now();
    let mut failures: u32 = 0;
    let mut warned = false;
    health.mark_establishing(node_id);
    loop {
        let last_error = match run_subscription_once(
            node_id, backend, &events, &clusters, &health, down_since, failures,
        )
        .await
        {
            Ok(reason) => {
                // 購読が成立して喪失した: 状態遷移なので info、状態リセット。
                tracing::info!(node_id, "subscription lost; resubscribing");
                backoff = Duration::ZERO;
                down_since = tokio::time::Instant::now();
                failures = 0;
                warned = false;
                MatError::new(mat_core::error::ErrorKind::Other, reason)
            }
            Err(e) => {
                failures += 1;
                match classify_failure(failures, down_since.elapsed(), warned) {
                    FailureLog::First => {
                        tracing::info!(
                            node_id,
                            kind = ?e.kind,
                            detail = %e.detail,
                            "subscription attempt failed; retrying with backoff"
                        );
                    }
                    FailureLog::StuckWarn => {
                        warned = true;
                        tracing::warn!(
                            node_id,
                            attempts = failures,
                            down_s = down_since.elapsed().as_secs(),
                            kind = ?e.kind,
                            detail = %e.detail,
                            "subscription still not established"
                        );
                    }
                    FailureLog::Quiet => {
                        tracing::debug!(node_id, kind = ?e.kind, detail = %e.detail, "subscription attempt failed");
                    }
                }
                e
            }
        };
        backoff = next_backoff(backoff);
        health.mark_down(node_id, down_since, failures, backoff, last_error);
        tokio::time::sleep(backoff).await;
    }
```

`run_subscription_once` の変更:

1. シグネチャ: `-> Result<(), MatError>` を `-> Result<String, MatError>` へ。
   doc コメントに「Ok(reason) = pump 終了理由（Down.last_error の detail に使う）」を追記。
2. `subscribe_wildcard` 成功の `tracing::info!("subscription established")` の直後に:
   ```rust
   health.mark_established(node_id, info.subscription_id, info.max_interval_s);
   ```
3. pump の `Ok(Some(msg))` 分岐の `health.clear_pending(node_id);` の隣に:
   ```rust
   health.note_device_msg(node_id);
   ```
4. `return Ok(())` を全 4 箇所で理由文字列に置換（既存ログ文言に対応）:
   - `PumpEnd::OpGrace { since_op }` の分岐末尾:
     `return Ok(format!("op-correlated: no device message {}s after op", since_op.as_secs()));`
   - `PumpEnd::BornDeadSilence`:
     `return Ok(format!("born-dead: no device message since establishment ({}s silent)", last_msg.elapsed().as_secs()));`
   - `PumpEnd::Silence`:
     `return Ok(format!("silence past deadline ({}s)", last_msg.elapsed().as_secs()));`
   - pump のセッションエラー分岐（`Err(e)` で "report pump ended" を出す所）:
     `return Ok(format!("pump ended: {}", e.detail));`

   （`match end` を「ログ + return」の 3 分岐に書き換える。ログは既存文言のまま。）

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p matd`
Expected: 全 PASS（既存の manager テスト群 — op_grace / live_report / backoff ラダー等 — が回帰の釘）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): 購読ループの遷移をレジストリへ配線 — established/down/理由付き"
```

---

### Task 4: server — `status` 応答の組み立てと dispatch 配線

**Files:**
- Modify: `crates/matd/src/server.rs`
- Modify: `crates/matd/src/main.rs`（serve 呼び出し側の最小追随のみ）
- Modify: `crates/matd/tests/integration.rs`

**Interfaces:**
- Consumes: Task 1 `Op::Status`、Task 2 `SubHealth::{clusters, status_nodes}`。
- Produces:
  - `pub struct DaemonInfo { pub version: &'static str, pub started: std::time::Instant, pub iface: String, pub fabric_index: u8 }`（server.rs、Task 5 の main.rs も使う）
  - `serve()` シグネチャ変更: 末尾に `daemon: std::sync::Arc<DaemonInfo>` を追加
  - `dispatch()` シグネチャ変更: `daemon: &DaemonInfo, events: &broadcast::Sender<Event>` を追加

- [ ] **Step 1: Write the failing tests**

(a) `server.rs` の `mod tests` に追加（`make_store` は既存ヘルパ）:

```rust
/// status は Unavailable でも応答し、構築エラーと空 nodes が見える。
/// subscribed_clusters は ids 名で返る。
#[tokio::test]
async fn dispatch_status_reports_native_unavailable() {
    let (_dir, store_path) = make_store();
    let state = NativeState::Unavailable(MatError::store_missing("no KVS materials"));
    let health = SubHealth::new(Some(vec![0x0006]));
    let daemon = DaemonInfo {
        version: "test",
        started: std::time::Instant::now(),
        iface: "lo".into(),
        fabric_index: 2,
    };
    let (events, rx) = tokio::sync::broadcast::channel::<crate::subscription::Event>(8);
    drop(rx);

    let (body, is_shutdown) = dispatch(
        r#"{"op":"status","id":3}"#,
        &state,
        &store_path,
        &health,
        &daemon,
        &events,
    )
    .await;

    assert!(!is_shutdown);
    assert_eq!(body["id"], 3);
    assert_eq!(body["native"]["kind"], "store_missing");
    assert_eq!(body["native"]["detail"], "no KVS materials");
    assert_eq!(body["version"], "test");
    assert_eq!(body["iface"], "lo");
    assert_eq!(body["fabric_index"], 2);
    assert_eq!(body["store"], store_path.display().to_string());
    assert_eq!(body["subscribed_clusters"], json!(["onoff"]));
    assert_eq!(body["listen_clients"], 0);
    assert!(body["nodes"].as_array().unwrap().is_empty());
    assert!(body["uptime_s"].is_u64());
    assert!(body["timestamp"].is_string());
}
```

（既存の dispatch 呼び出しテストがあれば新シグネチャに追随させる。無ければ
run_op テストはそのままで良い — run_op のシグネチャは不変。）

(b) `tests/integration.rs` に追加:

```rust
/// status op: ワイヤ越しのスキーマ骨格。listen クライアントが付くと
/// listen_clients に反映される。
#[tokio::test]
async fn status_op_returns_daemon_snapshot() {
    let (_dir, store_path) = make_store();
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    let (socket, _handle) =
        start_matd(store_path.clone(), NativeState::Ready(Box::new(native))).await;

    let resps = roundtrip(&socket, &[json!({"op":"status","id":9})]).await;
    let r = &resps[0];
    assert_eq!(r["id"], 9);
    assert_eq!(r["native"], "ready");
    assert_eq!(r["iface"], "lo");
    assert_eq!(r["fabric_index"], 1);
    assert_eq!(r["version"], "test");
    assert_eq!(r["listen_clients"], 0);
    assert_eq!(r["subscribed_clusters"], Value::Null);
    assert!(r["nodes"].as_array().unwrap().is_empty());
    assert_eq!(r["store"], store_path.display().to_string());
    assert!(r["uptime_s"].is_u64());
    assert!(r["timestamp"].is_string());

    // listen を 1 本張る（ack を読むまで）→ listen_clients = 1。
    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(b"{\"op\":\"listen\"}\n").await.unwrap();
    let mut lines = BufReader::new(read_half).lines();
    let ack: Value =
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ack["listening"], json!(true));

    let resps = roundtrip(&socket, &[json!({"op":"status"})]).await;
    assert_eq!(resps[0]["listen_clients"], 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p matd dispatch_status_reports` および `cargo test -p matd --test integration status_op_returns`
Expected: コンパイルエラー（`DaemonInfo` 未定義 / dispatch シグネチャ不一致）

- [ ] **Step 3: Write the implementation**

server.rs:

1. `NativeState` の近くに追加:

```rust
/// 起動時に確定するデーモン基本情報（status op が返す）。
pub struct DaemonInfo {
    pub version: &'static str,
    pub started: std::time::Instant,
    pub iface: String,
    pub fabric_index: u8,
}
```

2. `serve()` に `daemon: std::sync::Arc<DaemonInfo>` パラメータを追加し、
   accept ループで `Arc::clone(&daemon)` して `handle_conn` へ渡す。
   `handle_conn` も `daemon: Arc<DaemonInfo>` を受け、dispatch 呼び出しを
   `dispatch(&line, &native, &store_path, &health, &daemon, &events)` に変更
   （`events` は listen 分岐で使う既存の値をそのまま参照渡し）。

3. `dispatch()` シグネチャに `daemon: &DaemonInfo, events: &broadcast::Sender<Event>`
   を追加し、`run_op` 呼び出しを分岐に変える:

```rust
    // status はレジストリ snapshot の JSON 化のみ（デバイス・ワイヤに触れず
    // per-node Mutex も取らない）— run_op を通さず dispatch で完結する。
    let result = match &req.op {
        Op::Status => Ok(status_body(native, store_path, daemon, health, events)),
        _ => run_op(&req.op, native, store_path, health, deadline).await,
    };
```

4. `run_op` の先頭 match（Ping/Shutdown/Listen の隣）に防御 arm を追加:

```rust
        // status は dispatch が先取りする（防御的に拒否）。
        Op::Status => {
            return Err(MatError::parse_error("status is handled in dispatch"))
        }
```

5. `error_response` の近くに追加:

```rust
/// `status` op の応答ボディ（timestamp / id は dispatch が付ける）。
fn status_body(
    native: &NativeState,
    store_path: &Path,
    daemon: &DaemonInfo,
    health: &SubHealth,
    events: &broadcast::Sender<Event>,
) -> Value {
    let native_json = match native {
        NativeState::Ready(_) => json!("ready"),
        NativeState::Unavailable(e) => json!({ "kind": e.kind, "detail": e.detail }),
    };
    // subscriptions.toml 由来の絞り込み。ids に無いクラスタは数値のまま
    // （listen イベントの Event::to_json と同じ規律）。無し = wildcard = null。
    let clusters = health.clusters().map(|ids| {
        ids.iter()
            .map(|&id| match mat_core::ids::find_cluster(id) {
                Some(def) => json!(def.name),
                None => json!(id),
            })
            .collect::<Vec<_>>()
    });
    json!({
        "version": daemon.version,
        "uptime_s": daemon.started.elapsed().as_secs(),
        "native": native_json,
        "iface": daemon.iface,
        "fabric_index": daemon.fabric_index,
        "store": store_path.display().to_string(),
        "subscribed_clusters": clusters,
        "listen_clients": events.receiver_count(),
        "nodes": health.status_nodes(),
    })
}
```

main.rs（serve_daemon 内、最小追随）:

1. broadcast の初期 receiver を即 drop する（status の `listen_clients` を
   1 過大にしない — 受信者ゼロの send エラーは購読側で正常扱い済み）:

```rust
    let (events_tx, initial_rx) = tokio::sync::broadcast::channel(1024);
    drop(initial_rx);
```

2. serve 呼び出しの直前に DaemonInfo を組んで渡す:

```rust
    let daemon = std::sync::Arc::new(server::DaemonInfo {
        version: env!("CARGO_PKG_VERSION"),
        started: std::time::Instant::now(),
        iface: iface.clone(),
        fabric_index: cli.fabric_index,
    });
    server::serve(&socket, store_path, native, events_tx, sub_health, daemon)
```

（`started` は serve 直前で十分 — native 構築の数百 ms は uptime の用途に影響しない。）

tests/integration.rs の `start_matd_with_events` を追随:

```rust
    let daemon = std::sync::Arc::new(matd::server::DaemonInfo {
        version: "test",
        started: std::time::Instant::now(),
        iface: "lo".into(),
        fabric_index: 1,
    });
    let handle = tokio::spawn(async move {
        let _ = matd::server::serve(&socket_clone, store_path, native, tx2, health, daemon).await;
    });
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p matd`
Expected: 全 PASS（dispatch/integration の新テスト + 既存全部）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/server.rs crates/matd/src/main.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): status op — 購読レジストリ+デーモン情報の snapshot 応答"
```

---

### Task 5: `matd status` サブコマンド（send_admin_op 一般化）

**Files:**
- Modify: `crates/matd/src/main.rs`
- Test: `crates/matd/tests/cli.rs`

**Interfaces:**
- Consumes: Task 4 までの status op（socket 側）。
- Produces: `matd status` CLI。`send_shutdown` は `send_admin_op(socket: &Path, op: &str) -> Result<Value, MatError>` へ一般化（stop / status 共用）。

- [ ] **Step 1: Write the failing test**

`tests/cli.rs` に追加（既存 `stop_without_running_daemon_errors` と同型）:

```rust
/// status 先の matd が居なければ「not running」エラーで exit 1。
#[test]
fn status_without_running_daemon_errors() {
    let sock = std::env::temp_dir().join(format!("matd-cli-nostatus-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);

    Command::cargo_bin("matd")
        .unwrap()
        .args(["status", "--socket", sock.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not running"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p matd --test cli status_without_running`
Expected: FAIL（`status` は未知のサブコマンド — clap がエラーを出すが
"not running" は stderr に出ない）

- [ ] **Step 3: Write the implementation**

main.rs:

1. `Command` に variant 追加:

```rust
    /// 稼働中 matd の購読とデーモンの現況を JSON で返す（socket 経由）。
    Status,
```

2. `run()` の match:

```rust
    match cli.command {
        Some(Command::Stop) => admin_op(cli.socket, "shutdown").await,
        Some(Command::Status) => admin_op(cli.socket, "status").await,
        None => serve_daemon(cli).await,
    }
```

3. `stop()` を一般化（fn 名変更、`send_shutdown` → `send_admin_op`）:

```rust
/// stop / status: 稼働中 matd の socket へ admin op を 1 行送り、応答 JSON を
/// stdout へ出す。居なければ「not running」で exit 1。
async fn admin_op(socket: Option<PathBuf>, op: &str) -> Result<(), MatError> {
    let socket = socket.unwrap_or_else(mat_core::socket::default_socket_path);
    let resp = send_admin_op(&socket, op).await?;
    // 成功応答は stdout（純粋 JSON）。
    println!("{resp}");
    Ok(())
}

/// socket に `{"op":"<op>"}` を送り応答 1 行を読む。接続不能は「not running」
/// （応答なし拒否 = stale socket はどの op でも掃除してよい）。
async fn send_admin_op(socket: &Path, op: &str) -> Result<Value, MatError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let stream = UnixStream::connect(socket).await.map_err(|e| {
        // 応答なしで拒否 = stale socket が残っているだけのことがある。掃除する。
        if e.kind() == std::io::ErrorKind::ConnectionRefused {
            let _ = std::fs::remove_file(socket);
        }
        MatError::new(
            ErrorKind::Other,
            format!("matd not running at {} ({e})", socket.display()),
        )
    })?;

    let (read_half, mut write_half) = stream.into_split();
    let mut line = serde_json::to_vec(&serde_json::json!({ "op": op })).unwrap();
    line.push(b'\n');
    write_half
        .write_all(&line)
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to send {op}: {e}")))?;

    let mut lines = BufReader::new(read_half).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to read response: {e}")))?
        .ok_or_else(|| {
            MatError::new(
                ErrorKind::Other,
                "matd closed the connection without responding".to_string(),
            )
        })?;
    serde_json::from_str(&line)
        .map_err(|e| MatError::parse_error(format!("matd response was not JSON: {e}; body={line}")))
}
```

（`serde_json::json` の import が main.rs に無ければ `serde_json::json!` の
フルパスで書くか use を足す。既存 `stop` / `send_shutdown` は削除。）

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p matd --test cli` および `cargo test -p matd`
Expected: 全 PASS（stop の既存 CLI テストが send_admin_op 化の回帰の釘）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/main.rs crates/matd/tests/cli.rs
git commit -m "feat(matd): status サブコマンド — stop と共用の admin op 送信へ一般化"
```

---

### Task 6: ドキュメント + バージョン 1.12.0

**Files:**
- Modify: `README.md`（`matd stop` 節の直後、815 行付近）
- Modify: `ARCHITECTURE.md`（実装記録）
- Modify: `Cargo.toml`（workspace version 1.11.0 → 1.12.0）

**Interfaces:** なし（ドキュメントのみ）。

- [ ] **Step 1: README に status 節を追加**

`matd stop` のコードブロック（`matd stop --socket ...` の閉じ ``` の後、
「Only one matd runs per socket」段落の前）に挿入:

````markdown
Ask the running daemon what it is doing with `matd status` — one JSON line on
stdout with daemon basics and the per-node state of the resident subscriptions
(the same lifecycle the logs narrate: `establishing` → `established` →
`down` with backoff and the last error). Durations are all "seconds ago"
fields; `subscribed_clusters` mirrors `subscriptions.toml` (`null` = full
wildcard); `pending_op_ago_s` is non-null only while a state-changing op has
gone unanswered by the device (the op-correlation window):

```bash
matd status                           # default socket
matd status --socket /run/mat/matd.sock
```

```json
{
  "timestamp": "2026-06-03T12:34:56+09:00",
  "version": "1.12.0",
  "uptime_s": 86400,
  "native": "ready",
  "iface": "wpan0",
  "fabric_index": 1,
  "store": "/home/user/.config/mat",
  "subscribed_clusters": ["onoff", "occupancysensing"],
  "listen_clients": 1,
  "nodes": [
    {"node_id": 5, "state": "established", "for_s": 3600,
     "subscription_id": 7, "max_interval_s": 300,
     "last_device_msg_ago_s": 42, "pending_op_ago_s": null},
    {"node_id": 6, "state": "down", "for_s": 120, "attempts": 14,
     "backoff_s": 60, "last_error": {"kind": "unreachable", "detail": "..."}}
  ]
}
```

If the native backend failed to build at startup, `native` carries that error
(`{"kind": "store_missing", ...}`) and `nodes` is empty. If no daemon answers
the socket, `matd status` exits `1` with `matd not running at ...` (same
contract as `matd stop`).
````

- [ ] **Step 2: ARCHITECTURE.md に記録を追記**

既存の実装記録群（1.11.0 の記録の後）に、直近の記録と同じ体裁で追記。
含める内容: 監査 Tier 2 ⑥への帰結として `matd status` を追加（socket admin op
`status` + `SubHealth` の購読ライフサイクルレジストリ）。運用の `ss -uanp`
間接推定を置き換える観測口であること。spec / plan のパス。

- [ ] **Step 3: バージョン bump**

`Cargo.toml`（workspace root）の `version = "1.11.0"` を `"1.12.0"` へ。
`cargo build -p matd` で Cargo.lock を追随させる。

- [ ] **Step 4: Full check**

Run: `task check`
Expected: fmt / clippy (-D warnings) / test 全 PASS

- [ ] **Step 5: Commit**

```bash
git add README.md ARCHITECTURE.md Cargo.toml Cargo.lock
git commit -m "chore: 1.12.0（matd status — 監査 Tier 2 ⑥）+ README/ARCHITECTURE"
```

---

### Task 7: 実機 E2E（マージ前必須 — メインセッションで実施）

**Files:** なし（検証のみ。実施はサブエージェントではなくメインセッション）

マージ前に jarvis で実機 E2E（ユーザー方針: mat の変更は main マージ前に実機 E2E 必須）。隔離 matd 方式（別 socket + store コピー。手順詳細は運用メモ jarvis-matd-deploy）で:

- [ ] **Step 1: ビルドと転送** — `task dist:arm64` → `dist/arm64/{mat,matd}` を jarvis へ `*.new` として scp（本番バイナリは未置換のまま）。
- [ ] **Step 2: 骨格確認** — 台帳 1 ノードの隔離 store で隔離 matd を起動し、`./matd.new status --socket <隔離socket>` が `native: "ready"` / `subscribed_clusters` / `nodes` を返すこと。`ss -uanp` のソケット台帳（購読×N）と `nodes` の `established` 集合が整合すること。
- [ ] **Step 3: down 遷移の観測** — 対象ノードを沈黙させる（op 相関 or 無音 deadline。誘発手順は運用メモ）→ status が `down`（attempts / backoff_s / last_error）を経て再確立で `established` に戻ることを確認。
- [ ] **Step 4: 不在時の応答** — 隔離 matd を止めて `./matd.new status --socket <隔離socket>` が exit 1 / `not running` を返すこと。
- [ ] **Step 5: 合格を記録して finishing-a-development-branch へ**（マージ後: jarvis 本番デプロイ + 運用メモの「生存確認は `ss -uanp`」を status 併記へ更新）。
