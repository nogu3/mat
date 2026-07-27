# matd 購読台帳の定期再読（監査#4）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd 稼働中に commission されたノードを 60 秒周期の台帳再読で検知し、購読を自動で張る（spec: `docs/superpowers/specs/2026-07-27-matd-ledger-rescan-design.md`）。

**Architecture:** `spawn_subscription_manager`（`crates/matd/src/subscription.rs`）を「supervisor タスク 1 本を spawn して返す」形に変える。supervisor は 60 秒ごとに `Store::open` で台帳を読み直し、未知の node_id に `node_subscription_loop` を追加 spawn する。`node_subscription_loop` 自体は無変更。

**Tech Stack:** Rust / tokio（`start_paused` テスト）/ 既存の `FakeEstablisher` テスト足場（`crates/mat-native/src/test_support.rs`）。

## Global Constraints

- 依存クレート追加は禁止（ファイル監視 crate 等は使わない）。
- `LEDGER_RESCAN_INTERVAL` は 60 秒のコード内定数。設定ファイル化しない。
- commission 成功 JSON への note 追加は**しない**(ユーザー決定 2026-07-27)。
- ノード削除は非対応(台帳 API に削除が存在しない)。supervisor は追加だけを扱う。
- ログ規律: 新規ノード検出は info、台帳読み失敗はストリーク初回 warn・以降 debug、変化のないティックはログ無し。
- コミット前に `task check`（fmt:check + clippy -D warnings + test）を通すこと。
- バージョンは 1.7.0（workspace の `Cargo.toml:6`）。
- main マージ前に jarvis 実機 E2E 必須（Task 4、メインセッションが実施）。

---

### Task 1: 台帳再読 supervisor 化

**Files:**
- Modify: `crates/matd/src/subscription.rs:350-382`（`spawn_subscription_manager`）
- Modify: `crates/matd/src/subscription.rs:586-616`（テスト足場 `spawn_manager` の戻り値型）
- Modify: `crates/matd/src/main.rs:230-238`（呼び出し側）
- Test: `crates/matd/src/subscription.rs`（`tests` モジュールに追加）

**Interfaces:**
- Consumes: 既存の `node_subscription_loop(node_id: u64, native: Arc<NativeState>, events: broadcast::Sender<Event>, clusters: Arc<[u32]>, health: Arc<SubHealth>)`（無変更）、`mat_core::store::Store::{open, open_or_init, nodes, upsert_node}`。
- Produces: `pub fn spawn_subscription_manager(native: Arc<NativeState>, store_path: PathBuf, events: broadcast::Sender<Event>, clusters: Option<Vec<u32>>, health: Arc<SubHealth>) -> tokio::task::JoinHandle<()>`（戻り値が `Vec<JoinHandle<()>>` から単一 `JoinHandle<()>` に変わる）。`pub(crate) const LEDGER_RESCAN_INTERVAL: Duration`。

- [ ] **Step 1: Write the failing test**

`crates/matd/src/subscription.rs` の `tests` モジュール（既存の `#[tokio::test(start_paused = true)]` 群の並び）に追加:

```rust
/// 監査#4: matd 稼働中に台帳へ追加されたノードの購読が、次の再読ティック
/// （60s）で自動的に張られる。従来は起動時スナップショットのみで、稼働中
/// commission ノードは matd 再起動まで永久に購読されなかった。
#[tokio::test(start_paused = true)]
async fn manager_picks_up_node_added_after_start() {
    let (mut rx, _health, dir, _handle) = spawn_manager(FakeEstablisher::default(), None);
    // 起動時から台帳に居る node 5 の priming が届く（初回読みは従来どおり）。
    let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
        .await
        .expect("node5 priming should arrive")
        .unwrap();
    assert_eq!(ev.node_id, 5);
    // 稼働中に node 6 を commission（= 台帳へ追記）。
    let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
    store
        .upsert_node(mat_core::store::NodeRecord {
            node_id: 6,
            address: Some("192.0.2.11".into()),
            commissioned_at: "2026-07-27T00:00:00+09:00".into(),
        })
        .unwrap();
    // 次の再読ティック（60s）以内に node 6 の購読が張られ priming が届く。
    // node 5 側のイベントが混ざり得るので node 6 が来るまで読み飛ばす。
    let ev = loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
            .await
            .expect("node6 priming should arrive within one rescan tick")
            .unwrap();
        if ev.node_id == 6 {
            break ev;
        }
    };
    assert!(ev.priming);
}
```

このステップでは既存の `spawn_manager` 足場（戻り値 4 要素目が `Vec<JoinHandle<()>>`）のままなので、束縛名は `_handle` でも型は Vec のまま — コンパイルは通る（束縛名は任意）。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p matd manager_picks_up_node_added_after_start`
Expected: FAIL — 2 つ目の `timeout` が `expect("node6 priming should arrive within one rescan tick")` で panic（現行実装は台帳を再読しないため node 6 の購読は永久に張られない。paused clock は待ちタスクが尽きると自動前進するので、テスト自体は数秒で終わる）。

- [ ] **Step 3: Write the implementation**

`crates/matd/src/subscription.rs:350-382` の `spawn_subscription_manager` を以下に置き換える（直前に定数も追加）:

```rust
/// 台帳の再読間隔。稼働中に `mat commission` されたノードを最大この遅延で
/// 拾って購読を張る（監査#4: 従来は起動時スナップショットのみで、稼働中
/// commission ノードは matd 再起動まで購読されず `mat listen` が無音だった）。
pub(crate) const LEDGER_RESCAN_INTERVAL: Duration = Duration::from_secs(60);

/// commissioned 全ノードへ購読タスクを張る supervisor を起動する。
/// `LEDGER_RESCAN_INTERVAL` ごとに台帳を読み直し、新規ノードに購読ループを
/// 追加 spawn する（op 経路の `require_node` が毎回 store を開き直すのと同じ
/// 「常駐中の台帳更新を拾う」規律）。ノード削除は台帳 API に存在しないため
/// 扱わない。cluster 絞り込みは subscriptions.toml で実装済み（`clusters`
/// パラメータに配線）。native が Unavailable なら何もしない（`mat fabric
/// init` 後の再起動で解消 — 再読で直る状態ではないので空回りさせない）。
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Event>,
    clusters: Option<Vec<u32>>,
    health: Arc<SubHealth>,
) -> tokio::task::JoinHandle<()> {
    // None = subscriptions.toml 無し = full wildcard（空 slice がワイヤ上の wildcard 形）。
    let clusters: Arc<[u32]> = clusters.unwrap_or_default().into();
    tokio::spawn(async move {
        if !matches!(&*native, NativeState::Ready(_)) {
            return;
        }
        // 購読ループを張った node_id。台帳は増える一方（削除 API 無し）なので
        // 集合の縮小は考えない。
        let mut subscribed = std::collections::HashSet::new();
        let mut announced = false;
        let mut read_fail_streak: u32 = 0;
        loop {
            match Store::open(&store_path) {
                Ok(store) => {
                    read_fail_streak = 0;
                    let node_ids: Vec<u64> = store.nodes().map(|n| n.node_id).collect();
                    // 初回の成功読みだけ台数つきの starting ログ（現行踏襲）。
                    // 以降の新規検出はノード単位の info（commission は稀な操作
                    // なのでノイズにならず、「ログに一切現れない」誤診の罠を潰す）。
                    let initial = !announced;
                    if initial {
                        tracing::info!(nodes = node_ids.len(), "subscription manager starting");
                        announced = true;
                    }
                    for node_id in node_ids {
                        if !subscribed.insert(node_id) {
                            continue;
                        }
                        if !initial {
                            tracing::info!(node_id, "ledger rescan: new node; subscribing");
                        }
                        let native = Arc::clone(&native);
                        let events = events.clone();
                        let clusters = Arc::clone(&clusters);
                        let health = Arc::clone(&health);
                        tokio::spawn(async move {
                            node_subscription_loop(node_id, native, events, clusters, health)
                                .await
                        });
                    }
                }
                Err(e) => {
                    // ストリーク初回 warn、以降 debug（60 秒ごとの warn 連打を
                    // 避ける — classify_failure と同じ思想）。transient な失敗
                    //（flock 競合等）は次のティックで自己回復する。
                    read_fail_streak += 1;
                    if read_fail_streak == 1 {
                        tracing::warn!(error = %e.detail, "subscription manager: store unreadable; will retry");
                    } else {
                        tracing::debug!(error = %e.detail, "subscription manager: store unreadable");
                    }
                }
            }
            tokio::time::sleep(LEDGER_RESCAN_INTERVAL).await;
        }
    })
}
```

`crates/matd/src/main.rs:230-238` の呼び出し側を戻り値 1 本に合わせる:

```rust
    // 常駐購読は native が使えるときだけ張る（Unavailable なら listen は
    // ack だけ返り、イベントは流れない — `mat fabric init` 後の再起動で解消）。
    // supervisor は 60s 周期で台帳を再読し、稼働中に commission された
    // ノードにも購読を張る（監査#4）。
    let _sub_handle = matd::subscription::spawn_subscription_manager(
        std::sync::Arc::clone(&native),
        store_path.clone(),
        events_tx.clone(),
        sub_clusters,
        std::sync::Arc::clone(&sub_health),
    );
```

テスト足場 `spawn_manager`（`subscription.rs:586-616`）の戻り値型を合わせる。
シグネチャの `Vec<tokio::task::JoinHandle<()>>` を `tokio::task::JoinHandle<()>` に、
本体の `let handles = spawn_subscription_manager(...)` / `(rx, health, dir, handles)`
を `let handle = ...` / `(rx, health, dir, handle)` に変える。doc コメントの
「`JoinHandle` も同様に束縛しておく」の記述はそのまま有効。既存テストの呼び出し
側は `_handles` 等の束縛名で受けているだけなので修正不要（コンパイルが教えて
くれる — エラーが出た箇所だけ束縛名を直す）。

- [ ] **Step 4: Run the new test and the full suite**

Run: `cargo test -p matd manager_picks_up_node_added_after_start`
Expected: PASS

Run: `task check`
Expected: fmt:check / clippy / 全テスト PASS（既存テストは型変更の追随以外無風のはず。`HashSet` の import は `std::collections::HashSet` をフルパスで使っているので `use` 追加不要）。

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs crates/matd/src/main.rs
git commit -m "feat(matd): 購読台帳を60s周期で再読し稼働中commissionノードに購読を張る（監査#4）"
```

---

### Task 2: 起動時 store 読み失敗 → 次ティック自己回復の釘打ちテスト

**Files:**
- Test: `crates/matd/src/subscription.rs`（`tests` モジュールに追加）

**Interfaces:**
- Consumes: Task 1 の `spawn_subscription_manager`（単一 `JoinHandle` 戻り値）、`FakeEstablisher`、`SubHealth::new`、`crate::native::NativeBackend::with_establisher`、`crate::server::NativeState`。

- [ ] **Step 1: Write the pin test**

Task 1 の supervisor 化により「起動時に store が読めないと永久に購読ゼロ」が
「次のティックで再試行」に変わったはず。その回復挙動を釘打ちする（Task 1 実装
済みなら最初から PASS が期待値 — 回帰防止のピン）:

```rust
/// 監査#4 の副次修正: 起動時に store が読めなくても supervisor は次の
/// 再読ティックで自己回復する（従来は warn を出して購読ゼロで確定だった）。
#[tokio::test(start_paused = true)]
async fn manager_recovers_from_unreadable_store_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    // まだ存在しないパス → 初回 Store::open は store_missing で失敗する。
    let store_path = dir.path().join("store");
    let est = FakeEstablisher::default();
    let native = crate::native::NativeBackend::with_establisher(Box::new(est));
    let state = Arc::new(crate::server::NativeState::Ready(Box::new(native)));
    let (tx, mut rx) = broadcast::channel(64);
    let health = Arc::new(SubHealth::new(None));
    let _handle = spawn_subscription_manager(
        state,
        store_path.clone(),
        tx,
        None,
        Arc::clone(&health),
    );
    // supervisor に初回ティック（読み失敗）を踏ませてから store を作る。
    // start_paused の単一スレッド実行では、この sleep の await 中に
    // supervisor タスクが走る。
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    let mut store = mat_core::store::Store::open_or_init(&store_path).unwrap();
    store
        .upsert_node(mat_core::store::NodeRecord {
            node_id: 7,
            address: Some("192.0.2.12".into()),
            commissioned_at: "2026-07-27T00:00:00+09:00".into(),
        })
        .unwrap();
    // 次のティック（60s）で購読が張られ priming が届く。
    let ev = loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(120), rx.recv())
            .await
            .expect("node7 priming should arrive after store becomes readable")
            .unwrap();
        if ev.node_id == 7 {
            break ev;
        }
    };
    assert!(ev.priming);
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p matd manager_recovers_from_unreadable_store_at_startup`
Expected: PASS（FAIL する場合は Task 1 の実装がティック間で `Store::open` を
やり直していない — supervisor ループの `match Store::open` が `loop` の**内側**に
あるか確認して直す）。

- [ ] **Step 3: Run the full suite**

Run: `task check`
Expected: 全 PASS。

- [ ] **Step 4: Commit**

```bash
git add crates/matd/src/subscription.rs
git commit -m "test(matd): 起動時store読み失敗→次ティック自己回復を釘打ち（監査#4副次修正）"
```

---

### Task 3: バージョン 1.7.0

**Files:**
- Modify: `Cargo.toml:6`（workspace `version = "1.6.0"` → `"1.7.0"`）
- Modify: `Cargo.lock`（`cargo check` で自動追随）

**Interfaces:**
- Consumes: なし。
- Produces: 1.7.0（挙動追加のためマイナーを上げる）。

- [ ] **Step 1: Bump version**

`Cargo.toml:6` を `version = "1.7.0"` に変更し、`cargo check --workspace` を実行して
`Cargo.lock` を追随させる。

- [ ] **Step 2: Run the full suite**

Run: `task check`
Expected: 全 PASS。

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.7.0（購読台帳の60s周期再読 — 監査#4）"
```

---

### Task 4: jarvis 実機 E2E（マージ前必須 — メインセッションが実施）

**このタスクは subagent ではなくメインセッションが実施する**（jarvis への ssh と
本番環境の隔離 matd 運用を伴うため）。手順は [[jarvis-matd-deploy]] の隔離 matd
方式（別 socket + store コピー + 台帳 1 ノード）に従う。

- [ ] **Step 1: arm64 ビルドと転送**

`task dist:arm64` → `dist/arm64/matd` を jarvis へ `matd.new` として scp。

- [ ] **Step 2: 隔離 matd 起動**

store コピー（台帳 1 ノードに削る）+ 別 socket で `matd.new` を起動し、
既存ノードの購読確立（`subscription established`）と
`subscription manager starting` をログで確認。

- [ ] **Step 3: 稼働中の台帳追記**

隔離 store の `nodes.json` に 2 ノード目を追記（実在ノードなら購読確立まで、
実在しないノードでも `ledger rescan: new node; subscribing` →
`subscription attempt failed` が出れば台帳追随の証明になる）。

- [ ] **Step 4: 判定**

追記から 60 秒以内に `ledger rescan: new node; subscribing` が出ること。
既存ノードの購読・op に影響が無いこと（無風確認）。

- [ ] **Step 5: 後始末**

隔離 matd を停止し、コピー store と socket を削除。

E2E 合格後: main へマージ → 本番デプロイ（despliegue skill）→ スモーク。
