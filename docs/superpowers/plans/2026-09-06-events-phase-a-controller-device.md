# イベント購読 フェーズ A（mat-controller + mat-device + matv）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Matter イベント（EventRequests / EventReports）を `mat-controller` の IM 層と購読セッション API で扱えるようにし、`mat-device` / `matv` が Generic Switch と Boolean State のイベントを発生・配信できるようにする。

**Architecture:** `mat-controller::im` に新モジュール `event.rs`（EventPathIB / EventFilterIB / EventReportIB のコーデック）を足し、SubscribeRequest / ReportData の符号化・復号を**新しい名前の関数**で拡張する（既存関数はシグネチャ・出力とも無改変）。`mat-device` は `core::events`（I/O-free イベントログ）と `core::stimulus`（外部刺激）を足し、`ClusterHandler` にイベント宣言・発生の口を開け、`net::runtime` の購読に urgent/non-urgent の報告規則と priming の EventFilters を実装する。`matv` は `--stdin-control` で刺激を注入する。

**Tech Stack:** Rust（workspace 1.34.0）、tokio、serde_json、既存 `mat_controller::tlv` Writer/Reader。

**Spec:** `docs/superpowers/specs/2026-09-06-events-subscribe-design.md`

## Global Constraints

- **`crates/matd` と `crates/mat-native/src/runner.rs` には触らない**（並行セッション S2 が編集中）。`crates/mat` にも触らない。
- `ReportDataMessage` に**フィールドを足さない**（matd / mat-native に struct literal がある）。
- `subscribe_wildcard` / `next_subscription_report` / `encode_subscribe_request` / `decode_report_data_message` / `encode_report_data_entries` はシグネチャ・出力ともに無改変。追加はすべて新しい名前。
- `mat-device::core` は I/O-free（tokio / socket / file なし）。`cargo check -p mat-device --no-default-features` が通ること。
- worktree 内の git は `/usr/bin/git` の単純コマンド。編集は Edit/Write ツール。
- 各タスクの最後に `cargo fmt --all` と当該 crate の `cargo clippy -p <crate> --all-targets -- -D warnings` を通す。最終タスクで `task check`。
- コメント・ドキュメントの言語は周辺に合わせる（既存ファイルが日本語なら日本語、英語なら英語）。
- コミットメッセージ末尾に必ず:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01NNjK1Jg2v53ruufGet8eht
  ```

## File Structure

| ファイル | 役割 |
|---|---|
| `crates/mat-controller/src/im/event.rs`（新規） | イベント関連の型（`EventPriority`, `EventPathIn`, `EventTimestamp`, `EventData`, `EventReport`, `EventReportOut`, `EventEntryOut`）と EventPathIB / EventFilterIB / EventReportIB のコーデック、`decode_event_reports`、`encode_report_data_full` |
| `crates/mat-controller/src/im/mod.rs` | 定数追加（Switch / BooleanState / device types / `STATUS_UNSUPPORTED_EVENT`）、`mod event; pub use event::*;` |
| `crates/mat-controller/src/im/subscribe.rs` | `SubscribeSpec`, `encode_subscribe_request_full`、`decode_subscribe_request` の EventRequests / EventFilters 対応（`SubscribeRequestIn` にフィールド追加） |
| `crates/mat-controller/src/session/subscribe.rs` | `SubscribeOutcome`, `SubscriptionReport`, `subscribe`, `next_subscription_report_full`。旧 API はラッパ化 |
| `crates/mat-device/src/core/events.rs`（新規） | `EmittedEvent`, `StoredEvent`, `EventLog` |
| `crates/mat-device/src/core/stimulus.rs`（新規） | `Stimulus`, `PressKind`, `StimulusReply`, `StimulusOutcome`, `StimulusError` |
| `crates/mat-device/src/core/datamodel.rs` | `ClusterHandler::{events, event_privilege, stimulate}`、`InvokeCtx::events`、`Node::{stimulate, event_entries, has_readable_event_path, next_event_number, set_event_log}`、`read_chunks` の `trailer_follows` |
| `crates/mat-device/src/core/generic_switch.rs`（新規） | `GenericSwitchHandler` |
| `crates/mat-device/src/core/boolean_state.rs`（新規） | `BooleanStateHandler` |
| `crates/mat-device/src/core/bridge.rs` | `DeviceKind::{Switch, ContactSensor}`, `BridgedState` |
| `crates/mat-device/src/device.rs` | `states: Vec<(String, BridgedState)>`, `endpoint_by_device`, stimulus チャネル、`stimulus_handle()`、EventLog 初期値 |
| `crates/mat-device/src/net/subscription.rs` | `ActiveSubscription::{event_paths, next_event, pending_urgent}`, `note_events`, deadline 規則 |
| `crates/mat-device/src/net/stimulus.rs`（新規） | `StimulusHandle`（mpsc + oneshot） |
| `crates/mat-device/src/net/runtime.rs` | purpose: 購読へのイベント配信（priming / dirty）、`select!` の stimulus 分岐 |
| `crates/mat-device/tests/support/mod.rs` | `device_config_with(store_dir, devices)` |
| `crates/mat-device/tests/events_subscribe.rs`（新規） | loopback 統合テスト |
| `crates/matv/src/main.rs`, `crates/matv/src/control.rs`（新規） | `--stdin-control`、行パーサ |
| `crates/matv/tests/cli.rs` | stdin-control の 1 往復 |
| `README.md`, `docs/commands.md`, `ARCHITECTURE.md` | kind 表・フェーズ記録・スコープ更新 |

---

### Task 1: `mat-controller::im` — イベント定数・型・EventPathIB/EventFilterIB コーデック・SubscribeRequest 拡張

**Files:**
- Create: `crates/mat-controller/src/im/event.rs`
- Modify: `crates/mat-controller/src/im/mod.rs`（定数、`mod event;`）
- Modify: `crates/mat-controller/src/im/subscribe.rs`

**Interfaces:**
- Produces（後続タスクが使う名前）:
  ```rust
  // im/mod.rs
  pub const CLUSTER_SWITCH: u32 = 0x003B;
  pub const ATTR_SWITCH_NUMBER_OF_POSITIONS: u32 = 0x0000;
  pub const ATTR_SWITCH_CURRENT_POSITION: u32 = 0x0001;
  pub const ATTR_SWITCH_MULTI_PRESS_MAX: u32 = 0x0002;
  pub const EVENT_SWITCH_SWITCH_LATCHED: u32 = 0x00;
  pub const EVENT_SWITCH_INITIAL_PRESS: u32 = 0x01;
  pub const EVENT_SWITCH_LONG_PRESS: u32 = 0x02;
  pub const EVENT_SWITCH_SHORT_RELEASE: u32 = 0x03;
  pub const EVENT_SWITCH_LONG_RELEASE: u32 = 0x04;
  pub const EVENT_SWITCH_MULTI_PRESS_ONGOING: u32 = 0x05;
  pub const EVENT_SWITCH_MULTI_PRESS_COMPLETE: u32 = 0x06;
  pub const SWITCH_FEATURE_LATCHING: u32 = 0x01;
  pub const SWITCH_FEATURE_MOMENTARY: u32 = 0x02;
  pub const SWITCH_FEATURE_MOMENTARY_RELEASE: u32 = 0x04;
  pub const SWITCH_FEATURE_MOMENTARY_LONG_PRESS: u32 = 0x08;
  pub const SWITCH_FEATURE_MOMENTARY_MULTI_PRESS: u32 = 0x10;
  pub const CLUSTER_BOOLEAN_STATE: u32 = 0x0045;
  pub const ATTR_BS_STATE_VALUE: u32 = 0x0000;
  pub const EVENT_BS_STATE_CHANGE: u32 = 0x00;
  pub const DEVICE_TYPE_GENERIC_SWITCH: u32 = 0x000F;
  pub const DEVICE_TYPE_CONTACT_SENSOR: u32 = 0x0015;
  pub const STATUS_UNSUPPORTED_EVENT: u8 = 0x8F;
  // im/event.rs
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum EventPriority { Debug = 0, Info = 1, Critical = 2 }
  impl EventPriority { pub fn from_wire(v: u64) -> Result<Self, ImError>; pub fn as_str(self) -> &'static str /* "debug"/"info"/"critical" */ }
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub struct EventPathIn { pub endpoint: Option<u16>, pub cluster: Option<u32>, pub event: Option<u32>, pub urgent: bool }
  impl EventPathIn { pub const WILDCARD_URGENT: EventPathIn = EventPathIn { endpoint: None, cluster: None, event: None, urgent: true }; }
  pub(super) fn encode_event_path_ib(w: &mut Writer, tag: Tag, path: &EventPathIn);   // list {1,2,3,4}
  pub(super) fn decode_event_path_ib(r: &mut Reader) -> Result<EventPathIn, ImError>; // ListStart 既読前提
  pub(super) fn decode_event_requests(r: &mut Reader) -> Result<Vec<EventPathIn>, ImError>; // ArrayStart 既読前提
  pub(super) fn decode_event_filters(r: &mut Reader) -> Result<Option<u64>, ImError>;      // ArrayStart 既読前提、最初の EventMin
  // im/subscribe.rs
  #[derive(Debug, Clone, PartialEq, Eq, Default)]
  pub struct SubscribeSpec { pub min_interval_floor_s: u16, pub max_interval_ceiling_s: u16, pub keep_subscriptions: bool,
                             pub clusters: Vec<u32>, pub event_paths: Vec<EventPathIn>, pub event_min: Option<u64> }
  pub fn encode_subscribe_request_full(spec: &SubscribeSpec) -> Vec<u8>;
  pub struct SubscribeRequestIn { /* 既存 5 フィールド */ pub event_paths: Vec<EventPathIn>, pub event_min: Option<u64> }
  ```

- [ ] **Step 1: 失敗するテストを書く（`im/event.rs` の tests）**

`crates/mat-controller/src/im/event.rs` を作り、まず型・関数のスタブなしで tests だけ置くとコンパイルできないので、型定義 + 関数シグネチャ（本体 `todo!()`）を先に書く。テスト:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn event_path_ib_roundtrips_all_fields() {
        let path = EventPathIn { endpoint: Some(2), cluster: Some(CLUSTER_SWITCH), event: Some(EVENT_SWITCH_INITIAL_PRESS), urgent: true };
        let mut w = Writer::new();
        encode_event_path_ib(&mut w, Tag::Anonymous, &path);
        let b = w.finish();
        let mut r = Reader::new(&b);
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        assert_eq!(decode_event_path_ib(&mut r).unwrap(), path);
    }

    #[test]
    fn event_path_ib_wildcard_encodes_only_urgent() {
        // 全省略 + urgent: list { 4: true } だけ。
        let mut w = Writer::new();
        encode_event_path_ib(&mut w, Tag::Anonymous, &EventPathIn::WILDCARD_URGENT);
        let b = w.finish();
        let mut r = Reader::new(&b);
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(4));
        assert_eq!(el.value, Value::Bool(true));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ContainerEnd));
    }

    #[test]
    fn event_path_ib_without_urgent_decodes_false_and_skips_node() {
        let mut w = Writer::new();
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0x1234); // Node — 無視
        w.put_uint(Tag::Context(2), u64::from(CLUSTER_BOOLEAN_STATE));
        w.end_container();
        let b = w.finish();
        let mut r = Reader::new(&b);
        r.next().unwrap();
        let p = decode_event_path_ib(&mut r).unwrap();
        assert_eq!(p, EventPathIn { endpoint: None, cluster: Some(CLUSTER_BOOLEAN_STATE), event: None, urgent: false });
    }

    #[test]
    fn event_filters_take_the_first_event_min() {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 77);
        w.end_container();
        w.end_container();
        let b = w.finish();
        let mut r = Reader::new(&b);
        r.next().unwrap();
        assert_eq!(decode_event_filters(&mut r).unwrap(), Some(77));
    }

    #[test]
    fn priority_from_wire_rejects_unknown() {
        assert_eq!(EventPriority::from_wire(1).unwrap(), EventPriority::Info);
        assert!(EventPriority::from_wire(3).is_err());
        assert_eq!(EventPriority::Critical.as_str(), "critical");
    }
}
```

`im/subscribe.rs` の tests に追加:

```rust
    /// events 無しの `_full` は従来の encode_subscribe_request と byte-equal（matd 経路の無退行）。
    #[test]
    fn subscribe_request_full_without_events_is_byte_equal_to_legacy() {
        for clusters in [vec![], vec![CLUSTER_ON_OFF, 0x0402]] {
            let spec = SubscribeSpec { min_interval_floor_s: 0, max_interval_ceiling_s: 300, keep_subscriptions: false, clusters: clusters.clone(), ..SubscribeSpec::default() };
            assert_eq!(encode_subscribe_request_full(&spec), encode_subscribe_request(0, 300, false, &clusters));
        }
    }

    #[test]
    fn subscribe_request_full_with_events_roundtrips_through_server_decode() {
        let spec = SubscribeSpec {
            min_interval_floor_s: 0, max_interval_ceiling_s: 300, keep_subscriptions: false,
            clusters: vec![],
            event_paths: vec![EventPathIn::WILDCARD_URGENT, EventPathIn { endpoint: Some(2), cluster: Some(CLUSTER_SWITCH), event: None, urgent: false }],
            event_min: Some(1000),
        };
        let req = decode_subscribe_request(&encode_subscribe_request_full(&spec)).unwrap();
        // AttributeRequests は従来どおり full wildcard 1 本（clusters 空）。
        assert_eq!(req.paths, vec![AttrPathIn { endpoint: None, cluster: None, attribute: None }]);
        assert_eq!(req.event_paths, spec.event_paths);
        assert_eq!(req.event_min, Some(1000));
    }

    #[test]
    fn legacy_subscribe_request_decodes_with_no_events() {
        let req = decode_subscribe_request(&encode_subscribe_request(0, 60, false, &[])).unwrap();
        assert!(req.event_paths.is_empty());
        assert_eq!(req.event_min, None);
    }

    #[test]
    fn subscribe_request_full_tags_events_at_4_and_filters_at_5() {
        let spec = SubscribeSpec { max_interval_ceiling_s: 60, event_paths: vec![EventPathIn::WILDCARD_URGENT], event_min: Some(5), ..SubscribeSpec::default() };
        let b = encode_subscribe_request_full(&spec);
        let mut r = Reader::new(&b);
        r.next().unwrap(); // struct
        r.next().unwrap(); r.next().unwrap(); r.next().unwrap(); // keep/min/max
        let el = r.next().unwrap().unwrap(); // AttributeRequests
        assert_eq!(el.tag, Tag::Context(3));
        crate::tlv::skip_container(&mut r).unwrap();
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(4));
        assert!(matches!(el.value, Value::ArrayStart));
        crate::tlv::skip_container(&mut r).unwrap();
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(5));
        assert!(matches!(el.value, Value::ArrayStart));
    }
```

`SubscribeSpec` に `Default` を derive するので `..SubscribeSpec::default()` が使える。

- [ ] **Step 2: テストが失敗（コンパイルエラー / todo panic）することを確認**

Run: `cargo test -p mat-controller --lib im::` → 未定義でコンパイルエラー。

- [ ] **Step 3: 実装**

`im/mod.rs`: `pub const` 群を `ATTR_BDBI_REACHABLE` の後ろに追加（doc コメントに spec 章 §1.13 Switch / §1.7 Boolean State / Device Library §6.6 Generic Switch / §7.1 Contact Sensor を書く）。`STATUS_UNSUPPORTED_EVENT` は `STATUS_UNSUPPORTED_ATTRIBUTE` の隣。`mod event; pub use event::*;` を `mod subscribe;` の前に追加。

`im/event.rs`（この Task では path/filter/priority のみ。EventReport 系は Task 2）:

```rust
//! イベント関連の IM 型とコーデック（spec §8.9.2.2 EventPathIB / §8.9.2.4
//! EventFilterIB / §8.9.2.6 EventDataIB / EventStatusIB）。
use crate::tlv::{Reader, Tag, Value, Writer};
use super::{skip_container, ImError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPriority { Debug = 0, Info = 1, Critical = 2 }

impl EventPriority {
    pub fn from_wire(v: u64) -> Result<Self, ImError> {
        match v { 0 => Ok(Self::Debug), 1 => Ok(Self::Info), 2 => Ok(Self::Critical),
                  _ => Err(ImError::Malformed("unknown event priority")) }
    }
    pub fn as_str(self) -> &'static str {
        match self { Self::Debug => "debug", Self::Info => "info", Self::Critical => "critical" }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventPathIn { pub endpoint: Option<u16>, pub cluster: Option<u32>, pub event: Option<u32>, pub urgent: bool }

impl EventPathIn {
    pub const WILDCARD_URGENT: EventPathIn = EventPathIn { endpoint: None, cluster: None, event: None, urgent: true };
}

/// EventPathIB (list): {0: Node?, 1: Endpoint?, 2: Cluster?, 3: Event?, 4: IsUrgent?}. Node は出さない。
pub(super) fn encode_event_path_ib(w: &mut Writer, tag: Tag, path: &EventPathIn) {
    w.start_list(tag);
    if let Some(e) = path.endpoint { w.put_uint(Tag::Context(1), u64::from(e)); }
    if let Some(c) = path.cluster { w.put_uint(Tag::Context(2), u64::from(c)); }
    if let Some(ev) = path.event { w.put_uint(Tag::Context(3), u64::from(ev)); }
    if path.urgent { w.put_bool(Tag::Context(4), true); }
    w.end_container();
}

pub(super) fn decode_event_path_ib(r: &mut Reader) -> Result<EventPathIn, ImError> {
    let mut p = EventPathIn::default();
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated event path"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => p.endpoint = Some(u16::try_from(v).map_err(|_| ImError::Malformed("endpoint out of range"))?),
            (Tag::Context(2), Value::Uint(v)) => p.cluster = Some(u32::try_from(v).map_err(|_| ImError::Malformed("cluster id out of range"))?),
            (Tag::Context(3), Value::Uint(v)) => p.event = Some(u32::try_from(v).map_err(|_| ImError::Malformed("event id out of range"))?),
            (Tag::Context(4), Value::Bool(b)) => p.urgent = b,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(p)
}

/// EventRequests (array[EventPathIB])。ArrayStart 既読前提。
pub(super) fn decode_event_requests(r: &mut Reader) -> Result<Vec<EventPathIn>, ImError> {
    let mut out = Vec::new();
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated event requests"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::ListStart => out.push(decode_event_path_ib(r)?),
            Value::StructStart | Value::ArrayStart => skip_container(r)?,
            _ => return Err(ImError::Malformed("unexpected element in event requests")),
        }
    }
    Ok(out)
}

/// EventFilters (array[EventFilterIB{0: Node?, 1: EventMin}])。最初のフィルタの EventMin だけ使う
/// （Node フィルタは spec 上任意でこの実装は自ノードのみ）。ArrayStart 既読前提。
pub(super) fn decode_event_filters(r: &mut Reader) -> Result<Option<u64>, ImError> {
    let mut min = None;
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated event filters"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => {
                loop {
                    let f = r.next()?.ok_or(ImError::Malformed("truncated event filter"))?;
                    match (f.tag, f.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(1), Value::Uint(v)) => { if min.is_none() { min = Some(v); } }
                        (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
                        _ => {}
                    }
                }
            }
            Value::ArrayStart | Value::ListStart => skip_container(r)?,
            _ => return Err(ImError::Malformed("unexpected element in event filters")),
        }
    }
    Ok(min)
}
```

`im/subscribe.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubscribeSpec {
    pub min_interval_floor_s: u16,
    pub max_interval_ceiling_s: u16,
    pub keep_subscriptions: bool,
    pub clusters: Vec<u32>,
    pub event_paths: Vec<EventPathIn>,
    pub event_min: Option<u64>,
}

/// events 有りの SubscribeRequest。`event_paths` 空かつ `event_min` None なら
/// `encode_subscribe_request` と byte-equal（tag 4/5 を省略）。
pub fn encode_subscribe_request_full(spec: &SubscribeSpec) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), spec.keep_subscriptions);
    w.put_uint(Tag::Context(1), u64::from(spec.min_interval_floor_s));
    w.put_uint(Tag::Context(2), u64::from(spec.max_interval_ceiling_s));
    w.start_array(Tag::Context(3));
    if spec.clusters.is_empty() { w.start_list(Tag::Anonymous); w.end_container(); }
    else { for &c in &spec.clusters { w.start_list(Tag::Anonymous); w.put_uint(Tag::Context(3), u64::from(c)); w.end_container(); } }
    w.end_container();
    if !spec.event_paths.is_empty() {
        w.start_array(Tag::Context(4));
        for p in &spec.event_paths { encode_event_path_ib(&mut w, Tag::Anonymous, p); }
        w.end_container();
    }
    if let Some(min) = spec.event_min {
        w.start_array(Tag::Context(5));
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), min);
        w.end_container();
        w.end_container();
    }
    w.put_bool(Tag::Context(7), true);
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}
```

既存 `encode_subscribe_request` の本体は `encode_subscribe_request_full(&SubscribeSpec{..})` への委譲に置き換える（byte-equal テストで釘打ち済み）。`decode_subscribe_request` に `(Tag::Context(4), Value::ArrayStart) => event_paths = decode_event_requests(&mut r)?` と `(Tag::Context(5), Value::ArrayStart) => event_min = decode_event_filters(&mut r)?` を追加し、`SubscribeRequestIn` に `pub event_paths: Vec<EventPathIn>, pub event_min: Option<u64>` を足す。`use super::{... , decode_event_requests, decode_event_filters, encode_event_path_ib, EventPathIn}`。

- [ ] **Step 4: テスト通過確認**

Run: `cargo test -p mat-controller --lib im::` → 全 PASS（既存テスト含む）。`cargo clippy -p mat-controller --all-targets -- -D warnings`、`cargo fmt --all`。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-controller/src/im
/usr/bin/git commit -m "feat(im): EventPathIB / EventFilterIB コーデックと SubscribeRequest の EventRequests 対応 — 既存 encode_subscribe_request と byte-equal"
```

---

### Task 2: `mat-controller::im` — EventReportIB の復号（client）と符号化（server）

**Files:**
- Modify: `crates/mat-controller/src/im/event.rs`

**Interfaces:**
- Consumes: Task 1 の型、`super::json::tlv_element_to_json`（`pub(super)`）、`skip_container`、`expect_struct_start`。
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum EventTimestamp { Epoch(u64), System(u64), DeltaEpoch(u64), DeltaSystem(u64) }
  #[derive(Debug, Clone, PartialEq)]
  pub struct EventData { pub endpoint: u16, pub cluster: u32, pub event: u32, pub event_number: u64,
                         pub priority: EventPriority, pub timestamp: Option<EventTimestamp>, pub data: Option<serde_json::Value> }
  #[derive(Debug, Clone, PartialEq)]
  pub enum EventReport { Data(EventData), Status { endpoint: Option<u16>, cluster: Option<u32>, event: Option<u32>, status: u8 } }
  pub fn decode_event_reports(payload: &[u8]) -> Result<Vec<EventReport>, ImError>;
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct EventReportOut { pub endpoint: u16, pub cluster: u32, pub event: u32, pub event_number: u64,
                              pub priority: EventPriority, pub system_timestamp_ms: u64, pub data_tlv: Option<Vec<u8>> }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub enum EventEntryOut { Data(EventReportOut), Status { endpoint: u16, cluster: u32, event: u32, status: u8 } }
  pub fn encode_report_data_full(attrs: &[ReportEntryOut], events: &[EventEntryOut], suppress_response: bool,
                                 subscription_id: Option<u32>, more_chunks: bool) -> Vec<u8>;
  ```

- [ ] **Step 1: 失敗するテスト**

```rust
    fn switch_press_data_tlv(new_position: u8) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(new_position));
        w.end_container();
        w.finish()
    }

    #[test]
    fn report_data_full_without_events_is_byte_equal_to_entries_encoder() {
        let attrs = vec![ReportEntryOut::Data(AttrReportOut { endpoint: 2, cluster: CLUSTER_ON_OFF, attribute: ATTR_ON_OFF, data_version: 7, value_tlv: vec![0x09] })];
        for (sup, sid, more) in [(false, Some(9), true), (true, None, false)] {
            assert_eq!(encode_report_data_full(&attrs, &[], sup, sid, more), encode_report_data_entries(&attrs, sup, sid, more));
        }
    }

    #[test]
    fn event_reports_roundtrip_data_and_status() {
        let events = vec![
            EventEntryOut::Data(EventReportOut { endpoint: 2, cluster: CLUSTER_SWITCH, event: EVENT_SWITCH_INITIAL_PRESS, event_number: 100, priority: EventPriority::Info, system_timestamp_ms: 5000, data_tlv: Some(switch_press_data_tlv(1)) }),
            EventEntryOut::Status { endpoint: 9, cluster: CLUSTER_SWITCH, event: EVENT_SWITCH_LONG_PRESS, status: STATUS_UNSUPPORTED_ENDPOINT },
        ];
        let payload = encode_report_data_full(&[], &events, false, Some(42), false);
        // 既存デコーダは tag 2 を読み飛ばし、attribute 無しの report として成立する（無退行）。
        let legacy = decode_report_data_message(&payload).unwrap();
        assert!(legacy.reports.is_empty());
        assert_eq!(legacy.subscription_id, Some(42));
        let got = decode_event_reports(&payload).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], EventReport::Data(EventData { endpoint: 2, cluster: CLUSTER_SWITCH, event: EVENT_SWITCH_INITIAL_PRESS, event_number: 100, priority: EventPriority::Info, timestamp: Some(EventTimestamp::System(5000)), data: Some(serde_json::json!({"0": 1})) }));
        assert_eq!(got[1], EventReport::Status { endpoint: Some(9), cluster: Some(CLUSTER_SWITCH), event: Some(EVENT_SWITCH_LONG_PRESS), status: STATUS_UNSUPPORTED_ENDPOINT });
    }

    /// 手組みの EventDataIB: Epoch / DeltaSystem / Delta 先頭 の 3 ケース。
    fn hand_event(w: &mut Writer, number: u64, ts_tag: u8, ts: u64) {
        w.start_struct(Tag::Anonymous); // EventReportIB
        w.start_struct(Tag::Context(1)); // EventDataIB
        w.start_list(Tag::Context(0)); w.put_uint(Tag::Context(1), 1); w.put_uint(Tag::Context(2), u64::from(CLUSTER_BOOLEAN_STATE)); w.put_uint(Tag::Context(3), u64::from(EVENT_BS_STATE_CHANGE)); w.end_container();
        w.put_uint(Tag::Context(1), number);
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(ts_tag), ts);
        w.end_container(); w.end_container();
    }

    #[test]
    fn delta_timestamps_resolve_against_the_previous_event_in_the_message() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(2));
        hand_event(&mut w, 1, 6, 10);      // DeltaSystem 先頭 → 未解決のまま
        hand_event(&mut w, 2, 4, 1000);    // System 1000
        hand_event(&mut w, 3, 6, 25);      // DeltaSystem → System 1025
        hand_event(&mut w, 4, 3, 1_700_000_000_000); // Epoch
        hand_event(&mut w, 5, 5, 7);       // DeltaEpoch → Epoch +7
        w.end_container();
        w.put_bool(Tag::Context(4), false);
        w.end_container();
        let got = decode_event_reports(&w.finish()).unwrap();
        let ts = |i: usize| match &got[i] { EventReport::Data(d) => d.timestamp, _ => panic!() };
        assert_eq!(ts(0), Some(EventTimestamp::DeltaSystem(10)));
        assert_eq!(ts(1), Some(EventTimestamp::System(1000)));
        assert_eq!(ts(2), Some(EventTimestamp::System(1025)));
        assert_eq!(ts(3), Some(EventTimestamp::Epoch(1_700_000_000_000)));
        assert_eq!(ts(4), Some(EventTimestamp::Epoch(1_700_000_000_007)));
    }

    #[test]
    fn event_report_with_unknown_priority_is_malformed() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous); w.start_struct(Tag::Context(1));
        w.start_list(Tag::Context(0)); w.put_uint(Tag::Context(1), 1); w.put_uint(Tag::Context(2), 1); w.put_uint(Tag::Context(3), 1); w.end_container();
        w.put_uint(Tag::Context(1), 1); w.put_uint(Tag::Context(2), 9); w.put_uint(Tag::Context(4), 1);
        w.end_container(); w.end_container();
        w.end_container(); w.put_bool(Tag::Context(4), false); w.end_container();
        assert!(matches!(decode_event_reports(&w.finish()), Err(ImError::Malformed(_))));
    }

    #[test]
    fn report_without_event_reports_decodes_to_empty() {
        let payload = encode_report_data_entries(&[], false, Some(1), false);
        assert!(decode_event_reports(&payload).unwrap().is_empty());
    }
```

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-controller --lib im::event` → コンパイルエラー。

- [ ] **Step 3: 実装**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTimestamp { Epoch(u64), System(u64), DeltaEpoch(u64), DeltaSystem(u64) }

#[derive(Debug, Clone, PartialEq)]
pub struct EventData { pub endpoint: u16, pub cluster: u32, pub event: u32, pub event_number: u64,
                       pub priority: EventPriority, pub timestamp: Option<EventTimestamp>, pub data: Option<serde_json::Value> }

#[derive(Debug, Clone, PartialEq)]
pub enum EventReport { Data(EventData), Status { endpoint: Option<u16>, cluster: Option<u32>, event: Option<u32>, status: u8 } }

/// ReportData の EventReports(tag 2) だけを読む。tag 1 等は読み飛ばす。Delta 形は同一メッセージ内の
/// 直前 Data イベントの絶対値に解決（先頭が Delta なら Delta のまま）。
pub fn decode_event_reports(payload: &[u8]) -> Result<Vec<EventReport>, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut out = Vec::new();
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated report data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::ArrayStart) => {
                let mut prev: Option<EventTimestamp> = None;
                loop {
                    let e2 = r.next()?.ok_or(ImError::Malformed("truncated event reports"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart => {
                            let rep = decode_event_report_ib(&mut r, &mut prev)?;
                            out.push(rep);
                        }
                        _ => return Err(ImError::Malformed("unexpected element in event reports")),
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(&mut r)?,
            _ => {}
        }
    }
    Ok(out)
}

/// EventReportIB = {0: EventStatusIB} | {1: EventDataIB}。StructStart 既読前提。
fn decode_event_report_ib(r: &mut Reader, prev: &mut Option<EventTimestamp>) -> Result<EventReport, ImError> {
    let mut out = None;
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated event report"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => out = Some(decode_event_status_ib(r)?),
            (Tag::Context(1), Value::StructStart) => {
                let d = decode_event_data_ib(r, prev)?;
                out = Some(EventReport::Data(d));
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    out.ok_or(ImError::Malformed("event report without data or status"))
}

fn decode_event_status_ib(r: &mut Reader) -> Result<EventReport, ImError> {
    let mut path = EventPathIn::default();
    let mut status = None;
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated event status"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => path = decode_event_path_ib(r)?,
            (Tag::Context(1), Value::StructStart) => {
                loop {
                    let s = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
                    match (s.tag, s.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(0), Value::Uint(v)) => status = Some(u8::try_from(v).map_err(|_| ImError::Malformed("status out of range"))?),
                        (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
                        _ => {}
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(EventReport::Status { endpoint: path.endpoint, cluster: path.cluster, event: path.event,
                             status: status.ok_or(ImError::Malformed("event status without status"))? })
}

/// EventDataIB = {0: Path, 1: EventNumber, 2: Priority, 3..6: timestamp（ちょうど 1 つ）, 7: Data}
fn decode_event_data_ib(r: &mut Reader, prev: &mut Option<EventTimestamp>) -> Result<EventData, ImError> {
    let mut path = EventPathIn::default();
    let (mut number, mut priority, mut ts, mut data) = (None, None, None, None);
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated event data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => path = decode_event_path_ib(r)?,
            (Tag::Context(1), Value::Uint(v)) => number = Some(v),
            (Tag::Context(2), Value::Uint(v)) => priority = Some(EventPriority::from_wire(v)?),
            (Tag::Context(3), Value::Uint(v)) => ts = Some(EventTimestamp::Epoch(v)),
            (Tag::Context(4), Value::Uint(v)) => ts = Some(EventTimestamp::System(v)),
            (Tag::Context(5), Value::Uint(v)) => ts = Some(EventTimestamp::DeltaEpoch(v)),
            (Tag::Context(6), Value::Uint(v)) => ts = Some(EventTimestamp::DeltaSystem(v)),
            (Tag::Context(7), v) => {
                data = Some(super::json::tlv_element_to_json(r, crate::tlv::Element { tag: el.tag, value: v })?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    let resolved = match (ts, *prev) {
        (Some(EventTimestamp::DeltaEpoch(d)), Some(EventTimestamp::Epoch(p))) => Some(EventTimestamp::Epoch(p.wrapping_add(d))),
        (Some(EventTimestamp::DeltaSystem(d)), Some(EventTimestamp::System(p))) => Some(EventTimestamp::System(p.wrapping_add(d))),
        (other, _) => other,
    };
    if matches!(resolved, Some(EventTimestamp::Epoch(_) | EventTimestamp::System(_))) { *prev = resolved; }
    let (Some(endpoint), Some(cluster), Some(event)) = (path.endpoint, path.cluster, path.event) else {
        return Err(ImError::Malformed("event data with incomplete path"));
    };
    Ok(EventData { endpoint, cluster, event,
                   event_number: number.ok_or(ImError::Malformed("event data without number"))?,
                   priority: priority.ok_or(ImError::Malformed("event data without priority"))?,
                   timestamp: resolved, data })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReportOut { pub endpoint: u16, pub cluster: u32, pub event: u32, pub event_number: u64,
                            pub priority: EventPriority, pub system_timestamp_ms: u64, pub data_tlv: Option<Vec<u8>> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventEntryOut { Data(EventReportOut), Status { endpoint: u16, cluster: u32, event: u32, status: u8 } }

/// server 側 ReportData: 属性 entries + イベント entries。`events` 空なら
/// `encode_report_data_entries` と byte-equal（tag 2 を省略）。
pub fn encode_report_data_full(attrs: &[ReportEntryOut], events: &[EventEntryOut], suppress_response: bool,
                               subscription_id: Option<u32>, more_chunks: bool) -> Vec<u8> {
    // 属性部分は既存エンコーダの本体をここへ移し、既存関数はこれを events=[] で呼ぶラッパにする。
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    if let Some(sub_id) = subscription_id { w.put_uint(Tag::Context(0), u64::from(sub_id)); }
    w.start_array(Tag::Context(1));
    for entry in attrs { encode_attribute_report_ib(&mut w, entry); }   // 既存ループ本体を関数化
    w.end_container();
    if !events.is_empty() {
        w.start_array(Tag::Context(2));
        for ev in events {
            w.start_struct(Tag::Anonymous);
            match ev {
                EventEntryOut::Data(d) => {
                    w.start_struct(Tag::Context(1));
                    encode_event_path_ib(&mut w, Tag::Context(0), &EventPathIn { endpoint: Some(d.endpoint), cluster: Some(d.cluster), event: Some(d.event), urgent: false });
                    w.put_uint(Tag::Context(1), d.event_number);
                    w.put_uint(Tag::Context(2), d.priority as u64);
                    w.put_uint(Tag::Context(4), d.system_timestamp_ms);
                    if let Some(tlv) = &d.data_tlv { w.put_raw_element(Tag::Context(7), tlv); }
                    w.end_container();
                }
                EventEntryOut::Status { endpoint, cluster, event, status } => {
                    w.start_struct(Tag::Context(0));
                    encode_event_path_ib(&mut w, Tag::Context(0), &EventPathIn { endpoint: Some(*endpoint), cluster: Some(*cluster), event: Some(*event), urgent: false });
                    w.start_struct(Tag::Context(1)); w.put_uint(Tag::Context(0), u64::from(*status)); w.end_container();
                    w.end_container();
                }
            }
            w.end_container();
        }
        w.end_container();
    }
    if more_chunks { w.put_bool(Tag::Context(3), true); }
    w.put_bool(Tag::Context(4), suppress_response);
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}
```

`read.rs` の `encode_report_data_entries` は本体を `encode_attribute_report_ib(w, entry)`（`pub(super)`、AttributeReportIB 1 件）に切り出し、関数自体は `encode_report_data_full(entries, &[], suppress, sid, more)` を返すだけにする。`EventPriority as u64` は `#[repr(u8)]` を付けたうえで `u64::from(d.priority as u8)` と書く（clippy の `as` 警告回避）。

- [ ] **Step 4: テスト通過確認** — `cargo test -p mat-controller --lib im::` 全 PASS。`cargo test -p mat-controller`（他の統合テストで `encode_report_data_entries` を使うものが壊れていないこと）。clippy / fmt。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-controller/src/im
/usr/bin/git commit -m "feat(im): EventReportIB（EventDataIB / EventStatusIB）の復号と符号化 — decode_event_reports / encode_report_data_full、Delta タイムスタンプ解決"
```

---

### Task 3: `mat-controller::session` — イベント付き購読 API（`subscribe` / `next_subscription_report_full`）

**Files:**
- Modify: `crates/mat-controller/src/session/subscribe.rs`

**Interfaces:**
- Consumes: `im::{SubscribeSpec, encode_subscribe_request_full, decode_event_reports, EventReport, ReportDataMessage, SubscribeResponse}`。
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq)]
  pub struct SubscriptionReport { pub data: crate::im::ReportDataMessage, pub events: Vec<crate::im::EventReport> }
  #[derive(Debug, Clone, PartialEq)]
  pub struct SubscribeOutcome { pub response: crate::im::SubscribeResponse, pub priming: Vec<crate::im::ReportDataMessage>, pub priming_events: Vec<crate::im::EventReport> }
  impl SecureSession {
      pub async fn subscribe(&mut self, spec: &crate::im::SubscribeSpec, cfg: &MrpConfig) -> Result<SubscribeOutcome, SessionError>;
      pub async fn next_subscription_report_full(&mut self, timeout: Duration, cfg: &MrpConfig) -> Result<SubscriptionReport, SessionError>;
  }
  ```
  `subscribe_wildcard` と `next_subscription_report` は無改変シグネチャのラッパ（前者は `(o.response, o.priming)`、後者は `.data`）。
- `SubscriptionReport` / `SubscribeOutcome` は `crate::session` から re-export（`pub use subscribe::{SubscribeOutcome, SubscriptionReport};` を `session/mod.rs` に追加。既存の `mod subscribe;` 宣言を確認して合わせる）。

- [ ] **Step 1: 失敗するテスト（既存 tests モジュール内、`test_util` のヘルパを使う）**

既存テスト `subscribe_wildcard_handshake_with_chunked_priming` を読み、同じ骨格で:

```rust
    /// priming の 2 チャンク目が EventReports だけを運ぶハンドシェイク。
    #[tokio::test]
    async fn subscribe_with_events_collects_priming_events() {
        let (mut s, dev) = reliable_session_pair();
        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
            // ワイヤに EventRequests が載っていること（server decode で確認）。
            let req = crate::im::decode_subscribe_request(&body).unwrap();
            assert_eq!(req.event_paths, vec![crate::im::EventPathIn::WILDCARD_URGENT]);
            assert_eq!(req.event_min, Some(10));
            let ex = p.exchange_id;
            // チャンク1: 属性のみ, more=true
            let d = device_datagram(ex, crate::im::PROTOCOL_ID_IM, crate::im::OPCODE_REPORT_DATA, None, false, 9000, &subscription_report_payload(42, true, true));
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p2, _) = open_from_controller(&buf[..n]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            // チャンク2: イベントのみ, more=false
            let events = vec![crate::im::EventEntryOut::Data(crate::im::EventReportOut { endpoint: 2, cluster: crate::im::CLUSTER_SWITCH, event: crate::im::EVENT_SWITCH_INITIAL_PRESS, event_number: 11, priority: crate::im::EventPriority::Info, system_timestamp_ms: 1, data_tlv: None })];
            let payload = crate::im::encode_report_data_full(&[], &events, false, Some(42), false);
            let d = device_datagram(ex, crate::im::PROTOCOL_ID_IM, crate::im::OPCODE_REPORT_DATA, None, false, 9001, &payload);
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, _) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            let d = device_datagram(ex, crate::im::PROTOCOL_ID_IM, crate::im::OPCODE_SUBSCRIBE_RESPONSE, None, false, 9002, &subscribe_response_payload(42, 120));
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        });
        let spec = crate::im::SubscribeSpec { max_interval_ceiling_s: 3600, event_paths: vec![crate::im::EventPathIn::WILDCARD_URGENT], event_min: Some(10), ..Default::default() };
        let o = s.subscribe(&spec, &fast_cfg()).await.unwrap();
        assert_eq!(o.response.subscription_id, 42);
        assert_eq!(o.priming.len(), 2);
        assert_eq!(o.priming[0].reports[0].data, Some(serde_json::json!(true)));
        assert_eq!(o.priming_events.len(), 1);
        assert!(matches!(&o.priming_events[0], crate::im::EventReport::Data(d) if d.event_number == 11));
        dev_task.await.unwrap();
    }

    /// 購読後の 1 report に属性とイベントが同居 → `_full` は両方返し、旧 API は属性だけ返す。
    #[tokio::test]
    async fn next_subscription_report_full_returns_attributes_and_events() {
        let (mut s, dev) = reliable_session_pair();
        let attrs = vec![crate::im::ReportEntryOut::Data(crate::im::AttrReportOut { endpoint: 2, cluster: crate::im::CLUSTER_BOOLEAN_STATE, attribute: crate::im::ATTR_BS_STATE_VALUE, data_version: 1, value_tlv: vec![0x09] })];
        let events = vec![crate::im::EventEntryOut::Data(crate::im::EventReportOut { endpoint: 2, cluster: crate::im::CLUSTER_BOOLEAN_STATE, event: crate::im::EVENT_BS_STATE_CHANGE, event_number: 5, priority: crate::im::EventPriority::Info, system_timestamp_ms: 9, data_tlv: None })];
        let payload = crate::im::encode_report_data_full(&attrs, &events, false, Some(42), false);
        // device 発の exchange（initiator ビット付き）。既存 `next_subscription_report` テストの
        // datagram 組み立て（device_datagram の initiator 引数）に合わせる。
        let d = device_datagram(0x5001, crate::im::PROTOCOL_ID_IM, crate::im::OPCODE_REPORT_DATA, None, true, 9100, &payload);
        dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        let rep = s.next_subscription_report_full(Duration::from_secs(2), &fast_cfg()).await.unwrap();
        assert_eq!(rep.data.reports.len(), 1);
        assert_eq!(rep.events.len(), 1);
        // StatusResponse(0) が返ること（既存契約）。
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = dev.recv_from(&mut buf).await.unwrap();
        let (_, p, _) = open_from_controller(&buf[..n]);
        assert_eq!(p.opcode, crate::im::OPCODE_STATUS_RESPONSE);
    }
```

`device_datagram` の引数順・`initiator` フラグの扱いは `session/test_util.rs` と既存の `next_subscription_report_*` テストを読んで合わせる（引数名が違えばそれに従う）。

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-controller --lib session::subscribe` → コンパイルエラー。

- [ ] **Step 3: 実装**

- `subscribe_wildcard` の本体を `subscribe` に移す。`encode_subscribe_request(...)` 呼び出しを `encode_subscribe_request_full(spec)` に、priming ループで `decode_report_data_message` の直後に `decode_event_reports(&msg.payload)` を呼び（失敗は `debug!` ログして空扱い — 属性側が復号できているのにイベント側だけ壊れている場合も購読を殺さない）、`priming_events.extend(...)`。復号失敗時の空 rd 差し替えはそのまま。
- `subscribe_wildcard(min, max, keep, clusters, cfg)`:
  ```rust
  let spec = crate::im::SubscribeSpec { min_interval_floor_s, max_interval_ceiling_s, keep_subscriptions, clusters: clusters.to_vec(), ..Default::default() };
  let o = self.subscribe(&spec, cfg).await?;
  Ok((o.response, o.priming))
  ```
- `next_subscription_report` の本体を `next_subscription_report_full` に移し、`rd` を作った直後に `let events = im::decode_event_reports(&msg.payload).unwrap_or_else(|e| { tracing::debug!(error = %e, "sub pump: undecodable event reports; delivering none"); Vec::new() });` を足して `SubscriptionReport { data: rd, events }` を返す。旧関数は `Ok(self.next_subscription_report_full(timeout, cfg).await?.data)`。
- `session/mod.rs` に re-export を追加。

- [ ] **Step 4: テスト通過確認** — `cargo test -p mat-controller --lib session::` 全 PASS（既存 subscribe_wildcard / next_subscription_report テスト含む）。`cargo test -p mat-controller`。clippy / fmt。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-controller/src/session
/usr/bin/git commit -m "feat(session): イベント付き購読 API subscribe / next_subscription_report_full — 旧 API は無改変ラッパ"
```

---

### Task 4: `mat-device::core` — イベントログ・刺激・`ClusterHandler` / `Node` の拡張

**Files:**
- Create: `crates/mat-device/src/core/events.rs`, `crates/mat-device/src/core/stimulus.rs`
- Modify: `crates/mat-device/src/core/mod.rs`（`pub mod events; pub mod stimulus;`）
- Modify: `crates/mat-device/src/core/datamodel.rs`

**Interfaces:**
- Consumes: `mat_controller::im::{EventPriority, EventPathIn, EventEntryOut, EventReportOut, STATUS_UNSUPPORTED_EVENT, STATUS_UNSUPPORTED_ENDPOINT, STATUS_UNSUPPORTED_CLUSTER}`。
- Produces:
  ```rust
  // core/events.rs
  #[derive(Debug, Clone, PartialEq, Eq)] pub struct EmittedEvent { pub event: u32, pub priority: EventPriority, pub data_tlv: Option<Vec<u8>> }
  #[derive(Debug, Clone, PartialEq, Eq)] pub struct StoredEvent { pub number: u64, pub endpoint: u16, pub cluster: u32, pub event: u32, pub priority: EventPriority, pub system_timestamp_ms: u64, pub data_tlv: Option<Vec<u8>> }
  pub struct EventLog { .. }
  impl EventLog {
      pub const DEFAULT_CAP: usize = 64;
      pub fn new(first_number: u64, cap: usize) -> Self;
      pub fn next_number(&self) -> u64;
      pub fn append(&mut self, endpoint: u16, cluster: u32, ev: EmittedEvent, system_timestamp_ms: u64) -> u64;
      pub fn since(&self, min: u64) -> impl Iterator<Item = &StoredEvent>;
      pub fn len(&self) -> usize; pub fn is_empty(&self) -> bool;
  }
  impl Default for EventLog { fn default() -> Self { Self::new(1, Self::DEFAULT_CAP) } }
  // core/stimulus.rs
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum PressKind { Short, Long, Multi(u8) }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Stimulus { Press(PressKind), SetState(bool) }
  #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum StimulusReply { Applied, Unsupported, Rejected(&'static str) }
  #[derive(Debug, Clone, PartialEq, Eq, Default)] pub struct StimulusOutcome { pub changed: Vec<(u16, u32, u32)>, pub event_numbers: Vec<u64> }
  #[derive(Debug, Clone, PartialEq, Eq)] pub enum StimulusError { UnknownEndpoint, Unsupported, Rejected(&'static str) }
  impl std::fmt::Display for StimulusError; impl std::error::Error for StimulusError;
  // core/datamodel.rs
  pub struct InvokeCtx { /* 既存 */ pub events: Vec<EmittedEvent> }
  pub trait ClusterHandler { /* 既存 */
      fn events(&self) -> Vec<u32> { Vec::new() }
      fn event_privilege(&self, _event: u32) -> u8 { PRIVILEGE_VIEW }
      fn stimulate(&mut self, _stimulus: &Stimulus, _ctx: &mut InvokeCtx) -> StimulusReply { StimulusReply::Unsupported }
  }
  impl Node {
      pub fn set_event_log(&mut self, log: EventLog);
      pub fn next_event_number(&self) -> u64;
      pub fn stimulate(&mut self, endpoint: u16, stimulus: &Stimulus, system_timestamp_ms: u64) -> Result<StimulusOutcome, StimulusError>;
      pub fn event_entries(&self, paths: &[EventPathIn], event_min: u64, read_ctx: &ReadCtx) -> Vec<EventEntryOut>;
      pub fn has_readable_event_path(&self, paths: &[EventPathIn], read_ctx: &ReadCtx) -> bool;
      pub fn read_chunks(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx, budget: usize, subscription_id: Option<u32>, trailer_follows: bool) -> Vec<Vec<u8>>;  // 引数追加
      pub fn recent_events(&self, since: u64) -> Vec<StoredEvent>;  // `since(min)` を clone して返す（runtime の note_events 用）
  }
  ```
  `invoke` / `write` / `group_invoke` 経路でも `ctx.events` をログへ追記する（`invoke_on_endpoint` の changed 処理の隣で `drain_events(endpoint, cluster, ctx)`）。既存 `ImOutcome` は無改変（イベント番号を返す必要はない — 購読は `note_events(node.recent_events(sub.next_event))` で拾う）。

- [ ] **Step 1: 失敗するテスト**

`core/events.rs` tests:

```rust
    fn ev(id: u32) -> EmittedEvent { EmittedEvent { event: id, priority: EventPriority::Info, data_tlv: None } }

    #[test]
    fn numbers_are_monotonic_from_the_seed() {
        let mut log = EventLog::new(1000, 8);
        assert_eq!(log.next_number(), 1000);
        assert_eq!(log.append(2, 0x3B, ev(1), 5), 1000);
        assert_eq!(log.append(2, 0x3B, ev(3), 6), 1001);
        assert_eq!(log.next_number(), 1002);
        let all: Vec<u64> = log.since(0).map(|e| e.number).collect();
        assert_eq!(all, vec![1000, 1001]);
        let tail: Vec<u64> = log.since(1001).map(|e| e.number).collect();
        assert_eq!(tail, vec![1001]);
        assert!(log.since(1002).next().is_none());
    }

    #[test]
    fn cap_drops_the_oldest_but_keeps_numbering() {
        let mut log = EventLog::new(1, 2);
        for i in 0..3 { log.append(1, 1, ev(i), 0); }
        let nums: Vec<u64> = log.since(0).map(|e| e.number).collect();
        assert_eq!(nums, vec![2, 3]);
        assert_eq!(log.next_number(), 4);
    }
```

`core/datamodel.rs` tests（既存 tests モジュールに追加。テスト用 handler は既存の inline `impl ClusterHandler` パターンを踏襲）:

```rust
    /// stimulate を実装するテスト用クラスタ: SetState で属性 0 を変え、イベント 0 を出す。
    struct StimHandler { state: bool }
    impl ClusterHandler for StimHandler {
        fn cluster_id(&self) -> u32 { 0xFC01 }
        fn attributes(&self) -> Vec<u32> { vec![0] }
        fn events(&self) -> Vec<u32> { vec![0] }
        fn read(&self, a: u32, _: &ReadCtx) -> Option<Vec<u8>> {
            (a == 0).then(|| { let mut w = Writer::new(); w.put_bool(Tag::Anonymous, self.state); w.finish() })
        }
        fn invoke(&mut self, _: u32, _: &[u8], _: &mut InvokeCtx) -> InvokeReply { InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND) }
        fn stimulate(&mut self, s: &Stimulus, ctx: &mut InvokeCtx) -> StimulusReply {
            match s {
                Stimulus::SetState(v) => {
                    if *v != self.state { self.state = *v; ctx.changed.push(0); ctx.events.push(EmittedEvent { event: 0, priority: EventPriority::Info, data_tlv: None }); }
                    StimulusReply::Applied
                }
                Stimulus::Press(_) => StimulusReply::Unsupported,
            }
        }
    }

    fn node_with_stim() -> Node {
        let mut node = Node::with_root_endpoint(0xFFF1, 0x8000);
        node.add_endpoint(2, vec![Box::new(StimHandler { state: false })]);
        node.set_event_log(EventLog::new(100, 8));
        node
    }

    #[test]
    fn stimulate_appends_events_bumps_data_version_and_reports_changed() {
        let mut node = node_with_stim();
        let before = node.data_version(2, 0xFC01);
        let out = node.stimulate(2, &Stimulus::SetState(true), 777).unwrap();
        assert_eq!(out.changed, vec![(2, 0xFC01, 0)]);
        assert_eq!(out.event_numbers, vec![100]);
        assert_eq!(node.data_version(2, 0xFC01), before.wrapping_add(1));
        assert_eq!(node.next_event_number(), 101);
        let ev = &node.recent_events(0)[0];
        assert_eq!((ev.endpoint, ev.cluster, ev.event, ev.system_timestamp_ms), (2, 0xFC01, 0, 777));
        // 同値: 変化なし、イベントなし、Applied。
        let out = node.stimulate(2, &Stimulus::SetState(true), 778).unwrap();
        assert!(out.changed.is_empty() && out.event_numbers.is_empty());
    }

    #[test]
    fn stimulate_errors_for_unknown_endpoint_and_unsupported_stimulus() {
        let mut node = node_with_stim();
        assert_eq!(node.stimulate(9, &Stimulus::SetState(true), 0), Err(StimulusError::UnknownEndpoint));
        assert_eq!(node.stimulate(2, &Stimulus::Press(PressKind::Short), 0), Err(StimulusError::Unsupported));
        // 刺激を受けない endpoint 0（Descriptor/BasicInformation のみ）も Unsupported。
        assert_eq!(node.stimulate(0, &Stimulus::SetState(true), 0), Err(StimulusError::Unsupported));
    }

    #[test]
    fn event_entries_expand_wildcards_and_honor_event_min() {
        let mut node = node_with_stim();
        node.stimulate(2, &Stimulus::SetState(true), 1).unwrap();  // #100
        node.stimulate(2, &Stimulus::SetState(false), 2).unwrap(); // #101
        let all = node.event_entries(&[EventPathIn::WILDCARD_URGENT], 0, &ReadCtx::default());
        assert_eq!(all.len(), 2);
        let tail = node.event_entries(&[EventPathIn::WILDCARD_URGENT], 101, &ReadCtx::default());
        assert!(matches!(&tail[..], [EventEntryOut::Data(d)] if d.event_number == 101 && d.system_timestamp_ms == 2));
        // 別クラスタの wildcard は黙る。
        let other = node.event_entries(&[EventPathIn { cluster: Some(0xFC02), ..EventPathIn::default() }], 0, &ReadCtx::default());
        assert!(other.is_empty());
    }

    #[test]
    fn concrete_unresolvable_event_paths_report_status() {
        let node = node_with_stim();
        let st = |p: EventPathIn| node.event_entries(&[p], 0, &ReadCtx::default());
        assert!(matches!(&st(EventPathIn { endpoint: Some(9), cluster: Some(0xFC01), event: Some(0), urgent: false })[..],
            [EventEntryOut::Status { status, .. }] if *status == im::STATUS_UNSUPPORTED_ENDPOINT));
        assert!(matches!(&st(EventPathIn { endpoint: Some(2), cluster: Some(0xFC02), event: Some(0), urgent: false })[..],
            [EventEntryOut::Status { status, .. }] if *status == im::STATUS_UNSUPPORTED_CLUSTER));
        assert!(matches!(&st(EventPathIn { endpoint: Some(2), cluster: Some(0xFC01), event: Some(5), urgent: false })[..],
            [EventEntryOut::Status { status, .. }] if *status == im::STATUS_UNSUPPORTED_EVENT));
        // 解決できる具体 path でログが空なら何も返さない（status ではない）。
        assert!(st(EventPathIn { endpoint: Some(2), cluster: Some(0xFC01), event: Some(0), urgent: false }).is_empty());
    }

    #[test]
    fn has_readable_event_path_accepts_wildcards_only_when_some_cluster_has_events() {
        let node = node_with_stim();
        assert!(node.has_readable_event_path(&[EventPathIn::WILDCARD_URGENT], &ReadCtx::default()));
        assert!(!node.has_readable_event_path(&[EventPathIn { cluster: Some(0xFC02), ..EventPathIn::default() }], &ReadCtx::default()));
        // 具体 path は常に true（status で答える）。
        assert!(node.has_readable_event_path(&[EventPathIn { endpoint: Some(9), cluster: Some(1), event: Some(1), urgent: false }], &ReadCtx::default()));
        assert!(!node.has_readable_event_path(&[], &ReadCtx::default()));
    }

    #[test]
    fn read_chunks_trailer_follows_marks_the_last_chunk_more() {
        let node = node_with_stim();
        let paths = vec![AttrPathIn { endpoint: Some(2), cluster: Some(0xFC01), attribute: Some(0) }];
        let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, Some(1), true);
        assert_eq!(chunks.len(), 1);
        assert!(im::decode_report_data_message(&chunks[0]).unwrap().more_chunks);
        let chunks = node.read_chunks(&paths, &ReadCtx::default(), 900, Some(1), false);
        assert!(!im::decode_report_data_message(&chunks[0]).unwrap().more_chunks);
    }
```

drift_guard に追加:

```rust
    #[test]
    fn switch_and_boolean_state_ids_match_mat_core_ids() {
        assert_eq!(resolve_cluster("switch"), Some(im::CLUSTER_SWITCH));
        let attr = |name: &str| resolve_attribute(im::CLUSTER_SWITCH, name).unwrap().id;
        assert_eq!(attr("number-of-positions"), im::ATTR_SWITCH_NUMBER_OF_POSITIONS);
        assert_eq!(attr("current-position"), im::ATTR_SWITCH_CURRENT_POSITION);
        assert_eq!(attr("multi-press-max"), im::ATTR_SWITCH_MULTI_PRESS_MAX);
        assert_eq!(resolve_cluster("booleanstate"), Some(im::CLUSTER_BOOLEAN_STATE));
        assert_eq!(resolve_attribute(im::CLUSTER_BOOLEAN_STATE, "state-value").unwrap().id, im::ATTR_BS_STATE_VALUE);
    }
```

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-device --lib core::` → コンパイルエラー。

- [ ] **Step 3: 実装**

`core/events.rs`:

```rust
//! I/O-free なイベントログ（spec §7.14 / §8.9.2.6）。EventNumber はノード単位で単調増加、
//! 容量固定の FIFO（満杯なら最古を捨てる — chip の EventManagement と同じ）。
use std::collections::VecDeque;
use mat_controller::im::EventPriority;

pub struct EventLog { next_number: u64, entries: VecDeque<StoredEvent>, cap: usize }

impl EventLog {
    pub const DEFAULT_CAP: usize = 64;
    pub fn new(first_number: u64, cap: usize) -> Self { Self { next_number: first_number, entries: VecDeque::with_capacity(cap.min(Self::DEFAULT_CAP)), cap: cap.max(1) } }
    pub fn next_number(&self) -> u64 { self.next_number }
    pub fn append(&mut self, endpoint: u16, cluster: u32, ev: EmittedEvent, system_timestamp_ms: u64) -> u64 {
        let number = self.next_number;
        self.next_number = self.next_number.wrapping_add(1);
        if self.entries.len() == self.cap { self.entries.pop_front(); }
        self.entries.push_back(StoredEvent { number, endpoint, cluster, event: ev.event, priority: ev.priority, system_timestamp_ms, data_tlv: ev.data_tlv });
        number
    }
    pub fn since(&self, min: u64) -> impl Iterator<Item = &StoredEvent> { self.entries.iter().filter(move |e| e.number >= min) }
    pub fn len(&self) -> usize { self.entries.len() }
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
}
```

`core/stimulus.rs`: 上記 Interfaces の型をそのまま定義（`Display` は `UnknownEndpoint => "no such endpoint"`, `Unsupported => "stimulus not supported by any cluster on this endpoint"`, `Rejected(r) => r`）。

`core/datamodel.rs`:
- `InvokeCtx` に `pub events: Vec<EmittedEvent>` を追加（`Default` derive 済みなので literal 箇所は `..Default::default()` を使っているか確認。使っていない literal があれば `events: Vec::new()` を足す）。
- `Node` に `event_log: EventLog` を追加（`Node::new` で `EventLog::default()`）。`set_event_log`, `next_event_number`, `recent_events(since) -> Vec<StoredEvent>`（clone）。
- `fn drain_events(&mut self, endpoint: u16, cluster: u32, ctx: &mut InvokeCtx, system_timestamp_ms: u64) -> Vec<u64>`: `ctx.events.drain(..)` を `event_log.append` に流す。`invoke_on_endpoint` / `handle_write` の changed 処理の直後で呼ぶ（invoke 経路の timestamp は `Node` が時刻を知らないので **0** を渡す — 現時点で invoke からイベントを出す cluster は無い。コメントでそう書く。runtime 経由の `stimulate` は正しい uptime ms を渡す）。
- `ClusterHandler` に 3 メソッド（デフォルト実装付き）。
- `stimulate`:
  ```rust
  pub fn stimulate(&mut self, endpoint: u16, stimulus: &Stimulus, system_timestamp_ms: u64) -> Result<StimulusOutcome, StimulusError> {
      let Some((_, clusters)) = self.endpoints.iter_mut().find(|(id, _)| *id == endpoint) else { return Err(StimulusError::UnknownEndpoint); };
      let mut ctx = InvokeCtx::default();
      let mut target = None;
      for handler in clusters.iter_mut() {
          ctx.changed.clear(); ctx.events.clear();
          match handler.stimulate(stimulus, &mut ctx) {
              StimulusReply::Unsupported => continue,
              StimulusReply::Rejected(r) => return Err(StimulusError::Rejected(r)),
              StimulusReply::Applied => { target = Some(handler.cluster_id()); break; }
          }
      }
      let Some(cluster) = target else { return Err(StimulusError::Unsupported); };
      let changed: Vec<(u16, u32, u32)> = ctx.changed.drain(..).map(|a| (endpoint, cluster, a)).collect();
      if !changed.is_empty() { let v = self.versions.entry((endpoint, cluster)).or_insert(self.version_base); *v = v.wrapping_add(1); }
      let event_numbers = self.drain_events(endpoint, cluster, &mut ctx, system_timestamp_ms);
      Ok(StimulusOutcome { changed, event_numbers })
  }
  ```
  借用: `clusters` の可変借用中に `self.versions` を触るので、ループを抜けてから（`target` 確定後、`clusters` の借用が終わってから）DataVersion を触る構成にする（上のコードはそうなっている）。
- `event_entries`: paths ごとに (a) 全 concrete → endpoint/cluster/handler.events() を検査し無ければ `Status`、あれば log を走査; (b) wildcard → log の各 entry について `path_matches`（endpoint/cluster/event の `None` = wildcard）かつ `acl_allows(&self.acl, fabric, subject, handler.event_privilege(ev), endpoint, cluster)`（handler は endpoint/cluster から引く。引けない = 既にログにある古い endpoint → 落とす）。重複排除: 複数 path が同じイベントに当たっても 1 回だけ（`seen: BTreeSet<u64>`）。出力は EventNumber 昇順。
- `has_readable_event_path`: 空なら false。具体 3 つ揃いは true。それ以外は endpoints × clusters をフィルタして `handler.events()` のうち `acl_allows(.., event_privilege(e), ..)` を通る物が 1 つでもあれば true（`path.event` が Some ならその id が `events()` に含まれるかも見る）。
- `read_chunks(.., trailer_follows: bool)`: 最終バッチの `more_chunks` を `!is_last || (is_last && trailer_follows)` にする。既存呼び出し 6 箇所（runtime 2、datamodel 4 テスト/`handle_read`）に `false` を足す。

- [ ] **Step 4: テスト通過確認** — `cargo test -p mat-device --lib` 全 PASS、`cargo check -p mat-device --no-default-features`、clippy / fmt。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-device/src/core
/usr/bin/git commit -m "feat(mat-device): イベントログと外部刺激 — EventLog / Stimulus、ClusterHandler::{events,event_privilege,stimulate}、Node::{stimulate,event_entries,has_readable_event_path}"
```

---

### Task 5: `mat-device::core` — Generic Switch / Boolean State クラスタと bridged kind `switch` / `contact-sensor`

**Files:**
- Create: `crates/mat-device/src/core/generic_switch.rs`, `crates/mat-device/src/core/boolean_state.rs`
- Modify: `crates/mat-device/src/core/mod.rs`, `crates/mat-device/src/core/bridge.rs`, `crates/mat-device/src/device.rs`

**Interfaces:**
- Consumes: Task 4 の `Stimulus` / `PressKind` / `StimulusReply` / `EmittedEvent`、Task 1 の定数。
- Produces:
  ```rust
  pub struct GenericSwitchHandler { .. }
  impl GenericSwitchHandler { pub const NUMBER_OF_POSITIONS: u8 = 2; pub const MULTI_PRESS_MAX: u8 = 3; pub fn new() -> (Self, Arc<AtomicU8>); }
  pub struct BooleanStateHandler { .. }
  impl BooleanStateHandler { pub fn new() -> (Self, Arc<AtomicBool>); }
  pub enum DeviceKind { OnOffLight /* "onoff-light" */, Switch /* "switch" */, ContactSensor /* "contact-sensor" */ }
  #[derive(Debug, Clone)] pub enum BridgedState { OnOff(Arc<AtomicBool>), Switch(Arc<AtomicU8>), Contact(Arc<AtomicBool>) }
  pub struct BridgedEndpoint { pub clusters: Vec<Box<dyn ClusterHandler>>, pub state: BridgedState }   // `onoff_state` フィールドは廃止
  ```
  `device.rs`: `onoff_states: Vec<(String, Arc<AtomicBool>)>` → `states: Vec<(String, BridgedState)>`（`#[allow(dead_code)]` 維持）。

- [ ] **Step 1: 失敗するテスト**

`core/generic_switch.rs` tests:

```rust
    fn numbers(ctx: &InvokeCtx) -> Vec<u32> { ctx.events.iter().map(|e| e.event).collect() }
    fn field(ctx: &InvokeCtx, i: usize, tag: u8) -> u64 {
        let tlv = ctx.events[i].data_tlv.as_ref().unwrap();
        let v = mat_controller::im::tlv_to_json(tlv).unwrap();
        v[tag.to_string()].as_u64().unwrap()
    }

    #[test]
    fn short_press_emits_initial_press_then_short_release() {
        let (mut h, pos) = GenericSwitchHandler::new();
        let mut ctx = InvokeCtx::default();
        assert_eq!(h.stimulate(&Stimulus::Press(PressKind::Short), &mut ctx), StimulusReply::Applied);
        assert_eq!(numbers(&ctx), vec![im::EVENT_SWITCH_INITIAL_PRESS, im::EVENT_SWITCH_SHORT_RELEASE]);
        assert_eq!(field(&ctx, 0, 0), 1); // NewPosition
        assert_eq!(field(&ctx, 1, 0), 1); // PreviousPosition
        assert!(ctx.events.iter().all(|e| e.priority == EventPriority::Info));
        assert!(ctx.changed.is_empty(), "CurrentPosition ends where it started");
        assert_eq!(pos.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn long_press_emits_initial_long_press_long_release() {
        let (mut h, _) = GenericSwitchHandler::new();
        let mut ctx = InvokeCtx::default();
        h.stimulate(&Stimulus::Press(PressKind::Long), &mut ctx);
        assert_eq!(numbers(&ctx), vec![im::EVENT_SWITCH_INITIAL_PRESS, im::EVENT_SWITCH_LONG_PRESS, im::EVENT_SWITCH_LONG_RELEASE]);
    }

    #[test]
    fn double_press_follows_the_msm_sequence() {
        let (mut h, _) = GenericSwitchHandler::new();
        let mut ctx = InvokeCtx::default();
        h.stimulate(&Stimulus::Press(PressKind::Multi(2)), &mut ctx);
        assert_eq!(numbers(&ctx), vec![
            im::EVENT_SWITCH_INITIAL_PRESS, im::EVENT_SWITCH_SHORT_RELEASE,
            im::EVENT_SWITCH_INITIAL_PRESS, im::EVENT_SWITCH_MULTI_PRESS_ONGOING, im::EVENT_SWITCH_SHORT_RELEASE,
            im::EVENT_SWITCH_MULTI_PRESS_COMPLETE,
        ]);
        assert_eq!(field(&ctx, 3, 1), 2); // CurrentNumberOfPressesCounted
        assert_eq!(field(&ctx, 5, 0), 1); // PreviousPosition
        assert_eq!(field(&ctx, 5, 1), 2); // TotalNumberOfPressesCounted
    }

    #[test]
    fn multi_press_out_of_range_is_rejected_and_set_state_unsupported() {
        let (mut h, _) = GenericSwitchHandler::new();
        assert!(matches!(h.stimulate(&Stimulus::Press(PressKind::Multi(1)), &mut InvokeCtx::default()), StimulusReply::Rejected(_)));
        assert!(matches!(h.stimulate(&Stimulus::Press(PressKind::Multi(4)), &mut InvokeCtx::default()), StimulusReply::Rejected(_)));
        assert_eq!(h.stimulate(&Stimulus::SetState(true), &mut InvokeCtx::default()), StimulusReply::Unsupported);
    }

    #[test]
    fn attributes_feature_map_and_events_match_the_spec_shape() {
        let (h, _) = GenericSwitchHandler::new();
        assert_eq!(h.cluster_id(), im::CLUSTER_SWITCH);
        assert_eq!(h.revision(), 2);
        assert_eq!(h.feature_map(), im::SWITCH_FEATURE_MOMENTARY | im::SWITCH_FEATURE_MOMENTARY_RELEASE | im::SWITCH_FEATURE_MOMENTARY_LONG_PRESS | im::SWITCH_FEATURE_MOMENTARY_MULTI_PRESS);
        assert_eq!(h.attributes(), vec![im::ATTR_SWITCH_NUMBER_OF_POSITIONS, im::ATTR_SWITCH_CURRENT_POSITION, im::ATTR_SWITCH_MULTI_PRESS_MAX]);
        assert_eq!(h.events(), vec![im::EVENT_SWITCH_INITIAL_PRESS, im::EVENT_SWITCH_LONG_PRESS, im::EVENT_SWITCH_SHORT_RELEASE, im::EVENT_SWITCH_LONG_RELEASE, im::EVENT_SWITCH_MULTI_PRESS_ONGOING, im::EVENT_SWITCH_MULTI_PRESS_COMPLETE]);
        let v = mat_controller::im::tlv_to_json(&h.read(im::ATTR_SWITCH_MULTI_PRESS_MAX, &ReadCtx::default()).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!(3));
    }
```

`core/boolean_state.rs` tests:

```rust
    #[test]
    fn set_state_changes_attribute_and_emits_state_change_only_on_transition() {
        let (mut h, state) = BooleanStateHandler::new();
        let mut ctx = InvokeCtx::default();
        assert_eq!(h.stimulate(&Stimulus::SetState(true), &mut ctx), StimulusReply::Applied);
        assert_eq!(ctx.changed, vec![im::ATTR_BS_STATE_VALUE]);
        assert_eq!(ctx.events.len(), 1);
        assert_eq!(ctx.events[0].event, im::EVENT_BS_STATE_CHANGE);
        assert_eq!(mat_controller::im::tlv_to_json(ctx.events[0].data_tlv.as_ref().unwrap()).unwrap(), serde_json::json!({"0": true}));
        assert!(state.load(Ordering::SeqCst));
        let mut ctx = InvokeCtx::default();
        h.stimulate(&Stimulus::SetState(true), &mut ctx);
        assert!(ctx.changed.is_empty() && ctx.events.is_empty());
        assert_eq!(h.stimulate(&Stimulus::Press(PressKind::Short), &mut InvokeCtx::default()), StimulusReply::Unsupported);
        assert_eq!(h.cluster_id(), im::CLUSTER_BOOLEAN_STATE);
        assert_eq!(h.attributes(), vec![im::ATTR_BS_STATE_VALUE]);
        assert_eq!(h.events(), vec![im::EVENT_BS_STATE_CHANGE]);
    }
```

`core/bridge.rs` tests に追加:

```rust
    #[test]
    fn switch_and_contact_sensor_kinds_yield_their_cluster_sets() {
        let sw = build_bridged_endpoint(DeviceKind::Switch, "Btn", "uid-2", 3, &GroupMembershipStore::new());
        let ids: Vec<u32> = sw.clusters.iter().map(|c| c.cluster_id()).collect();
        assert_eq!(ids, vec![im::CLUSTER_DESCRIPTOR, im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION, im::CLUSTER_IDENTIFY, im::CLUSTER_GROUPS, im::CLUSTER_SWITCH]);
        assert!(matches!(sw.state, BridgedState::Switch(_)));
        let cs = build_bridged_endpoint(DeviceKind::ContactSensor, "Door", "uid-3", 4, &GroupMembershipStore::new());
        let ids: Vec<u32> = cs.clusters.iter().map(|c| c.cluster_id()).collect();
        assert_eq!(ids, vec![im::CLUSTER_DESCRIPTOR, im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION, im::CLUSTER_IDENTIFY, im::CLUSTER_GROUPS, im::CLUSTER_BOOLEAN_STATE]);
        assert!(matches!(cs.state, BridgedState::Contact(_)));
    }

    #[test]
    fn deserializes_new_kinds_from_their_config_spelling() {
        assert_eq!(serde_json::from_str::<DeviceKind>("\"switch\"").unwrap(), DeviceKind::Switch);
        assert_eq!(serde_json::from_str::<DeviceKind>("\"contact-sensor\"").unwrap(), DeviceKind::ContactSensor);
    }

    #[test]
    fn switch_descriptor_lists_generic_switch_and_bridged_node() {
        let sw = build_bridged_endpoint(DeviceKind::Switch, "Btn", "uid-2", 3, &GroupMembershipStore::new());
        let desc = sw.clusters.iter().find(|c| c.cluster_id() == im::CLUSTER_DESCRIPTOR).unwrap();
        let v = mat_controller::im::tlv_to_json(&desc.read(im::ATTR_DEVICE_TYPE_LIST, &crate::core::datamodel::ReadCtx::default()).unwrap()).unwrap();
        // DeviceTypeStruct {0: DeviceType, 1: Revision} の配列。
        let types: Vec<u64> = v.as_array().unwrap().iter().map(|d| d["0"].as_u64().unwrap()).collect();
        assert_eq!(types, vec![u64::from(im::DEVICE_TYPE_GENERIC_SWITCH), u64::from(im::DEVICE_TYPE_BRIDGED_NODE)]);
    }
```

既存テスト `onoff_state_handle_reflects_the_registered_onoff_handler` は `endpoint.onoff_state` → `let BridgedState::OnOff(state) = &endpoint.state else { panic!() }` に書き換える。

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-device --lib core::` → コンパイルエラー。

- [ ] **Step 3: 実装**

`core/generic_switch.rs`（`onoff.rs` の流儀に合わせる）:

```rust
//! Generic Switch クラスタサーバ (spec §1.13, cluster 0x003B)。momentary switch
//! （MS|MSR|MSL|MSM）。ボタン押下は `stimulate(Press)` で注入され、spec §1.13.6 の
//! イベント列を `InvokeCtx::events` に積む。コマンドは無い。
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use mat_controller::im::{self, EventPriority};
use mat_controller::tlv::{Tag, Writer};
use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::events::EmittedEvent;
use crate::core::stimulus::{PressKind, Stimulus, StimulusReply};

pub struct GenericSwitchHandler { position: Arc<AtomicU8> }

impl GenericSwitchHandler {
    pub const NUMBER_OF_POSITIONS: u8 = 2;
    pub const MULTI_PRESS_MAX: u8 = 3;
    pub fn new() -> (Self, Arc<AtomicU8>) { let p = Arc::new(AtomicU8::new(0)); (Self { position: Arc::clone(&p) }, p) }
}

fn uint_tlv(v: u64) -> Vec<u8> { let mut w = Writer::new(); w.put_uint(Tag::Anonymous, v); w.finish() }
fn one_field(v: u8) -> Vec<u8> { let mut w = Writer::new(); w.start_struct(Tag::Anonymous); w.put_uint(Tag::Context(0), u64::from(v)); w.end_container(); w.finish() }
fn two_fields(a: u8, b: u8) -> Vec<u8> { let mut w = Writer::new(); w.start_struct(Tag::Anonymous); w.put_uint(Tag::Context(0), u64::from(a)); w.put_uint(Tag::Context(1), u64::from(b)); w.end_container(); w.finish() }
fn info(event: u32, data_tlv: Vec<u8>) -> EmittedEvent { EmittedEvent { event, priority: EventPriority::Info, data_tlv: Some(data_tlv) } }

impl ClusterHandler for GenericSwitchHandler {
    fn cluster_id(&self) -> u32 { im::CLUSTER_SWITCH }
    fn revision(&self) -> u16 { 2 }
    fn feature_map(&self) -> u32 { im::SWITCH_FEATURE_MOMENTARY | im::SWITCH_FEATURE_MOMENTARY_RELEASE | im::SWITCH_FEATURE_MOMENTARY_LONG_PRESS | im::SWITCH_FEATURE_MOMENTARY_MULTI_PRESS }
    fn attributes(&self) -> Vec<u32> { vec![im::ATTR_SWITCH_NUMBER_OF_POSITIONS, im::ATTR_SWITCH_CURRENT_POSITION, im::ATTR_SWITCH_MULTI_PRESS_MAX] }
    fn events(&self) -> Vec<u32> { vec![im::EVENT_SWITCH_INITIAL_PRESS, im::EVENT_SWITCH_LONG_PRESS, im::EVENT_SWITCH_SHORT_RELEASE, im::EVENT_SWITCH_LONG_RELEASE, im::EVENT_SWITCH_MULTI_PRESS_ONGOING, im::EVENT_SWITCH_MULTI_PRESS_COMPLETE] }
    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_SWITCH_NUMBER_OF_POSITIONS => Some(uint_tlv(u64::from(Self::NUMBER_OF_POSITIONS))),
            im::ATTR_SWITCH_CURRENT_POSITION => Some(uint_tlv(u64::from(self.position.load(Ordering::SeqCst)))),
            im::ATTR_SWITCH_MULTI_PRESS_MAX => Some(uint_tlv(u64::from(Self::MULTI_PRESS_MAX))),
            _ => None,
        }
    }
    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply { InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND) }
    fn stimulate(&mut self, stimulus: &Stimulus, ctx: &mut InvokeCtx) -> StimulusReply {
        let Stimulus::Press(kind) = stimulus else { return StimulusReply::Unsupported; };
        // 押下中は position 1、離すと 0。1 刺激で往復するので CurrentPosition は変化なし
        // （changed に積まない）— イベント列が本体。
        match kind {
            PressKind::Short => { ctx.events.push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1))); ctx.events.push(info(im::EVENT_SWITCH_SHORT_RELEASE, one_field(1))); }
            PressKind::Long => { ctx.events.push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1))); ctx.events.push(info(im::EVENT_SWITCH_LONG_PRESS, one_field(1))); ctx.events.push(info(im::EVENT_SWITCH_LONG_RELEASE, one_field(1))); }
            PressKind::Multi(n) => {
                if *n < 2 { return StimulusReply::Rejected("multi press count must be at least 2"); }
                if *n > Self::MULTI_PRESS_MAX { return StimulusReply::Rejected("multi press count exceeds MultiPressMax"); }
                ctx.events.push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1)));
                ctx.events.push(info(im::EVENT_SWITCH_SHORT_RELEASE, one_field(1)));
                for i in 2..=*n {
                    ctx.events.push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1)));
                    ctx.events.push(info(im::EVENT_SWITCH_MULTI_PRESS_ONGOING, two_fields(1, i)));
                    ctx.events.push(info(im::EVENT_SWITCH_SHORT_RELEASE, one_field(1)));
                }
                ctx.events.push(info(im::EVENT_SWITCH_MULTI_PRESS_COMPLETE, two_fields(1, *n)));
            }
        }
        StimulusReply::Applied
    }
}
```

`core/boolean_state.rs`: `BooleanStateHandler { state: Arc<AtomicBool> }`、`cluster_id` = `CLUSTER_BOOLEAN_STATE`、`revision` 1、`attributes` = `[ATTR_BS_STATE_VALUE]`、`events` = `[EVENT_BS_STATE_CHANGE]`、`read` は bool TLV、`invoke` は `UNSUPPORTED_COMMAND`、`stimulate(SetState(v))` は `swap` で前値と比較し変化時のみ `changed.push(ATTR_BS_STATE_VALUE)` + `events.push(EmittedEvent { event: EVENT_BS_STATE_CHANGE, priority: Info, data_tlv: Some(struct{0: bool}) })`、常に `Applied`。`Press` は `Unsupported`。

`core/bridge.rs`: `DeviceKind::Switch`（`#[serde(rename = "switch")]`）, `ContactSensor`（`"contact-sensor"`）、`BridgedState` enum、`BridgedEndpoint { clusters, state }`。`build_bridged_endpoint` の match に 2 分岐（Descriptor は `for_device_types(&[DEVICE_TYPE_GENERIC_SWITCH, DEVICE_TYPE_BRIDGED_NODE])` / `&[DEVICE_TYPE_CONTACT_SENSOR, DEVICE_TYPE_BRIDGED_NODE]`、BDBI / Identify / Groups は onoff-light と同じ、末尾に本体クラスタ）。モジュール doc の「種別追加は 1 値 + 1 分岐」を維持。

`core/mod.rs`: `pub mod boolean_state; pub mod events; pub mod generic_switch; pub mod stimulus;`（アルファベット順）。

`device.rs`: `onoff_states` → `states: Vec<(String, BridgedState)>`、`onoff_states.push((device.id.clone(), built.onoff_state))` → `states.push((device.id.clone(), built.state))`。doc コメントを「各 bridged device の観測ハンドル（kind ごと）」に直す。

- [ ] **Step 4: テスト通過確認** — `cargo test -p mat-device`（統合テスト含む: `device_config` は onoff-light のままなので全部通ること）。`cargo check -p mat-device --no-default-features`。clippy / fmt。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-device/src
/usr/bin/git commit -m "feat(mat-device): Generic Switch / Boolean State クラスタと bridged kind switch / contact-sensor — 押下・開閉の刺激からイベント列を生成"
```

---

### Task 6: `mat-device::net` — 購読へのイベント配信と刺激チャネル

**Files:**
- Modify: `crates/mat-device/src/net/subscription.rs`
- Create: `crates/mat-device/src/net/stimulus.rs`
- Modify: `crates/mat-device/src/net/mod.rs`（`pub mod stimulus;`）
- Modify: `crates/mat-device/src/net/runtime.rs`
- Modify: `crates/mat-device/src/device.rs`

**Interfaces:**
- Consumes: Task 4/5 の `Node::{stimulate, event_entries, has_readable_event_path, next_event_number, recent_events, read_chunks(.., trailer_follows)}`、`StoredEvent`、`Stimulus`、`StimulusOutcome`、`StimulusError`、`im::{encode_report_data_full, EventPathIn, EventEntryOut, EventReportOut}`、`SubscribeRequestIn::{event_paths, event_min}`。
- Produces:
  ```rust
  // net/subscription.rs
  pub struct ActiveSubscription { /* 既存 */ pub event_paths: Vec<EventPathIn>, pub next_event: u64, pub pending_urgent: bool }
  impl ActiveSubscription {
      pub fn note_events(&mut self, events: &[StoredEvent]);          // event_paths にマッチ & その path が urgent → pending_urgent = true
      pub fn covers_event(&self, endpoint: u16, cluster: u32, event: u32) -> Option<bool>; // None = 非対象, Some(urgent)
  }
  pub fn event_path_matches(p: &EventPathIn, endpoint: u16, cluster: u32, event: u32) -> bool;
  // net/stimulus.rs
  #[derive(Clone)] pub struct StimulusHandle { tx: tokio::sync::mpsc::Sender<StimulusRequest> }
  pub struct StimulusRequest { pub device_id: String, pub stimulus: Stimulus, pub reply: tokio::sync::oneshot::Sender<Result<StimulusOutcome, StimulusApplyError>> }
  #[derive(Debug, Clone, PartialEq, Eq)] pub enum StimulusApplyError { UnknownDevice(String), Node(StimulusError), Closed }
  impl std::fmt::Display for StimulusApplyError; impl std::error::Error for StimulusApplyError;
  impl StimulusHandle {
      pub fn channel(capacity: usize) -> (Self, tokio::sync::mpsc::Receiver<StimulusRequest>);
      pub async fn apply(&self, device_id: &str, stimulus: Stimulus) -> Result<StimulusOutcome, StimulusApplyError>;
  }
  // device.rs
  impl Device { pub fn stimulus_handle(&self) -> StimulusHandle; }
  ```

- [ ] **Step 1: 失敗するテスト（`net/subscription.rs` tests）**

```rust
    fn stored(endpoint: u16, cluster: u32, event: u32, number: u64) -> crate::core::events::StoredEvent {
        crate::core::events::StoredEvent { number, endpoint, cluster, event, priority: mat_controller::im::EventPriority::Info, system_timestamp_ms: 0, data_tlv: None }
    }

    #[tokio::test(start_paused = true)]
    async fn pending_urgent_events_are_due_at_the_min_interval() {
        let now = Instant::now();
        let mut s = ActiveSubscription { event_paths: vec![EventPathIn::WILDCARD_URGENT], ..sub(2, 60, Vec::new(), now) };
        assert_eq!(s.next_report_deadline(), now + Duration::from_secs(60) - KEEP_ALIVE_MARGIN);
        s.note_events(&[stored(2, im::CLUSTER_SWITCH, im::EVENT_SWITCH_INITIAL_PRESS, 1)]);
        assert!(s.pending_urgent);
        assert_eq!(s.next_report_deadline(), now + Duration::from_secs(2));
    }

    #[test]
    fn non_urgent_events_do_not_advance_the_deadline() {
        let now = Instant::now();
        let mut s = ActiveSubscription { event_paths: vec![EventPathIn { urgent: false, ..EventPathIn::WILDCARD_URGENT }], ..sub(0, 60, Vec::new(), now) };
        s.note_events(&[stored(2, im::CLUSTER_SWITCH, im::EVENT_SWITCH_INITIAL_PRESS, 1)]);
        assert!(!s.pending_urgent);
    }

    #[test]
    fn events_outside_the_subscribed_paths_are_ignored() {
        let now = Instant::now();
        let mut s = ActiveSubscription { event_paths: vec![EventPathIn { cluster: Some(im::CLUSTER_BOOLEAN_STATE), ..EventPathIn::WILDCARD_URGENT }], ..sub(0, 60, Vec::new(), now) };
        s.note_events(&[stored(2, im::CLUSTER_SWITCH, im::EVENT_SWITCH_INITIAL_PRESS, 1)]);
        assert!(!s.pending_urgent);
        assert_eq!(s.covers_event(2, im::CLUSTER_BOOLEAN_STATE, im::EVENT_BS_STATE_CHANGE), Some(true));
        assert_eq!(s.covers_event(2, im::CLUSTER_SWITCH, 1), None);
    }
```

既存テストの `sub(..)` ヘルパと `sub_with_paths` に `event_paths: Vec::new(), next_event: 0, pending_urgent: false` を足す。

- [ ] **Step 2: 失敗確認** — `cargo test -p mat-device --lib net::subscription` → コンパイルエラー。

- [ ] **Step 3: 実装**

`net/subscription.rs`:
- フィールド 3 つ追加。`next_report_deadline`: `if self.dirty.is_empty() && !self.pending_urgent { keep-alive } else { min }`。
- `event_path_matches(p, endpoint, cluster, event)` = 3 フィールドとも `is_none_or(==)`。
- `covers_event` = `event_paths` のうちマッチする物があれば `Some(any urgent)`、無ければ `None`。
- `note_events`: `for e in events { if let Some(true) = self.covers_event(e.endpoint, e.cluster, e.event) { self.pending_urgent = true; } }`。
- モジュール doc に「イベント: urgent path にマッチした新イベントは dirty と同じく min-interval で報告、non-urgent は次の報告に相乗り（chip ReportScheduler と同じ）」を追記。

`net/stimulus.rs`: Interfaces のとおり。`apply` は `oneshot::channel()` → `tx.send(StimulusRequest{..}).await.map_err(|_| Closed)?` → `rx.await.map_err(|_| Closed)?`。

`net/runtime.rs`:
- `NodeState` に `endpoint_by_device: HashMap<String, u16>` と `started_at: Instant`（`boot` で `Instant::now()`）。`system_timestamp_ms()` = `started_at.elapsed().as_millis() as u64`。
- `run(...)` / `boot(...)` に `stimuli: mpsc::Receiver<StimulusRequest>` と `endpoint_by_device: HashMap<String, u16>` を引数追加（`Device::run` から渡す）。`Runtime` に `stimuli` フィールド。
- `serve_forever` の `select!` に:
  ```rust
  Some(req) = self.stimuli.recv() => self.on_stimulus(req),
  ```
  （`recv()` は送信側が全部 drop されると `None` を返し続ける → `Some(..)` パターンで分岐が不成立になり select! が busy loop しないよう、`None` のときは `self.stimuli_closed = true` を立てて以後この分岐を `if !self.stimuli_closed` のガードで外す。`tokio::select!` の分岐前条件 `, if cond` 構文を使う。）
- `on_stimulus(&mut self, req: StimulusRequest)`:
  ```rust
  let result = match self.state.endpoint_by_device.get(&req.device_id).copied() {
      None => Err(StimulusApplyError::UnknownDevice(req.device_id.clone())),
      Some(endpoint) => {
          let ts = self.state.system_timestamp_ms();
          match self.state.node.stimulate(endpoint, &req.stimulus, ts) {
              Ok(out) => {
                  tracing::debug!(device = %req.device_id, endpoint, ?req.stimulus, changed = out.changed.len(), events = out.event_numbers.len(), "stimulus applied");
                  if let Some(sub) = self.state.subscription.as_mut() {
                      sub.note_changed(&out.changed);
                      sub.note_events(&self.state.node.recent_events(sub.next_event));
                  }
                  Ok(out)
              }
              Err(e) => Err(StimulusApplyError::Node(e)),
          }
      }
  };
  let _ = req.reply.send(result);
  ```
  借用: `self.state.subscription` と `self.state.node` は別フィールドなので同時借用可（`on_group_datagram` と同じ形）。
- `serve_subscribe_request`:
  - INVALID_ACTION 判定を `!node.has_readable_path(&req.paths, &read_ctx) && !node.has_readable_event_path(&req.event_paths, &read_ctx)` に。
  - priming: `let event_entries = node.event_entries(&req.event_paths, req.event_min.unwrap_or(0), &read_ctx);` → `let mut chunks = node.read_chunks(&req.paths, &read_ctx, REPORT_CHUNK_BUDGET, Some(subscription_id), !event_entries.is_empty());` → イベントを `chunk_events(&event_entries, REPORT_CHUNK_BUDGET, subscription_id)` で分割して `chunks.extend(..)`。
    ```rust
    /// priming のイベントチャンク列: 属性の `read_chunks` と同じ予算 probe（more=true 形で測る）。
    /// 最終チャンクは more=false、suppress=false（SubscribeResponse が続く）。
    fn chunk_events(entries: &[im::EventEntryOut], budget: usize, subscription_id: u32) -> Vec<Vec<u8>> {
        let mut batches: Vec<Vec<im::EventEntryOut>> = Vec::new();
        let mut current: Vec<im::EventEntryOut> = Vec::new();
        for e in entries {
            let mut candidate = current.clone(); candidate.push(e.clone());
            if im::encode_report_data_full(&[], &candidate, false, Some(subscription_id), true).len() > budget && !current.is_empty() {
                batches.push(std::mem::take(&mut current)); current.push(e.clone());
            } else { current = candidate; }
        }
        if !current.is_empty() { batches.push(current); }
        let last = batches.len().saturating_sub(1);
        batches.into_iter().enumerate().map(|(i, b)| im::encode_report_data_full(&[], &b, false, Some(subscription_id), i != last)).collect()
    }
    ```
    `ActiveSubscription` の生成に `event_paths: req.event_paths, next_event: node.next_event_number(), pending_urgent: false` を足す。
  - 注意: attribute paths が空（イベントだけの購読）で `read_chunks` は空 ReportData 1 チャンクを返す。イベントチャンクが続くので `trailer_follows=true` により more=true になる。属性・イベントとも空なら従来どおり空チャンク 1 つ。
- `send_subscription_report`: `let events = node.event_entries(&sub.event_paths, sub.next_event, &read_ctx);`（`event_paths` 空なら空）→ `let payload = im::encode_report_data_full(&entries, &events, false, Some(sub.id), false);`。成功時 `sub.next_event = node.next_event_number(); sub.pending_urgent = false;`。ログの `reports` に `events = events.len()` を足す。keep-alive 判定 `keep_alive = entries.is_empty() && events.is_empty()`。
- `serve_read_request_chunked` の `read_chunks` 呼び出しに `false` を足す（Task 4 で済んでいれば不要）。

`device.rs`:
- `Device` に `stimulus_handle: StimulusHandle`, `stimuli: Option<mpsc::Receiver<StimulusRequest>>`（`run(self)` で take）, `endpoint_by_device: HashMap<String, u16>`。`new` で `StimulusHandle::channel(16)`、`bridged_eps` の zip から map を作る。`node.set_event_log(EventLog::new(unix_ms_now(), EventLog::DEFAULT_CAP))`（`std::time::SystemTime::now().duration_since(UNIX_EPOCH)` の ms、失敗時 1）。
- `pub fn stimulus_handle(&self) -> StimulusHandle { self.stimulus_handle.clone() }`。
- `run(self)` → `runtime::run(.., self.stimuli.expect("run is called once"), self.endpoint_by_device)`。

- [ ] **Step 4: テスト通過確認** — `cargo test -p mat-device`（既存統合テスト `subscribe_loop.rs` / `subscribe_denied.rs` を含む全通過 = attribute 購読の無退行）。clippy / fmt。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/mat-device/src
/usr/bin/git commit -m "feat(mat-device): 購読へのイベント配信と刺激チャネル — priming の EventFilters、urgent/non-urgent 報告規則、Device::stimulus_handle"
```

---

### Task 7: `mat-device` loopback 統合テスト — 刺激 → イベント購読で受信

**Files:**
- Modify: `crates/mat-device/tests/support/mod.rs`（`device_config_with`）
- Create: `crates/mat-device/tests/events_subscribe.rs`

**Interfaces:**
- Consumes: `SecureSession::{subscribe, next_subscription_report_full}`、`im::SubscribeSpec`、`Device::stimulus_handle`、`Stimulus`。
- Produces: `pub fn device_config_with(store_dir: PathBuf, devices: Vec<VirtualDeviceConfig>) -> DeviceConfig`（`device_config` はこれを onoff-light 1 台で呼ぶラッパ）。`#[allow(dead_code)]` を付ける。

- [ ] **Step 1: テストを書く**

```rust
//! イベント購読の閉ループ: `switch` + `contact-sensor` を持つ Device に対し、
//! mat-controller の `subscribe`（EventRequests wildcard urgent）で購読し、刺激
//! （ボタン押下 / 開閉）が EventReport として届くことを判定する。属性購読の
//! 無退行は既存 `subscribe_loop.rs` が担当。
#![cfg(feature = "net")]
use std::net::SocketAddr;
use std::time::Duration;
use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, EventPathIn, EventReport, SubscribeSpec};
use mat_device::core::bridge::DeviceKind;
use mat_device::core::stimulus::{PressKind, Stimulus};
use mat_device::device::{Device, VirtualDeviceConfig};
mod support;
use support::{commission_directly, device_config_with};

const ADMIN_NODE_ID: u64 = 778_900;
const REPORT_WAIT: Duration = Duration::from_secs(10);
const SWITCH_EP: u16 = 2;   // 宣言順: switch → EP2, contact → EP3
const CONTACT_EP: u16 = 3;

fn devices() -> Vec<VirtualDeviceConfig> {
    vec![
        VirtualDeviceConfig { id: "btn".into(), kind: DeviceKind::Switch, name: "Button".into() },
        VirtualDeviceConfig { id: "door".into(), kind: DeviceKind::ContactSensor, name: "Door".into() },
    ]
}

fn event_ids(events: &[EventReport]) -> Vec<(u16, u32, u32)> {
    events.iter().filter_map(|e| match e { EventReport::Data(d) => Some((d.endpoint, d.cluster, d.event)), _ => None }).collect()
}

#[tokio::test]
async fn button_press_and_contact_change_arrive_as_events() {
    let store_dir = tempfile::tempdir().unwrap();
    let device = Device::new(device_config_with(store_dir.path().to_path_buf(), devices())).unwrap();
    let addr = SocketAddr::new(std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), device.local_addr().port());
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der")).unwrap();
    let handle = device.stimulus_handle();
    let device_task = tokio::spawn(async move { let _ = device.run().await; });

    let fabric = CommissioningFabric::generate(0x2233_4466, ADMIN_NODE_ID).unwrap();
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // 1. イベント wildcard(urgent) + 属性は booleanstate だけ。priming にイベントは無い。
    let spec = SubscribeSpec { min_interval_floor_s: 0, max_interval_ceiling_s: 5, keep_subscriptions: false,
        clusters: vec![im::CLUSTER_BOOLEAN_STATE], event_paths: vec![EventPathIn::WILDCARD_URGENT], event_min: None };
    let o = session.subscribe(&spec, &cfg).await.expect("subscribe");
    assert!(o.priming_events.is_empty(), "fresh device has no events: {:?}", o.priming_events);
    assert!(o.priming.iter().flat_map(|m| &m.reports).any(|r| r.attribute == Some(im::ATTR_BS_STATE_VALUE)));

    // 2. 短押し → InitialPress + ShortRelease が番号昇順で届く。
    let out = handle.apply("btn", Stimulus::Press(PressKind::Short)).await.expect("press applied");
    assert_eq!(out.event_numbers.len(), 2);
    let rep = session.next_subscription_report_full(REPORT_WAIT, &cfg).await.expect("press report");
    assert_eq!(rep.data.subscription_id, Some(o.response.subscription_id));
    assert_eq!(event_ids(&rep.events), vec![(SWITCH_EP, im::CLUSTER_SWITCH, im::EVENT_SWITCH_INITIAL_PRESS), (SWITCH_EP, im::CLUSTER_SWITCH, im::EVENT_SWITCH_SHORT_RELEASE)]);
    let nums: Vec<u64> = rep.events.iter().filter_map(|e| match e { EventReport::Data(d) => Some(d.event_number), _ => None }).collect();
    assert_eq!(nums, out.event_numbers);
    assert!(rep.data.reports.is_empty(), "a press changes no subscribed attribute");

    // 3. 開閉 → 同一 report に StateValue 属性と StateChange イベント。
    handle.apply("door", Stimulus::SetState(true)).await.expect("state applied");
    let rep = session.next_subscription_report_full(REPORT_WAIT, &cfg).await.expect("contact report");
    assert!(rep.data.reports.iter().any(|r| r.endpoint == Some(CONTACT_EP) && r.attribute == Some(im::ATTR_BS_STATE_VALUE) && r.data == Some(serde_json::json!(true))));
    assert_eq!(event_ids(&rep.events), vec![(CONTACT_EP, im::CLUSTER_BOOLEAN_STATE, im::EVENT_BS_STATE_CHANGE)]);
    let last = match &rep.events[0] { EventReport::Data(d) => { assert_eq!(d.data, Some(serde_json::json!({"0": true}))); d.event_number } _ => unreachable!() };

    // 4. 刺激エラー。
    assert!(matches!(handle.apply("nope", Stimulus::SetState(true)).await, Err(mat_device::net::stimulus::StimulusApplyError::UnknownDevice(_))));
    assert!(matches!(handle.apply("btn", Stimulus::SetState(true)).await, Err(mat_device::net::stimulus::StimulusApplyError::Node(_))));

    // 5. 再購読: event_min = last+1 → priming events 空。event_min 無し → ログ全量 3 件。
    let o2 = session.subscribe(&SubscribeSpec { event_min: Some(last + 1), ..spec.clone() }, &cfg).await.expect("resubscribe");
    assert!(o2.priming_events.is_empty());
    let o3 = session.subscribe(&SubscribeSpec { event_min: None, ..spec.clone() }, &cfg).await.expect("resubscribe all");
    assert_eq!(o3.priming_events.len(), 3);
    assert_eq!(event_ids(&o3.priming_events)[2], (CONTACT_EP, im::CLUSTER_BOOLEAN_STATE, im::EVENT_BS_STATE_CHANGE));

    // 6. イベントだけの購読（属性 path なし）も受理される。
    let o4 = session.subscribe(&SubscribeSpec { clusters: vec![], event_paths: vec![EventPathIn { cluster: Some(im::CLUSTER_SWITCH), ..EventPathIn::WILDCARD_URGENT }], event_min: Some(0), ..spec.clone() }, &cfg).await.expect("events-only subscribe");
    assert_eq!(o4.priming_events.len(), 2);

    device_task.abort();
    let _ = device_task.await;
}
```

注: 手順 6 は `clusters: vec![]` で AttributeRequests が full wildcard になる（`encode_subscribe_request_full` の仕様）。「属性 path が本当に空」の購読はワイヤに乗らないので、この手順は「イベント絞り込み購読」の検証と読み替え、コメントにそう書く。

`support/mod.rs`: `device_config_with(store_dir, devices)` を追加し `device_config` をそれ経由に。

- [ ] **Step 2: 実行して通す** — `cargo test -p mat-device --test events_subscribe -- --nocapture`。失敗したら Task 4〜6 の実装を直す（テストの期待は spec 由来なので原則テスト側は変えない）。既存 `cargo test -p mat-device` 全通過も確認。

- [ ] **Step 3: Commit**

```bash
/usr/bin/git add crates/mat-device/tests
/usr/bin/git commit -m "test(mat-device): イベント購読の loopback 統合テスト — 押下/開閉の刺激が EventReport で届く、EventMin 再購読"
```

---

### Task 8: `matv --stdin-control`、ドキュメント、ARCHITECTURE 更新

**Files:**
- Create: `crates/matv/src/control.rs`
- Modify: `crates/matv/src/main.rs`, `crates/matv/tests/cli.rs`
- Modify: `README.md`（matv の kind 表 / stdin-control）, `docs/commands.md`（Listen 節に「イベントはフェーズ B」の 1 段落は書かない — commands.md は mat の CLI 契約なので触らず、README と ARCHITECTURE のみ）, `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `Device::stimulus_handle`, `StimulusHandle::apply`, `Stimulus`, `PressKind`, `StimulusApplyError`.
- Produces（`control.rs`）:
  ```rust
  #[derive(Debug, PartialEq, Eq)] pub struct ControlLine { pub device: String, pub stimulus: Stimulus }
  pub fn parse_control_line(line: &str) -> Result<ControlLine, String>;
  pub fn applied_json(device: &str, stimulus: &Stimulus, event_numbers: &[u64]) -> serde_json::Value;  // {"device","applied":"press"|"state","event_numbers":[..]}
  pub fn error_json(kind: &str, detail: &str) -> serde_json::Value;                                  // {"error":{"kind","detail"}}
  pub async fn run_stdin_control(handle: StimulusHandle);   // stdin 行ループ。EOF で return
  ```

- [ ] **Step 1: 失敗するテスト（`control.rs` の unit + `tests/cli.rs`）**

```rust
    #[test]
    fn parses_press_and_state_lines() {
        assert_eq!(parse_control_line(r#"{"device":"btn","press":"short"}"#).unwrap(), ControlLine { device: "btn".into(), stimulus: Stimulus::Press(PressKind::Short) });
        assert_eq!(parse_control_line(r#"{"device":"btn","press":"long"}"#).unwrap().stimulus, Stimulus::Press(PressKind::Long));
        assert_eq!(parse_control_line(r#"{"device":"btn","press":"multi","count":2}"#).unwrap().stimulus, Stimulus::Press(PressKind::Multi(2)));
        assert_eq!(parse_control_line(r#"{"device":"door","state":false}"#).unwrap().stimulus, Stimulus::SetState(false));
    }

    #[test]
    fn rejects_malformed_lines_with_a_reason() {
        assert!(parse_control_line("not json").unwrap_err().contains("JSON"));
        assert!(parse_control_line(r#"{"press":"short"}"#).unwrap_err().contains("device"));
        assert!(parse_control_line(r#"{"device":"btn"}"#).unwrap_err().contains("press"));
        assert!(parse_control_line(r#"{"device":"btn","press":"multi"}"#).unwrap_err().contains("count"));
        assert!(parse_control_line(r#"{"device":"btn","press":"triple"}"#).unwrap_err().contains("triple"));
        assert!(parse_control_line(r#"{"device":"btn","press":"short","state":true}"#).unwrap_err().contains("both"));
    }

    #[test]
    fn json_shapes() {
        assert_eq!(applied_json("btn", &Stimulus::Press(PressKind::Short), &[5, 6]), serde_json::json!({"device":"btn","applied":"press","event_numbers":[5,6]}));
        assert_eq!(applied_json("door", &Stimulus::SetState(true), &[]), serde_json::json!({"device":"door","applied":"state","event_numbers":[]}));
        assert_eq!(error_json("not_found", "x"), serde_json::json!({"error":{"kind":"not_found","detail":"x"}}));
    }
```

`tests/cli.rs` に追加（既存 `prints_setup_payload_and_stays_up` と同じ spawn 骨格。stdin を `Stdio::piped()` にし、1 行目を読んだ後に `{"device":"btn","press":"short"}\n` を書き、2 行目が `applied` 行であること。さらに `{"device":"nope","state":true}\n` を書き、stderr に `"kind":"not_found"` を含む行が出ること。stderr は `RUST_LOG` 未設定でログが出ない前提で 1 行読む）:

```rust
#[test]
fn stdin_control_applies_a_press_and_reports_unknown_device() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("matv.toml");
    std::fs::write(&cfg, format!(
        "passcode = 20202021\ndiscriminator = 3840\nvendor_id = 65521\nproduct_id = 32768\nport = 0\ngroup_port = 0\nstore = \"{}\"\niface = \"lo\"\n\n[[device]]\nid = \"btn\"\nkind = \"switch\"\nname = \"Button\"\n\n[[device]]\nid = \"door\"\nkind = \"contact-sensor\"\nname = \"Door\"\n",
        dir.path().display())).unwrap();
    let mut child = Command::cargo_bin("matv").unwrap()
        .arg("--config").arg(&cfg).arg("--stdin-control")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("spawn matv");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || { let mut r = BufReader::new(stdout); loop { let mut l = String::new(); match r.read_line(&mut l) { Ok(0) | Err(_) => break, Ok(_) => { let _ = tx.send(l); } } } });
    let (etx, erx) = mpsc::channel();
    std::thread::spawn(move || { let mut r = BufReader::new(stderr); loop { let mut l = String::new(); match r.read_line(&mut l) { Ok(0) | Err(_) => break, Ok(_) => { let _ = etx.send(l); } } } });
    let first = rx.recv_timeout(STDOUT_LINE_TIMEOUT).expect("setup payload");
    assert!(first.contains("qr_payload"));
    use std::io::Write;
    writeln!(stdin, r#"{{"device":"btn","press":"short"}}"#).unwrap();
    let applied: serde_json::Value = serde_json::from_str(rx.recv_timeout(STDOUT_LINE_TIMEOUT).expect("applied line").trim()).unwrap();
    assert_eq!(applied["device"], "btn");
    assert_eq!(applied["applied"], "press");
    assert_eq!(applied["event_numbers"].as_array().unwrap().len(), 2);
    writeln!(stdin, r#"{{"device":"nope","state":true}}"#).unwrap();
    let err = erx.recv_timeout(STDOUT_LINE_TIMEOUT).expect("error line");
    assert!(err.contains("\"kind\":\"not_found\""), "stderr: {err}");
    child.kill().unwrap(); let _ = child.wait();
}
```

- [ ] **Step 2: 失敗確認** — `cargo test -p matv` → コンパイルエラー / `--stdin-control` 未知フラグで失敗。

- [ ] **Step 3: 実装**

`control.rs`:

```rust
//! `--stdin-control`: stdin の JSON 行をデバイスへの刺激に変換する開発用フック。
//! 1 行 = 1 刺激。成功は stdout に JSON 1 行、失敗は stderr に mat 形式の error JSON 1 行。
use mat_device::core::stimulus::{PressKind, Stimulus};
use mat_device::net::stimulus::{StimulusApplyError, StimulusHandle};

#[derive(Debug, PartialEq, Eq)]
pub struct ControlLine { pub device: String, pub stimulus: Stimulus }

pub fn parse_control_line(line: &str) -> Result<ControlLine, String> {
    let v: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("control line is not JSON: {e}"))?;
    let device = v.get("device").and_then(|d| d.as_str()).ok_or("control line needs a string \"device\"")?.to_string();
    let press = v.get("press").and_then(|p| p.as_str());
    let state = v.get("state").and_then(|s| s.as_bool());
    let stimulus = match (press, state) {
        (Some(_), Some(_)) => return Err("control line has both \"press\" and \"state\"; use one".into()),
        (None, None) => return Err("control line needs \"press\" or \"state\"".into()),
        (None, Some(s)) => Stimulus::SetState(s),
        (Some("short"), None) => Stimulus::Press(PressKind::Short),
        (Some("long"), None) => Stimulus::Press(PressKind::Long),
        (Some("multi"), None) => {
            let n = v.get("count").and_then(|c| c.as_u64()).ok_or("\"press\":\"multi\" needs an integer \"count\"")?;
            Stimulus::Press(PressKind::Multi(u8::try_from(n).map_err(|_| "\"count\" out of range".to_string())?))
        }
        (Some(other), None) => return Err(format!("unknown press kind {other:?} (short|long|multi)")),
    };
    Ok(ControlLine { device, stimulus })
}

pub fn applied_json(device: &str, stimulus: &Stimulus, event_numbers: &[u64]) -> serde_json::Value {
    let applied = match stimulus { Stimulus::Press(_) => "press", Stimulus::SetState(_) => "state" };
    serde_json::json!({"device": device, "applied": applied, "event_numbers": event_numbers})
}

pub fn error_json(kind: &str, detail: &str) -> serde_json::Value { serde_json::json!({"error": {"kind": kind, "detail": detail}}) }

pub async fn run_stdin_control(handle: StimulusHandle) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() { continue; }
        let parsed = match parse_control_line(&line) {
            Ok(p) => p,
            Err(e) => { eprintln!("{}", error_json("parse_error", &e)); continue; }
        };
        match handle.apply(&parsed.device, parsed.stimulus).await {
            Ok(out) => println!("{}", applied_json(&parsed.device, &parsed.stimulus, &out.event_numbers)),
            Err(StimulusApplyError::UnknownDevice(d)) => eprintln!("{}", error_json("not_found", &format!("no [[device]] with id {d:?}"))),
            Err(StimulusApplyError::Node(e)) => eprintln!("{}", error_json("other", &e.to_string())),
            Err(StimulusApplyError::Closed) => { eprintln!("{}", error_json("other", "device runtime is gone")); return; }
        }
    }
    tracing::info!("matv: stdin closed, control hook stopped");
}
```

`matv/Cargo.toml` の tokio features に `"io-std", "io-util"` を追加。`main.rs`: `mod control;`、`Cli` に `#[arg(long)] stdin_control: bool`、`run(cfg, stdin_control)` で `Device::new` の後 `let handle = device.stimulus_handle();`、`if stdin_control { tokio::spawn(control::run_stdin_control(handle)); }`（`select!` の前）。`FileDeviceConfig.kind` doc に新 kind を追記。

`README.md` の「Try it without hardware」に kind 表と stdin-control の例を追記:

```markdown
`kind` is one of `onoff-light`, `switch` (a momentary Generic Switch — short /
long / multi press events) or `contact-sensor` (Boolean State — `StateChange`
events). Virtual buttons and sensors are driven from stdin with
`matv --config matv.toml --stdin-control`, one JSON line per stimulus:

```json
{"device": "btn1", "press": "short"}
{"device": "btn1", "press": "multi", "count": 2}
{"device": "door", "state": true}
```

Each applied stimulus prints `{"device":..,"applied":..,"event_numbers":[..]}`
on stdout; a bad line prints a `{"error":{"kind":..}}` line on stderr and the
hook keeps reading. Events reach a subscriber through the subscription's
EventReports (`mat listen` support lands in a later release).
```

`ARCHITECTURE.md`:
- 「Phase 5 拡張 — matd 常駐 Subscribe + `mat listen`」の「v1 スコープ外（将来）」箇条の EventReport 部分を「→ 2026-09-06 にスコープへ取り込み（下記「Phase 5 拡張 — イベント購読 フェーズ A」）。残る将来項目: `DataVersionFilter`、LIT ICD、状態スナップショット / リプレイ」に書き換える。
- 新節「### Phase 5 拡張 — イベント購読 フェーズ A（mat-controller / mat-device / matv、2026-09-06）」を op 単一ソース化の節の前に追加: spec パス、入った物（§1 の A 列）、互換境界（§3.4）、urgent 規則、EventNumber 初期値、フェーズ B の入口（plan パスと着手条件）を 10 行程度で。
- 「Do not」節の「Bring a Matter bridge…」の隣に変更はなし。

- [ ] **Step 4: 全体検証** — `task check`（fmt:check + clippy + test 全 crate）。`cargo check -p mat-device --no-default-features`。

- [ ] **Step 5: Commit**

```bash
/usr/bin/git add crates/matv README.md ARCHITECTURE.md
/usr/bin/git commit -m "feat(matv): --stdin-control でボタン押下 / 開閉を注入する開発用フック、kind switch / contact-sensor を文書化、ARCHITECTURE にイベント購読フェーズ A を記録"
```

---

## Self-Review

- **Spec coverage**: §2 ワイヤ → Task 1/2。§3.1–3.2 → Task 1/2。§3.3 → Task 3。§3.4 → Global Constraints + Task 1–3 の byte-equal / ラッパ。§4.1 → Task 4。§4.2 → Task 4。§4.3 → Task 5。§4.4 → Task 6。§4.5 → Task 8。§5 テスト 1–7 → Task 1–8（7 = Task 8 Step 4）。§6/§8 フェーズ B → 別ドキュメント（本計画外、ARCHITECTURE の入口記述は Task 8）。
- **Placeholder**: なし（Task 6 の runtime 変更は既存関数名・引数で具体化済み）。
- **型整合**: `EventPathIn`（im）/ `EventEntryOut` / `EventReportOut` / `StoredEvent` / `EmittedEvent` / `Stimulus` / `StimulusOutcome` / `StimulusError` / `StimulusApplyError` / `StimulusHandle` / `SubscribeSpec` / `SubscribeOutcome` / `SubscriptionReport` の名前は全タスクで同一。`read_chunks` の第 5 引数 `trailer_follows: bool` は Task 4 で追加し Task 6 で使用。
