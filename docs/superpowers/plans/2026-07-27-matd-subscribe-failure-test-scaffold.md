# matd 購読の失敗経路テスト足場 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `matd` の常駐購読の**失敗側**（再確立 backoff ラダー / 無音 deadline / pump のセッション死）を、fake に失敗を表現させて統合テストで釘打ちする。

**Architecture:** `FakeEstablisher` に「残り失敗回数」カウンタ 2 本（`fail_subscription` / `fail_next_report`）を足し、`Arc` 共有で `FakeSubConn` にも伝える。`matd` 側は `mod tests` にローカルヘルパ `spawn_manager` を切って既存 7 本のコピペを畳み、その上に失敗経路のテストを 4 本足す。**プロダクションコードは 1 行も変えない。**

**Tech Stack:** Rust / tokio（`#[tokio::test(start_paused = true)]` の仮想時計） / `async_trait` / `tempfile`

## Global Constraints

- **プロダクション挙動を変えない。** `crates/matd/src/subscription.rs` の `mod tests` より上（`#[cfg(test)]` の外側）と `crates/mat-native/src/lib.rs` の非テストコードは変更禁止。変更してよいのは `crates/mat-native/src/test_support.rs` とテストモジュールだけ。
- **既存テストの主張を変えない。** Task 2 の移行は前処理の置換のみ。`assert!` / `assert_eq!` の中身・順序・メッセージは一切触らない。
- **既存 fake の既定挙動は不変。** 新フィールドは既定 `0` = 常に成功。`FakeEstablisher::default()` を使う既存の 30 箇所超（`mat` / `matd` / `mat-native`）が影響を受けないこと。
- **コミット前に必ず `task check`**（fmt:check + clippy `-D warnings` + test）。CLAUDE.md の規律。
- **コミット対象はこのセッションで編集したファイルのみ。** `thread-map.html`（セッション開始時から untracked）は絶対に `git add` しない。
- 作業ブランチは `test/matd-subscribe-failure-scaffold`（spec コミット 08d85eb が既に載っている）。
- タイミング定数（`subscription.rs` 実定数、テストの期待値の根拠）:
  - `BACKOFF_INITIAL = 5s` / `BACKOFF_MAX = 300s` / `PUMP_SLICE = 5s` / `OP_GRACE = 10s` / `SILENCE_SLACK = 30s`
  - `FakeSubConn::default().max_interval_s = 60` → **無音 deadline = 90s**

---

### Task 1: fake に失敗カウンタを足す

**Files:**
- Modify: `crates/mat-native/src/test_support.rs`（`FakeSubConn` / `FakeEstablisher` / `next_report` / `establish_subscription`）
- Test: `crates/mat-native/src/lib.rs`（末尾の `mod tests` — 既存 `fake_establisher_serves_scripted_subscription` の直後）

**Interfaces:**
- Produces:
  - `FakeEstablisher::fail_subscription: Arc<AtomicUsize>` — 残り回数だけ `establish_subscription` が `Err(fail_kind)`。既定 0。
  - `FakeEstablisher::fail_next_report: Arc<AtomicUsize>` — 払い出す `FakeSubConn` と共有。残り回数だけ `next_report` が `Err(ErrorKind::SessionFailed)`。既定 0。
  - `FakeSubConn::fail_next_report: Arc<AtomicUsize>` — 同上（既定 0）。
  - `FakeEstablisher::calls` は **失敗も含めた試行回数**を数える（既存の `Arc<AtomicUsize>` の意味を明文化）。

- [ ] **Step 1: 失敗テストを 2 本書く**

`crates/mat-native/src/lib.rs` の `mod tests` の末尾（`fake_establisher_serves_scripted_subscription` の閉じ括弧の直後、`}` でモジュールが閉じる前）に追記する。`ErrorKind` は同モジュールで既に import 済み。

```rust
    /// fake の失敗カウンタ: 残り回数だけ失敗し、尽きたら成功する
    /// （matd の再確立ラダーを回すための足場）。既定 0 = 常に成功なので
    /// 既存テストの挙動は変わらない。
    #[tokio::test]
    async fn fake_establisher_fails_subscription_n_times_then_succeeds() {
        use crate::test_support::FakeEstablisher;
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        est.fail_subscription.store(2, Ordering::SeqCst);
        for attempt in 1..=2 {
            let err = match est.establish_subscription(5).await {
                Err(e) => e,
                Ok(_) => panic!("attempt {attempt} は失敗するはず"),
            };
            assert_eq!(err.kind, ErrorKind::Timeout, "既定 fail_kind を使う");
        }
        assert!(
            est.establish_subscription(5).await.is_ok(),
            "カウンタが尽きたら成功する"
        );
        // 失敗も試行として数える（matd 側テストが calls で試行回数を主張できる）。
        assert_eq!(est.calls.load(Ordering::SeqCst), 3);
    }

    /// 確立の**あと**に注入した fail_next_report が pump 側（FakeSubConn）へ効く。
    /// Arc 共有でないとこの順序が表現できない。
    #[tokio::test]
    async fn fake_sub_conn_next_report_fails_when_injected_after_establish() {
        use crate::test_support::FakeEstablisher;
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let mut conn = est.establish_subscription(5).await.unwrap();
        conn.subscribe_wildcard(&[]).await.unwrap();

        est.fail_next_report.store(1, Ordering::SeqCst);
        let err = match conn.next_report(std::time::Duration::from_millis(50)).await {
            Err(e) => e,
            Ok(_) => panic!("注入した 1 回は Err になるはず"),
        };
        assert_eq!(err.kind, ErrorKind::SessionFailed);
        // 尽きたら従来どおり無音 Ok(None)。
        assert!(conn
            .next_report(std::time::Duration::from_millis(50))
            .await
            .unwrap()
            .is_none());
    }
```

- [ ] **Step 2: 失敗することを確認**

Run: `cargo test -p mat-native --lib fake_ 2>&1 | tail -20`
Expected: コンパイルエラー `no field 'fail_subscription' on type 'FakeEstablisher'` および `no field 'fail_next_report'`。

- [ ] **Step 3: `take_failure` ヘルパを足す**

`crates/mat-native/src/test_support.rs`、`onoff_report` 関数の直前（`FakeSubConn` 構造体定義の後）に追記:

```rust
/// カウンタが正なら 1 減らして `true`（= この呼び出しは失敗させる）を返す。
/// 0 なら `false`（成功）。fake の「残り失敗回数」表現の共通部品 — bool では
/// 「N 回失敗してから成功」が表現できず、matd の backoff ラダーが回せない。
fn take_failure(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}
```

- [ ] **Step 4: `FakeSubConn` にフィールドを足す**

構造体定義（`pub seen_clusters: ...` の直後）に追記:

```rust
    /// 残り回数だけ `next_report` を `SessionFailed` で失敗させる（0 = 常に成功）。
    /// `FakeEstablisher::fail_next_report` と同一の Arc — テストが確立後に
    /// 注入して pump を狙って殺せる。
    pub fail_next_report: std::sync::Arc<AtomicUsize>,
```

`impl Default for FakeSubConn` の中、`seen_clusters: std::sync::Arc::default(),` の直後に追記:

```rust
            fail_next_report: std::sync::Arc::default(),
```

- [ ] **Step 5: `next_report` の先頭で失敗を見る**

`impl crate::SubscribeConn for FakeSubConn` の `next_report` の本体先頭（`if let Some(r) = self.live...` の前）に挿入:

```rust
        if take_failure(&self.fail_next_report) {
            return Err(MatError::new(
                ErrorKind::SessionFailed,
                "fake subscription session error",
            ));
        }
```

- [ ] **Step 6: `FakeEstablisher` にフィールドを足す**

構造体定義（`pub sub_live: ...` の直後）に追記:

```rust
    /// `establish_subscription` を残り回数だけ `fail_kind` で失敗させる
    /// （0 = 常に成功）。matd の再確立 backoff ラダーを回すためのカウンタ。
    pub fail_subscription: std::sync::Arc<AtomicUsize>,
    /// 払い出す `FakeSubConn` の `next_report` を残り回数だけ失敗させる
    /// （0 = 常に成功）。Arc 共有なのでテストが確立後に注入できる。
    pub fail_next_report: std::sync::Arc<AtomicUsize>,
```

`impl Default for FakeEstablisher` の中、`sub_live: std::sync::Arc::default(),` の直後に追記:

```rust
            fail_subscription: std::sync::Arc::default(),
            fail_next_report: std::sync::Arc::default(),
```

- [ ] **Step 7: `establish_subscription` を書き換える**

既存の実装を丸ごと次に置き換える:

```rust
    async fn establish_subscription(
        &self,
        _node_id: u64,
    ) -> Result<Box<dyn crate::SubscribeConn>, MatError> {
        // 失敗も試行として数える（テストが calls で試行回数を主張できる）。
        self.calls.fetch_add(1, Ordering::SeqCst);
        if take_failure(&self.fail_subscription) {
            return Err(MatError::new(self.fail_kind, "fake subscription failure"));
        }
        Ok(Box::new(FakeSubConn {
            seen_clusters: std::sync::Arc::clone(&self.sub_clusters),
            live: std::sync::Arc::clone(&self.sub_live),
            fail_next_report: std::sync::Arc::clone(&self.fail_next_report),
            ..Default::default()
        }))
    }
```

- [ ] **Step 8: テストが通ることを確認**

Run: `cargo test -p mat-native --lib fake_ 2>&1 | tail -20`
Expected: `fake_establisher_fails_subscription_n_times_then_succeeds`、`fake_sub_conn_next_report_fails_when_injected_after_establish`、既存の `fake_establisher_serves_scripted_subscription` が全て PASS。

- [ ] **Step 9: 既存テスト全数が無傷であることを確認**

Run: `task check`
Expected: fmt / clippy / test すべて成功。`mat` / `matd` の既存テスト（`FakeEstablisher::default()` を使う 30 箇所超）が 1 本も落ちないこと。

- [ ] **Step 10: コミット**

```bash
git add crates/mat-native/src/test_support.rs crates/mat-native/src/lib.rs
git commit -m "test(native): fake に購読失敗カウンタ 2 本（残り回数式）

matd の再確立ラダーと無音 deadline を統合テストで回すための足場。
FakeEstablisher に fail_subscription / fail_next_report（どちらも
Arc<AtomicUsize> の残り失敗回数、既定 0 = 常に成功）を足し、
fail_next_report は払い出す FakeSubConn と Arc 共有する（確立後に
注入して pump を狙って殺せるようにするため）。既定 0 なので既存
テストの挙動は不変。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: `spawn_manager` ヘルパを切り、既存 7 本を寄せる

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`mod tests` のみ）

**Interfaces:**
- Consumes: Task 1 の `FakeEstablisher`（既定挙動のみ — 失敗カウンタはここでは使わない）
- Produces:
  - `fn spawn_manager(est: FakeEstablisher, clusters: Option<Vec<u32>>) -> (broadcast::Receiver<Event>, Arc<SubHealth>, tempfile::TempDir, Vec<tokio::task::JoinHandle<()>>)`
    Task 3 / Task 4 の 4 本が全てこれを使う。

**このタスクはリファクタなので新規テストは書かない。** 検証は「既存テストが全数そのまま通る」こと。

- [ ] **Step 1: ヘルパを追加する**

`crates/matd/src/subscription.rs` の `mod tests` 内、`use serde_json::json;` の直後に追記:

```rust
    /// node 5 だけの台帳と fake establisher で購読マネージャを起動する共通足場。
    ///
    /// 戻り値の `TempDir` は**テスト側が束縛して生かし続ける**こと（`_dir` は可、
    /// `_` は不可 — `_` は即 drop され store ごと消える）。`JoinHandle` も同様に
    /// 束縛しておく（既存テストの寿命の握り方をそのまま踏襲）。
    fn spawn_manager(
        est: FakeEstablisher,
        clusters: Option<Vec<u32>>,
    ) -> (
        broadcast::Receiver<Event>,
        Arc<SubHealth>,
        tempfile::TempDir,
        Vec<tokio::task::JoinHandle<()>>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-20T00:00:00+09:00".into(),
            })
            .unwrap();
        let native = crate::native::NativeBackend::with_establisher(Box::new(est));
        let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, rx) = broadcast::channel(64);
        let health = Arc::new(SubHealth::new(None));
        let handles = spawn_subscription_manager(
            state,
            dir.path().to_path_buf(),
            tx,
            clusters,
            Arc::clone(&health),
        );
        (rx, health, dir, handles)
    }
```

- [ ] **Step 2: `manager_emits_priming_events_from_fake_subscription` を寄せる**

`let dir = tempfile::tempdir()` から `spawn_subscription_manager(state, dir.path().to_path_buf(), tx, None, health);` までの前処理ブロック全体を、次の 1 行に置き換える（`let ev = tokio::time::timeout(...)` 以降の assert は一切触らない）:

```rust
        let (mut rx, _health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
```

- [ ] **Step 3: `manager_passes_clusters_to_subscribe` を寄せる**

同じ前処理ブロックを次に置き換える:

```rust
        let est = FakeEstablisher::default();
        let seen = Arc::clone(&est.sub_clusters);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, Some(vec![0x0006, 0x0406]));
```

- [ ] **Step 4: `op_grace_triggers_fast_resubscribe` を寄せる**

`health` を後段で使うので `_health` ではなく `health` で受ける:

```rust
        let (mut rx, health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
```

- [ ] **Step 5: `live_report_clears_pending_without_resubscribe` を寄せる**

```rust
        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);
```

- [ ] **Step 6: `priming_diff_after_resubscribe_is_promoted_to_recovered_event` を寄せる**

```rust
        let est = FakeEstablisher::default();
        let live = Arc::clone(&est.sub_live);
        let (mut rx, health, _dir, _handles) = spawn_manager(est, None);
```

- [ ] **Step 7: `noop_op_does_not_kill_healthy_subscription` を寄せる**

```rust
        let (mut rx, health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
```

- [ ] **Step 8: `changing_op_with_silent_device_triggers_fast_resubscribe` を寄せる**

```rust
        let (mut rx, health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);
```

- [ ] **Step 9: 既存テストが全数そのまま通ることを確認**

Run: `cargo test -p matd --lib subscription:: 2>&1 | tail -20`
Expected: 移行した 7 本を含む `subscription::tests` の全テストが PASS。

移行漏れの検出: `grep -c "tempfile::tempdir" crates/matd/src/subscription.rs` が **1**（ヘルパ内の 1 箇所のみ）であること。

- [ ] **Step 10: `task check`**

Run: `task check`
Expected: 成功。特に clippy の `-D warnings` で未使用変数（寄せ損なった `store` / `tx` 等）が残っていないこと。

- [ ] **Step 11: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "test(matd): 購読テストの 20 行コピペを spawn_manager へ畳む

既存 7 本が台帳作成〜マネージャ起動の同じ 20 行を丸ごと複製していた。
ローカルヘルパ 1 本に寄せ、各テストには主張だけが残るようにする。
assert は 1 つも変えていない（純粋な前処理の置換）。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: 確立ラダーと pump セッション死のテスト

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`mod tests` の末尾に追記）

**Interfaces:**
- Consumes: Task 1 の `FakeEstablisher::fail_subscription` / `fail_next_report`、Task 2 の `spawn_manager`

**注意（characterization test）:** プロダクション挙動は変えないので、これらのテストは**書いた瞬間に PASS する**。よって「まず失敗させる」の代わりに、**プロダクション側を一時的に壊してテストが落ちることを確認 → 元に戻す**（mutation check）で、テストが空振りでないことを証明する。この確認をサボると、何も検証していないテストが緑のまま残る。

- [ ] **Step 1: テスト 2 本を書く**

`crates/matd/src/subscription.rs` の `mod tests` の末尾（最後のテスト `changing_op_with_silent_device_triggers_fast_resubscribe` の閉じ括弧の後、モジュールを閉じる `}` の前）に追記:

```rust
    /// 確立が 3 回失敗したら backoff ラダー（5s → 10s → 20s）を実際に登り、
    /// 4 回目で回復する。`next_backoff` の純関数テストはあったが、ループが
    /// その間隔で再試行することは一度も通されていなかった。
    #[tokio::test(start_paused = true)]
    async fn establish_failures_climb_backoff_then_recover() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let calls = Arc::clone(&est.calls);
        est.fail_subscription.store(3, Ordering::SeqCst);
        let t0 = tokio::time::Instant::now();
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("4 回目の確立で priming が届く")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(35),
            "5+10+20 のラダーを実際に登ること: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(75),
            "4 回目で成功し 40s の次段を待たないこと: {elapsed:?}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "失敗 3 + 成功 1 = 4 試行（失敗も試行として数える）"
        );
    }

    /// pump がセッションエラーで死んだら、無音 deadline (90s) を待たずに
    /// backoff 5s で再購読する（`run_subscription_once` の `Err` 分岐が
    /// `Ok(())` を返してループが「購読喪失」として扱う経路）。
    #[tokio::test(start_paused = true)]
    async fn pump_session_error_resubscribes_without_waiting_deadline() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let fail_next_report = Arc::clone(&est.fail_next_report);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // 確立の**あと**に注入する = 走っている pump を狙って殺す。
        let t0 = tokio::time::Instant::now();
        fail_next_report.store(1, Ordering::SeqCst);

        let ev = tokio::time::timeout(Duration::from_secs(60), rx.recv())
            .await
            .expect("再購読の priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(20),
            "無音 deadline (90s) を待たず backoff 5s で戻ること: {elapsed:?}"
        );
    }
```

- [ ] **Step 2: 2 本とも PASS することを確認**

Run: `cargo test -p matd --lib subscription::tests::establish_failures_climb_backoff_then_recover subscription::tests::pump_session_error 2>&1 | tail -20`
Expected: 2 passed。

- [ ] **Step 3: mutation check — ラダーのテストが空振りでないことを証明**

`crates/matd/src/subscription.rs:25` の `BACKOFF_INITIAL` を一時的に `Duration::from_secs(1)` へ変更する。

Run: `cargo test -p matd --lib subscription::tests::establish_failures_climb_backoff_then_recover 2>&1 | tail -20`
Expected: **FAIL**（`5+10+20 のラダーを実際に登ること: 7s` 相当のメッセージ）。

確認したら `BACKOFF_INITIAL` を `Duration::from_secs(5)` へ戻し、再度テストを流して PASS に戻ることを確認する。

- [ ] **Step 4: mutation check — pump セッション死のテストが空振りでないことを証明**

`run_subscription_once` の `next_report` の `Err(e) =>` アームから `return Ok(());` の行だけを一時的に削除する（他はそのまま。アームは `()` を返すのでコンパイルは通り、ループが継続するようになる）。

Run: `cargo test -p matd --lib subscription::tests::pump_session_error 2>&1 | tail -20`
Expected: **FAIL**（セッションエラーで抜けなくなり、90s の無音 deadline まで待つので `< 20s` の assert が落ちる）。

確認したら `return Ok(());` を元の位置へ戻し、PASS に戻ることを確認する。

- [ ] **Step 5: `task check`**

Run: `task check`
Expected: 成功。mutation check の一時変更が残っていないことをここで担保する（`git diff` でプロダクションコードに差分が無いことも目視確認）。

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "test(matd): 確立 backoff ラダーと pump セッション死を統合で釘打ち

どちらも純関数テストだけがあり、ループを回した検証がゼロだった。
確立 3 連続失敗 → 5+10+20 のラダーを登って 4 回目で回復すること、
pump のセッションエラーが無音 deadline を待たず backoff 5s で
再購読に落ちることを、仮想時計で確認する。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: 無音 deadline と backoff リセットのテスト

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`mod tests` の末尾に追記）

**Interfaces:**
- Consumes: Task 1 の `FakeEstablisher::fail_subscription` / `fail_next_report`、Task 2 の `spawn_manager`

Task 3 と同じく characterization test なので、**mutation check を必ず実施する**。

- [ ] **Step 1: テスト 2 本を書く**

`mod tests` の末尾（Task 3 で足した 2 本の後）に追記:

```rust
    /// 完全無音のまま無音 deadline（max_interval 60s + slack 30s = 90s）を
    /// 超えたら購読を殺して再購読する（`PumpEnd::BornDeadSilence` 経路を
    /// 統合で通す最初のテスト）。実機で最も頻繁に踏まれる死に方。
    #[tokio::test(start_paused = true)]
    async fn silent_subscription_dies_at_deadline_and_resubscribes() {
        let (mut rx, _health, _dir, _handles) = spawn_manager(FakeEstablisher::default(), None);

        let ev = tokio::time::timeout(Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);
        let t0 = tokio::time::Instant::now();

        // live キューへ何も入れない = デバイス発ゼロのまま（born-dead）。
        let ev = tokio::time::timeout(Duration::from_secs(180), rx.recv())
            .await
            .expect("deadline 超過で再購読の priming が届く")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(90),
            "deadline より早く購読を殺さないこと: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(120),
            "deadline + backoff 5s の範囲で再購読すること: {elapsed:?}"
        );
    }

    /// 確立に成功したら backoff ラダーがリセットされる。ラダーを 20s まで
    /// 育ててから確立させ、その購読を殺す。リセットされていれば次の再試行は
    /// 5s 後、されていなければ 40s 後 — 15s の閾値で明確に区別できる。
    #[tokio::test(start_paused = true)]
    async fn backoff_resets_after_successful_establishment() {
        use std::sync::atomic::Ordering;

        let est = FakeEstablisher::default();
        let fail_next_report = Arc::clone(&est.fail_next_report);
        est.fail_subscription.store(3, Ordering::SeqCst);
        let (mut rx, _health, _dir, _handles) = spawn_manager(est, None);

        // 3 回失敗（backoff は 20s まで育つ）→ 4 回目で確立。
        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("ラダーを登った先の priming")
            .unwrap();
        assert!(ev.priming);

        // 確立できた購読を殺す。
        let t0 = tokio::time::Instant::now();
        fail_next_report.store(1, Ordering::SeqCst);

        let ev = tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("再購読の priming")
            .unwrap();
        assert!(ev.priming);
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_secs(15),
            "確立成功で backoff が 5s へリセットされること（未リセットなら 40s）: {elapsed:?}"
        );
    }
```

- [ ] **Step 2: 2 本とも PASS することを確認**

Run: `cargo test -p matd --lib subscription::tests::silent_subscription_dies subscription::tests::backoff_resets 2>&1 | tail -20`
Expected: 2 passed。

- [ ] **Step 3: mutation check — 無音 deadline のテストが空振りでないことを証明**

`crates/matd/src/subscription.rs:41` の `SILENCE_SLACK` を一時的に `Duration::from_secs(600)` へ変更する（deadline が 660s になる）。

Run: `cargo test -p matd --lib subscription::tests::silent_subscription_dies 2>&1 | tail -20`
Expected: **FAIL**（`deadline 超過で再購読の priming が届く` の expect が 180s の timeout で落ちる）。

確認したら `SILENCE_SLACK` を `Duration::from_secs(30)` へ戻し、PASS に戻ることを確認する。

- [ ] **Step 4: mutation check — backoff リセットのテストが空振りでないことを証明**

`node_subscription_loop` の `Ok(()) =>` アームから `backoff = Duration::ZERO;` の行だけを一時的に削除する。

Run: `cargo test -p matd --lib subscription::tests::backoff_resets 2>&1 | tail -20`
Expected: **FAIL**（リセットされないので次の再試行が 40s 後になり、`< 15s` の assert が落ちる）。

確認したら `backoff = Duration::ZERO;` を元の位置へ戻し、PASS に戻ることを確認する。

- [ ] **Step 5: `task check` と差分の目視確認**

Run: `task check`
Expected: 成功。

Run: `git diff HEAD --stat`
Expected: `crates/matd/src/subscription.rs` のみ。mutation check の一時変更が残っていないこと（`mod tests` の外側に差分が無いこと）を `git diff HEAD -- crates/matd/src/subscription.rs` で確認する。

- [ ] **Step 6: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "test(matd): 無音 deadline と backoff リセットを統合で釘打ち

PumpEnd::BornDeadSilence（実機で最も頻繁に踏まれる死に方）は
pump_verdict の純関数テストだけで、ループを通した検証が無かった。
無音 90s で殺して再購読すること、確立成功で backoff ラダーが 5s へ
リセットされること（未リセットなら 40s）を仮想時計で確認する。

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

## 完了条件

- `cargo test -p matd --lib subscription::` が全数 PASS（既存 + 新規 4 本）
- `cargo test -p mat-native --lib` が全数 PASS（既存 + 新規 2 本）
- `task check` 成功
- `git diff main...HEAD` にプロダクションコードの差分が無い（spec / plan の md、`test_support.rs`、テストモジュールのみ）
- 4 本の mutation check が全て「壊すと落ちる」ことを実演済み
