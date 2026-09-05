//! SubscribeRequest / SubscribeResponse（spec §8.5–8.6）。priming ReportData
//! 自体は read と同じ `decode_report_data_message` で読む。

use crate::tlv::{Reader, Tag, Value, Writer};

use super::{
    decode_attribute_requests, expect_struct_start, skip_container, AttrPathIn, ImError,
    IM_REVISION,
};

/// SubscribeRequestMessage (spec §8.10)。`clusters` が空なら全フィールド省略の
/// AttributePathIB 1 本（= 全 endpoint / 全 cluster / 全 attribute の full
/// wildcard）。非空なら「endpoint wildcard + cluster 指定 + attribute wildcard」
/// の AttributePathIB をクラスタ数ぶん並べる（priming 軽量化 — 弱リンクでは
/// full wildcard priming の数十往復が完走できない）。EventRequests は載せない
/// （v1 は attribute report のみ）。
pub fn encode_subscribe_request(
    min_interval_floor_s: u16,
    max_interval_ceiling_s: u16,
    keep_subscriptions: bool,
    clusters: &[u32],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), keep_subscriptions);
    w.put_uint(Tag::Context(1), u64::from(min_interval_floor_s));
    w.put_uint(Tag::Context(2), u64::from(max_interval_ceiling_s));
    w.start_array(Tag::Context(3)); // AttributeRequests
    if clusters.is_empty() {
        w.start_list(Tag::Anonymous); // AttributePathIB（全省略 = wildcard）
        w.end_container();
    } else {
        for &cluster in clusters {
            w.start_list(Tag::Anonymous); // AttributePathIB
            w.put_uint(Tag::Context(3), u64::from(cluster)); // Cluster のみ指定
            w.end_container();
        }
    }
    w.end_container();
    // IsFabricFiltered = true: read と同じ既定（encode_read_request のコメント参照）。
    w.put_bool(Tag::Context(7), true);
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

/// Decoded SubscribeRequestMessage (spec §8.10): server-side counterpart of
/// `encode_subscribe_request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeRequestIn {
    pub keep_subscriptions: bool,
    pub min_interval_floor_s: u16,
    pub max_interval_ceiling_s: u16,
    pub paths: Vec<AttrPathIn>,
    pub fabric_filtered: bool,
}

/// SubscribeRequestMessage (spec §8.10): server-side decode of
/// `encode_subscribe_request`'s payload. Shape: `{0: KeepSubscriptions,
/// 1: MinIntervalFloor, 2: MaxIntervalCeiling, 3: AttributeRequests
/// (array[AttributePathIB]), 7: IsFabricFiltered, 255: rev(無視)}`.
/// AttributeRequests の読みは `decode_read_request` と共通の
/// `decode_attribute_requests` を使う。`IsFabricFiltered` が欠落している場合は
/// `decode_read_request_message` と同じ既定（`true` — 開示が少ない側）。
pub fn decode_subscribe_request(payload: &[u8]) -> Result<SubscribeRequestIn, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut keep_subscriptions = None;
    let mut min_interval_floor_s = None;
    let mut max_interval_ceiling_s = None;
    let mut paths = Vec::new();
    let mut fabric_filtered = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated subscribe request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bool(b)) => keep_subscriptions = Some(b),
            (Tag::Context(1), Value::Uint(v)) => {
                min_interval_floor_s = Some(
                    u16::try_from(v)
                        .map_err(|_| ImError::Malformed("min interval floor out of range"))?,
                );
            }
            (Tag::Context(2), Value::Uint(v)) => {
                max_interval_ceiling_s = Some(
                    u16::try_from(v)
                        .map_err(|_| ImError::Malformed("max interval ceiling out of range"))?,
                );
            }
            (Tag::Context(3), Value::ArrayStart) => {
                // AttributeRequests
                paths = decode_attribute_requests(&mut r)?;
            }
            (Tag::Context(7), Value::Bool(b)) => fabric_filtered = Some(b),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(SubscribeRequestIn {
        keep_subscriptions: keep_subscriptions.ok_or(ImError::Malformed(
            "subscribe request without keep_subscriptions",
        ))?,
        min_interval_floor_s: min_interval_floor_s.ok_or(ImError::Malformed(
            "subscribe request without min interval floor",
        ))?,
        max_interval_ceiling_s: max_interval_ceiling_s.ok_or(ImError::Malformed(
            "subscribe request without max interval ceiling",
        ))?,
        paths,
        fabric_filtered: fabric_filtered.unwrap_or(true),
    })
}

/// SubscribeResponseMessage (spec §8.10): {0: SubscriptionId, 2: MaxInterval}.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeResponse {
    pub subscription_id: u32,
    pub max_interval_s: u16,
}

pub fn decode_subscribe_response(payload: &[u8]) -> Result<SubscribeResponse, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut id = None;
    let mut max_interval = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated subscribe response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                id = Some(
                    u32::try_from(v)
                        .map_err(|_| ImError::Malformed("subscription id out of range"))?,
                );
            }
            (Tag::Context(2), Value::Uint(v)) => {
                max_interval = Some(
                    u16::try_from(v)
                        .map_err(|_| ImError::Malformed("max interval out of range"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(SubscribeResponse {
        subscription_id: id.ok_or(ImError::Malformed("subscribe response without id"))?,
        max_interval_s: max_interval.ok_or(ImError::Malformed(
            "subscribe response without max interval",
        ))?,
    })
}

/// SubscribeResponseMessage (spec §8.10): server-side encode, mirroring
/// `decode_subscribe_response`'s shape. `{0: SubscriptionId, 2: MaxInterval,
/// 255: IM_REVISION}`.
pub fn encode_subscribe_response(subscription_id: u32, max_interval_s: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(subscription_id));
    w.put_uint(Tag::Context(2), u64::from(max_interval_s));
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn subscribe_request_wildcard_shape() {
        // SubscribeRequestMessage (spec §8.10): {0: KeepSubscriptions, 1: MinIntervalFloor,
        // 2: MaxIntervalCeiling, 3: AttributeRequests[[]], 7: IsFabricFiltered, 255: rev}
        let b = encode_subscribe_request(0, 3600, false, &[]);
        let mut r = Reader::new(&b);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let el = r.next().unwrap().unwrap(); // KeepSubscriptions
        assert_eq!(el.tag, Tag::Context(0));
        assert_eq!(el.value, Value::Bool(false));
        let el = r.next().unwrap().unwrap(); // MinIntervalFloorSeconds
        assert_eq!(el.tag, Tag::Context(1));
        assert_eq!(el.value, Value::Uint(0));
        let el = r.next().unwrap().unwrap(); // MaxIntervalCeilingSeconds
        assert_eq!(el.tag, Tag::Context(2));
        assert_eq!(el.value, Value::Uint(3600));
        let el = r.next().unwrap().unwrap(); // AttributeRequests
        assert_eq!(el.tag, Tag::Context(3));
        assert!(matches!(el.value, Value::ArrayStart));
        // wildcard AttributePathIB = 空 list（endpoint/cluster/attribute 全省略）
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        )); // path
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        )); // requests
        let el = r.next().unwrap().unwrap(); // IsFabricFiltered
        assert_eq!(el.tag, Tag::Context(7));
        assert_eq!(el.value, Value::Bool(true));
    }

    #[test]
    fn subscribe_request_cluster_paths_shape() {
        // clusters 非空: AttributePathIB を「cluster(Context(3)) のみ指定」で
        // クラスタ数ぶん並べる（endpoint/attribute は省略 = wildcard）。
        let b = encode_subscribe_request(0, 300, false, &[0x0006, 0x0402]);
        let mut r = Reader::new(&b);
        // 外殻 struct → keep/min/max を読み飛ばして AttributeRequests へ。
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        r.next().unwrap().unwrap(); // KeepSubscriptions
        r.next().unwrap().unwrap(); // MinIntervalFloorSeconds
        r.next().unwrap().unwrap(); // MaxIntervalCeilingSeconds
        let el = r.next().unwrap().unwrap(); // AttributeRequests
        assert_eq!(el.tag, Tag::Context(3));
        assert!(matches!(el.value, Value::ArrayStart));
        // path 1: list { Context(3) = 0x0006 }
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(3));
        assert_eq!(el.value, Value::Uint(0x0006));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
        // path 2: list { Context(3) = 0x0402 }
        assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(3));
        assert_eq!(el.value, Value::Uint(0x0402));
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
        // AttributeRequests 閉じ → IsFabricFiltered
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::ContainerEnd
        ));
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Context(7));
        assert_eq!(el.value, Value::Bool(true));
    }
    #[test]
    fn subscribe_response_decodes_id_and_max_interval() {
        // SubscribeResponseMessage: {0: SubscriptionId(u32), 2: MaxInterval(u16), 255: rev}
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0xDEAD_BEEF);
        w.put_uint(Tag::Context(2), 120);
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let resp = decode_subscribe_response(&w.finish()).unwrap();
        assert_eq!(resp.subscription_id, 0xDEAD_BEEF);
        assert_eq!(resp.max_interval_s, 120);
    }

    #[test]
    fn subscribe_response_without_id_is_malformed() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(2), 120);
        w.end_container();
        assert!(decode_subscribe_response(&w.finish()).is_err());
    }
    #[test]
    fn decode_subscribe_request_extracts_fabric_filtered() {
        let payload = encode_subscribe_request(1, 60, false, &[]);
        let req = decode_subscribe_request(&payload).unwrap();
        assert!(req.fabric_filtered);
    }

    #[test]
    fn decode_subscribe_request_extracts_fabric_filtered_false_from_wire() {
        // Hand-built SubscribeRequest with IsFabricFiltered explicitly
        // false at Context(7) — asserts the decoded value comes from the
        // actual wire byte, not just "field present" (encode_subscribe_
        // request only ever emits true).
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false); // KeepSubscriptions
        w.put_uint(Tag::Context(1), 1); // MinIntervalFloor
        w.put_uint(Tag::Context(2), 60); // MaxIntervalCeiling
        w.start_array(Tag::Context(3)); // AttributeRequests
        w.end_container();
        w.put_bool(Tag::Context(7), false); // IsFabricFiltered
        w.end_container();
        let req = decode_subscribe_request(&w.finish()).unwrap();
        assert!(
            !req.fabric_filtered,
            "explicit false on the wire must decode to false"
        );
    }
    #[test]
    fn subscribe_request_roundtrips_through_server_decode() {
        let payload = encode_subscribe_request(2, 60, false, &[CLUSTER_ON_OFF]);
        let req = decode_subscribe_request(&payload).unwrap();
        assert!(!req.keep_subscriptions);
        assert_eq!(req.min_interval_floor_s, 2);
        assert_eq!(req.max_interval_ceiling_s, 60);
        assert_eq!(
            req.paths,
            vec![AttrPathIn {
                endpoint: None,
                cluster: Some(CLUSTER_ON_OFF),
                attribute: None
            }]
        );
    }

    #[test]
    fn subscribe_response_roundtrips_through_client_decode() {
        let payload = encode_subscribe_response(0xDEADBEEF, 60);
        let sr = decode_subscribe_response(&payload).unwrap();
        assert_eq!(sr.subscription_id, 0xDEADBEEF);
        assert_eq!(sr.max_interval_s, 60);
    }

    #[test]
    fn full_wildcard_subscribe_request_decodes_to_one_empty_path() {
        let payload = encode_subscribe_request(0, 30, true, &[]);
        let req = decode_subscribe_request(&payload).unwrap();
        assert_eq!(
            req.paths,
            vec![AttrPathIn {
                endpoint: None,
                cluster: None,
                attribute: None
            }]
        );
    }
}
