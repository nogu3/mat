# node-touched ヒント Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 直経路 op / matd cold-establish がセッションを新設したら該当ノードの購読を即時張り直し、FP300 の「最新セッション再アンカー」による購読 silent 死の盲目窓を 5.5 分 → 約10秒に縮める（Issue #20 実効修正、spec = `docs/superpowers/specs/2026-07-31-node-touched-hint-design.md`）。

**Architecture:** シグナルは既存の `SubHealth`（server↔購読タスク間の唯一の共有シグナル路、`note_op`/`pending_elapsed` と同型）を拡張: `note_touched` はフラグ+`Notify`。pump は 5s スライス毎の `pump_verdict` でフラグを拾って `PumpEnd::Touched` で終了（pump は cancel-unsafe なので割り込まずポーリング）、supervisor ループは Touched 理由ならバックオフ無しで即再確立し、バックオフ睡眠中は `Notify` で叩き起こす。外部トリガ = 新ソケット op `node_touched`、内部トリガ = `NativeBackend` の `on_new_session` コールバック（main.rs で注入）。mat 側は close 済み直後に fire-and-forget でヒント送信。

**Tech Stack:** Rust (tokio, serde)。ブランチ feat/node-touched-hint。

## Global Constraints

- ヒントは best-effort: mat 側は接続失敗・タイムアウト・エラー応答（旧 matd の `parse_error` 含む）を全て `tracing::debug!` で握りつぶし、exit code / stdout JSON に影響させない
- op タグは snake_case 慣例に従い **`node_touched`**（spec 中の表記 `node-touched` は wire 上 `node_ted...` ではなく `node_touched` に読み替え）
- `Op::node_id()` は NodeTouched で **`None`** を返す（`abort_op` の `drop_session` 誤爆と deadline 付与を避ける — server.rs:241 参照）。variant 自身のフィールドはハンドラが直接読む
- pump は cancel-unsafe（subscription.rs:36-39）— `next_report` を select で中断しない。検知は 5s スライスのポーリングで良い（スペックの「約10秒」に収まる）
- コメントは日本語・「なぜ」だけ・既存流儀
- 各タスク: `cargo test -p <crate>` green、最後に `task check`
- コミット末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` と `Claude-Session: https://claude.ai/code/session_01GkZMfhCpteXdQMueRBBHn2`

---

### Task 1: SubHealth touched シグナル + pump/supervisor の即時再購読

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`SubHealth` :117-163、`PumpEnd` :54-61、`pump_verdict` :65-84、`node_subscription_loop` :563-628、`run_subscription_once` :633-749、tests）

**Interfaces:**
- Produces: `SubHealth::note_touched(&self, node_id: u64)`（フラグ set + 該当ノードの Notify 起床。購読が無いノードでも安全な no-op — フラグは pump 不在なら誰も読まないだけ）
- Produces: `SubHealth::touched(&self, node_id: u64) -> bool` / `clear_touched(&self, node_id: u64)`
- Produces: `SubHealth::touch_notify(&self, node_id: u64) -> Arc<tokio::sync::Notify>`（ノード毎 lazy 生成）
- Produces: `PumpEnd::Touched` variant

- [ ] **Step 1: 失敗するテストを書く**

既存テスト `op_grace_triggers_fast_resubscribe`（:1062）と `establish_failures_climb_backoff_then_recover`（:1435）の組み立てを流用して3本:

```rust
/// note_touched で pump がスライス内に終了し、バックオフ無しで再確立する（Issue #20）。
#[tokio::test(start_paused = true)]
async fn touched_ends_pump_and_resubscribes_without_backoff() {
    // 確立済み pump に health.note_touched(node) →
    // PUMP_SLICE(5s) 以内に「subscription lost」相当で終了し、
    // sleep(5s バックオフ) を挟まず即 re-establish されること
    // （FakeEstablisher の establish 回数と時刻で検証）。
}

/// バックオフ睡眠中の note_touched が sleep を打ち切って即再試行する。
#[tokio::test(start_paused = true)]
async fn touched_wakes_backoff_sleep() {
    // establish を数回失敗させてバックオフを伸ばした状態で note_touched →
    // 残り待ち時間を消化せず直ちに次の establish が走ること。
}

/// touched フラグは消費後クリアされ、次周回で再発火しない。
#[tokio::test(start_paused = true)]
async fn touched_flag_is_consumed_once() {
    // 1回の note_touched → 再確立1回のみ。その後の pump が
    // 即 Touched 終了しないこと（PUMP_SLICE×2 生存を確認）。
}
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p matd touched_ -- --nocapture`
Expected: FAIL（`note_touched` 未定義）

- [ ] **Step 3: 実装**

`SubHealth` の per-node state に `touched: bool` と `notify: Arc<Notify>` を追加
（既存 `pending` と同じ Mutex<HashMap> 内。`note_op` :141-149 が手本）:

```rust
/// 直経路 op / cold establish がこのノードのセッションを新設した合図。
/// FP300 系はレポートを最新セッションへ付け替えるため、購読を即時
/// 張り直して「最新」を購読セッションに塗り替える（Issue #20、spec
/// 2026-07-31-node-touched-hint）。pump は cancel-unsafe なので
/// フラグ+スライスポーリング、バックオフ睡眠だけ Notify で起こす。
pub(crate) fn note_touched(&self, node_id: u64) { /* flag+notify */ }
```

`pump_verdict` に `touched: bool` 引数を追加し、最優先で
`Some(PumpEnd::Touched)`。`run_subscription_once` のループ頭で
`health.touched(node_id)` を渡し、`PumpEnd::Touched` 腕は
`health.clear_touched(node_id)` してから
`break "touched: direct-path session superseded".to_string()`
（tracing INFO 付き、既存腕と同型。末尾の `conn.close().await` は共通路で実行される）。

`node_subscription_loop` :624-626 の後始末:

```rust
// Touched は「セッションが塗り替えられた」ことが確定している喪失 —
// バックオフで待つ理由がない（Issue #20）。
if reason_is_touched { backoff = Duration::ZERO; continue; }
backoff = next_backoff(backoff);
health.mark_down(...);
tokio::select! {
    _ = tokio::time::sleep(backoff) => {}
    _ = health.touch_notify(node_id).notified() => {
        // 睡眠中に新セッションが立った — 待ち続けるとその間ずっと盲目。
        backoff = Duration::ZERO;
    }
}
```

（`run_subscription_once` が `Ok(reason)` の reason 文字列で Touched を運ぶか、
戻り値を enum 化するかは既存コードの摩擦が小さい方を選ぶ。文字列 prefix 判定で
足りるならそれで良い。）

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p matd`
Expected: all PASS（既存の backoff / op-grace テスト無傷）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): SubHealth touched シグナル — 購読の即時張り直し (Issue #20)"
```

---

### Task 2: ソケット op `node_touched`

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`Op` :27-186、`node_id()` :193、`name()` :220、`group_id()` :245、`endpoint()` :271、`log_path()` :299）
- Modify: `crates/matd/src/server.rs`（`dispatch` :534 付近に Status 同様の短絡腕、`op_state_target` :738 に None 腕）
- Test: `crates/matd/tests/integration.rs`（`roundtrip()` :41 / `start_matd_with_events` :70 流用）

**Interfaces:**
- Consumes: Task 1 の `SubHealth::note_touched`
- Produces: wire op `{"op":"node_touched","node_id":N}` → 応答 `{"resubscribing":true,"timestamp":...}`（+ `id` echo）。native 状態に依存しない（Status 同様 dispatch で処理）

- [ ] **Step 1: 失敗するテストを書く**

```rust
/// node_touched は即 ack し、SubHealth に touched が立つ。
#[tokio::test]
async fn node_touched_acks_and_flags_health() {
    // start_matd_with_events で起動 → {"op":"node_touched","node_id":16} 送信
    // → 応答に resubscribing:true と timestamp。health.touched(16) == true。
}
```

（`start_matd_with_events` が返す health ハンドルの有無は現物に合わせる —
無ければ `SubHealth` を組み立てて `serve` に渡す構成を流用。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p matd --test integration node_touched`
Expected: FAIL（unknown variant）

- [ ] **Step 3: 実装**

- `Op::NodeTouched { node_id: u64 }` を追加（`rename_all = "snake_case"` により
  wire タグは `node_touched`）。
- アクセサ群: `node_id()` は **None**（abort/deadline 対象外 — Global Constraints
  参照）、`name()` = `"node_touched"`、`group_id()`/`endpoint()`/`log_path()` = None、
  `op_state_target()` = None。
- `dispatch` の Status 短絡（server.rs:534-537）の直後に:

```rust
Op::NodeTouched { node_id } => {
    // native 不要・per-node Mutex 不要 — SubHealth に合図して即 ack。
    // 再購読完了は待たない（ヒントは fire-and-forget が契約）。
    health.note_touched(node_id);
    tracing::info!(node_id, source = "external", "node touched; resubscribing");
    Ok(json!({ "resubscribing": true }))
}
```

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p matd`
Expected: all PASS

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/protocol.rs crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): node_touched op — 外部からの購読即時張り直しトリガ (Issue #20)"
```

---

### Task 3: 内部トリガ — cold establish 後に自己ヒント

**Files:**
- Modify: `crates/matd/src/native.rs`（`NativeBackend` :82-85、`with_session` :189-292）
- Modify: `crates/matd/src/main.rs`（`serve_daemon` :186-246 — コールバック注入）
- Test: `crates/matd/src/native.rs` tests

**Interfaces:**
- Consumes: Task 1 の `SubHealth::note_touched`
- Produces: `NativeBackend::set_on_new_session(cb: Box<dyn Fn(u64) + Send + Sync>)`
  （`OnceLock` フィールド。native.rs は subscription を知らないまま — 汎用コールバック）

- [ ] **Step 1: 失敗するテストを書く**

```rust
/// cold establish で op を完了するとコールバックが1回発火する。
#[tokio::test]
async fn cold_establish_fires_on_new_session() { /* 記録用 Arc<AtomicUsize> */ }

/// warm 再利用ではコールバックは発火しない。
#[tokio::test]
async fn warm_reuse_does_not_fire() { /* cold 1回 → 2回目の op → 計1回のまま */ }

/// Timeout 後の resend-establish でも発火する。
#[tokio::test]
async fn resend_establish_fires_on_new_session() { /* fail_first_send: Timeout */ }
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p matd fires_on_new_session`
Expected: FAIL

- [ ] **Step 3: 実装**

`NativeBackend` に `on_new_session: std::sync::OnceLock<Box<dyn Fn(u64) + Send + Sync>>`。
`with_session` は cold establish（:205-219）/ resend-establish（:250-258）で
`let mut established_new = false;` を立て、**関数を抜ける直前**（op の成否確定後）に:

```rust
// セッションを新設した = デバイスの「最新セッション」が op 用に変わった。
// FP300 はここへ購読レポートを付け替えるので、購読側へ即時張り直しを
// 合図する（Issue #20 経路2 — 2026-07-30 17:09 hold-time write 事故の再発防止）。
if established_new {
    if let Some(cb) = self.on_new_session.get() { cb(node_id); }
}
```

`main.rs` は `spawn_subscription_manager` の後、`server::serve` の前に
`backend.set_on_new_session(Box::new(move |n| sub_health.note_touched(n)))`
（`Arc<SubHealth>` clone をキャプチャ。NativeState::Ready 構築順は現物に合わせて
`OnceLock` 注入のタイミングだけ保証する）。

注意: warm op セッションは生き続けるため、張り直した購読セッションより新しい
まま残るケースがある（op 直後にさらにレポートが warm op ソケットへ向く可能性）。
それでも matd の warm ソケットは MRP ack を返す生きたソケットなので黒穴には
ならず、次の張り直しで購読が最新化される。この既知の限界は spec の非目標
（セッション統合）に属する — コメントで触れるだけで良い。

- [ ] **Step 4: 通ることを確認 + クレート回帰**

Run: `cargo test -p matd`
Expected: all PASS

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/native.rs crates/matd/src/main.rs
git commit -m "feat(matd): cold establish 後の内部 touched トリガ (Issue #20)"
```

---

### Task 4: mat 側 — 直経路 op 後の fire-and-forget ヒント

**Files:**
- Modify: `crates/mat/src/matd_client.rs`（`connect_candidates` :433 / `exchange_on_stream` :452 の隣に新ヘルパ）
- Modify: `crates/mat/src/native_direct.rs`（1.14.0 の `conn.close().await` 全16サイトを `finish_conn` 集約に置換）
- Test: 両ファイルの tests

**Interfaces:**
- Consumes: Task 2 の wire op `node_touched`
- Produces: `matd_client::hint_node_touched(node_id: u64)`（同期・infallible。
  `default_socket_candidates` → 接続 → `{"op":"node_touched","node_id":N}` 1行送信 →
  読み取りタイムアウト 300ms で応答1行読み捨て。全失敗は `tracing::debug!` のみ。
  **`attach_deadline` / `emit_response` を通さない専用送信路**）
- Produces: `native_direct::finish_conn(conn: &mut Box<dyn NodeConn>, node_id: u64)`（async: `conn.close().await` → `hint_node_touched(node_id)`）

- [ ] **Step 1: 失敗するテストを書く**

```rust
/// finish_conn は close と hint を両方行う（op 成否によらず呼ばれるのは
/// 1.14.0 の既存テストが担保済み — ここでは hint 送信の有無を見る）。
#[tokio::test]
async fn finish_conn_sends_hint_when_matd_socket_present() {
    // テンポラリ dir に fake unix socket サーバ（1行読んで ack を返す）を立て、
    // MAT_MATD_SOCKET（既存の env 上書き機構）をそこへ向けて finish_conn →
    // サーバが {"op":"node_touched","node_id":N} を受信したことを assert。
}

#[tokio::test]
async fn finish_conn_is_silent_without_matd() {
    // socket 不在でも panic せず、op 結果に影響しない（戻り値なしで完走）。
}
```

（`MAT_MATD_SOCKET` 相当の env 名・fake ソケットの立て方は
`sockets_from_env_or_default` :91 と既存 matd_client テストの現物に合わせる。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat finish_conn`
Expected: FAIL

- [ ] **Step 3: 実装**

`hint_node_touched`: `connect_candidates()` 成功時のみ送信。ブロッキング I/O だが
300ms 上限・one-shot CLI の終了間際なので許容（コメントに明記）。旧 matd の
`parse_error` 応答も読み捨て（Global Constraints）。

`native_direct.rs`: 16サイトの `conn.close().await;` を
`finish_conn(&mut conn, node_id).await;` に置換（`diag mesh` はノード毎に発火）。
establish 失敗時は conn が無いので従来通り何もしない。

- [ ] **Step 4: 通ることを確認 + 回帰**

Run: `cargo test -p mat && cargo test -p matd`
Expected: all PASS（1.14.0 の close テストは finish_conn 経由でも通ること）

- [ ] **Step 5: コミット**

```bash
git add crates/mat/src/matd_client.rs crates/mat/src/native_direct.rs
git commit -m "feat(mat): 直経路 op 後に matd へ node_touched ヒント (Issue #20)"
```

---

### Task 5: バージョン 1.15.0 + ドキュメント + task check

**Files:**
- Modify: `Cargo.toml` / `Cargo.lock`（1.14.0 → 1.15.0）
- Modify: `ARCHITECTURE.md`（1.14.0 段落の直後に 1.15.0 段落: ヒント方式の理由 =
  CloseSession の実機 E2E 不合格の続き、経路1/2、盲目窓 5.5分→約10秒）
- Modify: `README.md`（matd ソケットプロトコルの op 一覧に `node_touched` を追記 —
  一覧が存在する場合のみ。「diag は matd を通らない」記述は不変）

- [ ] **Step 1: バージョン + ARCHITECTURE + README**
- [ ] **Step 2: `task check` 完全 green**
- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock ARCHITECTURE.md README.md
git commit -m "chore: 1.15.0（node_touched ヒント — 購読即時張り直し、Issue #20）"
```

---

### Task 6: 実機 E2E（マージ前必須 — main セッションが実施）

- [ ] **Step 1:** `task dist:arm64` → `scp dist/arm64/{mat,matd} jarvis:~/{mat,matd}.new-hint`
- [ ] **Step 2:** 隔離ではなく本番 matd を 1.15.0 に更新（backup → install → restart。
  ヒントは matd 側実装が必須のため本番更新が前提。失敗時は `.bak-1.14.0` へ戻す）
- [ ] **Step 3:** 経路1: node16 購読 established を確認 →
  `MAT_FABRIC_INDEX=2 mat diag thread --node 16` → journal で
  数秒以内に `node touched; resubscribing` → `subscription established` を確認。
  330s deadline の `subscription lost` が**出ない**こと
- [ ] **Step 4:** 経路2: node16 宛 matd 経由 op（warm が無い状態を作って read）→
  cold establish 後に内部トリガの再購読が走ること
- [ ] **Step 5:** 15分放置して定常劣化なし（他ノードの churn が増えていない）を確認
- [ ] **Step 6:** 合格 → main マージ（--no-ff）・push・Issue #20 に E2E 結果コメント。
  盲目窓の実測値（diag 完了→established の秒数）を記録
