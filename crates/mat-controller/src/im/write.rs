//! WriteRequest / WriteResponse の encode / decode。コントローラ側の write
//! と、device 側の WriteRequest 受理 / WriteResponse 送出の両方向。

use crate::tlv::{copy_value, Reader, Tag, Value, Writer};

use super::read::{decode_attribute_path_ib, decode_attribute_status_ib};
use super::{encode_im_value, expect_struct_start, skip_container, ImError, ImValue, IM_REVISION};

/// WriteRequestMessage (spec §8.9.2.4) の共通本体。`timed` が TimedRequest
/// フィールドの値になる。公開関数 `encode_write_request_tlv` /
/// `encode_write_request_tlv_timed` はどちらもこれを呼ぶだけの薄いラッパで、
/// `encode_invoke_request` / `encode_invoke_request_timed` と同じ手筋。
fn encode_write_request_inner(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    data_tlv: &[u8],
    timed: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse
    w.put_bool(Tag::Context(1), timed); // TimedRequest
    w.start_array(Tag::Context(2)); // WriteRequests
    w.start_struct(Tag::Anonymous); // AttributeDataIB
    w.start_list(Tag::Context(1)); // AttributePathIB
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    w.put_uint(Tag::Context(4), u64::from(attribute));
    w.end_container(); // AttributePathIB
    w.put_raw_element(Tag::Context(2), data_tlv); // Data
    w.end_container(); // AttributeDataIB
    w.end_container(); // WriteRequests
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// WriteRequestMessage (spec §8.9.2.4) for a single attribute path.
/// TimedRequest is always `false` — see `encode_write_request_tlv_timed` for
/// the timed variant (spec §8.5, タイムド呼び出し). `data_tlv` must be one
/// complete, well-formed TLV element (any top-level tag; it is re-tagged) —
/// the attribute's `Data` value.
pub fn encode_write_request_tlv(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    data_tlv: &[u8],
) -> Vec<u8> {
    encode_write_request_inner(endpoint, cluster, attribute, data_tlv, false)
}

/// WriteRequestMessage (spec §8.9.2.4) with TimedRequest = true. Must be
/// sent on the same exchange as a preceding `encode_timed_request` whose
/// StatusResponse(SUCCESS) has already been received (spec §8.5.1). Same
/// `data_tlv` contract as `encode_write_request_tlv`.
pub fn encode_write_request_tlv_timed(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    data_tlv: &[u8],
) -> Vec<u8> {
    encode_write_request_inner(endpoint, cluster, attribute, data_tlv, true)
}

/// Scalar sugar over `encode_write_request_tlv`: encodes `value` as TLV and
/// splices it in as the `Data` element. M2-scope values only (see `ImValue`).
pub fn encode_write_request(
    endpoint: u16,
    cluster: u32,
    attribute: u32,
    value: &ImValue,
) -> Vec<u8> {
    encode_write_request_tlv(endpoint, cluster, attribute, &encode_im_value(value))
}

/// Decoded AttributeDataIB (spec §8.9.2.2) from a WriteRequest's
/// `WriteRequests` array: server-side counterpart of `encode_write_request_tlv`/
/// `encode_write_request_tlv_timed`. `None` path fields are wildcards (omitted
/// on the wire, as `AttrPathIn`'s doc explains for reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAttrIn {
    pub endpoint: Option<u16>,
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
    /// The AttributeDataIB's `Data` element (tag 2), re-tagged to `Anonymous`
    /// as one complete, well-formed TLV element — same convention as
    /// `encode_write_request_tlv`'s `data_tlv` input, just decoded instead of
    /// encoded.
    pub data_tlv: Vec<u8>,
    /// Whether the path's AttributePathIB carried a `ListIndex` (spec
    /// §8.9.2.2) — i.e. this write targets one element of a list attribute
    /// rather than replacing the whole attribute. chip-tool-family
    /// controllers write a list attribute as "replace whole list" followed
    /// by a chunk train of `ListIndex: null` appends; `mat-device`'s data
    /// model dispatch has no list-attribute write implemented yet and so no
    /// consumer for this, but plumbing it through from
    /// `decode_attribute_path_ib` now means a future list-attribute
    /// `ClusterHandler::write` doesn't need another wire-decode change.
    pub list_append: bool,
}

/// Decoded WriteRequestMessage (spec §8.9.2.4): server-side counterpart of
/// `encode_write_request_tlv`/`encode_write_request_tlv_timed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRequestIn {
    pub timed: bool,
    pub suppress_response: bool,
    pub writes: Vec<WriteAttrIn>,
}

/// AttributeDataIB (spec §8.9.2.2) as it appears inside a WriteRequest's
/// `WriteRequests` array: `{0: DataVersion?, 1: Path(list), 2: Data}`.
/// Assumes the caller already consumed the anonymous `StructStart` opening
/// this AttributeDataIB. Unlike `decode_attribute_data_ib` (M2, report-side,
/// scalar-only `ImValue`), this keeps `Data` as a raw re-tagged TLV element
/// (any shape) and also extracts the path — a write, unlike a report, always
/// carries both.
fn decode_write_attribute_data_ib(r: &mut Reader) -> Result<WriteAttrIn, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut attribute = None;
    let mut data_tlv = None;
    let mut list_append = false;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ListStart) => {
                let (ep, cl, attr, la) = decode_attribute_path_ib(r)?;
                endpoint = ep;
                cluster = cl;
                attribute = attr;
                list_append = la;
            }
            (Tag::Context(2), v) => {
                // Data: re-tag to Anonymous, same convention as
                // `encode_write_request_tlv`'s `data_tlv` input and
                // `decode_invoke_request`'s CommandFields echo.
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, v)?;
                data_tlv = Some(w.finish());
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok(WriteAttrIn {
        endpoint,
        cluster,
        attribute,
        data_tlv: data_tlv.ok_or(ImError::Malformed("write attribute without Data field"))?,
        list_append,
    })
}

/// `WriteRequests`（array[AttributeDataIB]）の中身を読む共通処理。呼び出し側は
/// array を開く `ArrayStart` を既に読んでいる前提で、対応する `ContainerEnd`
/// まで読み切る。`decode_attribute_requests`（read 側）の write 版。
fn decode_write_requests_array(r: &mut Reader) -> Result<Vec<WriteAttrIn>, ImError> {
    let mut writes = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated write requests"))?;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => writes.push(decode_write_attribute_data_ib(r)?),
            Value::ArrayStart | Value::ListStart => skip_container(r)?,
            _ => return Err(ImError::Malformed("unexpected element in write requests")),
        }
    }
    Ok(writes)
}

/// WriteRequestMessage (spec §8.9.2.4): server-side decode of
/// `encode_write_request_tlv`/`encode_write_request_tlv_timed`'s payload.
/// Returns every AttributeDataIB in `WriteRequests` (tag 2) — a device must
/// answer every write a controller asks for, mirroring `decode_read_request`.
pub fn decode_write_request(payload: &[u8]) -> Result<WriteRequestIn, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut suppress_response = false;
    let mut timed = false;
    let mut writes = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated write request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bool(b)) => suppress_response = b,
            (Tag::Context(1), Value::Bool(b)) => timed = b,
            (Tag::Context(2), Value::ArrayStart) => {
                writes = decode_write_requests_array(&mut r)?;
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(WriteRequestIn {
        timed,
        suppress_response,
        writes,
    })
}

/// WriteResponseMessage (spec §8.9.2.4): `{0: [AttributeStatusIB, ...], 255:
/// revision}`. Only the first `AttributeStatusIB`'s status is interpreted
/// (M8a scope: one attribute per write). Reuses `decode_attribute_status_ib`
/// (same `{0: Path, 1: StatusIB{0: status, ...}}` shape as a WriteResponses
/// entry).
pub fn decode_write_response(payload: &[u8]) -> Result<u8, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated write response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ArrayStart) => {
                // WriteResponses
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated write responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            status = Some(decode_attribute_status_ib(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed("unexpected element in write responses"))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    status.ok_or(ImError::Malformed(
        "write response without AttributeStatusIB",
    ))
}

/// WriteResponseMessage (spec §8.9.2.4): encodes one `AttributeStatusIB` per
/// `results` entry `(endpoint, cluster, attribute, status)`. Device-side
/// counterpart of `decode_write_response` — produces exactly the shape that
/// function reads: `{0: [AttributeStatusIB{0: Path(list), 1: StatusIB{0:
/// status}}, ...], 255: revision}`.
pub fn encode_write_response(results: &[(u16, u32, u32, u8)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0)); // WriteResponses
    for &(endpoint, cluster, attribute, status) in results {
        w.start_struct(Tag::Anonymous); // AttributeStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.put_uint(Tag::Context(2), u64::from(endpoint));
        w.put_uint(Tag::Context(3), u64::from(cluster));
        w.put_uint(Tag::Context(4), u64::from(attribute));
        w.end_container(); // Path
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), u64::from(status));
        w.end_container(); // StatusIB
        w.end_container(); // AttributeStatusIB
    }
    w.end_container(); // WriteResponses
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
    fn write_request_roundtrip_scalar() {
        let b = encode_write_request(1, 0x0008, 0x0011, &ImValue::Uint(128));
        // 形の検証: WriteRequests(2) 配列の中に AttributeDataIB があり、
        // path(ep=1, cluster=8, attr=0x11) と Data(Context2)=128 を含む。
        let mut r = Reader::new(&b);
        let (mut saw_ep, mut saw_data) = (false, false);
        while let Some(el) = r.next().unwrap() {
            if el.tag == Tag::Context(2) && el.value == Value::Uint(128) {
                saw_data = true;
            }
            if el.tag == Tag::Context(2) && el.value == Value::Uint(1) {
                saw_ep = true;
            }
        }
        assert!(saw_ep && saw_data);
    }

    #[test]
    fn decode_write_response_returns_first_status() {
        // WriteResponse { 0: [ AttrStatusIB{0: path, 1: StatusIB{0: 0}} ] }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0)); // path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        assert_eq!(decode_write_response(&w.finish()).unwrap(), 0);
    }

    #[test]
    fn write_request_roundtrips_with_device_side_decoder() {
        let mut w = Writer::new();
        w.put_uint(Tag::Anonymous, 42);
        let data = w.finish();
        let payload = encode_write_request_tlv(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, &data);
        let req = decode_write_request(&payload).unwrap();
        assert!(!req.timed);
        assert_eq!(req.writes.len(), 1);
        let wr = &req.writes[0];
        assert_eq!(
            (wr.endpoint, wr.cluster, wr.attribute),
            (Some(0), Some(CLUSTER_ACCESS_CONTROL), Some(ATTR_ACL))
        );
        assert!(!wr.list_append);
        let mut r = Reader::new(&wr.data_tlv);
        let el = r.next().unwrap().unwrap();
        assert_eq!(el.tag, Tag::Anonymous);
        assert_eq!(el.value, Value::Uint(42));
    }

    #[test]
    fn write_response_encodes_attribute_status_ibs() {
        let payload = encode_write_response(&[(0, CLUSTER_ACCESS_CONTROL, ATTR_ACL, 0x00)]);
        // 自前 decoder（decode_attribute_status_ib 経由の decode_write_response）で読み戻す
        assert_eq!(decode_write_response(&payload).unwrap(), 0x00);
    }
}
