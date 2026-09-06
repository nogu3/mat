# Matter イベント購読（EventRequests / EventReports）設計

日付: 2026-09-06 / 対象ブランチ: worktree-events-controller（base: main 8638898 = v1.34.0）
実装は **2 フェーズ**に分ける（§8）。本ブランチはフェーズ A（`mat-controller` の IM 符号化・
復号 + `mat-device` / `matv` のイベント発生側）。フェーズ B（`matd` の常駐購読にイベントを
載せて `mat listen` で流す）は並行セッション S2 の matd reload op がマージされてから別
ブランチで着手する。**このブランチは `matd` と `mat-native::runner` に触らない**。

## 0. 位置づけ — v1 スコープ外からの拡張

ARCHITECTURE.md「Phase 5 拡張 — matd 常駐 Subscribe + `mat listen`」は EventReport 受信を
**v1 スコープ外（将来）** と明記し、`im/read.rs` に「EventRequests/EventReportIB デコード追加
で載る設計余地」だけを残していた。本 spec はその余地を使う**スコープ拡張**である。

なぜ今か: 現行の `mat listen` は属性変化しか流せない。Generic Switch（ボタン）は属性
`CurrentPosition` の変化では押下の種別（短押し・長押し・複数押し）が表現できず、Matter は
それを**イベント**（InitialPress / ShortRelease / MultiPressComplete …）で運ぶ。Boolean State
も `StateChange` イベントを持つ（属性 `StateValue` と二重だが、購読の盲目窓中の遷移は
イベント番号で欠落なく回収できる — 属性の `recovered` 推定より強い）。casa のトリガ源として
「ボタン」を扱えるようにするのが目的で、オートメーションは引き続き casa の責務
（設計ルール「scenes, automation はスコープ外」は不変）。

設計ルール 4（KVS 以外の永続状態を持たない）も不変: フェーズ B の matd は「ノードごとの
最後に見た EventNumber」をプロセスメモリにだけ持つ（再起動で消え、priming でやり直す）。

## 1. スコープ

**フェーズ A（本ブランチ）に入る**

- `mat-controller::im`: SubscribeRequest への `EventRequests` / `EventFilters` の符号化（client）と
  復号（server）、ReportData の `EventReports`（EventReportIB = EventDataIB | EventStatusIB）の
  復号（client）と符号化（server）。既存 API は**シグネチャ・挙動ともに無改変**（§3.4）。
- `mat-controller::session`: イベント付き購読の新 API（`subscribe` / `next_subscription_report_full`）。
  既存 `subscribe_wildcard` / `next_subscription_report` は無改変のラッパとして残す（matd は
  フェーズ B まで既存 API のまま動く）。
- `mat-device`: イベントログ（EventNumber 単調増加・優先度・タイムスタンプ）、
  `ClusterHandler` のイベント宣言・発生、購読へのイベント配信（priming の EventFilters 尊重、
  urgent / non-urgent の報告タイミング）、Generic Switch クラスタ（bridged kind `switch`）と
  Boolean State クラスタ（bridged kind `contact-sensor`）、外部刺激（stimulus）注入経路。
- `matv`: `--stdin-control` — stdin の JSON 行でボタン押下 / 開閉状態を注入する開発用フック。
- テスト: 各コーデックの unit、device 側 I/O-free unit、`mat-device` の loopback 統合テスト
  （matv 相当の `Device` に対して mat-controller の新 API で購読し、刺激→イベント受信）。
  既存 attribute 購読の無退行（既存テスト全通過 + 「EventRequests 無しの SubscribeRequest の
  ワイヤが byte-equal」を釘打ち）。

**フェーズ A に入らない**（フェーズ B で必要になれば足す）

- 属性なし（AttributeRequests = tag 3 省略）の SubscribeRequest を**出す** client。
  フェーズ A の client は AttributeRequests を常に出す（`SubscribeSpec.clusters` が空 =
  full wildcard なので、属性なしは表現できない）。server 側の受理は入っている（§2.1 /
  §4.2）。必要になったらフェーズ B で `SubscribeSpec.clusters: Option<Vec<u32>>` として足す。

**フェーズ B（別ブランチ、§8）に入る**

- `matd`: 常駐購読に EventRequests（wildcard, urgent）を載せ、EventReport を `mat listen`
  イベントとしてファンアウト。ノードごとの last EventNumber を保持し再購読時に EventMin で
  盲目窓を回収。`subscriptions.toml` に `events` 絞り込み。
- `mat listen`: `--event <name>` フィルタ、イベント行の JSON 形。
- `mat-core::ids`: イベント名テーブル（`gen-ids.py` に events 出力を追加）。
- 実機 E2E（Aqara 系のボタン/開閉があれば）+ matv 相手の e2e スクリプト。

**入らない（両フェーズとも）**

- ReadRequest の EventRequests（one-shot でイベントログを読む `mat read-events`）。購読経路のみ。
- `DataVersionFilters`、EventFilters の `Node` フィールド（常に省略 = 自ノード）。
- 優先度別のイベントログ分割（chip は debug/info/critical で別バッファ。matv は単一リングで足りる）。
- Generic Switch の `AS`（Action Switch, Matter 1.4）フィーチャと `SwitchLatched`（latching
  switch）。matv の `switch` は momentary（MS|MSR|MSL|MSM）のみ。
- Boolean State Configuration クラスタ（0x0080）。
- イベントのリプレイ op / スナップショット（matd は状態を持たない、既存契約どおり）。
- 実デバイスのイベント優先度ごとのバッファ枯渇挙動への追従（controller は受けた物を
  そのまま流す）。

## 2. ワイヤ（Matter Core 1.4 §8.9–8.10 / §10.7 IM 符号化）

既存の tag 付け（`im/subscribe.rs` / `im/read.rs` のコメント）に揃えて全部書き下す。

### 2.1 SubscribeRequestMessage

```
struct {
  0: KeepSubscriptions      bool
  1: MinIntervalFloor       uint16
  2: MaxIntervalCeiling     uint16
  3: AttributeRequests      array[AttributePathIB]     （既存）
  4: EventRequests          array[EventPathIB]         （新規、任意）
  5: EventFilters           array[EventFilterIB]       （新規、任意）
  7: IsFabricFiltered       bool
  255: InteractionModelRevision
}
EventPathIB   = list  { 0: Node?, 1: Endpoint?, 2: Cluster?, 3: Event?, 4: IsUrgent? bool }
EventFilterIB = struct{ 0: Node?, 1: EventMin uint64 }
```

- 省略フィールドは wildcard。client は `Node` を出さない。
- `IsUrgent` は path ごと。matd（フェーズ B）は全 path を urgent で購読する（§6.3）。
- `AttributeRequests` が**空の配列**でも構わない（イベントだけの購読）— server 側は既に
  受理する（§4.2 の `has_readable_path || has_readable_event_path`）。ただし
  **フェーズ A の client は AttributeRequests を常に出す**（`SubscribeSpec.clusters` が空 =
  full wildcard なので、属性なしの要求は表現できない）。属性なし（tag 3 省略）の
  SubscribeRequest はフェーズ B で必要になったら `SubscribeSpec.clusters: Option<Vec<u32>>`
  で足す。client は event paths が空なら tag 4/5 を省略する。従来の
  `encode_subscribe_request` は attribute wildcard 1 本を出す挙動を変えない。

### 2.2 ReportDataMessage

```
struct {
  0: SubscriptionId?        uint32
  1: AttributeReports?      array[AttributeReportIB]   （既存）
  2: EventReports?          array[EventReportIB]       （新規）
  3: MoreChunkedMessages?   bool
  4: SuppressResponse       bool
  255: InteractionModelRevision
}
EventReportIB = struct{ 0: EventStatusIB } | struct{ 1: EventDataIB }
EventDataIB   = struct{
  0: Path                   EventPathIB（Endpoint/Cluster/Event は具体値、IsUrgent 無し）
  1: EventNumber            uint64
  2: Priority               uint8   （0 debug / 1 info / 2 critical）
  3: EpochTimestamp?        uint64  ms
  4: SystemTimestamp?       uint64  ms
  5: DeltaEpochTimestamp?   uint64  ms（同一 ReportData 内の直前 EventDataIB からの差分）
  6: DeltaSystemTimestamp?  uint64  ms
  7: Data?                  任意の TLV（クラスタ定義のイベント struct）
}
EventStatusIB = struct{ 0: Path EventPathIB, 1: StatusIB{0: status} }
```

- タイムスタンプは 3〜6 のうち**ちょうど 1 つ**。matv は壁時計同期を持たない device として
  `SystemTimestamp`（プロセス起動からの ms）を出す。controller は 4 形式すべてを復号し、
  Delta 形は同一メッセージ内の直前イベントの絶対値から解決する（先頭が Delta なら未解決 =
  `None` のまま流す）。
- 既存の `decode_report_data_message` は tag 2 を「未知コンテナ」として読み飛ばしている
  （= 現行 matd はイベントを黙って捨てている）。この挙動はそのまま残す（§3.4）。

### 2.3 IM ステータス

- `STATUS_UNSUPPORTED_EVENT = 0x8F` を追加（具体 event path が存在しないイベント id を指す時）。
- 具体 path の endpoint / cluster 不在は既存の `UNSUPPORTED_ENDPOINT` / `UNSUPPORTED_CLUSTER`。

### 2.4 対象クラスタの定義（`mat-controller::im` に数値定数として追加）

| クラスタ | id | 属性 | イベント（id, priority, Data フィールド） |
|---|---|---|---|
| Generic Switch (`switch`) | 0x003B | NumberOfPositions(0) uint8, CurrentPosition(1) uint8, MultiPressMax(2) uint8 | SwitchLatched(0, info, {0: NewPosition}) ※matv 未使用 / InitialPress(1, info, {0: NewPosition}) / LongPress(2, info, {0: NewPosition}) / ShortRelease(3, info, {0: PreviousPosition}) / LongRelease(4, info, {0: PreviousPosition}) / MultiPressOngoing(5, info, {0: NewPosition, 1: CurrentNumberOfPressesCounted}) / MultiPressComplete(6, info, {0: PreviousPosition, 1: TotalNumberOfPressesCounted}) |
| Boolean State (`booleanstate`) | 0x0045 | StateValue(0) bool | StateChange(0, info, {0: StateValue}) |

- Generic Switch FeatureMap: LS=0x01, MS=0x02, MSR=0x04, MSL=0x08, MSM=0x10（AS=0x20 は非対応）。
  matv の `switch` は `MS|MSR|MSL|MSM = 0x1E`、ClusterRevision 2、NumberOfPositions=2、
  MultiPressMax=3。デバイスタイプ Generic Switch = 0x000F。
- Boolean State: FeatureMap 0、ClusterRevision 1。デバイスタイプ Contact Sensor = 0x0015
  （StateValue: true = 閉/接触、false = 開。spec の Contact Sensor 定義に従う）。
- 属性名は `mat-core::ids` の生成テーブルに既にある（`switch` / `booleanstate`）。イベント名は
  テーブルに無い → フェーズ B で `gen-ids.py` に `events` 出力を足す。フェーズ A は数値のみ。

## 3. `mat-controller`（フェーズ A）

### 3.1 型（`im/mod.rs` + 新 `im/event.rs`）

```rust
pub enum EventPriority { Debug = 0, Info = 1, Critical = 2 }   // 未知値は Malformed

pub struct EventPathIn  { pub endpoint: Option<u16>, pub cluster: Option<u32>,
                          pub event: Option<u32>, pub urgent: bool }      // 要求側（server decode / client encode 共用）
pub struct EventFilterIn { pub event_min: u64 }

pub enum EventTimestamp { Epoch(u64), System(u64), DeltaEpoch(u64), DeltaSystem(u64) }

pub struct EventData {  // 復号済み EventDataIB
    pub endpoint: u16, pub cluster: u32, pub event: u32,
    pub event_number: u64, pub priority: EventPriority,
    pub timestamp: Option<EventTimestamp>,   // Delta 解決後（解決不能なら Delta のまま）
    pub data: Option<serde_json::Value>,     // `tlv_to_json` 規約（struct キー = context tag 10 進）
}
pub enum EventReport { Data(EventData), Status { endpoint: Option<u16>, cluster: Option<u32>,
                                                 event: Option<u32>, status: u8 } }

pub struct EventReportOut {  // server 側の符号化入力（mat-device が作る）
    pub endpoint: u16, pub cluster: u32, pub event: u32,
    pub event_number: u64, pub priority: EventPriority,
    pub system_timestamp_ms: u64, pub data_tlv: Option<Vec<u8>> }
pub enum EventEntryOut { Data(EventReportOut), Status { endpoint: u16, cluster: u32, event: u32, status: u8 } }
```

### 3.2 コーデック（すべて新規関数。既存関数は無改変）

- `encode_subscribe_request_full(&SubscribeSpec) -> Vec<u8>`:
  `SubscribeSpec { min_interval_floor_s, max_interval_ceiling_s, keep_subscriptions,
  clusters: Vec<u32>, event_paths: Vec<EventPathIn>, event_min: Option<u64> }`。
  `event_paths` 空 + `event_min` None のときの出力は `encode_subscribe_request(...)` と
  **byte-equal**（テストで釘打ち。matd 経路の無退行保証）。
- `decode_subscribe_request` は `SubscribeRequestIn` に `event_paths: Vec<EventPathIn>` と
  `event_min: Option<u64>` を**追加**する（フィールド追加。`SubscribeRequestIn` の struct
  literal は workspace 内に無いことを確認済み — 壊れる呼び出し側なし）。tag 4/5 が無ければ
  空 / None。
- `decode_event_reports(payload) -> Result<Vec<EventReport>, ImError>`: 同じ ReportData
  payload から tag 2 だけを読む独立デコーダ。`decode_report_data_message` は無改変（tag 2 を
  読み飛ばす現行挙動のまま）。両者を同じ payload に掛けるのが session 層の仕事（§3.3）。
  Delta タイムスタンプの解決はここで行う。
- `encode_report_data_full(attrs: &[ReportEntryOut], events: &[EventEntryOut],
  suppress_response, subscription_id, more_chunks) -> Vec<u8>`: server 側符号化。
  `events` 空のとき `encode_report_data_entries(...)` と byte-equal（テストで釘打ち）。
  EventDataIB は常に `SystemTimestamp`（tag 4）を出す（Delta 形は出さない）。
- 定数: `CLUSTER_SWITCH`, `ATTR_SWITCH_*`, `EVENT_SWITCH_*`, `CLUSTER_BOOLEAN_STATE`,
  `ATTR_BS_STATE_VALUE`, `EVENT_BS_STATE_CHANGE`, `DEVICE_TYPE_GENERIC_SWITCH`,
  `DEVICE_TYPE_CONTACT_SENSOR`, `STATUS_UNSUPPORTED_EVENT`, `SWITCH_FEATURE_*`。
  クラスタ/属性 id は `mat-device` の既存 drift-guard テストの流儀で `mat_core::ids` と照合する。

### 3.3 session 層（`session/subscribe.rs`）

```rust
pub struct SubscriptionReport { pub data: ReportDataMessage, pub events: Vec<EventReport> }
pub struct SubscribeOutcome  { pub response: SubscribeResponse,
                               pub priming: Vec<ReportDataMessage>, pub priming_events: Vec<EventReport> }

impl SecureSession {
    pub async fn subscribe(&mut self, spec: &SubscribeSpec, cfg: &MrpConfig) -> Result<SubscribeOutcome, SessionError>;
    pub async fn next_subscription_report_full(&mut self, timeout, cfg) -> Result<SubscriptionReport, SessionError>;
}
```

- `subscribe_wildcard(min, max, keep, clusters, cfg)` = `subscribe(spec without events)` の結果を
  `(response, priming)` に落とすラッパ。`next_subscription_report` = `_full` の `.data`。
  ハンドシェイク本体（priming チャンクの StatusResponse(0)、MAX_REPORT_CHUNKS、復号失敗時の
  空 rd 差し替え）は 1 箇所（新 API 側）に移し、旧 API はそれを呼ぶだけ — ロジックの二重化なし。
- 復号失敗時の扱いは既存と同じ（監査⑨: 認証済みチャンクは ack して先へ、events は空）。

### 3.4 互換性の約束（matd / mat-native を触らないための境界）

- `ReportDataMessage` に**フィールドを足さない**（`matd/src/subscription.rs` と
  `mat-native/src/test_support.rs` に struct literal があり、足すと S2 のブランチが壊れる）。
- `subscribe_wildcard` / `next_subscription_report` / `encode_subscribe_request` /
  `decode_report_data_message` / `encode_report_data_entries` はシグネチャ・出力とも無改変。
- 追加はすべて新しい名前。フェーズ B で matd が新 API へ乗り換える。

## 4. `mat-device`（フェーズ A）

### 4.1 イベントログ（`core/events.rs`、I/O-free）

```rust
pub struct EmittedEvent { pub event: u32, pub priority: EventPriority, pub data_tlv: Option<Vec<u8>> }  // handler が出す形
pub struct StoredEvent  { pub number: u64, pub endpoint: u16, pub cluster: u32, pub event: u32,
                          pub priority: EventPriority, pub system_timestamp_ms: u64, pub data_tlv: Option<Vec<u8>> }
pub struct EventLog { next_number: u64, entries: VecDeque<StoredEvent>, cap: usize }
```

- `EventLog::new(first_number, cap)`。`Node` は `Device::new` から **起動時の Unix ms** を初期
  EventNumber として渡される（spec は再起動を跨いだ単調増加を要求する。chip は永続カウンタ +
  ストライドで実現するが、matv は「起動時刻 ms を初期値に、以後 +1」で永続 I/O なしに満たす —
  1 秒以内の再起動で 1000 個以上のイベントが出ることはない）。テストは任意の初期値を渡せる。
- `cap = 64`。満杯なら最古を捨てる（chip と同じ FIFO）。
- `append(endpoint, cluster, EmittedEvent, system_timestamp_ms) -> u64`（採番して返す）。
- `since(min: u64) -> impl Iterator<Item=&StoredEvent>`（number >= min、古い順）。

### 4.2 `ClusterHandler` 拡張（`core/datamodel.rs`）

```rust
fn events(&self) -> Vec<u32> { Vec::new() }            // 生成しうるイベント id（wildcard 展開・具体 path 検査用）
fn event_privilege(&self, _event: u32) -> u8 { PRIVILEGE_VIEW }
fn stimulate(&mut self, _stimulus: &Stimulus, _ctx: &mut InvokeCtx) -> StimulusReply { StimulusReply::Unsupported }
```

- `InvokeCtx` に `events: Vec<EmittedEvent>` を追加（`changed` と同じ契約: handler が push、
  `Node` が endpoint/cluster と組にしてログへ追記・採番）。`invoke` / `write` / `stimulate` の
  どれからでも出せる（例: Boolean State は stimulate から、将来の Door Lock は invoke から）。
- `Stimulus`（`core/stimulus.rs`）= 仮想デバイスへの**外部刺激**。実 bridge なら native
  プロトコルから来る「状態が変わった」の抽象。matv では stdin から来る。
  ```rust
  pub enum Stimulus { Press(PressKind), SetState(bool) }
  pub enum PressKind { Short, Long, Multi(u8) }     // Multi(n): n >= 2
  pub enum StimulusReply { Applied, Unsupported, Rejected(&'static str) }
  ```
- `Node::stimulate(endpoint, &Stimulus) -> Result<StimulusOutcome, StimulusError>`:
  endpoint の全 handler に順に `stimulate` を試し、最初の `Applied` / `Rejected` で止める
  （全 handler が `Unsupported` → `StimulusError::Unsupported`）。`ctx.changed` を full path
  にして DataVersion を bump、`ctx.events` をログへ追記。
  `StimulusOutcome { changed: Vec<(u16,u32,u32)>, event_numbers: Vec<u64> }`。
- `Node::event_entries(paths: &[EventPathIn], event_min: u64, read_ctx) -> Vec<EventEntryOut>`:
  ログを `since(event_min)` で走査し、いずれかの path にマッチ（`None` = wildcard）かつ
  ACL（`event_privilege`、`read_allowed` と同じ gate）を通る物を `Data` で返す。具体 path
  （endpoint・cluster・event が全部 `Some`）が解決できなければ `Status`（UNSUPPORTED_ENDPOINT /
  _CLUSTER / _EVENT）を 1 件返す（属性の `read_entries` と同じ非対称: wildcard は黙る）。
- `Node::has_readable_event_path(paths, read_ctx) -> bool`: 属性側 `has_readable_path` の
  イベント版（具体 path は常に true、wildcard は 1 つでも `events()` ∩ ACL 許可があれば true）。
  `serve_subscribe_request` の INVALID_ACTION 判定は `has_readable_path(attr) ||
  has_readable_event_path(events)` になる（イベントだけの購読を受理する）。
- `Node::next_event_number() -> u64`（購読側が「ここから先」を記録するため）。

### 4.3 クラスタ実装

- `core/generic_switch.rs::GenericSwitchHandler`（cluster 0x003B）:
  属性 NumberOfPositions=2 / CurrentPosition（Arc<AtomicU8>、外部観測用）/ MultiPressMax=3。
  `stimulate(Press(kind))` が出すイベント列（spec §1.13.6 の MS|MSR|MSL|MSM シーケンス）:
  - `Short`: InitialPress{1} → ShortRelease{1}
  - `Long`: InitialPress{1} → LongPress{1} → LongRelease{1}
  - `Multi(n)`: InitialPress{1} → ShortRelease{1} → (i=2..n: InitialPress{1} →
    MultiPressOngoing{1, i} → ShortRelease{1}) → MultiPressComplete{1, n}
  - ShortRelease / LongRelease の値 1 は `PreviousPosition`（spec §1.13.6 =
    直前の `CurrentPosition` = 押下位置 1）であって離した後の位置ではない。
  - `Multi(n)` で n<2 または n>MultiPressMax は `Rejected`。
  - CurrentPosition は各 Press 中 1→0 と動くが、1 刺激 = 1 まとまりなので `changed` には
    最終値（0、変化なし）だけ載る → 属性としては dirty にならない。イベントが本体。
  - `SetState` は `Unsupported`。
- `core/boolean_state.rs::BooleanStateHandler`（cluster 0x0045）:
  属性 StateValue（Arc<AtomicBool>）。`stimulate(SetState(v))`: 値が変われば `changed` に
  StateValue + `events` に StateChange{v}; 同値なら何も出さず `Applied`（no-op、spec の
  「変化時のみ」）。`Press` は `Unsupported`。
- `core/bridge.rs`: `DeviceKind::Switch`（serde `"switch"`）と `DeviceKind::ContactSensor`
  （`"contact-sensor"`）。クラスタ構成は onoff-light と同じ骨格（Descriptor{device type +
  BridgedNode} / BDBI / Identify / Groups / 本体クラスタ）。`BridgedEndpoint.onoff_state` は
  `Option` にせず、kind ごとの観測ハンドルを `BridgedState` enum にまとめる
  （`OnOff(Arc<AtomicBool>) | Switch(Arc<AtomicU8>) | Contact(Arc<AtomicBool>)`）。

### 4.4 購読への配信（`net/subscription.rs` + `net/runtime.rs`）

- `ActiveSubscription` に追加: `event_paths: Vec<EventPathIn>`、`next_event: u64`
  （次に送るべき EventNumber。priming 後 = `node.next_event_number()`）、
  `pending_urgent: bool`。
- 報告タイミング（`next_report_deadline`）: `dirty` 非空 **または** `pending_urgent` なら
  min-interval 側、そうでなければ keep-alive 側。non-urgent なイベントは次の報告
  （dirty か keep-alive）に相乗りするだけで報告を早めない — chip の ReportScheduler と同じ
  規則で、ボタンを即時に受けたい購読者は `IsUrgent=true` を立てる責務を負う（matd はそうする）。
- `note_events(&[StoredEvent])`: 新イベントのうち `event_paths` にマッチする物があり、その path
  のいずれかが urgent なら `pending_urgent = true`。
- `send_subscription_report`: 属性 entries に加えて `node.event_entries(&sub.event_paths,
  sub.next_event, ctx)` を `encode_report_data_full` で同梱。送達成功で `next_event =
  node.next_event_number()`、`pending_urgent = false`。dirty 報告は従来どおり 1 メッセージ
  （分割しない）— 予算超過は既存と同じ debug ログ。
- priming（`serve_subscribe_request`）: 属性チャンク列（`read_chunks`、現状のまま）の後に、
  `event_entries(paths, req.event_min.unwrap_or(0))` が非空なら**イベントだけの追加チャンク**を
  1 つ以上足す（イベントも `REPORT_CHUNK_BUDGET` で分割）。属性側最終チャンクの
  `more_chunks` を true に立てるため、`read_chunks` に「後続チャンクあり」を伝える引数を
  足す（`Option<bool>` ではなく `trailer_follows: bool`）。
- 刺激の受け口: `Runtime` の `select!` に `stimuli.recv()` 分岐を追加。
  `Device::stimulus_handle() -> StimulusHandle`（`mpsc::Sender` のラッパ、`Clone`）。
  `StimulusHandle::apply(device_id: &str, Stimulus) -> Result<StimulusOutcome, StimulusError>`
  （oneshot で結果を返す。`UnknownDevice` / `Unsupported` / `Rejected` / `Closed`）。
  runtime は device id → endpoint を `Device::new` の採番結果から `HashMap` で持つ。
  適用後 `sub.note_changed(&changed)` と `sub.note_events(...)`（`on_group_datagram` と同じ形）。
- 購読が無い間のイベントもログに残る（次の priming で EventFilters 次第で流れる）。

### 4.5 `matv` の開発用フック

- `matv --config x.toml --stdin-control`: 追加フラグ。立てると tokio タスクで stdin を行単位に
  読み、1 行 = 1 JSON:
  ```json
  {"device": "btn1", "press": "short"}
  {"device": "btn1", "press": "long"}
  {"device": "btn1", "press": "multi", "count": 2}
  {"device": "door", "state": true}
  ```
  成功は stdout に 1 行 `{"device":"btn1","applied":"press","event_numbers":[1725600000123,1725600000124]}`
  （stdout = JSON 行の mat 流儀。setup payload 行と同じストリーム）、失敗は stderr に
  `{"error":{"kind":"parse_error"|"not_found"|"other","detail":"..."}}` を 1 行出して次の行へ。
  stdin EOF でタスク終了（デバイスは動き続ける）。フラグ無しなら stdin に触らない
  （systemd / パイプ下の既存運用に影響なし）。
- `[[device]]` の `kind` に `"switch"` / `"contact-sensor"` を追加（README の表を更新）。

## 5. テスト（フェーズ A）

1. **コーデック unit**（`mat-controller::im`）: SubscribeRequest（events 有/無、`_full` と既存の
   byte-equal）、EventPathIB/EventFilterIB の decode、EventReportIB Data/Status の encode→decode
   往復、4 種タイムスタンプ + Delta 解決、未知 priority = Malformed、`decode_report_data_message`
   が tag 2 を読み飛ばして従来どおり attribute だけ返すこと（無退行）。
2. **session unit**（`ReliableChannel` ペア）: priming にイベントチャンクが混ざるハンドシェイク、
   `next_subscription_report_full` が同一メッセージの attribute と events を両方返す、
   `subscribe_wildcard` / `next_subscription_report` の既存テスト全通過。
3. **device core unit**: `EventLog` の採番・cap・since、GenericSwitch の 3 種シーケンス
   （イベント列・Data の TLV・Rejected 条件）、BooleanState の変化時のみ発火、
   `Node::stimulate` の DataVersion bump とログ追記、`event_entries` の wildcard/具体/ACL、
   `has_readable_event_path`、`bridge` の新 kind 2 種の cluster 集合。
4. **device net unit**: `ActiveSubscription` の urgent/non-urgent deadline、`note_events`。
5. **loopback 統合**（`crates/mat-device/tests/events_subscribe.rs`、`support` 再利用）:
   `switch` + `contact-sensor` を持つ `Device` を起動 → commission → `subscribe(spec{events
   wildcard urgent, min 0, max 5})` → priming events 空 → `stimulus_handle().apply("btn", Press(Short))`
   → `next_subscription_report_full` に InitialPress + ShortRelease が EventNumber 昇順で届く
   → `apply("door", SetState(true))` → 同一 report に StateValue 属性 + StateChange イベント
   → 再購読を `event_min = 最後+1` で張ると priming events が空、`event_min = 0` なら 3 件流れる。
   さらに `subscribe_wildcard`（旧 API、attribute only）で同じ device に張っても既存
   `subscribe_loop.rs` と同じ結果（無退行）。
6. **matv**: `load_config` の新 kind 受理、stdin 行のパース unit（純関数）、`tests/cli.rs`
   相当に `--stdin-control` の 1 往復（spawn して stdin に 1 行書き、stdout に applied 行）。
7. `task check`（fmt / clippy / test）全通過。`cargo check -p mat-device --no-default-features`
   （core の I/O-free 規律）。

## 6. フェーズ B の設計（matd + `mat listen`、実装は別ブランチ）

### 6.1 `mat listen` の出力

属性行は**無改変**。イベント行を新しく流す（`attribute` ではなく `event` キーを持つのが判別点）:

```json
{"timestamp":"2026-09-06T21:00:00+09:00","node_id":25,"endpoint":2,"cluster":"switch","event":"initial-press","event_number":1725600000123,"priority":"info","data":{"new-position":1},"priming":false}
{"timestamp":"...","node_id":25,"endpoint":2,"cluster":"switch","event":"multi-press-complete","event_number":1725600000130,"priority":"info","data":{"previous-position":1,"total-number-of-presses-counted":2},"priming":false}
{"timestamp":"...","node_id":24,"endpoint":1,"cluster":"booleanstate","event":"state-change","event_number":9,"priority":"info","data":{"state-value":true},"priming":false}
```

- `timestamp` は受信時刻（属性行と同じ契約）。デバイス側時刻は `"device_time": {"system_ms": N}`
  または `{"epoch_ms": N}` として任意で付ける（Delta 未解決なら省略）。
- `data` は `mat-core::ids` のイベント field 名（kebab）でキー付け、テーブルに無ければ
  context tag の 10 進文字列（read の struct 規約と同じ）。`event` 名も同様に無ければ数値。
- `recovered` は付けない。イベントは EventNumber で欠落回収するので推定が要らない。
- `priority` は `"debug" | "info" | "critical"`。
- `--event <name>` フィルタを追加（`--attribute` と排他、`--cluster` は両方に掛かる）。
  `--attribute` を指定した listen にはイベント行は流れない、`--event` を指定した listen には
  属性行は流れない。どちらも無指定なら両方流れる（**既存の消費者は `attribute` キーの有無で
  判別が必要** — casa 側は 1 行の追記で済む想定、リリースノートに明記）。

### 6.2 matd の購読

- `subscribe(spec)` に `event_paths = [wildcard, urgent]`（`subscriptions.toml` の `events =
  ["switch", "booleanstate"]` があればクラスタ絞り込み、`events = []` で無効化 = 現行と同じ
  ワイヤ）。属性の `clusters` 絞り込みと独立。
- ノードごとに `last_event_number: Option<u64>` をプロセスメモリに保持。再購読時は
  `event_min = last + 1` を EventFilters に載せる → 盲目窓中のイベントが priming で届き、
  それらは `priming: false` で流す（番号が last より大きい = 未観測の実イベント。属性の
  `recovered` 相当を推定なしで実現）。matd 起動直後（`last = None`）は EventFilters 無し →
  デバイスのログ全量が `priming: true` で流れる（消費者は無視する既存契約）。
- 1 ReportData 内のイベントは EventNumber 昇順でファンアウト。同一 ReportData の属性行と
  イベント行は同じ `timestamp`。
- ロールアウト: 本番 matd はまず `subscriptions.toml` に `events = ["switch", "booleanstate"]`
  を置いて限定的に有効化し、19 ノードの priming 所要時間（現状 ~137s）の悪化を見る。
  悪化や未対応デバイスの拒否（INVALID_ACTION）が出たら `events = []` で即戻せる。

### 6.3 なぜ全 path を urgent にするか

non-urgent は次回 keep-alive（≤300s）まで届かない。ボタンの用途に 5 分遅延は無意味なので
matd は常に urgent。urgent の代償（min interval 0 で即時報告）は属性購読と同じで既に受容済み。

## 7. リスクと対処

- **既存 matd 経路の退行**: `_full` 系と旧 API の byte-equal テスト + 旧 API テスト全通過で
  釘打ち。matd はフェーズ B まで旧 API のまま。
- **`ReportDataMessage` の struct literal**: フィールドを足さない（§3.4）。
- **イベントログのメモリ**: cap 64、Data TLV は数バイト。上限固定。
- **priming の肥大**: EventFilters の `event_min` で 2 回目以降は差分のみ。初回は最大 64 件。
- **`has_readable_path` の緩和**: 「イベント path だけの購読」を受理するようになる。属性 path
  が全部拒否でイベント path が通るケースは priming が属性空 + イベントで正しく成立する。
- **EventNumber の初期値が Unix ms**: `u64` で十分（1.7e12）。テストは小さい初期値を渡す。

## 8. フェーズ分割と着手条件

| | フェーズ A（本ブランチ） | フェーズ B |
|---|---|---|
| 触る crate | `mat-controller`（im / session）、`mat-device`、`matv`、docs | `matd`、`mat`（listen CLI）、`mat-core::ids`（events）、`scripts/gen-ids.py`、`scripts/e2e-*`、docs |
| 触らない | `matd`、`mat-native::runner`、`mat` | — |
| 着手条件 | — | S2 の matd reload op が main にマージ済み。フェーズ A がマージ済み |
| 完了条件 | §5 全項目 + `task check` + main へ no-ff マージ | matv 相手の e2e（listen でイベント行が流れる）+ 実機（あれば）+ 本番 canary 手順の記録 |

フェーズ B の実装計画は `docs/superpowers/plans/2026-09-06-events-phase-b-matd-listen.md` に
書き（本ブランチで作成、実装はしない）、着手条件を先頭に記す。
