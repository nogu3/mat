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
- `mat listen` の既定（filter 無し）は属性行とイベント行の**両方**を流す。`--attribute <name>` 指定で属性行のみ、`--event` 指定でイベント行のみ。`--event` は値が**任意**（`--event` 単独 = 全イベント名、`--event <name>` = そのイベントだけ。ワイヤでは単独指定を `"event": "*"` で運ぶ）。両方指定は `parse_error`（exit 2）。旧 `mat` クライアント（`event` キー無し）は従来どおり接続でき、両方の行が流れる（属性行の形は無改変）。
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
| `crates/mat-native/src/lib.rs` | `SubscribeConn` trait（購読専用 conn、`subscribe_wildcard` / `next_report` の隣）に `subscribe(clusters, event_paths, event_min)` / `next_report_full` を足し、`SubscriptionSession` で `SecureSession::subscribe` / `next_subscription_report_full` に配線 |
| `crates/mat-native/src/test_support.rs` | `FakeSubConn` / `FakeEstablisher` に新 2 メソッドの fake（`priming_events` / `live_events` 注入口） |
| `crates/matd/src/server.rs` | `ListenFilter::event`、`matches(&Emitted)`、ストリームの JSON 化 |
| `crates/matd/src/protocol.rs` | `Op::Listen { event: Option<String> }` |
| `crates/mat/src/cli.rs`, `resolve.rs`, `matd_client.rs` | `--event` フラグ、排他チェック、wire |
| `scripts/e2e-device-m3.sh` | 既存 matd×matv e2e に events 脚を追加（matv に `switch` + `contact-sensor` を足し `--stdin-control`、`mat listen --event`） |
| `docs/commands.md`, `docs/configuration.md`, `ARCHITECTURE.md`, `README.md` | イベント行の契約、`events` 設定、記録（リリース / 本番デプロイは本計画の外 = 別セッション、version bump しない） |

---

### Task 0: 前提コード確認（2026-09-06 実施済み、base = main f93845d = v1.35.0）

- [x] `crates/matd/src/subscription.rs::run_subscription_once`（824 行）は `conn.subscribe_wildcard(clusters)`（834 行）/ `conn.next_report(slice)`（927 行）を呼ぶ。`conn` は `Box<dyn mat_native::SubscribeConn>`（trait は `crates/mat-native/src/lib.rs` 197 行付近、実装 `SubscriptionSession` は同 722 行付近、fake は `crates/mat-native/src/test_support.rs::FakeSubConn`）。`crates/matd/src/native.rs::establish_subscription`（357 行）は `Establisher::establish_subscription` へ委譲するだけ。**→ 計画の「matd/src/native.rs（または runner）」は誤り、Task 3 を `mat-native/src/lib.rs` + `test_support.rs` に訂正済み。**
- [x] `crates/matd/src/server.rs::ListenFilter { node_id, endpoint, cluster, attribute }`（370 行）と `from_op` / `matches(&Event)`（378 / 424 行）。
- [x] `crates/matd/src/protocol.rs::Op::Listen`（174 行、全フィールド `#[serde(default)]`、`deny_unknown_fields` 無し = 新キー追加で旧クライアント無退行）。
- [x] `crates/mat/src/matd_client.rs::listen_request_json`（635 行）/ `dispatch_listen`（661 行）、`crates/mat/src/cli.rs::Command::Listen`（342 行、`attribute: Option<String>`）。
- [x] `mat_controller::session::{SubscribeOutcome { response, priming, priming_events }, SubscriptionReport { data, events }}`（`session/subscribe.rs`）、`SecureSession::subscribe(&SubscribeSpec, &MrpConfig)` / `next_subscription_report_full`、`im::{SubscribeSpec { clusters: Vec<u32>, event_paths, event_min, .. }, EventPathIn::WILDCARD_URGENT, EventReport::{Data, Status}, EventData, EventPriority::as_str, EventTimestamp}` はフェーズ A のとおり存在。
- [x] `crates/matd/src/subscribe_config.rs::load(store_root) -> Result<Option<Vec<u32>>, MatError>`、呼び出しは `crates/matd/src/main.rs` 254 行のみ。
- [x] matv は 1 CASE セッションしか同時に持てない（`scripts/e2e-device-m3.sh` 冒頭コメント）。直経路 op は matd の購読セッションを追い出す — Task 5 の EventMin 回収脚はこの性質を使う（matd 再起動ではない: 再起動は `last = None` → 全量 `priming: true` が spec §6.2 の正しい挙動）。
- [x] ユーザー指示（2026-09-06）: e2e は新規 m5 ではなく **`scripts/e2e-device-m3.sh` にステップ追加**、リリース / hogar デプロイ / version bump は別セッション。Task 5 / 6 を訂正済み。

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
- Modify: `crates/mat-native/src/lib.rs`（`SubscribeConn` trait + `SubscriptionSession`）, `crates/mat-native/src/test_support.rs`（`FakeSubConn` / `FakeEstablisher`）, `crates/matd/src/subscription.rs`, `crates/matd/src/main.rs`（`SubscribeConfig` の受け渡し）

**Interfaces:**
- Consumes: `SecureSession::subscribe(&SubscribeSpec, cfg) -> SubscribeOutcome`、`next_subscription_report_full -> SubscriptionReport`、`EventScope::to_paths`。
- Produces（`mat-native`）: `SubscribeConn` trait に追加
  ```rust
  async fn subscribe(&mut self, clusters: &[u32], event_paths: &[EventPathIn], event_min: Option<u64>)
      -> Result<(SubscriptionInfo, Vec<ReportDataMessage>, Vec<EventReport>), MatError>;   // (info, priming 属性チャンク, priming events)
  async fn next_report_full(&mut self, timeout: Duration) -> Result<Option<SubscriptionReport>, MatError>;
  ```
  `SubscriptionSession` は min/max/keep の既存定数で `SubscribeSpec` を組んで `SecureSession::subscribe` を呼ぶ。`event_paths` 空かつ `event_min` None のときワイヤは `subscribe_wildcard` と byte-equal（フェーズ A の釘打ちに乗る）。既存 `subscribe_wildcard` / `next_report` は無改変で残す（他の呼び出し元があれば）か、matd が使わなくなるなら `subscribe` の薄いラッパにする。`FakeSubConn` は `priming_events: Vec<EventReport>` と `live_events: Arc<Mutex<VecDeque<Vec<EventReport>>>>`（`live` の各 report に相乗りさせる形でもよい）、`seen_event_paths` / `seen_event_min` の記録先を持つ。
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
- 属性なし（AttributeRequests 省略）の SubscribeRequest は**選択肢**として使える（server 側は
  フェーズ A で既に受理する）。必要なら `SubscribeSpec.clusters: Option<Vec<u32>>` を足して
  「イベントだけの購読」を表現する — 必須ではない（matd は常に属性も購読する既定でよい。
  spec §1「フェーズ A に入らない」/ §2.1）。
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
- `Op::Listen { .., event: Option<String> }`（wire キー `"event"`、`#[serde(default)]`。`"*"` = イベント行のみ・全イベント名）。
- `ListenFilter { .., attribute: Option<u32>, event: Option<Option<u32>> }`（`event: Some(None)` = `"*"`、`Some(Some(id))` = 名前/数値を `resolve_event`（`--cluster` 必須、数値なら不要 — attribute と同じ規則）で解決）; `from_op` は attribute と event の両指定を `parse_error`。`matches(&Emitted)`: `Attribute` 行は `event.is_none()` かつ既存条件; `Event` 行は `attribute.is_none()` かつ node/endpoint/cluster 一致かつ `event` が `None` / `Some(None)` / `Some(Some(id == ev.event))` のいずれか。
- `mat listen --event [<name>]`（clap `num_args(0..=1)`、`default_missing_value = "*"`、`--attribute` と `conflicts_with`）。`listen_request_json` に `event`（未指定なら省略、旧 matd 互換）。

- [ ] Step 1: tests — protocol parse（`event` 省略/指定）、`ListenFilter::matches` の 2×2（attribute 指定 × 行種別）、両指定拒否、`listen_request_json` に `event` が載る、`mat listen --attribute x --event y` が exit 2。matd `tests/integration.rs` の listen テストに「イベント行が流れる」ケース（fake backend で `Emitted::Event` を流す）。
- [ ] Step 2〜5: 失敗確認 → 実装 → `cargo test -p matd -p mat` → Commit `feat(listen): --event フィルタとイベント行のストリーム配信`。

---

### Task 5: e2e-device-m3 に events 脚を追加

**Files:**
- Modify: `scripts/e2e-device-m3.sh`（ヘッダの Flow コメントも更新）, `docs/development.md`（e2e 一覧があれば）

**設計（matv は同時 1 CASE セッション — ヘッダコメント参照）:**
- matv の `[[device]]` に `id = "btn", kind = "switch"` と `id = "door", kind = "contact-sensor"` を足し、`--stdin-control` を付けて起動。stdin は名前付きパイプ（`mkfifo`、書き手側 fd をスクリプトが `exec 3>fifo` で開き続けて EOF を防ぐ）。エンドポイントは matv の JSON 出力 / `mat describe` から取る（EP2 = light、EP3 = btn、EP4 = door の想定だが決め打ちせず assert で確認）。
- matd の store に `subscriptions.toml` を `events = ["switch", "booleanstate"]` **のみ**（`clusters` 無し = 属性は full wildcard のまま。Task 2 の「events だけの config を受理」の実走確認を兼ねる）で置く。既存の onoff listen 脚は無改変で通ること。
- 脚 A（必須、ユーザー指示）: `mat listen --cluster switch --event --count 2 --timeout-ms T &` → fifo に `{"device":"btn","press":"short"}` → 出力 2 行に `"event":"initial-press"`（`"data":{"new-position":1}`）と `"event":"short-release"`（`"data":{"previous-position":1}`）が **EventNumber 昇順**で並び、`"priming":false`、`"attribute"` キー無し。
- 脚 B: `mat listen --cluster booleanstate --count 2 &` → `{"device":"door","state":true}` → 属性行（`"attribute":"state-value"`, `"value":true`）とイベント行（`"event":"state-change"`, `"data":{"state-value":true}`）の両方が届く（順不同で可）。
- 脚 C（EventMin 回収）: `MAT_MATD=0`（`MAT_MATD_SOCKET` **無し** = node_touched ヒントを送らない）で直経路 `mat on` を 1 本撃ち matv の唯一セッションを奪う → 即 `{"device":"door","state":false}`（購読が死んでいる間のイベント）→ `mat listen --cluster booleanstate --event state-change --count 1 &` → `MAT_MATD_SOCKET` 付きの直経路 op（既存脚と同じ）でヒントを送り matd を再購読させる → listen の出力が `"data":{"state-value":false}` かつ **`"priming":false`**（`event_min = last+1` で回収された = 盲目窓の実イベント）。matd stderr に EventFilters 付き再購読のログ（Task 3 で `event_min` を info ログに出す）があることも grep。
- 既存の「matd 再起動」相当は足さない（再起動後は `last = None` → 全量 `priming: true` が仕様）。

- [ ] Step 1: スクリプト編集（既存脚の assert は 1 文字も変えない）。
- [ ] Step 2: `task check` + `MAT_E2E_IFACE=<WSL の実 IF> bash scripts/e2e-device-m3.sh` 実走 → 終了コード 0。
- [ ] Step 3: Commit — `test(e2e): m3 に events 脚（matv --stdin-control → matd → mat listen --event、EventMin 回収）`

---

### Task 6: ドキュメント

- [ ] `docs/commands.md` Listen 節: イベント行の契約（spec §6.1 の 3 例、`event` キーで判別、`--event [<name>]`、`recovered` 無し、`priming` 規則 = matd 起動直後の全量は `true`、EventMin 回収分は `false`）。既存消費者への注意（casa は `attribute` キー有無で分岐）。「Routing through matd」の op 表に `event` キーを追記。
- [ ] `docs/configuration.md`: Task 2 で書いた `events` の 3 形を見直し、canary 手順（`events = ["switch","booleanstate"]` → 観察 → wildcard、戻しは `events = []`）を添える。
- [ ] `README.md`: `mat listen` の例にイベント行を 1 つ。
- [ ] `ARCHITECTURE.md`: 「Phase 5 拡張 — イベント購読 フェーズ B」節（設計判断: urgent 固定、last_event_number はプロセスメモリのみ、`"*"` ワイヤ、実機スモークの結果または「実機未実施」）。
- [ ] ロールアウト注意（フェーズ A レビュー由来）を ARCHITECTURE に残す: `IsUrgent` を**必ず on のまま**にする。urgent を落とすと報告は max-interval（最大 300 秒）まで待ち、溜まったイベントはデバイス側の 1 レポート上限で分割配送になる（欠落はしないが遅延が積む）。
- [ ] リリース（minor bump / crates.io）と本番デプロイは**本計画の外**（別セッション）。version bump しない。
- [ ] Commit — `docs: イベント購読 フェーズ B（mat listen --event、subscriptions.toml events、ARCHITECTURE 記録）`

## Self-Review

- Spec §6.1 → Task 3（JSON 形）+ Task 4（フィルタ）+ Task 6（docs）。§6.2 → Task 2（設定）+ Task 3（EventMin / priming 規則 / canary は Task 6）。§6.3 → Task 2 の `to_paths` が常に urgent。§2.4 のイベント名 → Task 1。
- 型名: `Emitted` / `EventItem` / `EventScope` / `EventDef` / `resolve_event` / `find_event` は Task 間で統一。
- S2 依存: Task 3 の conn 型と `FakeEstablisher` は S2 マージ後の形に合わせる（Task 0 で確認済み: `mat_native::SubscribeConn`）。
