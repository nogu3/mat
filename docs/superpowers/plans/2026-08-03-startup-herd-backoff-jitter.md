# 起動 herd stagger + backoff/MRP jitter 実装計画（監査 Tier 2 ⑧、1.18.0）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** デプロイ再起動のたびに起きる CASE no-ack バースト（1〜2 分）と、メッシュ全域イベント後の同期リトライ波を、3 点の jitter / stagger で潰す。

**Architecture:** 設計変更なし — 既存ループの「待ち時間の算出」だけを変える。(1) matd supervisor の初回バッチ spawn に index × 1s の決定論的 stagger、(2) matd 再購読 backoff の実 sleep に cap 後 ×[0.75, 1.25) jitter、(3) mat-controller の MRP 再送ループ 3 箇所に spec 4.12.2.1 準拠の ×(1 + 0.25·r) jitter（`MrpConfig::jitter` フィールド、既定 0.25、テストは 0.0 で決定論維持）。`total_budget` はジッタ最悪値込みに更新して Issue #16 の op 予算と整合させる。

**Tech Stack:** Rust / tokio / getrandom（mat-controller に既存依存）。乱数はヘルパー `unit_random()` に集約し、適用は乱数値を引数に取る純関数（既存の `next_backoff` / `pump_verdict` と同じテスト規律）。

**Spec:** `docs/superpowers/specs/2026-08-03-startup-herd-backoff-jitter-design.md`

## Global Constraints

- ブランチ `fix/tier2-herd-jitter` で作業（main 直コミット禁止）。
- 対象バージョン: 1.18.0（Task 6 で bump。それまでは触らない）。
- stdout 純 JSON / stderr tracing の規約は本変更では触らない。
- `unit_random()` は panic させない（getrandom 失敗は 0.5 退避）。暗号用途に使わない。
- 名目 backoff エンベロープ（`next_backoff` 5→10→…→60s）と `mark_down` / `matd status` の表示は名目値のまま — jitter は sleep 値にだけ掛かる。
- 既存テストの決定論を壊さない: テスト用 `MrpConfig` は全箇所 `jitter: 0.0`。
- 各タスク末尾で `cargo fmt` を掛けてからコミット（CI は fmt:check）。
- 実機 E2E（マージ前必須）は本計画の外 — 全タスク完了後に jarvis で実施してから main へマージする。

---

### Task 1: `MrpConfig::jitter` フィールド + `unit_random` + `jittered_interval`（mat-controller）

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs`（MrpConfig 定義 :15-43、tests mod :342-）
- Modify（コンパイル追随のみ）: `crates/mat-controller/src/test_support.rs:63`、`crates/mat-controller/src/pase.rs:518`、`crates/mat-controller/src/session.rs:1234` / `:1302` / `:1699`、`crates/mat-controller/src/commissioning.rs:926` / `:1016`、`crates/mat-controller/tests/btp_pase_plumbing.rs:94`
- Test: `crates/mat-controller/src/exchange.rs` の `mod tests`

**Interfaces:**
- Produces: `pub const MRP_BACKOFF_JITTER: f64 = 0.25`、`MrpConfig` の新フィールド `pub jitter: f64`、`pub fn unit_random() -> f64`（[0,1)）、`pub fn jittered_interval(interval: Duration, jitter: f64, r: f64) -> Duration`。Task 2〜4 がすべてこれを使う。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/exchange.rs` の `mod tests` 末尾に追加:

```rust
    /// jitter 純関数: r=0 で恒等、jitter=0 で恒等、上限は ×(1+jitter) 未満。
    #[test]
    fn jittered_interval_bounds() {
        let base = Duration::from_millis(300);
        assert_eq!(jittered_interval(base, 0.25, 0.0), base);
        assert_eq!(jittered_interval(base, 0.0, 0.9), base);
        let hi = jittered_interval(base, 0.25, 0.999_999);
        assert!(hi > base && hi < base.mul_f64(1.25));
    }

    /// unit_random: [0,1) に収まり、壊れて定数化していない（16 連続一致は
    /// 実装破損以外で起きない）。
    #[test]
    fn unit_random_in_range_and_varies() {
        let draws: Vec<f64> = (0..16).map(|_| unit_random()).collect();
        assert!(draws.iter().all(|r| (0.0..1.0).contains(r)));
        assert!(draws.iter().any(|r| *r != draws[0]), "16 draws all identical");
    }
```

- [ ] **Step 2: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test -p mat-controller --lib exchange::tests -- jittered unit_random`
Expected: FAIL（`jittered_interval` / `unit_random` 未定義のコンパイルエラー）

- [ ] **Step 3: 実装**

`crates/mat-controller/src/exchange.rs` — `MrpConfig` 定義（:17-28）へフィールド追加:

```rust
    pub max_retries: u32,
    pub backoff: f64,
    /// 各再送待ちに乗せるジッタ係数の上限（spec 4.12.2.1 MRP_BACKOFF_JITTER）。
    /// 実待ち = interval × (1 + jitter · r)、r ∈ [0,1)。0.0 = ジッタ無し
    /// （テストの決定論用）。
    pub jitter: f64,
}
```

`Default` impl（:30-39）に `jitter: MRP_BACKOFF_JITTER,` を追加し、定数と関数を `MrpConfig` 定義の直前に置く:

```rust
/// spec 4.12.2.1 MRP_BACKOFF_JITTER: 再送待ちジッタ係数の既定上限。
pub const MRP_BACKOFF_JITTER: f64 = 0.25;

/// [0,1) の一様乱数。getrandom 失敗時は 0.5 へ退避 — jitter は品質であって
/// 正しさではないので、ここでは panic させない（暗号用途には使わないこと）。
pub fn unit_random() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::getrandom(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

/// 1 回の再送待ちへジッタを乗せる（純関数 — r は `unit_random()` の値）。
pub fn jittered_interval(interval: Duration, jitter: f64, r: f64) -> Duration {
    interval.mul_f64(1.0 + jitter * r)
}
```

- [ ] **Step 4: 全構築箇所のコンパイル追随**

`MrpConfig` をフィールド全列挙で構築している箇所に 1 行足す。**テストはすべて `jitter: 0.0`**（決定論維持）、**プロダクションは `jitter: MRP_BACKOFF_JITTER`**:

| 箇所 | 追加する行 |
|---|---|
| `exchange.rs:354` `fast_cfg`（test） | `jitter: 0.0,` |
| `exchange.rs:437` inline cfg（test） | `jitter: 0.0,` |
| `session.rs:1234` `fast_cfg`（test） | `jitter: 0.0,` |
| `session.rs:1302` inline cfg（test） | `jitter: 0.0,` |
| `session.rs:1699` inline cfg（test） | `jitter: 0.0,` |
| `pase.rs:518` `fast_cfg`（test） | `jitter: 0.0,` |
| `test_support.rs:63` `pub fn fast_cfg`（テスト足場） | `jitter: 0.0,` |
| `tests/btp_pase_plumbing.rs:94` inline cfg（test） | `jitter: 0.0,` |
| `commissioning.rs:926`（BLE PASE 予算） | `jitter: MRP_BACKOFF_JITTER,` |
| `commissioning.rs:1016` `connect_cfg` | `jitter: MRP_BACKOFF_JITTER,` |

`dnssd.rs:189` の `mrp_config()` は `..MrpConfig::default()` なので追随不要（既定 0.25 が乗る）。`exchange.rs:413` の inline cfg も `..MrpConfig::default()` で追随不要。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-controller`
Expected: PASS（新規 2 件含む全件）

- [ ] **Step 6: fmt + コミット**

```bash
cargo fmt
git add -A crates/mat-controller
git commit -m "feat(mat-controller): MrpConfig::jitter + unit_random/jittered_interval（監査⑧の土台）"
```

---

### Task 2: `total_budget` をジッタ最悪値込みへ + session.rs の重複統合

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs:54-63`（`total_budget`）
- Modify: `crates/mat-controller/src/session.rs:26-47`（`worst_case_send_budget` コメント、local `total_budget` 削除）、`:456`（呼び替え）、`:3258-3263`（既存 assert 更新）
- Modify: `crates/matd/src/native.rs:36-38`（概算値コメント）
- Test: `crates/mat-controller/src/exchange.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 1 の `MrpConfig::jitter`。
- Produces: `exchange::total_budget(cfg)` がジッタ最悪値（×(1+jitter)）込みの上界を返す。`session.rs` に local `total_budget` は存在しなくなる（`crate::exchange::total_budget` に一本化）。

- [ ] **Step 1: 失敗するテストを書く**

`exchange.rs` の `mod tests` に追加:

```rust
    /// total_budget はジッタ最悪値込みの上界（Issue #16 の op 予算が実待ちより
    /// 短くならない）。jitter=0 なら従来値。
    #[test]
    fn total_budget_includes_jitter_worst_case() {
        let cfg = MrpConfig::default();
        let base: Duration = {
            let mut c = cfg.clone();
            c.jitter = 0.0;
            total_budget(&c)
        };
        assert_eq!(
            total_budget(&cfg).as_millis(),
            base.mul_f64(1.0 + MRP_BACKOFF_JITTER).as_millis()
        );
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller --lib exchange::tests::total_budget_includes_jitter_worst_case`
Expected: FAIL — 現行 `total_budget` は jitter を見ないので左辺 = base のまま、右辺 = base × 1.25 で不一致になる

- [ ] **Step 3: 実装**

`exchange.rs` の `total_budget` を置換:

```rust
/// MRP 再送が尽きるまでの待ち時間総和（ジッタ最悪値込みの上界）。op 予算
/// 設計（Issue #16）の成分 — 実待ちは各項 × (1 + jitter·r) なので、上界は
/// r=1 で見積もる。
pub fn total_budget(cfg: &MrpConfig) -> Duration {
    let mut total = Duration::ZERO;
    let mut interval = cfg.initial_interval;
    for _ in 0..=cfg.max_retries {
        total += interval.mul_f64(1.0 + cfg.jitter);
        interval = interval.mul_f64(cfg.backoff);
    }
    total
}
```

`session.rs`:
- local `fn total_budget`（:36-47、doc コメントごと）を**削除**し、`:456` の呼び出しを `crate::exchange::total_budget(cfg)` に変更。
- `worst_case_send_budget` の doc（:26-27）を更新: `≈ 14.74s` → `≈ 15.93s（ジッタ最悪値込み）`。
- 既存テスト `:3263` の `assert_eq!(worst_case_send_budget().as_millis(), 14742)` → `15928`（= 4742.88ms × 1.25 + 10000ms、切り捨て）。

`crates/matd/src/native.rs:36-38` のコメント更新: `既定 ≈ 4.74s` → `既定 ≈ 5.93s`、`worst_case_send_budget ≈ 14.74s` → `≈ 15.93s`。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller && cargo test -p matd`
Expected: PASS（`worst_case_send_budget` の更新 assert 含む。15928 でなければ実値を確認し、計算根拠ごとコミットメッセージに記す）

- [ ] **Step 5: fmt + コミット**

```bash
cargo fmt
git add -A crates/mat-controller crates/matd
git commit -m "feat(mat-controller): total_budget をジッタ最悪値込みの上界へ（session.rs 重複も統合）"
```

---

### Task 3: MRP 再送ループ 3 箇所へ jitter 適用

**Files:**
- Modify: `crates/mat-controller/src/exchange.rs:256-292`（`send_reliable`）
- Modify: `crates/mat-controller/src/session.rs:466-500`（secure `send_reliable`）、`:997-1041`（report ack 送信ループ）
- Test: 既存テスト（`jitter: 0.0` で決定論のまま）の回帰のみ

**Interfaces:**
- Consumes: Task 1 の `jittered_interval` / `unit_random`、`cfg.jitter`。
- Produces: ワイヤ挙動のみ（新 API なし）。実再送待ち = `interval × (1 + jitter·r)`、r は再送のたびに引き直し。

- [ ] **Step 1: 3 箇所の deadline 算出を変更**

`exchange.rs` `send_reliable`（:258 付近）:

```rust
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline =
                Instant::now() + jittered_interval(interval, cfg.jitter, unit_random());
```

`session.rs` の 2 ループ（:468 付近と :999 付近、どちらも同型）:

```rust
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now()
                + crate::exchange::jittered_interval(
                    interval,
                    cfg.jitter,
                    crate::exchange::unit_random(),
                );
```

3 箇所とも `interval = interval.mul_f64(cfg.backoff);` の決定論進行は変えない（jitter は sleep 値にだけ掛かる — 名目進行に累積させない）。

- [ ] **Step 2: 回帰テスト**

Run: `cargo test -p mat-controller`
Expected: PASS — 既存のタイミング依存テスト（`send_reliable_retransmits_at_active_interval_after_peer_rx` 等）は全て `jitter: 0.0` の cfg なので挙動不変。FAIL したら cfg の `jitter` が 0.0 になっているか確認する。

- [ ] **Step 3: fmt + コミット**

```bash
cargo fmt
git add -A crates/mat-controller
git commit -m "fix(mat-controller): MRP 再送待ちへ spec 4.12.2.1 の jitter を適用（3 ループ全数 — 監査⑧）"
```

---

### Task 4: matd 再購読 backoff の jitter

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`next_backoff` 直後に純関数追加、backoff sleep 箇所 :728-735）
- Test: `crates/matd/src/subscription.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 1 の `mat_controller::exchange::unit_random`。
- Produces: `pub(crate) fn jittered_backoff(nominal: Duration, r: f64) -> Duration`。`next_backoff` と `mark_down` / status の名目値は不変。

- [ ] **Step 1: 失敗するテストを書く**

`subscription.rs` の `mod tests`（`next_backoff` のテスト近傍）に追加:

```rust
    /// backoff jitter: cap 後の名目値 × [0.75, 1.25)。中央値（r=0.5）は名目値
    /// のまま = 設計軌道（down_s 中央値 7-9s）を変えない。
    #[test]
    fn jittered_backoff_range_preserves_median() {
        let n = Duration::from_secs(60);
        assert_eq!(jittered_backoff(n, 0.0), Duration::from_secs(45));
        assert_eq!(jittered_backoff(n, 0.5), n);
        assert!(jittered_backoff(n, 0.999_999) < Duration::from_secs(75));
        assert_eq!(jittered_backoff(Duration::ZERO, 0.7), Duration::ZERO);
    }
```

- [ ] **Step 2: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test -p matd --lib subscription::tests::jittered_backoff_range_preserves_median`
Expected: FAIL（`jittered_backoff` 未定義）

- [ ] **Step 3: 実装**

`next_backoff`（:562-569）の直後に追加:

```rust
/// backoff の実 sleep に乗せるジッタ: cap 適用後の名目値 × [0.75, 1.25)。
/// cap 後に掛けるので、長期障害で全ノードが BACKOFF_MAX に飽和しても実待ちは
/// 45〜75s に散り続け、リトライ波が再同期しない（cap 前に掛けると飽和ノードが
/// 全員ちょうど 60s で再同期する — 監査⑧）。`mark_down` / status の表示は
/// 名目値のまま（表示はエンベロープの説明であって実 sleep の予告ではない）。
pub(crate) fn jittered_backoff(nominal: Duration, r: f64) -> Duration {
    nominal.mul_f64(0.75 + 0.5 * r)
}
```

sleep 箇所（:728-735、`health.mark_down` の後）を変更:

```rust
        let touch_notify = health.touch_notify(node_id);
        let sleep_dur = jittered_backoff(backoff, mat_controller::exchange::unit_random());
        tokio::select! {
            _ = tokio::time::sleep(sleep_dur) => {}
            _ = touch_notify.notified() => {
                backoff = Duration::ZERO;
                health.clear_touched(node_id);
            }
        }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: PASS。start_paused 系の既存テストは sleep を auto-advance で消化するので、backoff が 3.75〜6.25s に散っても影響しない。仮に経過秒を厳密 assert しているテストが落ちたら、advance 量を上限（×1.25）以上に広げて直す。

- [ ] **Step 5: fmt + コミット**

```bash
cargo fmt
git add -A crates/matd
git commit -m "fix(matd): 再購読 backoff の実 sleep へ cap 後 ×[0.75,1.25) jitter（監査⑧）"
```

---

### Task 5: matd 起動 herd の stagger

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`spawn_subscription_manager` :614-628 の spawn 部、`node_subscription_loop` :650- のシグネチャ、定数 + 純関数追加）
- Test: `crates/matd/src/subscription.rs` の `mod tests`

**Interfaces:**
- Consumes: なし（matd 内で完結）。
- Produces: `pub(crate) const STAGGER_STEP: Duration`、`pub(crate) fn stagger_delay(batch_index: usize, batch_len: usize) -> Duration`。`node_subscription_loop` の第 2 引数に `initial_delay: Duration` が入る。

- [ ] **Step 1: 失敗するテストを書く（純関数）**

```rust
    /// 起動 stagger: 同一ティックのバッチ(>1)だけ index × 1s に分散。
    /// rescan の単発追加（バッチ 1）は現行どおり遅延ゼロ。
    #[test]
    fn stagger_delay_spreads_batches_only() {
        assert_eq!(stagger_delay(0, 1), Duration::ZERO);
        assert_eq!(stagger_delay(0, 13), Duration::ZERO);
        assert_eq!(stagger_delay(1, 13), Duration::from_secs(1));
        assert_eq!(stagger_delay(12, 13), Duration::from_secs(12));
    }
```

- [ ] **Step 2: テストが失敗する（コンパイルエラー）ことを確認**

Run: `cargo test -p matd --lib subscription::tests::stagger_delay_spreads_batches_only`
Expected: FAIL（`stagger_delay` 未定義）

- [ ] **Step 3: 実装**

`LEDGER_RESCAN_INTERVAL`（:574）の近くに追加:

```rust
/// 起動 herd の stagger 刻み。同一ティックで複数ノードを spawn するとき、
/// バッチ内 index × この値だけ初回確立を遅らせる（本番 13 台 → 0〜12s に
/// 均等分散）。デプロイ再起動のたびに全ノード同時 CASE で BR 無線が CCA
/// 飽和 → no-ack 1〜2 分、が監査⑧の実 symptom。乱数でなく index 均等なのは
/// herd が単一プロセス内の現象で、均等間隔が厳密に非衝突なため。
pub(crate) const STAGGER_STEP: Duration = Duration::from_secs(1);

/// バッチ内 index → 初期遅延（純関数）。バッチ 1 = 遅延ゼロ（rescan の
/// 単発追加を現行どおり即購読に保つ）。
pub(crate) fn stagger_delay(batch_index: usize, batch_len: usize) -> Duration {
    if batch_len <= 1 {
        Duration::ZERO
    } else {
        STAGGER_STEP * u32::try_from(batch_index).unwrap_or(u32::MAX)
    }
}
```

`spawn_subscription_manager` の spawn 部（:614-628）を、バッチを先に確定してから index つきで spawn する形に変更:

```rust
                    let new_nodes: Vec<u64> = node_ids
                        .into_iter()
                        .filter(|id| subscribed.insert(*id))
                        .collect();
                    for (i, node_id) in new_nodes.iter().copied().enumerate() {
                        if !initial {
                            tracing::info!(node_id, "ledger rescan: new node; subscribing");
                        }
                        let delay = stagger_delay(i, new_nodes.len());
                        let native = Arc::clone(&native);
                        let events = events.clone();
                        let clusters = Arc::clone(&clusters);
                        let health = Arc::clone(&health);
                        tokio::spawn(async move {
                            node_subscription_loop(node_id, delay, native, events, clusters, health)
                                .await
                        });
                    }
```

`node_subscription_loop` のシグネチャに `initial_delay: Duration` を `node_id` の直後に追加し、`health.mark_establishing(node_id);`（:666）の直後に挿入:

```rust
    health.mark_establishing(node_id);
    if !initial_delay.is_zero() {
        // 起動バッチの stagger（監査⑧）。establishing 表示にしてから待つ —
        // status に現れない 12 秒を作らない。
        tracing::debug!(
            node_id,
            delay_s = initial_delay.as_secs(),
            "staggering initial subscribe"
        );
        tokio::time::sleep(initial_delay).await;
    }
```

- [ ] **Step 4: 統合テストを書く（start_paused、既存 manager テストの型を踏襲）**

既存の `manager_recovers_from_unreadable_store_at_startup`（:1867 付近）を手本に、`mod tests` へ追加:

```rust
    /// 起動バッチ(>1)はノード毎に STAGGER_STEP ずつずれて確立する（監査⑧）。
    /// priming 到着の仮想時刻差で stagger を観測する。
    #[tokio::test(start_paused = true)]
    async fn initial_batch_staggers_subscriptions() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("store");
        let mut store = mat_core::store::Store::open_or_init(&store_path).unwrap();
        for node_id in [1u64, 2u64] {
            store
                .upsert_node(mat_core::store::NodeRecord {
                    node_id,
                    commissioned_at: "2026-08-03T00:00:00+09:00".into(),
                })
                .unwrap();
        }
        let est = FakeEstablisher::default();
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = broadcast::channel(64);
        let health = Arc::new(SubHealth::new(None));
        let _handle =
            spawn_subscription_manager(state, store_path.clone(), tx, None, Arc::clone(&health));
        // 2 ノードぶんの priming 初着時刻（仮想時計）を記録する。
        let mut first_seen: std::collections::HashMap<u64, tokio::time::Instant> =
            std::collections::HashMap::new();
        while first_seen.len() < 2 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
                .await
                .expect("both nodes should prime")
                .unwrap();
            first_seen.entry(ev.node_id).or_insert_with(tokio::time::Instant::now);
        }
        let gap = first_seen[&2].saturating_duration_since(first_seen[&1]);
        assert!(
            gap >= STAGGER_STEP,
            "batch spawn should stagger by STAGGER_STEP, gap={gap:?}"
        );
    }
```

注意: `new_nodes` の順序は台帳列挙順。node 1 が index 0（遅延 0）、node 2 が index 1（遅延 1s）になる前提で gap の向きを assert している。列挙順が保証されない実装だったら `gap` を両向きの絶対差で取ること。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: PASS（既存 manager テストはバッチ 1 なので遅延ゼロのまま影響なし）

- [ ] **Step 6: fmt + コミット**

```bash
cargo fmt
git add -A crates/matd
git commit -m "fix(matd): 起動バッチの購読 spawn を index×1s で stagger（監査⑧のデプロイバースト対策）"
```

---

### Task 6: バージョン 1.18.0 + 全体検証

**Files:**
- Modify: `Cargo.toml`（workspace `version = "1.17.0"` → `"1.18.0"`）、`Cargo.lock`（cargo が自動更新）

**Interfaces:**
- Consumes: Task 1〜5 すべて。
- Produces: リリース可能な 1.18.0（実機 E2E 待ち状態）。

- [ ] **Step 1: バージョン bump**

`Cargo.toml` の `[workspace.package]` の `version = "1.17.0"` を `"1.18.0"` へ。

- [ ] **Step 2: CI 相当の全体検証**

Run: `task check`
Expected: fmt:check + clippy + test 全部 PASS。clippy が新コードに文句を言ったらその場で直す（`#[allow]` でなくコードを直す）。

- [ ] **Step 3: コミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.18.0（起動 herd stagger + backoff/MRP jitter — 安定性監査 Tier 2 ⑧）"
```

- [ ] **Step 4: 実機 E2E ゲートの申し送り**

マージはまだしない。実機 E2E（jarvis、`*.new` 方式で本番未置換のまま検証 — despliegue skill）で以下を確認してから main へマージする:
1. 再起動直後の journal: CASE no-ack バースト長と attempts 分布が現行（毎回 1〜2 分）より改善。
2. `matd status`: 全ノード established 到達（最終ノードは stagger ≤12s 遅れるが総所要は悪化しない）。
3. 定常運転: silence 死からの再確立が設計軌道（attempts=1 / down_s 中央値 7-9s 近傍、jitter で ±25% 散る）を保つ。
