//! ReadRequest / ReportData の encode / decode（単一属性・cluster wildcard・
//! chunk 結合 `merge_reports`）。コントローラ側の read と、device 側の
//! ReadRequest 受理 / ReportData 送出の両方向。

use crate::tlv::{Reader, Tag, Value, Writer};

use super::json::tlv_element_to_json;
use super::{
    expect_struct_start, skip_container, value_to_im, ImError, ImValue, ReportData, IM_REVISION,
};

/// ReadRequestMessage (spec §8.9.2) for a single attribute path.
pub fn encode_read_request(endpoint: u16, cluster: u32, attribute: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0)); // AttributeRequests
    w.start_list(Tag::Anonymous); // AttributePathIB
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    w.put_uint(Tag::Context(4), u64::from(attribute));
    w.end_container(); // AttributePathIB
    w.end_container(); // AttributeRequests
                       // IsFabricFiltered = true: matches chip-tool's read default. This is the
                       // precondition for mat-native's ACL / group-key-map read-merge-write
                       // (ensure_group_acl / provision_node in mat-native::ops) not pulling in
                       // other fabrics' entries on multi-admin devices (e.g. a Home Assistant
                       // fabric alongside ours) — see mat_core::acl::merge_group_entry's doc.
    w.put_bool(Tag::Context(3), true); // IsFabricFiltered
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// AttributeStatusIB (spec §8.9.2.2): `{0: Path, 1: StatusIB{0: status, ...}}`.
/// Assumes the caller already consumed the `StructStart` (tag 0) that opens
/// this AttributeStatusIB.
pub(super) fn decode_attribute_status_ib(r: &mut Reader) -> Result<u8, ImError> {
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute status"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::StructStart) => {
                // StatusIB
                loop {
                    let e2 = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
                    match (e2.tag, e2.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(0), Value::Uint(v)) => {
                            status = Some(u8::try_from(v).map_err(|_| {
                                ImError::Malformed("attribute status code out of range")
                            })?);
                        }
                        (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                            skip_container(r)?;
                        }
                        _ => {}
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    status.ok_or(ImError::Malformed("attribute status without StatusIB"))
}

/// AttributeDataIB (spec §8.9.2.2): `{0: DataVersion, 1: Path, 2: Data}`.
/// Assumes the caller already consumed the `StructStart` (tag 1) that opens
/// this AttributeDataIB.
fn decode_attribute_data_ib(r: &mut Reader) -> Result<ImValue, ImError> {
    let mut data = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), v) => data = Some(value_to_im(v)?),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    data.ok_or(ImError::Malformed("attribute data without Data field"))
}

/// AttributeReportIB (spec §8.9.2.2): `{0: AttributeStatusIB} | {1: AttributeDataIB}`.
/// Assumes the caller already consumed the anonymous `StructStart` opening
/// this AttributeReportIB.
fn decode_attribute_report_ib(r: &mut Reader) -> Result<(Option<ImValue>, Option<u8>), ImError> {
    let mut value = None;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute report"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => {
                status = Some(decode_attribute_status_ib(r)?);
            }
            (Tag::Context(1), Value::StructStart) => {
                value = Some(decode_attribute_data_ib(r)?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok((value, status))
}

/// ReportDataMessage (spec §8.9.2). Only the first AttributeReportIB is
/// interpreted (M2 reads one attribute at a time).
pub fn decode_report_data(payload: &[u8]) -> Result<ReportData, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut suppress_response = false;
    let mut value: Option<ImValue> = None;
    let mut status: Option<u8> = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated report data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ArrayStart) => {
                // AttributeReportIBs
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated attribute reports"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            let (v, s) = decode_attribute_report_ib(&mut r)?;
                            value = v;
                            status = s;
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in attribute reports",
                            ))
                        }
                    }
                }
            }
            (Tag::Context(3), Value::Bool(true)) => {
                // MoreChunkedMessages: M2 has no chunk-reassembly support, so
                // silently returning the first chunk's partial data would be
                // wrong — the caller would see a "successful" read that is
                // actually incomplete.
                return Err(ImError::Malformed("chunked report data unsupported"));
            }
            (Tag::Context(4), Value::Bool(b)) => suppress_response = b,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    if value.is_none() && status.is_none() {
        return Err(ImError::Malformed("empty report"));
    }
    Ok(ReportData {
        suppress_response,
        value,
        status,
    })
}

/// 1 AttributeReportIB のデコード結果（汎用形）。`decode_report_data`（M2,
/// 単一属性・スカラーのみ）とは独立の新 API — 既存 API は無改変。
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeReport {
    pub endpoint: Option<u16>,
    /// パスの ClusterId（Context 3）。wildcard 購読 report のイベント化に必要。
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
    /// path に ListIndex(Context 5) があれば true = この IB は list の要素（scalar
    /// 属性値ではない）。null（チャンク化 list の item 追記）が通常だが、具体 index
    /// を入れてくる非準拠デバイスも同様に list 要素として扱う。
    pub list_append: bool,
    /// AttributeDataIB の Data 要素を JSON 化したもの（status レポートなら None）。
    pub data: Option<serde_json::Value>,
    pub status: Option<u8>,
}

/// ReportDataMessage (spec §8.9.2) の汎用デコード結果。複数 AttributeReportIB・
/// チャンク（MoreChunkedMessages）・list 追記に対応する。
#[derive(Debug, Clone, PartialEq)]
pub struct ReportDataMessage {
    pub reports: Vec<AttributeReport>,
    /// 購読 report が運ぶ SubscriptionId（tag 0）。read 応答では None。
    pub subscription_id: Option<u32>,
    pub more_chunks: bool,
    pub suppress_response: bool,
}

/// `decode_attribute_path_ib` の戻り値: (endpoint, cluster, attribute, list_append).
type AttributePathFields = (Option<u16>, Option<u32>, Option<u32>, bool);

/// AttributePathIB (spec §8.9.2.2, list) のうち endpoint(Context 2) /
/// cluster(Context 3) / attribute(Context 4) / ListIndex(Context 5, 存在すれば
/// list 要素 = `list_append`。`Null` はチャンク化 list への item 追記、具体 index は
/// 非準拠デバイスの edit/replace 表現) を拾う。他フィールド（Node/DataVersion 等）は
/// 読み飛ばす。呼び出し側は path を開く `ListStart` を既に読んでいる前提。
pub(super) fn decode_attribute_path_ib(r: &mut Reader) -> Result<AttributePathFields, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut attribute = None;
    let mut list_append = false;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute path"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::Uint(v)) => {
                endpoint = Some(
                    u16::try_from(v).map_err(|_| ImError::Malformed("endpoint out of range"))?,
                );
            }
            (Tag::Context(3), Value::Uint(v)) => {
                cluster = Some(
                    u32::try_from(v).map_err(|_| ImError::Malformed("cluster id out of range"))?,
                );
            }
            (Tag::Context(4), Value::Uint(v)) => {
                attribute = Some(
                    u32::try_from(v)
                        .map_err(|_| ImError::Malformed("attribute id out of range"))?,
                );
            }
            // ListIndex(Context 5) の存在自体が「この IB は list 要素」を意味する。
            // null（append）でも具体 index（非準拠デバイスの edit/replace 表現）でも
            // scalar 属性値ではないので list_append 扱いにする（下流の偽 recovered 抑止）。
            (Tag::Context(5), Value::Null | Value::Uint(_) | Value::Int(_)) => list_append = true,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok((endpoint, cluster, attribute, list_append))
}

/// `decode_attribute_status_ib_full` の戻り値: (endpoint, cluster, attribute, status).
type AttributeStatusFields = (Option<u16>, Option<u32>, Option<u32>, u8);

/// AttributeStatusIB (spec §8.9.2.2): `{0: Path, 1: StatusIB{0: status, ...}}`,
/// path も拾う汎用版（`decode_attribute_status_ib` の M2 版とは独立— 既存
/// API 無改変のため別関数にした）。呼び出し側は AttributeReportIB の
/// anonymous `StructStart`（tag 0）を既に読んでいる前提。
fn decode_attribute_status_ib_full(r: &mut Reader) -> Result<AttributeStatusFields, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut attribute = None;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute status"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => {
                let (ep, cl, attr, _) = decode_attribute_path_ib(r)?;
                endpoint = ep;
                cluster = cl;
                attribute = attr;
            }
            (Tag::Context(1), Value::StructStart) => {
                // StatusIB
                loop {
                    let e2 = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
                    match (e2.tag, e2.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(0), Value::Uint(v)) => {
                            status = Some(u8::try_from(v).map_err(|_| {
                                ImError::Malformed("attribute status code out of range")
                            })?);
                        }
                        (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                            skip_container(r)?;
                        }
                        _ => {}
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    let status = status.ok_or(ImError::Malformed("attribute status without StatusIB"))?;
    Ok((endpoint, cluster, attribute, status))
}

/// `decode_attribute_data_ib_full` の戻り値: (endpoint, cluster, attribute,
/// list_append, data).
type AttributeDataFields = (
    Option<u16>,
    Option<u32>,
    Option<u32>,
    bool,
    Option<serde_json::Value>,
);

/// AttributeDataIB (spec §8.9.2.2): `{0: DataVersion, 1: Path, 2: Data}`,
/// path も拾い Data を JSON 化する汎用版（`decode_attribute_data_ib` の M2
/// 版とは独立）。呼び出し側は AttributeReportIB の anonymous `StructStart`
/// （tag 1）を既に読んでいる前提。
fn decode_attribute_data_ib_full(r: &mut Reader) -> Result<AttributeDataFields, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut attribute = None;
    let mut list_append = false;
    let mut data = None;
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
            (Tag::Context(2), _) => {
                data = Some(tlv_element_to_json(r, el)?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok((endpoint, cluster, attribute, list_append, data))
}

/// AttributeReportIB (spec §8.9.2.2): `{0: AttributeStatusIB} | {1: AttributeDataIB}`,
/// 汎用版（path/JSON も拾う。`decode_attribute_report_ib` の M2 版とは独立）。
/// 呼び出し側は開く anonymous `StructStart` を既に読んでいる前提。
fn decode_attribute_report_ib_full(r: &mut Reader) -> Result<AttributeReport, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut attribute = None;
    let mut list_append = false;
    let mut data = None;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute report"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => {
                let (ep, cl, attr, s) = decode_attribute_status_ib_full(r)?;
                endpoint = ep;
                cluster = cl;
                attribute = attr;
                status = Some(s);
            }
            (Tag::Context(1), Value::StructStart) => {
                let (ep, cl, attr, la, d) = decode_attribute_data_ib_full(r)?;
                endpoint = ep;
                cluster = cl;
                attribute = attr;
                list_append = la;
                data = d;
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok(AttributeReport {
        endpoint,
        cluster,
        attribute,
        list_append,
        data,
        status,
    })
}

/// ReportDataMessage (spec §8.9.2) の汎用デコード。すべての AttributeReportIB
/// を読み（M2 の `decode_report_data` と違い最初の 1 件に限らない）、
/// MoreChunkedMessages(tag 3) はチャンク未完了フラグとしてそのまま
/// `more_chunks` へ反映するだけで拒否しない（チャンク統合は
/// `merge_reports` の責務）。
pub fn decode_report_data_message(payload: &[u8]) -> Result<ReportDataMessage, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut reports = Vec::new();
    let mut subscription_id = None;
    let mut more_chunks = false;
    let mut suppress_response = false;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated report data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                subscription_id = Some(
                    u32::try_from(v)
                        .map_err(|_| ImError::Malformed("subscription id out of range"))?,
                );
            }
            (Tag::Context(1), Value::ArrayStart) => {
                // AttributeReportIBs: every entry, not just the first.
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated attribute reports"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart => {
                            reports.push(decode_attribute_report_ib_full(&mut r)?);
                        }
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in attribute reports",
                            ))
                        }
                    }
                }
            }
            (Tag::Context(3), Value::Bool(b)) => more_chunks = b,
            (Tag::Context(4), Value::Bool(b)) => suppress_response = b,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(ReportDataMessage {
        reports,
        subscription_id,
        more_chunks,
        suppress_response,
    })
}

/// One attribute value to report: server-side counterpart of
/// `AttributeReport`. `value_tlv` must be one complete, well-formed TLV
/// element (any top-level tag; re-tagged on splice) — the attribute's `Data`.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrReportOut {
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub data_version: u32,
    pub value_tlv: Vec<u8>,
}

/// One AttributeReportIB (spec §8.9.2.2) to encode: either attribute data
/// (`AttrReportOut`, the `{1: AttributeDataIB}` shape) or a per-path failure
/// (`{0: AttributeStatusIB}`) — server-side counterpart of what
/// `decode_report_data_message` already decodes both variants of
/// (`AttributeReport::{data, status}`). `core::datamodel::Node::read_entries`
/// (mat-device) is this type's main producer: wildcard-expanded reads that
/// hit an unresolvable *concrete* path segment report a per-path status
/// instead of failing the whole read.
#[derive(Debug, Clone, PartialEq)]
pub enum ReportEntryOut {
    Data(AttrReportOut),
    Status {
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        status: u8,
    },
}

/// ReportDataMessage (spec §8.9.2): server-side encode, mirroring
/// `decode_report_data_message`'s nesting. Shape: `struct{0:
/// SubscriptionId?, 1: array[AttributeReportIB], 3: MoreChunkedMessages?,
/// 4: SuppressResponse, 255: IM_REVISION}`, where each AttributeReportIB is
/// either `{1: struct{0:DataVersion, 1:list{2:endpoint,3:cluster,
/// 4:attribute}, 2:Data}}` (data) or `{0: struct{0:
/// list{2:endpoint,3:cluster,4:attribute}, 1: struct{0:status}}}` (status —
/// AttributeStatusIB, spec §8.9.6). The general (subscription-capable,
/// mixed data/status) encoder; `encode_report_data` is the read-only
/// convenience wrapper over it.
pub fn encode_report_data_entries(
    entries: &[ReportEntryOut],
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
    for entry in entries {
        w.start_struct(Tag::Anonymous); // AttributeReportIB
        match entry {
            ReportEntryOut::Data(report) => {
                w.start_struct(Tag::Context(1)); // AttributeDataIB
                w.put_uint(Tag::Context(0), u64::from(report.data_version)); // DataVersion
                w.start_list(Tag::Context(1)); // Path
                w.put_uint(Tag::Context(2), u64::from(report.endpoint));
                w.put_uint(Tag::Context(3), u64::from(report.cluster));
                w.put_uint(Tag::Context(4), u64::from(report.attribute));
                w.end_container(); // Path
                w.put_raw_element(Tag::Context(2), &report.value_tlv); // Data
                w.end_container(); // AttributeDataIB
            }
            ReportEntryOut::Status {
                endpoint,
                cluster,
                attribute,
                status,
            } => {
                w.start_struct(Tag::Context(0)); // AttributeStatusIB
                w.start_list(Tag::Context(0)); // Path
                w.put_uint(Tag::Context(2), u64::from(*endpoint));
                w.put_uint(Tag::Context(3), u64::from(*cluster));
                w.put_uint(Tag::Context(4), u64::from(*attribute));
                w.end_container(); // Path
                w.start_struct(Tag::Context(1)); // StatusIB
                w.put_uint(Tag::Context(0), u64::from(*status));
                w.end_container(); // StatusIB
                w.end_container(); // AttributeStatusIB
            }
        }
        w.end_container(); // AttributeReportIB
    }
    w.end_container(); // AttributeReportIBs
    if more_chunks {
        w.put_bool(Tag::Context(3), true); // MoreChunkedMessages
    }
    w.put_bool(Tag::Context(4), suppress_response); // SuppressResponse
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// `encode_report_data_entries` convenience wrapper for the common read
/// case: every report is data (no per-path failures), no subscription id,
/// no chunking.
pub fn encode_report_data(reports: &[AttrReportOut], suppress_response: bool) -> Vec<u8> {
    let entries: Vec<ReportEntryOut> = reports.iter().cloned().map(ReportEntryOut::Data).collect();
    encode_report_data_entries(&entries, suppress_response, None, false)
}

/// ReadRequestMessage (spec §8.9.2) の wildcard 版: AttributePathIB から
/// attribute を省略し、cluster 内の全属性を要求する。
pub fn encode_read_request_cluster(endpoint: u16, cluster: u32) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0)); // AttributeRequests
    w.start_list(Tag::Anonymous); // AttributePathIB
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    w.end_container(); // AttributePathIB
    w.end_container(); // AttributeRequests
                       // IsFabricFiltered = true: matches chip-tool's read default. See
                       // encode_read_request's comment above for the rationale.
    w.put_bool(Tag::Context(3), true); // IsFabricFiltered
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// Decoded AttributePathIB (spec §8.9.2.2) from a ReadRequest: server-side
/// counterpart of `encode_read_request`/`encode_read_request_cluster`.
/// `None` fields are wildcards (omitted on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttrPathIn {
    pub endpoint: Option<u16>,
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
}

/// `AttributeRequests`（array[AttributePathIB]）の中身を読む共通処理。
/// 呼び出し側は array を開く `ArrayStart` を既に読んでいる前提で、この
/// 関数は対応する `ContainerEnd` まで読み切る。`decode_read_request` と
/// `decode_subscribe_request` の両方から使う。
pub(super) fn decode_attribute_requests(r: &mut Reader) -> Result<Vec<AttrPathIn>, ImError> {
    let mut paths = Vec::new();
    loop {
        let e2 = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute requests"))?;
        match e2.value {
            Value::ContainerEnd => break,
            Value::ListStart => {
                let (endpoint, cluster, attribute, _) = decode_attribute_path_ib(r)?;
                paths.push(AttrPathIn {
                    endpoint,
                    cluster,
                    attribute,
                });
            }
            Value::StructStart | Value::ArrayStart => skip_container(r)?,
            _ => {
                return Err(ImError::Malformed(
                    "unexpected element in attribute requests",
                ))
            }
        }
    }
    Ok(paths)
}

/// Decoded ReadRequestMessage (spec §8.9.2): server-side counterpart of
/// `encode_read_request`/`encode_read_request_cluster`, including
/// `IsFabricFiltered` (tag 3, bool) alongside the requested paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadRequestIn {
    pub paths: Vec<AttrPathIn>,
    pub fabric_filtered: bool,
}

/// ReadRequestMessage (spec §8.9.2): server-side decode of
/// `encode_read_request`/`encode_read_request_cluster`'s payload. Returns
/// every AttributePathIB in `AttributeRequests` (tag 0) — unlike the
/// client-side `decode_report_data*` helpers, a device must answer every
/// path a controller asks for, not just the first — plus `IsFabricFiltered`
/// (tag 3). `IsFabricFiltered` defaults to `true` when absent from the
/// wire: that's the side that discloses less (filters fabric-scoped
/// attributes down), so an omitted flag errs toward it rather than toward
/// leaking other fabrics' entries.
pub fn decode_read_request_message(payload: &[u8]) -> Result<ReadRequestIn, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut paths = Vec::new();
    let mut fabric_filtered = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated read request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ArrayStart) => {
                // AttributeRequests
                paths = decode_attribute_requests(&mut r)?;
            }
            (Tag::Context(3), Value::Bool(b)) => fabric_filtered = Some(b),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(ReadRequestIn {
        paths,
        fabric_filtered: fabric_filtered.unwrap_or(true),
    })
}

/// ReadRequestMessage (spec §8.9.2): server-side decode of
/// `encode_read_request`/`encode_read_request_cluster`'s payload, paths
/// only. Thin delegation to `decode_read_request_message` for callers that
/// don't need `IsFabricFiltered`.
pub fn decode_read_request(payload: &[u8]) -> Result<Vec<AttrPathIn>, ImError> {
    Ok(decode_read_request_message(payload)?.paths)
}

/// 複数 ReportDataMessage・リスト追記を統合し attribute id → JSON 値へ。
/// 同一 attribute の非追記レポートは最後のものが勝つ（Replace）。追記
/// （`list_append`）は既存値が JSON array ならそこへ push する（既存値が
/// array でない異常系は 1 要素の array から作り直す）。status のみの
/// レポート（`data: None`）は結果に出ない。出力順は最初に登場した順。
pub fn merge_reports(msgs: &[ReportDataMessage]) -> Vec<(u32, serde_json::Value)> {
    let mut order: Vec<u32> = Vec::new();
    let mut map: std::collections::HashMap<u32, serde_json::Value> =
        std::collections::HashMap::new();
    for m in msgs {
        for rep in &m.reports {
            let Some(attr) = rep.attribute else { continue };
            let Some(data) = rep.data.clone() else {
                continue; // status-only は値なし
            };
            if rep.list_append {
                match map.entry(attr).or_insert_with(|| serde_json::json!([])) {
                    serde_json::Value::Array(items) => items.push(data),
                    slot => *slot = serde_json::json!([data]), // 追記が先に来た異常系
                }
            } else {
                map.insert(attr, data);
            }
            if !order.contains(&attr) {
                order.push(attr);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|a| map.remove(&a).map(|v| (a, v)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::*;
    use crate::tlv::{copy_value, Reader, Tag, Value, Writer};

    #[test]
    fn read_request_has_spec_structure() {
        let buf = encode_read_request(1, CLUSTER_ON_OFF, ATTR_ON_OFF);
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        // struct{ 0: array[ list{2,3,4} ], 3: false, 255: 12 }
        assert_eq!(els[0].value, Value::StructStart);
        assert_eq!(
            (els[1].tag, els[1].value),
            (Tag::Context(0), Value::ArrayStart)
        );
        assert_eq!(els[2].value, Value::ListStart);
        assert_eq!(
            (els[3].tag, els[3].value),
            (Tag::Context(2), Value::Uint(1))
        );
        assert_eq!(
            (els[4].tag, els[4].value),
            (Tag::Context(3), Value::Uint(0x0006))
        );
        assert_eq!(
            (els[5].tag, els[5].value),
            (Tag::Context(4), Value::Uint(0))
        );
        assert_eq!(els[6].value, Value::ContainerEnd); // list
        assert_eq!(els[7].value, Value::ContainerEnd); // array
                                                       // IsFabricFiltered must be true: matches chip-tool's read default, and
                                                       // is the precondition for ensure_group_acl / provision_node's
                                                       // read-merge-write (mat-native) not pulling in other fabrics' ACL /
                                                       // group-key-map entries on multi-admin devices.
        assert_eq!(
            (els[8].tag, els[8].value),
            (Tag::Context(3), Value::Bool(true))
        );
        assert_eq!(
            (els[9].tag, els[9].value),
            (Tag::Context(255), Value::Uint(12))
        );
        assert_eq!(els[10].value, Value::ContainerEnd);
    }

    fn report_data(value: bool, suppress: bool) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1)); // AttributeReportIBs
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeData
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // Path
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(3), 6);
        w.put_uint(Tag::Context(4), 0);
        w.end_container();
        w.put_bool(Tag::Context(2), value); // Data
        w.end_container();
        w.end_container();
        w.end_container();
        if suppress {
            w.put_bool(Tag::Context(4), true);
        }
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    #[test]
    fn decodes_report_data_bool() {
        let rd = decode_report_data(&report_data(true, true)).unwrap();
        assert!(rd.suppress_response);
        assert_eq!(rd.value, Some(ImValue::Bool(true)));
        assert_eq!(rd.status, None);
        let rd = decode_report_data(&report_data(false, false)).unwrap();
        assert!(!rd.suppress_response);
        assert_eq!(rd.value, Some(ImValue::Bool(false)));
    }

    #[test]
    fn decodes_report_data_attribute_status() {
        // AttributeStatus (tag 0) = 読めない属性: struct{0: Path, 1: StatusIB{0: status}}
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(0)); // AttributeStatus
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0x86); // UNSUPPORTED_ATTRIBUTE
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(4), true);
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let rd = decode_report_data(&w.finish()).unwrap();
        assert_eq!(rd.status, Some(0x86));
        assert_eq!(rd.value, None);
    }

    #[test]
    fn decode_report_data_rejects_more_chunked_messages() {
        // ReportDataMessage の MoreChunkedMessages (tag 3) = true: M2 は
        // チャンク再構成をサポートしないので、部分データを黙って返しては
        // ならない。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1)); // AttributeReportIBs
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeData
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // Path
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(3), 6);
        w.put_uint(Tag::Context(4), 0);
        w.end_container();
        w.put_bool(Tag::Context(2), true); // Data
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(3), true); // MoreChunkedMessages = true
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        assert_eq!(
            decode_report_data(&w.finish()),
            Err(ImError::Malformed("chunked report data unsupported"))
        );
    }
    #[test]
    fn decode_report_data_message_multiple_ibs_and_types() {
        // ReportData { 1: [ AttrReport{1: Data{1: path(ep,cl,attr), 2: data}},
        //                   AttrReport{...} ], 4: suppress }
        // を Writer で組み、bool と list-of-struct の 2 属性が JSON になること。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1)); // AttributeReports
                                        // 属性1: on-off = true
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeDataIB
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // AttributePathIB
        w.put_uint(Tag::Context(2), 1); // endpoint
        w.put_uint(Tag::Context(3), 0x0006);
        w.put_uint(Tag::Context(4), 0x0000);
        w.end_container();
        w.put_bool(Tag::Context(2), true); // Data
        w.end_container();
        w.end_container();
        // 属性2: 構造体1件のリスト
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        w.start_list(Tag::Context(1));
        w.put_uint(Tag::Context(2), 0);
        w.put_uint(Tag::Context(3), 0x0035);
        w.put_uint(Tag::Context(4), 0x0007); // neighbor-table
        w.end_container();
        w.start_array(Tag::Context(2)); // Data: array of struct
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 42);
        w.put_int(Tag::Context(1), -60);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container(); // AttributeReports
        w.put_bool(Tag::Context(4), true); // SuppressResponse
        w.end_container();
        let msg = decode_report_data_message(&w.finish()).unwrap();
        assert!(msg.suppress_response);
        assert!(!msg.more_chunks);
        assert_eq!(msg.reports.len(), 2);
        assert_eq!(msg.reports[0].attribute, Some(0x0000));
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(true)));
        assert_eq!(msg.reports[1].attribute, Some(0x0007));
        assert_eq!(
            msg.reports[1].data,
            Some(serde_json::json!([{"0": 42, "1": -60}]))
        );
    }

    #[test]
    fn merge_reports_joins_chunked_list_appends() {
        // msg1: neighbor-table = []（Replace）+ more_chunks
        // msg2: ListIndex null の追記 IB × 2
        // → 統合結果は 2 要素の array。
        fn path(w: &mut Writer, attr: u32, append: bool) {
            w.start_list(Tag::Context(1));
            w.put_uint(Tag::Context(2), 0);
            w.put_uint(Tag::Context(3), 0x0035);
            w.put_uint(Tag::Context(4), u64::from(attr));
            if append {
                w.put_null(Tag::Context(5)); // ListIndex = null → 追記
            }
            w.end_container();
        }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        path(&mut w, 0x0007, false);
        w.start_array(Tag::Context(2));
        w.end_container(); // 空 array（replace）
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(3), true); // MoreChunkedMessages
        w.end_container();
        let m1 = decode_report_data_message(&w.finish()).unwrap();
        assert!(m1.more_chunks);

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        for v in [7u64, 8u64] {
            w.start_struct(Tag::Anonymous);
            w.start_struct(Tag::Context(1));
            path(&mut w, 0x0007, true);
            w.put_uint(Tag::Context(2), v); // Data = list item
            w.end_container();
            w.end_container();
        }
        w.end_container();
        w.end_container();
        let m2 = decode_report_data_message(&w.finish()).unwrap();
        assert_eq!(m2.reports.len(), 2);
        assert!(m2.reports[0].list_append);

        let merged = merge_reports(&[m1, m2]);
        assert_eq!(merged, vec![(0x0007, serde_json::json!([7, 8]))]);
    }

    #[test]
    fn attribute_path_with_concrete_list_index_is_list_append() {
        // 非準拠デバイスが list 要素レポートで ListIndex に null ではなく具体
        // index（Context(5) = Uint）を入れてくるケース。これも list 要素であって
        // scalar 属性値ではないため list_append 扱いにしないと、下流
        // （matd events_from_report / SubHealth）が scalar と誤認して偽 recovered
        // を出す。Context(5) の存在自体で list_append を立てる。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeDataIB.Data path
        w.start_list(Tag::Context(1)); // AttributePathIB
        w.put_uint(Tag::Context(2), 0);
        w.put_uint(Tag::Context(3), 0x0035);
        w.put_uint(Tag::Context(4), 0x0007);
        w.put_uint(Tag::Context(5), 0); // ListIndex = 具体 index（非準拠）
        w.end_container();
        w.put_uint(Tag::Context(2), 42); // Data = list item（scalar）
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        let m = decode_report_data_message(&w.finish()).unwrap();
        assert_eq!(m.reports.len(), 1);
        assert!(
            m.reports[0].list_append,
            "具体 index の list 要素も list_append 扱い: {:?}",
            m.reports[0]
        );
    }

    #[test]
    fn encode_read_request_cluster_omits_attribute() {
        let b = encode_read_request_cluster(1, 0x0035);
        let mut r = Reader::new(&b);
        let mut saw_attr_tag = false;
        while let Some(el) = r.next().unwrap() {
            if el.tag == Tag::Context(4) {
                saw_attr_tag = true;
            }
        }
        assert!(
            !saw_attr_tag,
            "wildcard read must omit the attribute path field"
        );
    }

    #[test]
    fn encode_read_request_cluster_is_fabric_filtered() {
        // struct{ 0: array[ list{2,3} ], 3: true, 255: 12 } — same top-level
        // shape as encode_read_request but without the attribute (Context(4))
        // path field. Depth-track so the AttributePathIB's own Context(3)
        // (cluster id, a Uint) at depth 3 is never mistaken for the outer
        // struct's Context(3) (IsFabricFiltered, a Bool) at depth 1.
        let b = encode_read_request_cluster(1, 0x0035);
        let mut r = Reader::new(&b);
        let mut depth = 0i32;
        let mut found = false;
        while let Some(el) = r.next().unwrap() {
            match el.value {
                Value::ContainerEnd => depth -= 1,
                Value::StructStart | Value::ArrayStart | Value::ListStart => {
                    depth += 1;
                }
                Value::Bool(b) if el.tag == Tag::Context(3) && depth == 1 => {
                    found = true;
                    assert!(
                        b,
                        "IsFabricFiltered must be true (matches chip-tool's read default)"
                    );
                }
                _ => {}
            }
        }
        assert!(found, "IsFabricFiltered field not found at top level");
    }
    #[test]
    fn tlv_to_json_skips_noncontext_container_fields_safely() {
        // Struct with a non-Context (anonymous) container followed by a Context field.
        // Must safely skip the anonymous struct without misinterpreting the next field.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous); // Data struct
        w.start_struct(Tag::Anonymous); // Unexpected anonymous child struct
        w.put_uint(Tag::Context(9), 99); // Some data inside
        w.end_container(); // End anonymous struct
        w.put_uint(Tag::Context(0), 7); // Following Context field
        w.end_container(); // End Data struct
        let bytes = w.finish();

        // Wrap in a full ReportDataMessage to test via decode_report_data_message
        let mut full_msg = Writer::new();
        full_msg.start_struct(Tag::Anonymous);
        full_msg.start_array(Tag::Context(1)); // AttributeReportIBs
        full_msg.start_struct(Tag::Anonymous); // AttributeReportIB
        full_msg.start_struct(Tag::Context(1)); // AttributeDataIB
        full_msg.put_uint(Tag::Context(0), 1); // DataVersion
        full_msg.start_list(Tag::Context(1)); // Path
        full_msg.put_uint(Tag::Context(2), 0);
        full_msg.put_uint(Tag::Context(3), 0x0006);
        full_msg.put_uint(Tag::Context(4), 0x0000);
        full_msg.end_container();
        // The Data field with mixed anonymous/context tags
        let mut data_reader = Reader::new(&bytes);
        let data_el = data_reader.next().unwrap().unwrap();
        copy_value(
            &mut full_msg,
            &mut data_reader,
            Tag::Context(2),
            data_el.value,
        )
        .expect("valid TLV");
        full_msg.end_container(); // AttributeDataIB
        full_msg.end_container(); // AttributeReportIB
        full_msg.end_container(); // AttributeReportIBs
        full_msg.end_container(); // outer struct

        let msg = decode_report_data_message(&full_msg.finish()).unwrap();
        assert_eq!(msg.reports.len(), 1);
        let data = msg.reports[0].data.as_ref().unwrap();
        assert_eq!(data.get("0").and_then(|v| v.as_u64()), Some(7));
    }
    #[test]
    fn report_data_message_carries_subscription_id_and_cluster_path() {
        // 購読 report: {0: SubscriptionId, 1: [AttributeReportIB(onoff on-off=true)], 255: rev}
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 77); // SubscriptionId
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeDataIB
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // Path
        w.put_uint(Tag::Context(2), 1); // endpoint
        w.put_uint(Tag::Context(3), 6); // cluster ← 新規に拾う
        w.put_uint(Tag::Context(4), 0); // attribute
        w.end_container();
        w.put_bool(Tag::Context(2), true); // Data
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let m = decode_report_data_message(&w.finish()).unwrap();
        assert_eq!(m.subscription_id, Some(77));
        assert_eq!(m.reports.len(), 1);
        assert_eq!(m.reports[0].endpoint, Some(1));
        assert_eq!(m.reports[0].cluster, Some(6));
        assert_eq!(m.reports[0].attribute, Some(0));
        assert_eq!(m.reports[0].data, Some(serde_json::json!(true)));
    }

    #[test]
    fn empty_keepalive_report_decodes_with_no_reports() {
        // keep-alive: SubscriptionId + rev のみ（AttributeReports 無し）
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 77);
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let m = decode_report_data_message(&w.finish()).unwrap();
        assert_eq!(m.subscription_id, Some(77));
        assert!(m.reports.is_empty());
    }
    #[test]
    fn read_request_roundtrip() {
        let payload = encode_read_request(1, CLUSTER_ON_OFF, ATTR_ON_OFF);
        let paths = decode_read_request(&payload).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].endpoint, Some(1));
        assert_eq!(paths[0].cluster, Some(CLUSTER_ON_OFF));
        assert_eq!(paths[0].attribute, Some(ATTR_ON_OFF));
    }

    #[test]
    fn read_request_cluster_wildcard_roundtrip() {
        let payload = encode_read_request_cluster(0, CLUSTER_DESCRIPTOR);
        let paths = decode_read_request(&payload).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].endpoint, Some(0));
        assert_eq!(paths[0].cluster, Some(CLUSTER_DESCRIPTOR));
        assert_eq!(paths[0].attribute, None);
    }

    #[test]
    fn decode_read_request_message_extracts_fabric_filtered() {
        // encode_read_request は IsFabricFiltered=true を Context(3) に載せる
        // （このファイル既存の encode_read_request_cluster_is_fabric_filtered
        // テストが wire 位置を保証済み）。true がそのまま出ること。
        let payload = encode_read_request(0, CLUSTER_BASIC_INFORMATION, ATTR_VENDOR_ID);
        let req = decode_read_request_message(&payload).unwrap();
        assert_eq!(req.paths.len(), 1);
        assert!(req.fabric_filtered);
    }

    #[test]
    fn decode_read_request_message_defaults_fabric_filtered_when_absent() {
        // IsFabricFiltered を載せない ReadRequest を手組み（AttributeRequests のみ）
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0));
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Context(2), 0); // Endpoint
        w.put_uint(Tag::Context(3), u64::from(CLUSTER_BASIC_INFORMATION));
        w.put_uint(Tag::Context(4), u64::from(ATTR_VENDOR_ID));
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
        w.end_container();
        let req = decode_read_request_message(&w.finish()).unwrap();
        assert!(
            req.fabric_filtered,
            "absent IsFabricFiltered must default to true"
        );
    }

    #[test]
    fn decode_read_request_message_extracts_fabric_filtered_false_from_wire() {
        // Hand-built ReadRequest with IsFabricFiltered explicitly false at
        // Context(3) — asserts the decoded value comes from the actual wire
        // byte, not just "field present" (encode_read_request only ever
        // emits true, so a decoder that hardcoded `Some(true)` on seeing
        // the tag would pass every other test in this file).
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0));
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Context(2), 0); // Endpoint
        w.put_uint(Tag::Context(3), u64::from(CLUSTER_BASIC_INFORMATION));
        w.put_uint(Tag::Context(4), u64::from(ATTR_VENDOR_ID));
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(3), false); // IsFabricFiltered
        w.end_container();
        let req = decode_read_request_message(&w.finish()).unwrap();
        assert!(
            !req.fabric_filtered,
            "explicit false on the wire must decode to false"
        );
    }
    #[test]
    fn report_data_decodes_with_client_decoder() {
        let mut w = Writer::new();
        w.put_bool(Tag::Anonymous, true);
        let payload = encode_report_data(
            &[AttrReportOut {
                endpoint: 0,
                cluster: 0x0028,
                attribute: 0,
                data_version: 1,
                value_tlv: w.finish(),
            }],
            true,
        );
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 1);
        assert!(msg.suppress_response);
        assert_eq!(msg.reports[0].endpoint, Some(0));
        assert_eq!(msg.reports[0].cluster, Some(0x0028));
        assert_eq!(msg.reports[0].attribute, Some(0));
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(true)));
    }

    #[test]
    fn report_data_multiple_reports_decode() {
        let mut w1 = Writer::new();
        w1.put_uint(Tag::Anonymous, 12);
        let mut w2 = Writer::new();
        w2.put_uint(Tag::Anonymous, 1);
        let payload = encode_report_data(
            &[
                AttrReportOut {
                    endpoint: 0,
                    cluster: CLUSTER_BASIC_INFORMATION,
                    attribute: ATTR_DATA_MODEL_REVISION,
                    data_version: 1,
                    value_tlv: w1.finish(),
                },
                AttrReportOut {
                    endpoint: 0,
                    cluster: CLUSTER_BASIC_INFORMATION,
                    attribute: ATTR_VENDOR_ID,
                    data_version: 1,
                    value_tlv: w2.finish(),
                },
            ],
            false,
        );
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.reports.len(), 2);
        assert!(!msg.suppress_response);
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(12)));
        assert_eq!(msg.reports[1].data, Some(serde_json::json!(1)));
    }

    /// Hand-reads the wire bytes of one `ReportEntryOut::Status` entry to
    /// pin the exact AttributeStatusIB nesting (spec §8.9.6):
    /// `AttributeReportIB{0: AttributeStatusIB{0: Path(list), 1:
    /// StatusIB{0: status}}}` — the shape `decode_attribute_status_ib_full`
    /// (this file's client-side `decode_report_data_message` decoder)
    /// already expects.
    #[test]
    fn encode_report_data_entries_status_ib_wire_shape() {
        let payload = encode_report_data_entries(
            &[ReportEntryOut::Status {
                endpoint: 1,
                cluster: CLUSTER_ON_OFF,
                attribute: ATTR_ON_OFF,
                status: STATUS_UNSUPPORTED_ATTRIBUTE,
            }],
            true,
            None,
            false,
        );
        let mut r = Reader::new(&payload);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart); // outer struct

        let reports = r.next().unwrap().unwrap();
        assert_eq!(reports.tag, Tag::Context(1));
        assert_eq!(reports.value, Value::ArrayStart); // AttributeReportIBs

        let report_ib = r.next().unwrap().unwrap();
        assert_eq!(report_ib.value, Value::StructStart); // AttributeReportIB

        let status_ib = r.next().unwrap().unwrap();
        assert_eq!(status_ib.tag, Tag::Context(0));
        assert_eq!(status_ib.value, Value::StructStart); // AttributeStatusIB

        let path = r.next().unwrap().unwrap();
        assert_eq!(path.tag, Tag::Context(0));
        assert_eq!(path.value, Value::ListStart); // Path

        let ep = r.next().unwrap().unwrap();
        assert_eq!((ep.tag, ep.value), (Tag::Context(2), Value::Uint(1)));
        let cl = r.next().unwrap().unwrap();
        assert_eq!(
            (cl.tag, cl.value),
            (Tag::Context(3), Value::Uint(u64::from(CLUSTER_ON_OFF)))
        );
        let attr = r.next().unwrap().unwrap();
        assert_eq!(
            (attr.tag, attr.value),
            (Tag::Context(4), Value::Uint(u64::from(ATTR_ON_OFF)))
        );
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // end Path

        let status_field = r.next().unwrap().unwrap();
        assert_eq!(status_field.tag, Tag::Context(1));
        assert_eq!(status_field.value, Value::StructStart); // StatusIB

        let status = r.next().unwrap().unwrap();
        assert_eq!(
            (status.tag, status.value),
            (
                Tag::Context(0),
                Value::Uint(u64::from(STATUS_UNSUPPORTED_ATTRIBUTE))
            )
        );
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // end StatusIB
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // end AttributeStatusIB
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // end AttributeReportIB
    }

    /// A mixed data+status batch, plus a subscription id, decodes correctly
    /// with the client-side decoder — the realistic path
    /// `core::datamodel::Node::read_entries` (mat-device) exercises.
    #[test]
    fn encode_report_data_entries_mixed_data_and_status_decodes() {
        let mut w = Writer::new();
        w.put_bool(Tag::Anonymous, true);
        let payload = encode_report_data_entries(
            &[
                ReportEntryOut::Data(AttrReportOut {
                    endpoint: 0,
                    cluster: CLUSTER_BASIC_INFORMATION,
                    attribute: ATTR_VENDOR_ID,
                    data_version: 1,
                    value_tlv: w.finish(),
                }),
                ReportEntryOut::Status {
                    endpoint: 0,
                    cluster: CLUSTER_BASIC_INFORMATION,
                    attribute: 0x7777,
                    status: STATUS_UNSUPPORTED_ATTRIBUTE,
                },
            ],
            true,
            Some(42),
            false,
        );
        let msg = decode_report_data_message(&payload).unwrap();
        assert_eq!(msg.subscription_id, Some(42));
        assert_eq!(msg.reports.len(), 2);
        assert_eq!(msg.reports[0].data, Some(serde_json::json!(true)));
        assert_eq!(msg.reports[0].status, None);
        assert_eq!(msg.reports[1].data, None);
        assert_eq!(msg.reports[1].attribute, Some(0x7777));
        assert_eq!(msg.reports[1].status, Some(STATUS_UNSUPPORTED_ATTRIBUTE));
    }
}
