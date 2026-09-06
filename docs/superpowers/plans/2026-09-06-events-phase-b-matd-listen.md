# イベント購読 フェーズ B（matd 常駐購読 + `mat listen`）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **着手条件（満たすまで実装しない）:**
> 1. フェーズ A（`docs/superpowers/plans/2026-09-06-events-phase-a-controller-device.md`）が main にマージ済み。
> 2. 並行セッション S2 の **matd reload op** が main にマージ済み（`crates/matd` と `mat-native::runner` の並行編集を避ける）。**→ 2026-09-06 に満了（main d1727fb「Merge: matd reload」）。**
> 3. 着手時に main を pull し、本計画の「前提コード確認」（Task 0）で参照している関数名・行がまだ合っているかを確認してから始める。ずれていれば計画を先に直す。

**Goal:** `matd` の常駐 Subscribe に EventRequests を載せ、デバイス発の EventReport を `mat listen` の JSON 行として流す。盲目窓中のイベントは EventFilters（EventMin）で欠落なく回収する。

**Architecture:** `mat-controller` のフェーズ A API（`SecureSession::subscribe` / `next_subscription_report_full`）へ matd の購読ポンプを乗せ換え、broadcast に流す型を「属性イベント | Matter イベント」の enum にする。`ListenFilter` に `event` を足し、`mat listen --event` を wire に通す。イベント名は `mat-core::ids` に `events` テーブルを生成して引く。matd の状態はプロセスメモリの `last_event_number` のみ（設計ルール 4）。

**Tech Stack:** Rust workspace、tokio broadcast、`scripts/gen-ids.py`（connectedhomeip v1.4.2.0 の data-model XML）。

**Spec:** `docs/superpowers/specs/2026-09-06-events-subscribe-design.md` §6（フェーズ B の設計）と §2.4（クラスタ定義）。

## Global Constraints

- 属性イベント行（`attribute` キーを持つ JSON）の形は**無改変**。イベント行は `event` キーを持つ（spec §6.1 の 3 例が正）。
- `mat listen` の既定（filter 無し）は属性行とイベント行の**両方**を流す。`--attribute` 指定で属性行のみ、`--event` 指定でイベント行のみ。両方指定は `parse_error`（exit 2）。
- matd の EventRequests は `subscriptions.toml` の `events` で制御: 無指定 = wildcard（全クラスタ、urgent）、`events = ["switch", "booleanstate"]` = そのクラスタの wildcard event path、`events = []` = EventRequests 無し（フェーズ A 以前と byte-equal なワイヤ）。属性の `clusters` と独立。
- 全 event path は `IsUrgent = true`（spec §6.3）。
- matd は `last_event_number: Option<u64>` をノードごとにプロセスメモリだけで持つ。再購読の `event_min = last + 1`。matd 起動直後は EventFilters 無し。
- イベント行の `timestamp` は受信時刻（同一 ReportData の属性行と同じ値）。`recovered` は付けない。
- `mat-core::ids` のイベント名は `gen-ids.py` から生成（手書き禁止、既存の attrs/cmds と同じ規律）。生成元は connectedhomeip **v1.4.2.0**。
- `task check` 全通過。コミット末尾の Co-Authored-By / Claude-Session 行は当該セッションのものを付ける。

## File Structure

| ファイル | 役割 |
|---|---|
| `scripts/gen-ids.py` | `<cluster>` の `<event>` を読み `events: &[EventDef]`（name kebab, id, priority, fields）を出力 |
| `crates/mat-core/src/ids_gen.rs` | 再生成 |
| `crates/mat-core/src/ids.rs` | `EventDef`, `ClusterDef::events`, `resolve_event(cluster, input) -> Option<EventRef>`, `find_event(cluster, id)` |
| `crates/matd/src/subscribe_config.rs` | `events` キー（`Option<Vec<u32>>`、`Some(vec![])` = 無効） |
| `crates/matd/src/subscription.rs` | `Emitted { Attribute(Event), Event(EventItem) }`、`EventItem::to_json`、`events_from_report_full`、`last_event_number` 管理、`subscribe(spec)` への乗り換え |
| `crates/matd/src/native.rs`（または `mat-native::runner` の conn 型） | `SubscriptionConn::subscribe(&SubscribeSpec)` / `next_report_full` |
| `crates/matd/src/server.rs` | `ListenFilter::event`、`matches(&Emitted)`、ストリームの JSON 化 |
| `crates/matd/src/protocol.rs` | `Op::Listen { event: Option<String> }` |
| `crates/mat/src/cli.rs`, `resolve.rs`, `matd_client.rs` | `--event` フラグ、排他チェック、wire |
| `scripts/e2e-device-m5-events.sh` | matv（switch + contact-sensor、`--stdin-control`）相手の e2e |
| `docs/commands.md`, `docs/configuration.md`, `ARCHITECTURE.md`, `README.md` | イベント行の契約、`events` 設定、記録 |

---

### Task 0: 前提コード確認（着手時、コード変更なし）

- [ ] `crates/matd/src/subscription.rs` の `run_subscription_once` が `conn.subscribe_wildcard(clusters)` / `conn.next_report(slice)` を呼んでいること（2026-09-06 時点: 834 行・925 行付近）。`conn` の型と定義場所（`crates/matd/src/native.rs::establish_subscription` の戻り値。`mat-native::runner` 側にあれば S2 マージ後の形を確認）。
- [ ] `crates/matd/src/server.rs::ListenFilter { node_id, endpoint, cluster, attribute }` と `matches(&Event)`（337–400 行付近）。
- [ ] `crates/matd/src/protocol.rs::Op::Listen { node_id, endpoint, cluster, attribute }`（174 行付近）。
- [ ] `crates/mat/src/matd_client.rs::listen_request_json` / `dispatch_listen`、`crates/mat/src/cli.rs::Command::Listen`（342 行付近）。
- [ ] `mat_controller::session::{SubscribeOutcome, SubscriptionReport}`、`im::{SubscribeSpec, EventPathIn, EventReport, EventData, EventPriority}` がフェーズ A のとおり存在すること。
- [ ] ずれがあれば本計画の該当 Task を直してからコミットし、Task 1 へ。

---

### Task 1: `mat-core::ids` — イベント名テーブルの生成

**Files:**
- Modify: `scripts/gen-ids.py`, `crates/mat-core/src/ids.rs`
- Regenerate: `crates/mat-core/src/ids_gen.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct EventDef { pub name: &'static str, pub id: u32, pub priority: &'static str /* "debug"|"info"|"critical" */, pub fields: &'static [FieldDef] }
  pub struct ClusterDef { /* 既存 */ pub events: &'static [EventDef] }
  pub struct EventRef { pub id: u32, pub def: Option<&'static EventDef> }
  pub fn resolve_event(cluster: u32, input: &str) -> Option<EventRef>;   // 名前 or 数値
  pub fn find_event(cluster: u32, id: u32) -> Option<&'static EventDef>;
  ```
  `FieldDef` の TLV context tag は既存規約どおり配列添字（`fields[i]` = tag i）。data-model XML の `<event><field id=..>` は id 明示なので、添字と id が一致しない場合は `FieldDef` ではなく `EventFieldDef { name, id, ty, optional }` を新設して id を持つ（生成器で判定し、一致しない cluster が 1 つでもあれば新型を使う）。

- [ ] **Step 1: 失敗するテスト（`ids.rs` の spot-check tests に追加）**

```rust
    #[test]
    fn switch_and_boolean_state_events_resolve() {
        let sw = resolve_cluster("switch").unwrap();
        assert_eq!(resolve_event(sw, "initial-press").unwrap().id, 0x01);
        assert_eq!(resolve_event(sw, "multi-press-complete").unwrap().id, 0x06);
        assert_eq!(resolve_event(sw, "0x03").unwrap().id, 0x03);
        let def = find_event(sw, 0x06).unwrap();
        assert_eq!(def.name, "multi-press-complete");
        assert_eq!(def.priority, "info");
        assert_eq!(def.fields.iter().map(|f| f.name).collect::<Vec<_>>(), vec!["previous-position", "total-number-of-presses-counted"]);
        let bs = resolve_cluster("booleanstate").unwrap();
        assert_eq!(resolve_event(bs, "state-change").unwrap().id, 0x00);
        assert_eq!(find_event(bs, 0).unwrap().fields[0].name, "state-value");
        assert!(resolve_event(bs, "nosuch").is_none());
    }
```

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-core ids` → コンパイルエラー。

- [ ] **Step 3: 実装**

`gen-ids.py`: cluster ループに `for ev in c.iter("event")` を足し、`name`（kebab）、`code`、`priority`（属性 `priority`、無ければ `"info"`）、`<field name= type= id= optional=>` を収集。`emit` で `static EVENTS_<KEY>: &[EventDef] = &[..]` と `ClusterDef { .., events: EVENTS_<KEY> }` を出す。docstring の使い方（sparse checkout v1.4.2.0）に従って再生成:

```bash
git clone --depth 1 --branch v1.4.2.0 --filter=blob:none --sparse https://github.com/project-chip/connectedhomeip.git /tmp/chip
git -C /tmp/chip sparse-checkout set src/app/zap-templates/zcl/data-model/chip
python3 scripts/gen-ids.py /tmp/chip > crates/mat-core/src/ids_gen.rs
```

`ids.rs`: 型と 2 関数（`resolve_attribute` / `find_cluster` と同じ書き方）。

- [ ] **Step 4: テスト通過** — `cargo test -p mat-core`、`cargo test --workspace`（ids_gen の差分で既存 spot-check が壊れていないこと。属性/コマンドの並びが変わらないよう `sorted` を維持）。git diff で `ids_gen.rs` の差分が **events 追加のみ**であることを目視（`/usr/bin/git diff --stat` と `grep -c 'EVENTS_'`）。

- [ ] **Step 5: Commit** — `feat(mat-core): ids にイベント名テーブル（gen-ids.py の events 出力、resolve_event / find_event）`

---

### Task 2: `subscriptions.toml` の `events` キー

**Files:**
- Modify: `crates/matd/src/subscribe_config.rs`, `docs/configuration.md`

**Interfaces:**
- Produces: `pub struct SubscribeConfig { pub clusters: Option<Vec<u32>>, pub events: EventScope }`, `pub enum EventScope { Wildcard, Clusters(Vec<u32>), Off }`、`pub fn load(store_root) -> Result<Option<SubscribeConfig>, MatError>`。既存の `Option<Vec<u32>>` を返す呼び出し側（`matd/src/main.rs` / `lib.rs` の `SubHealth::new(clusters)` 周辺）は `cfg.clusters` に読み替える。`events` が無い = `Wildcard`、`[]` = `Off`。

- [ ] Step 1: tests: `events = ["switch", "0x45"]` → `Clusters([0x3B, 0x45])`、`events = []` → `Off`、キー無し → `Wildcard`、未知名 → エラー（既存 clusters と同じ文言）。`clusters` 未指定でも `events` だけの config を受理する（`clusters` 空エラーの条件を「キー有りかつ空」に限定）。
- [ ] Step 2: 失敗確認。
- [ ] Step 3: 実装（`EventScope::to_paths(&self) -> Vec<EventPathIn>`: Wildcard → `[WILDCARD_URGENT]`、Clusters → 各 `EventPathIn { cluster: Some(c), urgent: true, ..}`、Off → `[]`）。
- [ ] Step 4: `cargo test -p matd`。`docs/configuration.md` の Subscriptions 節に `events` の 3 形を追記。
- [ ] Step 5: Commit — `feat(matd): subscriptions.toml の events（wildcard / クラスタ絞り込み / 無効）`

---

### Task 3: matd 購読ポンプのイベント対応

**Files:**
- Modify: `crates/matd/src/subscription.rs`, `crates/matd/src/native.rs`（conn 型の場所に応じて）

**Interfaces:**
- Consumes: `SecureSession::subscribe(&SubscribeSpec, cfg) -> SubscribeOutcome`、`next_subscription_report_full -> SubscriptionReport`、`EventScope::to_paths`。
- Produces:
  ```rust
  pub enum Emitted { Attribute(Event), Event(EventItem) }
  pub struct EventItem { pub timestamp: String, pub node_id: u64, pub endpoint: u16, pub cluster: u32, pub event: u32,
                         pub event_number: u64, pub priority: EventPriority, pub data: Option<serde_json::Value>,
                         pub device_time: Option<EventTimestamp>, pub priming: bool }
  impl EventItem { pub fn to_json(&self) -> serde_json::Value; }   // spec §6.1 の形。data のキーは find_event の fields 名、無ければ数値文字列
  pub fn events_from_event_reports(node_id: u64, events: &[EventReport], priming: bool, ts: &str) -> Vec<EventItem>;  // Status は debug ログで捨てる
  impl SubHealth { pub fn last_event_number(&self, node_id) -> Option<u64>; pub fn note_event_number(&self, node_id, n: u64); }
  ```
  broadcast の型は `broadcast::Sender<Emitted>`。`run_subscription_once` は `SubscribeSpec { clusters, event_paths: scope.to_paths(), event_min: health.last_event_number(node).map(|n| n + 1), .. }` で `conn.subscribe(&spec)` を呼び、priming events は「`event_min` 有り（= 前回番号を知っている）なら `priming: false`、無しなら `priming: true`」で流す（spec §6.2）。ポンプは `next_report_full` を受け、`data` は従来どおり `events_from_report` → `Emitted::Attribute`、`events` は `Emitted::Event`。受けた最大 EventNumber を `note_event_number`。
- `mat-native::test_support::{onoff_report, FakeEstablisher}` を使う既存テストは `Emitted::Attribute` を剥がして通す（`FakeEstablisher` がフェーズ A の新 API を返すよう `mat-native/src/test_support.rs` に `subscribe`/`next_report_full` の fake を足す — S2 マージ後の形に合わせる）。

- [ ] Step 1: tests — `events_from_event_reports` の JSON 形（3 例）、`priming` フラグ規則（`event_min` 有/無）、`note_event_number` が最大値を保持、`Status` が捨てられる、既存 `events_from_report` テスト全通過。
- [ ] Step 2: 失敗確認。
- [ ] Step 3: 実装。
- [ ] Step 4: `cargo test -p matd`、`cargo test -p mat-native`。
- [ ] Step 5: Commit — `feat(matd): 常駐購読に EventRequests を載せ EventReport を Emitted::Event で配信、EventMin で盲目窓を回収`

---

### Task 4: `mat listen --event` と ListenFilter

**Files:**
- Modify: `crates/matd/src/protocol.rs`, `crates/matd/src/server.rs`, `crates/mat/src/cli.rs`, `crates/mat/src/resolve.rs`, `crates/mat/src/matd_client.rs`, `crates/mat/tests/listen.rs`, `crates/matd/tests/integration.rs`

**Interfaces:**
- `Op::Listen { .., event: Option<String> }`（wire キー `"event"`）。
- `ListenFilter { .., attribute: Option<u32>, event: Option<u32> }`; `from_op` は attribute と event の両指定を `parse_error`。`matches(&Emitted)`: `Attribute` 行は `event.is_none()` かつ既存条件; `Event` 行は `attribute.is_none()` かつ node/endpoint/cluster 一致かつ `event` 一致（None = wildcard）。
- `mat listen --event <name>`（`--attribute` と `conflicts_with`）。`listen_request_json` に `event`。

- [ ] Step 1: tests — protocol parse（`event` 省略/指定）、`ListenFilter::matches` の 2×2（attribute 指定 × 行種別）、両指定拒否、`listen_request_json` に `event` が載る、`mat listen --attribute x --event y` が exit 2。matd `tests/integration.rs` の listen テストに「イベント行が流れる」ケース（fake backend で `Emitted::Event` を流す）。
- [ ] Step 2〜5: 失敗確認 → 実装 → `cargo test -p matd -p mat` → Commit `feat(listen): --event フィルタとイベント行のストリーム配信`。

---

### Task 5: matv 相手の e2e と実機

**Files:**
- Create: `scripts/e2e-device-m5-events.sh`（`scripts/e2e-device-m4.sh` を雛形に）
- Modify: `docs/development.md`（e2e 一覧）

- [ ] Step 1: スクリプト — matv（`switch` + `contact-sensor`、`--stdin-control`、stdin は名前付きパイプ）→ `mat fabric init` → `mat commission` → `matd` 起動（`subscriptions.toml` に `events = ["switch", "booleanstate"]`）→ `mat listen --event initial-press --count 1 --timeout-ms 20000 &` → パイプへ `{"device":"btn","press":"short"}` → listen の出力 JSON に `"event":"initial-press"` と `"data":{"new-position":1}` があること → `mat listen --cluster booleanstate --count 2`（属性行 + イベント行の両方）→ matd 停止・再起動の間に `{"device":"door","state":false}` を注入 → 再起動後の `mat listen --event state-change --count 1` に `priming:false` で届く（EventMin 回収）。
- [ ] Step 2: `task check` + スクリプト実走（ローカル、`iface` は WSL の実 IF）。
- [ ] Step 3: 実機（任意）: Aqara の開閉/ボタンがあれば jarvis 経由で commission して `mat listen --event` を確認。無ければ「実機未実施」と ARCHITECTURE に明記。
- [ ] Step 4: Commit — `test(e2e): m5 イベント購読（matv --stdin-control → matd → mat listen --event）`

---

### Task 6: ドキュメントと本番ロールアウト記録

- [ ] `docs/commands.md` Listen 節: イベント行の契約（spec §6.1 の 3 例、`event` キーで判別、`--event`、`recovered` 無し、`priming` 規則）。既存消費者への注意（casa は `attribute` キー有無で分岐）。
- [ ] `README.md`: `mat listen` の例にイベント行を 1 つ。
- [ ] `ARCHITECTURE.md`: 「Phase 5 拡張 — イベント購読 フェーズ B」節（priming 所要時間の実測、canary の手順: `events = ["switch","booleanstate"]` → 24h 観察 → wildcard へ、戻しは `events = []`）。
- [ ] リリース: minor bump（ユーザー規律どおり major は打たない）、`task semver` で破壊点を棚卸し（`ReportDataMessage` 無改変なので外部破壊は無い見込み）。
- [ ] 本番デプロイ（despliegue skill）は canary 設定付きで。デプロイ後 `matd status` で established 19/19 と priming 所要時間を記録。

---

## Self-Review

- Spec §6.1 → Task 3（JSON 形）+ Task 4（フィルタ）+ Task 6（docs）。§6.2 → Task 2（設定）+ Task 3（EventMin / priming 規則 / canary は Task 6）。§6.3 → Task 2 の `to_paths` が常に urgent。§2.4 のイベント名 → Task 1。
- 型名: `Emitted` / `EventItem` / `EventScope` / `EventDef` / `resolve_event` / `find_event` は Task 間で統一。
- S2 依存: Task 3 の conn 型と `FakeEstablisher` は S2 マージ後の形に合わせる（Task 0 で確認）。
