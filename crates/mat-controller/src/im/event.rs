//! イベント関連の IM 型とコーデック（spec §8.9.2.2 EventPathIB / §8.9.2.4
//! EventFilterIB / §8.9.2.6 EventDataIB / EventStatusIB）。
use super::{skip_container, ImError};
use crate::tlv::{Reader, Tag, Value, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}
