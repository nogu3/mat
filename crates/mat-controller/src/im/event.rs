//! イベント関連の IM 型とコーデック（spec §8.9.2.2 EventPathIB / §8.9.2.4
//! EventFilterIB / §8.9.2.6 EventDataIB / EventStatusIB）。
use super::read::encode_attribute_report_ib;
use super::{expect_struct_start, skip_container, ImError, ReportEntryOut, IM_REVISION};
use crate::tlv::{Reader, Tag, Value, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum EventPriority {
    Debug = 0,
    Info = 1,
    Critical = 2,
}

impl EventPriority {
    pub fn from_wire(v: u64) -> Result<Self, ImError> {
        match v {
            0 => Ok(Self::Debug),
            1 => Ok(Self::Info),
            2 => Ok(Self::Critical),
            _ => Err(ImError::Malformed("unknown event priority")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventPathIn {
    pub endpoint: Option<u16>,
    pub cluster: Option<u32>,
    pub event: Option<u32>,
    pub urgent: bool,
}

impl EventPathIn {
    pub const WILDCARD_URGENT: EventPathIn = EventPathIn {
        endpoint: None,
        cluster: None,
        event: None,
        urgent: true,
    };
}

/// EventPathIB (list): {0: Node?, 1: Endpoint?, 2: Cluster?, 3: Event?, 4: IsUrgent?}. Node は出さない。
pub(super) fn encode_event_path_ib(w: &mut Writer, tag: Tag, path: &EventPathIn) {
    w.start_list(tag);
    if let Some(e) = path.endpoint {
        w.put_uint(Tag::Context(1), u64::from(e));
    }
    if let Some(c) = path.cluster {
        w.put_uint(Tag::Context(2), u64::from(c));
    }
    if let Some(ev) = path.event {
        w.put_uint(Tag::Context(3), u64::from(ev));
    }
    if path.urgent {
        w.put_bool(Tag::Context(4), true);
    }
    w.end_container();
}

/// EventPathIB のデコード。`ListStart` を読んだ後の呼び出しを前提とする。
pub(super) fn decode_event_path_ib(r: &mut Reader) -> Result<EventPathIn, ImError> {
    let mut p = EventPathIn::default();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated event path"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => {
                p.endpoint = Some(
                    u16::try_from(v).map_err(|_| ImError::Malformed("endpoint out of range"))?,
                )
            }
            (Tag::Context(2), Value::Uint(v)) => {
                p.cluster = Some(
                    u32::try_from(v).map_err(|_| ImError::Malformed("cluster id out of range"))?,
                )
            }
            (Tag::Context(3), Value::Uint(v)) => {
                p.event = Some(
                    u32::try_from(v).map_err(|_| ImError::Malformed("event id out of range"))?,
                )
            }
            (Tag::Context(4), Value::Bool(b)) => p.urgent = b,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(p)
}

/// EventRequests (array[EventPathIB])。`ArrayStart` を読んだ後の呼び出しを前提とする。
pub(super) fn decode_event_requests(r: &mut Reader) -> Result<Vec<EventPathIn>, ImError> {
    let mut out = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated event requests"))?;
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
/// （Node フィルタは spec 上任意でこの実装は自ノードのみ）。`ArrayStart` を読んだ後の呼び出しを前提とする。
pub(super) fn decode_event_filters(r: &mut Reader) -> Result<Option<u64>, ImError> {
    let mut min = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated event filters"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => loop {
                let f = r
                    .next()?
                    .ok_or(ImError::Malformed("truncated event filter"))?;
                match (f.tag, f.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(1), Value::Uint(v)) => {
                        if min.is_none() {
                            min = Some(v);
                        }
                    }
                    (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                        skip_container(r)?
                    }
                    _ => {}
                }
            },
            Value::ArrayStart | Value::ListStart => skip_container(r)?,
            _ => return Err(ImError::Malformed("unexpected element in event filters")),
        }
    }
    Ok(min)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventTimestamp {
    Epoch(u64),
    System(u64),
    DeltaEpoch(u64),
    DeltaSystem(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventData {
    pub endpoint: u16,
    pub cluster: u32,
    pub event: u32,
    pub event_number: u64,
    pub priority: EventPriority,
    pub timestamp: Option<EventTimestamp>,
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventReport {
    Data(EventData),
    Status {
        endpoint: Option<u16>,
        cluster: Option<u32>,
        event: Option<u32>,
        status: u8,
    },
}

/// ReportData の EventReports(tag 2) だけを読む。tag 1 等は読み飛ばす。Delta 形は同一メッセージ内の
/// 直前 Data イベントの絶対値に解決（先頭が Delta なら Delta のまま）。
pub fn decode_event_reports(payload: &[u8]) -> Result<Vec<EventReport>, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut out = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated report data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::ArrayStart) => {
                let mut prev: Option<EventTimestamp> = None;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated event reports"))?;
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
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    Ok(out)
}

/// EventReportIB = {0: EventStatusIB} | {1: EventDataIB}。StructStart 既読前提。
fn decode_event_report_ib(
    r: &mut Reader,
    prev: &mut Option<EventTimestamp>,
) -> Result<EventReport, ImError> {
    let mut out = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated event report"))?;
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
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated event status"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => path = decode_event_path_ib(r)?,
            (Tag::Context(1), Value::StructStart) => loop {
                let s = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
                match (s.tag, s.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(0), Value::Uint(v)) => {
                        status = Some(
                            u8::try_from(v)
                                .map_err(|_| ImError::Malformed("status out of range"))?,
                        )
                    }
                    (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                        skip_container(r)?
                    }
                    _ => {}
                }
            },
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(EventReport::Status {
        endpoint: path.endpoint,
        cluster: path.cluster,
        event: path.event,
        status: status.ok_or(ImError::Malformed("event status without status"))?,
    })
}

/// EventDataIB = {0: Path, 1: EventNumber, 2: Priority, 3..6: timestamp（ちょうど 1 つ）, 7: Data}
fn decode_event_data_ib(
    r: &mut Reader,
    prev: &mut Option<EventTimestamp>,
) -> Result<EventData, ImError> {
    let mut path = EventPathIn::default();
    let (mut number, mut priority, mut ts, mut data) = (None, None, None, None);
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated event data"))?;
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
                data = Some(super::json::tlv_element_to_json(
                    r,
                    crate::tlv::Element {
                        tag: el.tag,
                        value: v,
                    },
                )?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    let resolved = match (ts, *prev) {
        (Some(EventTimestamp::DeltaEpoch(d)), Some(EventTimestamp::Epoch(p))) => {
            Some(EventTimestamp::Epoch(p.wrapping_add(d)))
        }
        (Some(EventTimestamp::DeltaSystem(d)), Some(EventTimestamp::System(p))) => {
            Some(EventTimestamp::System(p.wrapping_add(d)))
        }
        (other, _) => other,
    };
    if matches!(
        resolved,
        Some(EventTimestamp::Epoch(_) | EventTimestamp::System(_))
    ) {
        *prev = resolved;
    }
    let (Some(endpoint), Some(cluster), Some(event)) = (path.endpoint, path.cluster, path.event)
    else {
        return Err(ImError::Malformed("event data with incomplete path"));
    };
    Ok(EventData {
        endpoint,
        cluster,
        event,
        event_number: number.ok_or(ImError::Malformed("event data without number"))?,
        priority: priority.ok_or(ImError::Malformed("event data without priority"))?,
        timestamp: resolved,
        data,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventReportOut {
    pub endpoint: u16,
    pub cluster: u32,
    pub event: u32,
    pub event_number: u64,
    pub priority: EventPriority,
    pub system_timestamp_ms: u64,
    pub data_tlv: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventEntryOut {
    Data(EventReportOut),
    Status {
        endpoint: u16,
        cluster: u32,
        event: u32,
        status: u8,
    },
}

/// server 側 ReportData: 属性 entries + イベント entries。`events` 空なら
/// `encode_report_data_entries` と byte-equal（tag 2 を省略）。
pub fn encode_report_data_full(
    attrs: &[ReportEntryOut],
    events: &[EventEntryOut],
    suppress_response: bool,
    subscription_id: Option<u32>,
    more_chunks: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    if let Some(sub_id) = subscription_id {
        w.put_uint(Tag::Context(0), u64::from(sub_id));
    }
    w.start_array(Tag::Context(1)); // AttributeReportIBs
    for entry in attrs {
        encode_attribute_report_ib(&mut w, entry);
    }
    w.end_container(); // AttributeReportIBs
    if !events.is_empty() {
        w.start_array(Tag::Context(2)); // EventReports
        for ev in events {
            w.start_struct(Tag::Anonymous); // EventReportIB
            match ev {
                EventEntryOut::Data(d) => {
                    w.start_struct(Tag::Context(1)); // EventDataIB
                    encode_event_path_ib(
                        &mut w,
                        Tag::Context(0),
                        &EventPathIn {
                            endpoint: Some(d.endpoint),
                            cluster: Some(d.cluster),
                            event: Some(d.event),
                            urgent: false,
                        },
                    );
                    w.put_uint(Tag::Context(1), d.event_number);
                    w.put_uint(Tag::Context(2), u64::from(d.priority as u8));
                    w.put_uint(Tag::Context(4), d.system_timestamp_ms);
                    if let Some(tlv) = &d.data_tlv {
                        w.put_raw_element(Tag::Context(7), tlv);
                    }
                    w.end_container(); // EventDataIB
                }
                EventEntryOut::Status {
                    endpoint,
                    cluster,
                    event,
                    status,
                } => {
                    w.start_struct(Tag::Context(0)); // EventStatusIB
                    encode_event_path_ib(
                        &mut w,
                        Tag::Context(0),
                        &EventPathIn {
                            endpoint: Some(*endpoint),
                            cluster: Some(*cluster),
                            event: Some(*event),
                            urgent: false,
                        },
                    );
                    w.start_struct(Tag::Context(1)); // StatusIB
                    w.put_uint(Tag::Context(0), u64::from(*status));
                    w.end_container(); // StatusIB
                    w.end_container(); // EventStatusIB
                }
            }
            w.end_container(); // EventReportIB
        }
        w.end_container(); // EventReports
    }
    if more_chunks {
        w.put_bool(Tag::Context(3), true); // MoreChunkedMessages
    }
    w.put_bool(Tag::Context(4), suppress_response); // SuppressResponse
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn event_path_ib_roundtrips_all_fields() {
        let path = EventPathIn {
            endpoint: Some(2),
            cluster: Some(CLUSTER_SWITCH),
            event: Some(EVENT_SWITCH_INITIAL_PRESS),
            urgent: true,
        };
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
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
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
        assert_eq!(
            p,
            EventPathIn {
                endpoint: None,
                cluster: Some(CLUSTER_BOOLEAN_STATE),
                event: None,
                urgent: false
            }
        );
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

    fn switch_press_data_tlv(new_position: u8) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(new_position));
        w.end_container();
        w.finish()
    }

    #[test]
    fn report_data_full_without_events_is_byte_equal_to_entries_encoder() {
        let attrs = vec![ReportEntryOut::Data(AttrReportOut {
            endpoint: 2,
            cluster: CLUSTER_ON_OFF,
            attribute: ATTR_ON_OFF,
            data_version: 7,
            value_tlv: vec![0x09],
        })];
        for (sup, sid, more) in [(false, Some(9), true), (true, None, false)] {
            assert_eq!(
                encode_report_data_full(&attrs, &[], sup, sid, more),
                encode_report_data_entries(&attrs, sup, sid, more)
            );
        }
    }

    #[test]
    fn event_reports_roundtrip_data_and_status() {
        let events = vec![
            EventEntryOut::Data(EventReportOut {
                endpoint: 2,
                cluster: CLUSTER_SWITCH,
                event: EVENT_SWITCH_INITIAL_PRESS,
                event_number: 100,
                priority: EventPriority::Info,
                system_timestamp_ms: 5000,
                data_tlv: Some(switch_press_data_tlv(1)),
            }),
            EventEntryOut::Status {
                endpoint: 9,
                cluster: CLUSTER_SWITCH,
                event: EVENT_SWITCH_LONG_PRESS,
                status: STATUS_UNSUPPORTED_ENDPOINT,
            },
        ];
        let payload = encode_report_data_full(&[], &events, false, Some(42), false);
        // 既存デコーダは tag 2 を読み飛ばし、attribute 無しの report として成立する（無退行）。
        let legacy = decode_report_data_message(&payload).unwrap();
        assert!(legacy.reports.is_empty());
        assert_eq!(legacy.subscription_id, Some(42));
        let got = decode_event_reports(&payload).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            EventReport::Data(EventData {
                endpoint: 2,
                cluster: CLUSTER_SWITCH,
                event: EVENT_SWITCH_INITIAL_PRESS,
                event_number: 100,
                priority: EventPriority::Info,
                timestamp: Some(EventTimestamp::System(5000)),
                data: Some(serde_json::json!({"0": 1})),
            })
        );
        assert_eq!(
            got[1],
            EventReport::Status {
                endpoint: Some(9),
                cluster: Some(CLUSTER_SWITCH),
                event: Some(EVENT_SWITCH_LONG_PRESS),
                status: STATUS_UNSUPPORTED_ENDPOINT,
            }
        );
    }

    /// 手組みの EventDataIB: Epoch / DeltaSystem / Delta 先頭 の 3 ケース。
    fn hand_event(w: &mut Writer, number: u64, ts_tag: u8, ts: u64) {
        w.start_struct(Tag::Anonymous); // EventReportIB
        w.start_struct(Tag::Context(1)); // EventDataIB
        w.start_list(Tag::Context(0));
        w.put_uint(Tag::Context(1), 1);
        w.put_uint(Tag::Context(2), u64::from(CLUSTER_BOOLEAN_STATE));
        w.put_uint(Tag::Context(3), u64::from(EVENT_BS_STATE_CHANGE));
        w.end_container();
        w.put_uint(Tag::Context(1), number);
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(ts_tag), ts);
        w.end_container();
        w.end_container();
    }

    #[test]
    fn delta_timestamps_resolve_against_the_previous_event_in_the_message() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(2));
        hand_event(&mut w, 1, 6, 10); // DeltaSystem 先頭 → 未解決のまま
        hand_event(&mut w, 2, 4, 1000); // System 1000
        hand_event(&mut w, 3, 6, 25); // DeltaSystem → System 1025
        hand_event(&mut w, 4, 3, 1_700_000_000_000); // Epoch
        hand_event(&mut w, 5, 5, 7); // DeltaEpoch → Epoch +7
        w.end_container();
        w.put_bool(Tag::Context(4), false);
        w.end_container();
        let got = decode_event_reports(&w.finish()).unwrap();
        let ts = |i: usize| match &got[i] {
            EventReport::Data(d) => d.timestamp,
            _ => panic!(),
        };
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
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        w.start_list(Tag::Context(0));
        w.put_uint(Tag::Context(1), 1);
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(3), 1);
        w.end_container();
        w.put_uint(Tag::Context(1), 1);
        w.put_uint(Tag::Context(2), 9);
        w.put_uint(Tag::Context(4), 1);
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(4), false);
        w.end_container();
        assert!(matches!(
            decode_event_reports(&w.finish()),
            Err(ImError::Malformed(_))
        ));
    }

    #[test]
    fn report_without_event_reports_decodes_to_empty() {
        let payload = encode_report_data_entries(&[], false, Some(1), false);
        assert!(decode_event_reports(&payload).unwrap().is_empty());
    }
}
