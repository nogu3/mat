# matd 属性値キャッシュ: priming 差分回復 + op 期待の健全化 — 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd に属性最終値キャッシュを 1 つ持たせ、(A) 購読の盲目期間中に失われた遷移を再購読時の priming から `recovered: true` イベントとして回復し、(B) 「op 成功 = レポートが出るはず」という unsound な前提を捨てて、実際に値が変わると証明できる op にだけレポート期待（pending）を打つようにする。

**Architecture:** キャッシュは `SubHealth`（server op 経路と購読 pump の共有構造、`crates/matd/src/subscription.rs`）に 1 つだけ置く（`values: Mutex<HashMap<(node_id, endpoint, cluster, attribute), Value>>`）。購読 pump は priming / live 双方の全 scalar イベントを純関数 `classify_against_cache` に通してから broadcast へ送る（差分 priming は `priming: false` + `recovered: true` へ昇格）。server の op 経路は同じキャッシュを読み、On/Off/Level の目標値と現在値が**不一致の時だけ** `note_op` する。grace 10s・無音 deadline・`pump_verdict` は無変更。

**Tech Stack:** Rust 2021 / tokio / serde_json / tracing。テストは既存の `mat_native::test_support::{FakeEstablisher, onoff_report}` 流儀（実デバイス不要）。

## Global Constraints

- 設計 spec は 2 本。**両方を満たすこと**:
  - `docs/superpowers/specs/2026-07-23-priming-diff-recovery-design.md`（差分回復）
  - `docs/superpowers/specs/2026-07-24-matd-op-expectation-noop-fix-design.md`（op 期待の健全化）
  - 後者の「実装の置き場所」指定が前者の「`node_subscription_loop` 内ローカル」を**上書き**する: キャッシュは最初から `SubHealth` 共有形で実装する。
- 変更は `crates/matd/` に閉じる（+ README / Cargo.toml のバージョン）。`crates/mat` は無変更（`mat listen` はイベント行を素通しするため `recovered` は自動で流れる）。
- `SILENCE_SLACK` / `OP_GRACE` / `pump_verdict` / `silence_deadline` は**変更しない**。
- キャッシュは ephemeral なプロセス内状態のみ。永続化しない（CLAUDE.md 設計ルール 4）。
- stdout は純 JSON、診断は stderr の `tracing` のみ（CLAUDE.md 出力規約）。
- 各タスクの最後に `task check`（fmt:check + clippy -D warnings + test）が通ること。コミットは各タスク末尾で 1 回。
- 実機 E2E はマージ前必須（Task 7 に手順あり。**実施は別セッション/ユーザー操作**）。

## File Structure

- `crates/matd/src/subscription.rs`（修正）— `Event.recovered` フィールド、`ValueKey` 型、純関数 `classify_against_cache`、`SubHealth` の値キャッシュ（`observe` / `cached_value`）、pump への配線。単体・統合テストもここ。
- `crates/matd/src/server.rs`（修正）— `op_report_expectation` をキャッシュ引数付きの純関数へ、`op_state_target` / `note_op_expectation` を追加、`run_op` の呼び出し差し替え、既存テストの更新。
- `README.md`（修正）— listen イベントの `recovered` フィールド説明。
- `Cargo.toml`（修正）— workspace version 1.1.0 → 1.2.0。

## 事前確認（実装者向けメモ）

- 既存の重要な不変条件: `run_subscription_once` は priming 配信の**前**に `health.clear_pending(node_id)` を呼ぶ。pump は report 受信ごとに `clear_pending` を呼ぶ。これらは触らない。
- `mat_controller::im` の定数（`crates/mat-controller/src/im.rs`）: `CLUSTER_ON_OFF = 0x0006`, `ATTR_ON_OFF = 0x0000`, `CLUSTER_LEVEL_CONTROL = 0x0008`, `ATTR_CURRENT_LEVEL = 0x0000`, `CLUSTER_COLOR_CONTROL = 0x0300`。`server.rs` は既に `use mat_controller::im;` 済み。
- `FakeEstablisher::establish_subscription` は**毎回新しい `FakeSubConn`** を返し、その priming は常に `onoff_report(1, true)`（endpoint=1, cluster=0x0006, attribute=0x0000, value=true）。`sub_live` キューは全 conn 共有で、テストが確立後に live report を注入できる。
- テストで時間を進める場合は既存流儀どおり `#[tokio::test(start_paused = true)]`。

---

### Task 1: `Event` に `recovered` フィールドを足す（挙動不変の土台）

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`Event` 構造体 / `to_json` / `events_from_report` / 既存テスト）
- Modify: `crates/matd/src/server.rs:903-911`（テスト内の `Event` リテラル）

**Interfaces:**
- Produces: `Event { timestamp, node_id, endpoint, cluster, attribute, value, priming, recovered }` — 以降の全タスクがこの形を前提にする。`to_json` は `recovered` を**常に**出力する。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/subscription.rs` の既存テスト `event_json_uses_chip_tool_names_and_numeric_fallback` を差し替える（`recovered: false` の指定と JSON アサートを追加）:

```rust
    #[test]
    fn event_json_uses_chip_tool_names_and_numeric_fallback() {
        let ev = Event {
            timestamp: "2026-07-20T00:00:00+09:00".to_string(),
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406,   // occupancysensing
            attribute: 0x0000, // occupancy
            value: json!(1),
            priming: false,
            recovered: false,
        };
        let j = ev.to_json();
        assert_eq!(j["node_id"], 21);
        assert_eq!(j["endpoint"], 1);
        assert_eq!(j["cluster"], "occupancysensing");
        assert_eq!(j["attribute"], "occupancy");
        assert_eq!(j["value"], 1);
        assert_eq!(j["priming"], false);
        // 差分回復で昇格したイベントかどうかは常に載る（既定 false）。
        assert_eq!(j["recovered"], false);
        assert_eq!(j["timestamp"], "2026-07-20T00:00:00+09:00");

        // ids テーブルに無いものは数値のまま。
        let ev = Event {
            cluster: 0xFFF1_0001,
            attribute: 0x9999,
            ..ev
        };
        let j = ev.to_json();
        assert_eq!(j["cluster"], 0xFFF1_0001u32);
        assert_eq!(j["attribute"], 0x9999);

        // 昇格イベントは priming=false と recovered=true が同居する。
        let ev = Event {
            priming: false,
            recovered: true,
            ..ev
        };
        assert_eq!(ev.to_json()["recovered"], true);
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd event_json_uses_chip_tool_names_and_numeric_fallback`
Expected: コンパイルエラー `struct `Event` has no field named `recovered``

- [ ] **Step 3: 最小実装**

`crates/matd/src/subscription.rs` の `Event` 定義（doc コメントも 1 行足す）:

```rust
#[derive(Debug, Clone)]
pub struct Event {
    pub timestamp: String,
    pub node_id: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub value: serde_json::Value,
    pub priming: bool,
    /// priming 差分回復で昇格したイベント（購読の盲目期間中に起きた実遷移を
    /// 再購読時の priming から検出したもの）。`priming` と直交し、昇格時は
    /// `priming: false` + `recovered: true` になる。timestamp は受信時刻で
    /// あり、実際の遷移時刻ではない。
    pub recovered: bool,
}
```

`to_json` の JSON へ 1 行追加:

```rust
        serde_json::json!({
            "timestamp": self.timestamp.clone(),
            "node_id": self.node_id,
            "endpoint": self.endpoint,
            "cluster": cluster,
            "attribute": attribute,
            "value": self.value,
            "priming": self.priming,
            "recovered": self.recovered,
        })
```

`events_from_report` の `Event` 生成（差分判定はここではやらない — Task 3 の `observe` が担当）:

```rust
        out.push(Event {
            timestamp: ts.clone(),
            node_id,
            endpoint,
            cluster,
            attribute,
            value: data.clone(),
            priming,
            recovered: false,
        });
```

`crates/matd/src/server.rs` のテスト `listen_filter_matches_by_resolved_ids` 内の `Event` リテラルにも `recovered: false,` を足す（`priming: false,` の直後）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 全 PASS（`recovered` を足しただけで挙動は不変）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs crates/matd/src/server.rs
git commit -m "feat(matd): listen イベントに recovered フィールドを追加（既定 false）"
```

---

### Task 2: 値キャッシュと純関数 `classify_against_cache`（未配線）

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`ValueKey` / `classify_against_cache` / `SubHealth` 拡張 + 単体テスト）

**Interfaces:**
- Consumes: Task 1 の `Event.recovered`。
- Produces:
  - `pub(crate) type ValueKey = (u64, u16, u32, u32);`（node_id, endpoint, cluster, attribute）
  - `pub(crate) fn classify_against_cache(cache: &mut HashMap<ValueKey, serde_json::Value>, ev: Event) -> Event`
  - `SubHealth::observe(&self, ev: Event) -> Event`（ロックして上の純関数へ委譲）
  - `SubHealth::cached_value(&self, node_id: u64, endpoint: u16, cluster: u32, attribute: u32) -> Option<serde_json::Value>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/subscription.rs` の `mod tests` の末尾に追加:

```rust
    /// 純関数の契約（priming 差分回復 spec の挙動表）:
    /// 初見 priming → 非昇格・格納 / 同値 priming → 非昇格・素通し /
    /// 差分 priming → 昇格 / 非 priming → 素通し・更新。
    #[test]
    fn classify_against_cache_promotes_only_changed_priming() {
        fn ev(value: serde_json::Value, priming: bool) -> Event {
            Event {
                timestamp: "2026-07-24T00:00:00+09:00".to_string(),
                node_id: 5,
                endpoint: 1,
                cluster: 0x0006,
                attribute: 0x0000,
                value,
                priming,
                recovered: false,
            }
        }
        let mut cache: HashMap<ValueKey, serde_json::Value> = HashMap::new();

        // 初見 priming: 昇格しない（matd 起動直後の全量で誤発火しないため）。
        let out = classify_against_cache(&mut cache, ev(json!(true), true));
        assert!(out.priming);
        assert!(!out.recovered);
        assert_eq!(cache[&(5, 1, 0x0006, 0x0000)], json!(true));

        // 同値 priming: 素通し（消費者は priming として無視する）。
        let out = classify_against_cache(&mut cache, ev(json!(true), true));
        assert!(out.priming);
        assert!(!out.recovered);

        // 差分 priming: 盲目期間中の実遷移 → 昇格 + キャッシュ更新。
        let out = classify_against_cache(&mut cache, ev(json!(false), true));
        assert!(!out.priming);
        assert!(out.recovered);
        assert_eq!(out.value, json!(false));
        assert_eq!(cache[&(5, 1, 0x0006, 0x0000)], json!(false));

        // 非 priming（live）: 素通し + キャッシュ更新。昇格フラグは立てない。
        let out = classify_against_cache(&mut cache, ev(json!(true), false));
        assert!(!out.priming);
        assert!(!out.recovered);
        assert_eq!(cache[&(5, 1, 0x0006, 0x0000)], json!(true));

        // キーは (node, endpoint, cluster, attribute) 単位で独立している。
        let other = Event {
            node_id: 6,
            ..ev(json!(false), true)
        };
        let out = classify_against_cache(&mut cache, other);
        assert!(out.priming, "別ノードの初見は昇格しない");
        assert_eq!(cache.len(), 2);
    }

    /// SubHealth 越しに同じキャッシュを読み書きできる（op 経路と pump の共有点）。
    #[test]
    fn sub_health_observe_updates_shared_value_cache() {
        let h = SubHealth::new(None);
        assert!(h.cached_value(5, 1, 0x0006, 0x0000).is_none());
        let ev = Event {
            timestamp: "2026-07-24T00:00:00+09:00".to_string(),
            node_id: 5,
            endpoint: 1,
            cluster: 0x0006,
            attribute: 0x0000,
            value: json!(true),
            priming: true,
            recovered: false,
        };
        let out = h.observe(ev);
        assert!(out.priming && !out.recovered, "初見は素通し");
        assert_eq!(h.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd classify_against_cache_promotes_only_changed_priming`
Expected: コンパイルエラー（`classify_against_cache` / `ValueKey` / `observe` / `cached_value` が無い）

- [ ] **Step 3: 最小実装**

`crates/matd/src/subscription.rs`。まず `Event` 定義の直後（`impl Event` の前後どちらでもよいが、ここでは `events_from_report` の直前）に純関数を置く:

```rust
/// 属性値キャッシュのキー: (node_id, endpoint, cluster, attribute)。
pub(crate) type ValueKey = (u64, u16, u32, u32);

/// priming イベントをキャッシュと突き合わせ、盲目期間中に起きた実遷移なら
/// 通常イベントへ昇格する（spec 2026-07-23 priming 差分回復）。
///
/// - 同値: 何も変えず素通し（消費者は priming を無視する）。
/// - 既知の値と異なる priming: `priming=false` + `recovered=true` へ昇格。
/// - 初見（キャッシュに無い）: 昇格**しない**（matd 起動直後の全量 priming で
///   誤発火させないため）。キャッシュには格納する。
/// - 非 priming: 素通し + キャッシュ更新。
pub(crate) fn classify_against_cache(
    cache: &mut HashMap<ValueKey, serde_json::Value>,
    ev: Event,
) -> Event {
    let key = (ev.node_id, ev.endpoint, ev.cluster, ev.attribute);
    if cache.get(&key).is_some_and(|prev| *prev == ev.value) {
        return ev;
    }
    let known = cache.contains_key(&key);
    cache.insert(key, ev.value.clone());
    if known && ev.priming {
        return Event {
            priming: false,
            recovered: true,
            ..ev
        };
    }
    ev
}
```

`SubHealth` にフィールドとメソッドを足す（doc コメント込み）:

```rust
pub struct SubHealth {
    /// 購読対象クラスタ集合（subscriptions.toml 由来。空 = full wildcard = 全対象）。
    clusters: Vec<u32>,
    /// node_id → 未消化の状態変更 op の時刻。
    pending: Mutex<HashMap<u64, tokio::time::Instant>>,
    /// 属性最終既知値。購読 pump（書き手: priming / live 全イベント）と
    /// server op 経路（読み手: 「この op は本当に値を変えるか」の証明）で共有する。
    /// ephemeral なプロセス内状態のみ（設計ルール4の永続状態には該当しない）。
    values: Mutex<HashMap<ValueKey, serde_json::Value>>,
}
```

`SubHealth::new` を更新:

```rust
    pub fn new(clusters: Option<Vec<u32>>) -> Self {
        Self {
            clusters: clusters.unwrap_or_default(),
            pending: Mutex::new(HashMap::new()),
            values: Mutex::new(HashMap::new()),
        }
    }
```

`impl SubHealth` の末尾へ 2 メソッド:

```rust
    /// pump が受けた 1 イベントをキャッシュへ反映し、差分 priming なら昇格して返す。
    /// listen クライアントの有無と無関係に呼ぶ（状態追跡は購読が生きている限り継続）。
    pub(crate) fn observe(&self, ev: Event) -> Event {
        let mut cache = self.values.lock().unwrap();
        classify_against_cache(&mut cache, ev)
    }

    /// 属性の最終既知値（未知なら None）。
    pub(crate) fn cached_value(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Option<serde_json::Value> {
        self.values
            .lock()
            .unwrap()
            .get(&(node_id, endpoint, cluster, attribute))
            .cloned()
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd && cargo clippy -p matd --all-targets -- -D warnings`
Expected: 全 PASS / warning なし（`observe` / `cached_value` は次タスクで使うが、テストが使っているので dead_code にはならない）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): SubHealth に属性最終値キャッシュと classify_against_cache を追加"
```

---

### Task 3: pump へ配線 — priming 差分回復を実際に効かせる

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`run_subscription_once` の 2 箇所 + 統合テスト）

**Interfaces:**
- Consumes: Task 2 の `SubHealth::observe` / `cached_value`。
- Produces: 「broadcast へ流れるイベントは必ず `observe` を通っている」という不変条件。以降のタスク（op 期待）はこれに依存する。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/subscription.rs` の `mod tests` 末尾に追加:

```rust
    /// 差分回復の統合: priming(true) → live(false) → 購読死 → 再 priming(true)。
    /// 2 回目の priming はキャッシュ(false)と異なるので昇格イベントとして届く。
    /// キャッシュが live イベントでも更新されること（spec テスト (c)）も同時に釘打ち。
    #[tokio::test(start_paused = true)]
    async fn priming_diff_after_resubscribe_is_promoted_to_recovered_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-24T00:00:00+09:00".into(),
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

        // 1 回目の priming（on-off=true）: 初見なので昇格しない。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming && !ev.recovered);
        assert_eq!(health.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));

        // live で false へ遷移 → キャッシュ更新（priming/live 両経路で更新される証明）。
        live.lock().unwrap().push_back(onoff_report(1, false));
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
            .await
            .expect("live event")
            .unwrap();
        assert!(!ev.priming && !ev.recovered);
        assert_eq!(health.cached_value(5, 1, 0x0006, 0x0000), Some(json!(false)));

        // 購読を殺して再購読させる（fake の priming は常に on-off=true）。
        health.note_op(5, 0x0006);
        let ev = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
            .await
            .expect("re-priming after resubscribe")
            .unwrap();
        // 盲目期間中に false→true の実遷移があったとみなし、通常イベントへ昇格。
        assert!(!ev.priming, "昇格イベントは priming=false");
        assert!(ev.recovered, "recovered=true で消費者の既存トリガが発火する");
        assert_eq!(ev.value, json!(true));
        assert_eq!(health.cached_value(5, 1, 0x0006, 0x0000), Some(json!(true)));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd priming_diff_after_resubscribe_is_promoted_to_recovered_event`
Expected: FAIL — 3 つ目のイベントが `priming=true` / `recovered=false` のまま（`assert!(!ev.priming, ...)` で落ちる）。加えて 1 つ目の `cached_value` アサートも None で落ちる。

- [ ] **Step 3: 最小実装**

`crates/matd/src/subscription.rs` の `run_subscription_once`、priming 配信ループ:

```rust
    // priming は現在状態の全量 — down 中の op はここで配信されるので pending 解除。
    health.clear_pending(node_id);
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            // 盲目期間中に起きた実遷移はここで通常イベントへ昇格する。
            let _ = events.send(health.observe(ev)); // 受信者ゼロは正常（listen 接続なし）
        }
    }
```

pump の live 受信側:

```rust
            Ok(Some(msg)) => {
                proven = true;
                last_msg = tokio::time::Instant::now();
                health.clear_pending(node_id);
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(health.observe(ev));
                }
                // keep-alive（reports 空）も受信 = 経路生存の証明として扱う。
            }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd`
Expected: 全 PASS（既存の `manager_emits_priming_events_from_fake_subscription` / `live_report_clears_pending_without_resubscribe` も PASS のまま）

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "feat(matd): priming 差分回復 — 盲目期間中の遷移を recovered イベントで回復"
```

---

### Task 4: op 期待の健全化（`server.rs` の分類を純関数化）

**Files:**
- Modify: `crates/matd/src/server.rs:447-462`（`op_report_expectation`）、`crates/matd/src/server.rs:354-366`（`run_op` の呼び出し）、`mod tests`

**Interfaces:**
- Consumes: Task 2 の `SubHealth::cached_value`。
- Produces:
  - `fn op_report_expectation(op: &Op, cached_on_off: Option<&Value>, cached_level: Option<&Value>) -> Option<(u64, u32)>`
  - `pub(crate) fn note_op_expectation(op: &Op, health: &SubHealth)` — Task 5 の統合テストが呼ぶ。

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/server.rs` の `mod tests` 末尾に追加:

```rust
    /// op → レポート期待の分類（spec 2026-07-24 の表）。
    /// 「op 成功」は「レポートが出る」を含意しない: 目標状態と現在値が一致する
    /// no-op はレポートを生まないので pending を打ってはならない。
    #[test]
    fn op_report_expectation_only_when_value_actually_changes() {
        let on = Op::On {
            node_id: 5,
            endpoint: 1,
        };
        let off = Op::Off {
            node_id: 5,
            endpoint: 1,
        };
        let level = Op::Level {
            node_id: 5,
            endpoint: 1,
            level: 128,
            percent: 50,
            transition: 0,
        };
        let t = json!(true);
        let f = json!(false);
        let l128 = json!(128);
        let l200 = json!(200);

        // On: 現在 off → 変化する → pending。
        assert_eq!(
            op_report_expectation(&on, Some(&f), None),
            Some((5, im::CLUSTER_ON_OFF))
        );
        // On: 既に on → no-op → 打たない。
        assert_eq!(op_report_expectation(&on, Some(&t), None), None);
        // Off: 現在 on → 変化する → pending。
        assert_eq!(
            op_report_expectation(&off, Some(&t), None),
            Some((5, im::CLUSTER_ON_OFF))
        );
        // Off: 既に off → no-op → 打たない（casa 人感ルールの誤キルの正体）。
        assert_eq!(op_report_expectation(&off, Some(&f), None), None);
        // Level: 現在値と異なる → pending / 同値 → 打たない。
        assert_eq!(
            op_report_expectation(&level, None, Some(&l200)),
            Some((5, im::CLUSTER_LEVEL_CONTROL))
        );
        assert_eq!(op_report_expectation(&level, None, Some(&l128)), None);

        // キャッシュ欠落: 証明できないので打たない（matd 起動直後・購読未確立）。
        assert_eq!(op_report_expectation(&on, None, None), None);
        assert_eq!(op_report_expectation(&off, None, None), None);
        assert_eq!(op_report_expectation(&level, None, None), None);
        // 型が想定外（level が null 等）でも打たない。
        assert_eq!(op_report_expectation(&level, None, Some(&json!(null))), None);

        // Color / ColorTemp / Write / Invoke は pending 対象から降格
        // （状態変化を証明できない。受け皿は無音 deadline）。
        let color_temp = Op::ColorTemp {
            node_id: 5,
            endpoint: 1,
            mireds: 370,
            kelvin: 2700,
            transition: 0,
        };
        assert_eq!(op_report_expectation(&color_temp, Some(&t), Some(&l128)), None);
        let invoke = Op::Invoke {
            node_id: 5,
            endpoint: 1,
            cluster: "onoff".into(),
            command: "toggle".into(),
            args: vec![],
        };
        assert_eq!(op_report_expectation(&invoke, Some(&t), Some(&l128)), None);
        let write = Op::Write {
            node_id: 5,
            endpoint: 1,
            cluster: "levelcontrol".into(),
            attribute: "on-level".into(),
            value: "128".into(),
        };
        assert_eq!(op_report_expectation(&write, Some(&t), Some(&l128)), None);
        // Read は元から対象外。
        let read = Op::Read {
            node_id: 5,
            endpoint: 1,
            cluster: "onoff".into(),
            attribute: "on-off".into(),
        };
        assert_eq!(op_report_expectation(&read, Some(&f), None), None);
    }
```

さらに既存テスト `run_op_success_marks_pending_op` を、キャッシュ前提に合わせて置き換える（`// off (onoff=0x0006) の success → pending。` のブロックを以下で差し替え、read の節はそのまま残す）:

```rust
        // キャッシュが空（購読未確立）なら、成功した off でも pending は打たない
        // — 「値が変わる」ことを証明できないため（spec 2026-07-24）。
        let body = run_op(
            &Op::Off {
                node_id: 5,
                endpoint: 1,
            },
            &state,
            dir.path(),
            &health,
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "success");
        assert!(health.pending_elapsed(5).is_none());

        // 購読キャッシュが on-off=true を知っている状態で off → 変化するので pending。
        health.observe(crate::subscription::Event {
            timestamp: "2026-07-24T00:00:00+09:00".to_string(),
            node_id: 5,
            endpoint: 1,
            cluster: 0x0006,
            attribute: 0x0000,
            value: json!(true),
            priming: true,
            recovered: false,
        });
        let body = run_op(
            &Op::Off {
                node_id: 5,
                endpoint: 1,
            },
            &state,
            dir.path(),
            &health,
        )
        .await
        .unwrap();
        assert_eq!(body["status"], "success");
        assert!(health.pending_elapsed(5).is_some());

        // 同じ off をもう一度: キャッシュはまだ true のままだが、
        // ここでは「値が一致していれば打たない」規則の確認として on を撃つ。
        health.clear_pending(5);
        let _ = run_op(
            &Op::On {
                node_id: 5,
                endpoint: 1,
            },
            &state,
            dir.path(),
            &health,
        )
        .await
        .unwrap();
        assert!(
            health.pending_elapsed(5).is_none(),
            "既に on のノードへの on は no-op — レポートは出ないので pending を打たない"
        );
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd op_report_expectation_only_when_value_actually_changes`
Expected: コンパイルエラー（`op_report_expectation` の引数が 1 つしかない）

- [ ] **Step 3: 最小実装**

`crates/matd/src/server.rs` の `op_report_expectation` を丸ごと差し替え、下に 2 関数を追加:

```rust
/// 状態変更 op → (node_id, 変化が現れる cluster)。op 相関の born-dead 検知
/// （`SubHealth::note_op`）の根拠。
///
/// **「op が成功した」は「レポートが出るはず」を含意しない**: すでに目標状態に
/// あるデバイスへの On/Off/Level は data model が変化せず、Matter 仕様上
/// 購読レポートは出ない（レポートは属性変化時のみ）。よって購読キャッシュの
/// 現在値と目標値が**不一致の時だけ**期待を返す（spec 2026-07-24）。
/// キャッシュ欠落（matd 起動直後・購読未確立）は「証明できない」ので None。
/// Color / ColorTemp / Write / Invoke は変化を証明できないため対象外
/// （受け皿は無音 deadline）。Read / Describe / Group 系も元から None。
fn op_report_expectation(
    op: &Op,
    cached_on_off: Option<&Value>,
    cached_level: Option<&Value>,
) -> Option<(u64, u32)> {
    match op {
        // 現在 off の時だけ on は変化を生む。
        Op::On { node_id, .. } => {
            (!cached_on_off?.as_bool()?).then_some((*node_id, im::CLUSTER_ON_OFF))
        }
        // 現在 on の時だけ off は変化を生む。
        Op::Off { node_id, .. } => {
            cached_on_off?.as_bool()?.then_some((*node_id, im::CLUSTER_ON_OFF))
        }
        // level は mat 側で換算済みの raw 0–254 が届く（protocol.rs の約束）。
        Op::Level { node_id, level, .. } => (cached_level?.as_u64()? != u64::from(*level))
            .then_some((*node_id, im::CLUSTER_LEVEL_CONTROL)),
        _ => None,
    }
}

/// 期待判定に使うキャッシュの参照先 (node_id, endpoint)。On/Off/Level のみ。
fn op_state_target(op: &Op) -> Option<(u64, u16)> {
    match op {
        Op::On { node_id, endpoint } | Op::Off { node_id, endpoint } => Some((*node_id, *endpoint)),
        Op::Level {
            node_id, endpoint, ..
        } => Some((*node_id, *endpoint)),
        _ => None,
    }
}

/// 成功した op に対し、レポート期待（pending）を打つべきなら打つ。
/// 購読の最終既知値を根拠にするので、no-op（すでに目標状態）では打たない。
pub(crate) fn note_op_expectation(op: &Op, health: &SubHealth) {
    let Some((node_id, endpoint)) = op_state_target(op) else {
        return;
    };
    let on_off = health.cached_value(node_id, endpoint, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
    let level = health.cached_value(
        node_id,
        endpoint,
        im::CLUSTER_LEVEL_CONTROL,
        im::ATTR_CURRENT_LEVEL,
    );
    if let Some((node_id, cluster)) = op_report_expectation(op, on_off.as_ref(), level.as_ref()) {
        health.note_op(node_id, cluster);
    }
}
```

`run_op` の呼び出し側（`crates/matd/src/server.rs:354-366`）を差し替え:

```rust
    if is_native_hotpath(op) {
        let result = native_op(op, native, store_path).await;
        if result.is_ok() {
            // 前提: デバイスは invoke 応答を先に、購読 report を後に送る。
            // report が note_op より先に pump へ届く逆順だと pending が残り
            // 健全購読を 1 回余分に再購読するが、それが最悪ケース（イベント
            // 自体は配信済みで priming が状態を再配達する）。
            note_op_expectation(op, health);
        }
        return result;
    }
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p matd && cargo clippy -p matd --all-targets -- -D warnings`
Expected: 全 PASS / warning なし

- [ ] **Step 5: コミット**

```bash
git add crates/matd/src/server.rs
git commit -m "fix(matd): op 相関検知の no-op 誤爆 — 値が変わると証明できる時だけ期待を打つ"
```

---

### Task 5: 統合テスト — 誤爆の釘打ちと真の born-dead 検知の維持

**Files:**
- Modify: `crates/matd/src/subscription.rs`（`mod tests` に 2 本追加）

**Interfaces:**
- Consumes: Task 4 の `crate::server::note_op_expectation`、Task 3 の pump 配線。

- [ ] **Step 1: テストを書く**

`crates/matd/src/subscription.rs` の `mod tests` 末尾に追加:

```rust
    /// 誤爆の釘打ち（spec テスト (a)）: priming でキャッシュが埋まった後、
    /// 同値の op（既に on のノードへの on）は pending を立てず、健全な購読を
    /// 無音 deadline 前に殺さない。
    #[tokio::test(start_paused = true)]
    async fn noop_op_does_not_kill_healthy_subscription() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-24T00:00:00+09:00".into(),
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
        // priming（on-off=true）でキャッシュが埋まる。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // 既に on のノードへ on = no-op。デバイスはレポートを出さないので
        // 期待を打ってはいけない。
        crate::server::note_op_expectation(
            &crate::protocol::Op::On {
                node_id: 5,
                endpoint: 1,
            },
            &health,
        );
        assert!(health.pending_elapsed(5).is_none(), "no-op で pending を打たない");

        // 無音 deadline (90s) 未満の 80s の間、再購読（= 追加イベント）は起きない。
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(80), rx.recv())
                .await
                .is_err(),
            "健全な購読を殺していないこと"
        );
    }

    /// 真の born-dead 検知の維持（spec テスト (b)）: 値が実際に変わる op
    /// （on のノードへの off）でデバイスが沈黙したままなら、従来どおり
    /// grace + backoff 内（<40s）に再購読する。
    #[tokio::test(start_paused = true)]
    async fn changing_op_with_silent_device_triggers_fast_resubscribe() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-24T00:00:00+09:00".into(),
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
        let ev = tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("first priming")
            .unwrap();
        assert!(ev.priming);

        // on のノードへ off = 値が変わる → レポートが出るはず → 期待を打つ。
        let t0 = tokio::time::Instant::now();
        crate::server::note_op_expectation(
            &crate::protocol::Op::Off {
                node_id: 5,
                endpoint: 1,
            },
            &health,
        );
        assert!(health.pending_elapsed(5).is_some());
        // デバイスは沈黙 → grace(10s) + backoff(5s) 内に再購読の priming が届く。
        let ev = tokio::time::timeout(std::time::Duration::from_secs(40), rx.recv())
            .await
            .expect("re-priming after op-grace")
            .unwrap();
        assert_eq!(ev.value, json!(true));
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= Duration::from_secs(10),
            "grace より早く殺さない: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(40),
            "無音 deadline (90s) を待っていないこと: {elapsed:?}"
        );
    }
```

注: 2 本目の再購読 priming は「キャッシュ(true) と同値」なので昇格せず `priming=true` のまま届く（Task 3 の挙動表どおり）。この不変も暗黙に釘打ちされる。

- [ ] **Step 2: テストを走らせる**

Run: `cargo test -p matd noop_op_does_not_kill_healthy_subscription changing_op_with_silent_device_triggers_fast_resubscribe`
Expected: 両方 PASS（Task 3・4 で実装済みのため。**もし落ちたら実装のバグ** — Task 4 の分類か Task 3 の配線を疑う）

- [ ] **Step 3: 全体テスト**

Run: `task check`
Expected: fmt:check / clippy(-D warnings) / test すべて PASS

- [ ] **Step 4: コミット**

```bash
git add crates/matd/src/subscription.rs
git commit -m "test(matd): no-op 誤爆の釘打ちと真の born-dead 検知の維持を統合テストで固定"
```

---

### Task 6: ドキュメントとバージョン

**Files:**
- Modify: `README.md`（listen セクション: 行 520 前後のイベント例と説明）
- Modify: `Cargo.toml`（workspace `version = "1.1.0"` → `"1.2.0"`）

- [ ] **Step 1: README の listen イベント説明を更新**

`README.md` の listen セクション、以下のブロック（サンプル JSON 行と `priming: true` の説明段落）を差し替える:

````markdown
  ```json
  {"timestamp":"...","listening":true}
  {"timestamp":"2026-07-20T21:00:00+09:00","node_id":21,"endpoint":1,"cluster":"occupancysensing","attribute":"occupancy","value":1,"priming":false,"recovered":false}
  ```
  `priming: true` marks events from the initial report burst right after
  matd (re)establishes a subscription, so a consumer does not mistake
  matd-restart residual state (e.g. `occupancy` still `1` from before a
  restart) for a fresh trigger. Only **scalar** values become events —
  `list`/`struct` attributes (ACL, server-list, etc., which show up in a
  wildcard priming burst) are dropped, the same known limitation as generic
  `read` (see [Scalar-only generic write / invoke](#scalar-only-generic-write--invoke)).
- `recovered: true` marks an event `matd` reconstructed from a priming
  report: the attribute's value in the new subscription's priming burst
  differs from the last value `matd` saw, so the transition happened while
  the subscription was down. Such an event is delivered with
  `priming: false` **and** `recovered: true`, so an existing consumer trigger
  fires on it without any change. Its `timestamp` is the **receive** time,
  not the time of the actual transition (which is unknowable — somewhere in
  the blind window). Values `matd` has never seen before (first priming after
  a `matd` restart) are **not** promoted: they are plain `priming: true`
  events, so a restart never fires a consumer's automation.
````

- [ ] **Step 2: バージョンを上げる**

`Cargo.toml`:

```toml
version = "1.2.0"
```

（`recovered` フィールドの追加は listen スキーマへの後方互換な追加 = minor。`Cargo.lock` はビルド時に自動更新される。）

- [ ] **Step 3: 検証**

Run: `task check`
Expected: 全 PASS

Run: `cargo build -p matd 2>&1 | tail -3 && git diff --stat Cargo.lock`
Expected: `Cargo.lock` の version が 1.2.0 に更新されている

- [ ] **Step 4: コミット**

```bash
git add README.md Cargo.toml Cargo.lock
git commit -m "docs: listen イベントの recovered を README に追記（1.2.0）"
```

---

### Task 7: 実機 E2E（jarvis、マージ前必須）

**Files:** なし（デプロイと観測のみ）

このタスクは**ユーザーの実機（jarvis）操作**を伴う。`superpowers:despliegue` / `jarvis` スキルの手順に従い、**本番バイナリは置換せず `*.new` の隔離 matd** で検証する（CLAUDE.md / メモリの「マージ前に必ず実機 E2E」）。

- [ ] **Step 1: arm64 ビルドと転送**

```bash
task dist:arm64
scp dist/arm64/matd jarvis:~/matd.new
```

- [ ] **Step 2: 隔離 matd で検証（本番 matd とは別ソケット）**

**重要（既知の落とし穴）**: 同一ノードへ本番 matd と隔離 matd が同時に購読すると
互いに購読を追い出し合う（`KeepSubscriptions = false`）。購読の死活を測る検証なので、
検証中は本番 matd を止めること。

```bash
# jarvis 上（ユーザー操作）
systemctl --user stop matd
MAT_MATD_FABRIC_INDEX=2 MAT_LOG=info ~/matd.new --socket /tmp/matd-new.sock 2>&1 | tee ~/matd-new.log
# 別セッションから: mat 側は --matd <SOCK> で隔離ソケットを指す
mat --matd /tmp/matd-new.sock listen --timeout-ms 0
mat --matd /tmp/matd-new.sock off --node <消灯済みノード> --endpoint 1
```

検証後は `systemctl --user start matd` で本番を戻す（本番置換はマージ後）。

確認項目（spec「受け入れ」）:

1. **消灯済みノードへ off** → journal に `report pump ended (op-correlated: ...)` が**出ない**こと。
   （0.28.0 では op から 10〜14s で必ず出ていた。）
2. **点灯中ノードへ off** → レポート到達で pending 解除（キルなし）、`mat listen` にイベントが流れること。
3. **回帰**: 通常の `on` / `off` / `read` / `describe` が従来どおり成功すること。
4. **差分回復**: 書斎人感（node 16）で「在室 → アイドル 5.5 分+ で購読喪失 → 退室 → 再購読」の順に操作し、`recovered: true` の `occupancy=0` イベントが `mat listen` に流れること。

- [ ] **Step 3: 結果を記録**

観測ログ（該当 journal 行）をこのタスクの完了報告に貼る。誤キルが 1 件でも出たら **マージしない** — 分類のバグとして Task 4 に戻る。

---

## 完了後

`superpowers:finishing-a-development-branch` に従い、main へのマージ方式をユーザーに確認する。マージ後に jarvis の本番 matd を 1.2.0 へ置換（`despliegue` スキル）。

**別リポジトリの残作業（本計画のスコープ外・完了報告で申し送る）:**

- casa: CLAUDE.md / README の「priming は発火しない」記述へ「matd 側の差分回復で
  失われた遷移は `recovered` イベントとして届く」旨を 1 行補足（priming spec）。
- casa: 「off 限定・状態キャッシュ鮮度条件付きの no-op スキップ」issue を起票
  （無線トラフィック削減が目的。matd 側の本修正で誤キル自体は消えるため必須では
  ない。**on 側スキップは NL68 の物理消灯固着があるため禁止**と issue に明記）。
