//! Minimal Interaction Model payloads (Matter Core Spec 1.4, Chapter 8).
//!
//! Only what M2's onoff read/invoke path needs: single-attribute
//! ReadRequest/ReportData, single-command InvokeRequest/InvokeResponse, and
//! StatusResponse. No subscriptions, no batched paths, no chunking.

use crate::tlv::{copy_value, Element, Reader, Tag, TlvError, Value, Writer};

pub const PROTOCOL_ID_IM: u16 = crate::message::PROTOCOL_ID_INTERACTION_MODEL;
pub const OPCODE_STATUS_RESPONSE: u8 = 0x01;
pub const OPCODE_READ_REQUEST: u8 = 0x02;
pub const OPCODE_REPORT_DATA: u8 = 0x05;
pub const OPCODE_WRITE_REQUEST: u8 = 0x06;
pub const OPCODE_WRITE_RESPONSE: u8 = 0x07;
pub const OPCODE_INVOKE_REQUEST: u8 = 0x08;
pub const OPCODE_INVOKE_RESPONSE: u8 = 0x09;
pub const OPCODE_TIMED_REQUEST: u8 = 0x0A;
pub const IM_REVISION: u8 = 12;
pub const CLUSTER_ON_OFF: u32 = 0x0006;
pub const ATTR_ON_OFF: u32 = 0x0000;
pub const CMD_ON_OFF_OFF: u32 = 0x00;
pub const CMD_ON_OFF_ON: u32 = 0x01;
pub const CMD_ON_OFF_TOGGLE: u32 = 0x02;
pub const CLUSTER_COLOR_CONTROL: u32 = 0x0300;
pub const ATTR_CURRENT_HUE: u32 = 0x0000;
pub const ATTR_CURRENT_SATURATION: u32 = 0x0001;
pub const ATTR_COLOR_TEMPERATURE_MIREDS: u32 = 0x0007;
pub const CMD_MOVE_TO_HUE_AND_SATURATION: u32 = 0x06;
pub const CMD_MOVE_TO_COLOR_TEMPERATURE: u32 = 0x0A;

/// A decoded scalar attribute/data value. Containers are not supported (M2
/// scope is single scalar attributes such as onoff's `OnOff` bool).
#[derive(Debug, Clone, PartialEq)]
pub enum ImValue {
    Bool(bool),
    Uint(u64),
    Int(i64),
    Utf8(String),
    Bytes(Vec<u8>),
    Null,
}

/// Decoded ReportData for a single-attribute read (first AttributeReportIB
/// only; see module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct ReportData {
    pub suppress_response: bool,
    pub value: Option<ImValue>,
    pub status: Option<u8>,
}

/// Decoded InvokeResponse outcome for a single command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvokeOutcome {
    pub status: u8,
    pub cluster_status: Option<u8>,
}

/// Decoded InvokeResponse for a single command, including any command-data
/// fields (spec §8.9.4.2: a successful response may carry a CommandDataIB
/// instead of a bare CommandStatusIB — e.g. commands that return a value).
/// `fields_tlv`, when present, is one complete, well-formed TLV element (its
/// top-level tag re-written to `Tag::Anonymous`) holding the response
/// CommandFields struct, ready to hand to a cluster-specific decoder.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeResponseData {
    pub status: u8,
    pub cluster_status: Option<u8>,
    pub fields_tlv: Option<Vec<u8>>,
}

/// Interaction Model level errors (decode failures and device-reported
/// rejections carried in IM status codes, spec §8.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImError {
    Tlv(TlvError),
    Malformed(&'static str),
    UnsupportedValue,
    AttributeStatus(u8),
    StatusResponse(u8),
    CommandStatus {
        status: u8,
        cluster_status: Option<u8>,
    },
}

impl std::fmt::Display for ImError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImError::Tlv(e) => write!(f, "malformed interaction model TLV: {e}"),
            ImError::Malformed(m) => write!(f, "malformed interaction model payload: {m}"),
            ImError::UnsupportedValue => write!(f, "unsupported attribute value encoding"),
            ImError::AttributeStatus(s) => write!(f, "device rejected read: IM status 0x{s:02X}"),
            ImError::StatusResponse(s) => {
                write!(f, "device sent StatusResponse: IM status 0x{s:02X}")
            }
            ImError::CommandStatus {
                status,
                cluster_status: Some(cs),
            } => write!(
                f,
                "device rejected command: IM status 0x{status:02X} (cluster status 0x{cs:02X})"
            ),
            ImError::CommandStatus {
                status,
                cluster_status: None,
            } => write!(f, "device rejected command: IM status 0x{status:02X}"),
        }
    }
}

impl std::error::Error for ImError {}

impl From<TlvError> for ImError {
    fn from(e: TlvError) -> Self {
        ImError::Tlv(e)
    }
}

/// Reads the next element and requires it to be a struct start (every IM
/// message is a top-level anonymous struct).
fn expect_struct_start(r: &mut Reader) -> Result<(), ImError> {
    match r.next()?.ok_or(ImError::Malformed("empty payload"))?.value {
        Value::StructStart => Ok(()),
        _ => Err(ImError::Malformed("expected struct")),
    }
}

/// Consumes the rest of a container whose `*Start` element has already been
/// read (depth 1), including its matching `ContainerEnd`. Used to skip
/// unknown tags/containers and additional report/response entries beyond
/// the first (M2 only interprets a single attribute/command per message).
fn skip_container(r: &mut Reader) -> Result<(), ImError> {
    let mut depth = 1usize;
    while depth > 0 {
        let el = r.next()?.ok_or(ImError::Malformed("truncated container"))?;
        match el.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => depth -= 1,
            _ => {}
        }
    }
    Ok(())
}

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
    w.put_bool(Tag::Context(3), false); // IsFabricFiltered
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

fn value_to_im(v: Value) -> Result<ImValue, ImError> {
    match v {
        Value::Bool(b) => Ok(ImValue::Bool(b)),
        Value::Uint(u) => Ok(ImValue::Uint(u)),
        Value::Int(i) => Ok(ImValue::Int(i)),
        Value::Utf8(s) => Ok(ImValue::Utf8(s.to_string())),
        Value::Bytes(b) => Ok(ImValue::Bytes(b.to_vec())),
        Value::Null => Ok(ImValue::Null),
        Value::StructStart | Value::ArrayStart | Value::ListStart => Err(ImError::UnsupportedValue),
        // ImValue has no float variant (M2 scope: bool/uint/int/string/bytes/null).
        Value::F32(_) | Value::F64(_) => Err(ImError::UnsupportedValue),
        Value::ContainerEnd => Err(ImError::Malformed("unexpected container end as data value")),
    }
}

/// AttributeStatusIB (spec §8.9.2.2): `{0: Path, 1: StatusIB{0: status, ...}}`.
/// Assumes the caller already consumed the `StructStart` (tag 0) that opens
/// this AttributeStatusIB.
fn decode_attribute_status_ib(r: &mut Reader) -> Result<u8, ImError> {
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
    pub attribute: Option<u32>,
    /// path に ListIndex(null) があれば true（チャンク化 list の item 追記）。
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
    pub more_chunks: bool,
    pub suppress_response: bool,
}

/// TLV 単一要素（コンテナ含む）を JSON へ。`first` は既に読んだ先頭要素。
/// JSON 化規約（固定）: Bool→bool, Uint/Int→number, F32/F64→number,
/// Utf8→string, Bytes→小文字hex文字列, Null→null, Array/List→JSON array,
/// Struct→JSON object（キーは context tag 番号の10進文字列。名前付けは
/// 上位層の責務）。
fn tlv_element_to_json(r: &mut Reader, first: Element) -> Result<serde_json::Value, ImError> {
    tlv_element_to_json_impl(r, first, 0)
}

fn tlv_element_to_json_impl(
    r: &mut Reader,
    first: Element,
    depth: usize,
) -> Result<serde_json::Value, ImError> {
    const MAX_DEPTH: usize = 32;
    if depth > MAX_DEPTH {
        return Err(ImError::Malformed("tlv nesting too deep"));
    }

    use serde_json::Value as J;
    Ok(match first.value {
        Value::Bool(b) => J::Bool(b),
        Value::Uint(u) => J::from(u),
        Value::Int(i) => J::from(i),
        Value::F32(f) => serde_json::json!(f),
        Value::F64(f) => serde_json::json!(f),
        Value::Utf8(s) => J::String(s.to_string()),
        Value::Bytes(b) => J::String(hex_lower(b)),
        Value::Null => J::Null,
        Value::ArrayStart | Value::ListStart => {
            let mut items = Vec::new();
            loop {
                let el = r.next()?.ok_or(ImError::Malformed("truncated array"))?;
                if el.value == Value::ContainerEnd {
                    break;
                }
                items.push(tlv_element_to_json_impl(r, el, depth + 1)?);
            }
            J::Array(items)
        }
        Value::StructStart => {
            let mut map = serde_json::Map::new();
            loop {
                let el = r.next()?.ok_or(ImError::Malformed("truncated struct"))?;
                if el.value == Value::ContainerEnd {
                    break;
                }
                let key = match el.tag {
                    Tag::Context(n) => n.to_string(),
                    _ => {
                        // 想定外タグはスキップ（前方互換）。ただしそれがコンテナ開始ならば
                        // 中身を読み飛ばす（そうでないと兄弟フィールドの解釈が壊れる）。
                        if matches!(
                            el.value,
                            Value::StructStart | Value::ArrayStart | Value::ListStart
                        ) {
                            skip_container(r)?;
                        }
                        continue;
                    }
                };
                map.insert(key, tlv_element_to_json_impl(r, el, depth + 1)?);
            }
            J::Object(map)
        }
        Value::ContainerEnd => return Err(ImError::Malformed("dangling container end")),
    })
}

fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// AttributePathIB (spec §8.9.2.2, list) のうち endpoint(Context 2) /
/// attribute(Context 4) / ListIndex(Context 5, `Null` ならチャンク化 list
/// への item 追記) を拾う。他フィールド（Node/Cluster/DataVersion 等）は
/// 読み飛ばす。呼び出し側は path を開く `ListStart` を既に読んでいる前提。
fn decode_attribute_path_ib(r: &mut Reader) -> Result<(Option<u16>, Option<u32>, bool), ImError> {
    let mut endpoint = None;
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
            (Tag::Context(4), Value::Uint(v)) => {
                attribute = Some(
                    u32::try_from(v)
                        .map_err(|_| ImError::Malformed("attribute id out of range"))?,
                );
            }
            (Tag::Context(5), Value::Null) => list_append = true,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok((endpoint, attribute, list_append))
}

/// AttributeStatusIB (spec §8.9.2.2): `{0: Path, 1: StatusIB{0: status, ...}}`,
/// path も拾う汎用版（`decode_attribute_status_ib` の M2 版とは独立— 既存
/// API 無改変のため別関数にした）。呼び出し側は AttributeReportIB の
/// anonymous `StructStart`（tag 0）を既に読んでいる前提。
fn decode_attribute_status_ib_full(
    r: &mut Reader,
) -> Result<(Option<u16>, Option<u32>, u8), ImError> {
    let mut endpoint = None;
    let mut attribute = None;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated attribute status"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => {
                let (ep, attr, _) = decode_attribute_path_ib(r)?;
                endpoint = ep;
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
    Ok((endpoint, attribute, status))
}

/// `decode_attribute_data_ib_full` の戻り値: (endpoint, attribute,
/// list_append, data).
type AttributeDataFields = (Option<u16>, Option<u32>, bool, Option<serde_json::Value>);

/// AttributeDataIB (spec §8.9.2.2): `{0: DataVersion, 1: Path, 2: Data}`,
/// path も拾い Data を JSON 化する汎用版（`decode_attribute_data_ib` の M2
/// 版とは独立）。呼び出し側は AttributeReportIB の anonymous `StructStart`
/// （tag 1）を既に読んでいる前提。
fn decode_attribute_data_ib_full(r: &mut Reader) -> Result<AttributeDataFields, ImError> {
    let mut endpoint = None;
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
                let (ep, attr, la) = decode_attribute_path_ib(r)?;
                endpoint = ep;
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
    Ok((endpoint, attribute, list_append, data))
}

/// AttributeReportIB (spec §8.9.2.2): `{0: AttributeStatusIB} | {1: AttributeDataIB}`,
/// 汎用版（path/JSON も拾う。`decode_attribute_report_ib` の M2 版とは独立）。
/// 呼び出し側は開く anonymous `StructStart` を既に読んでいる前提。
fn decode_attribute_report_ib_full(r: &mut Reader) -> Result<AttributeReport, ImError> {
    let mut endpoint = None;
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
                let (ep, attr, s) = decode_attribute_status_ib_full(r)?;
                endpoint = ep;
                attribute = attr;
                status = Some(s);
            }
            (Tag::Context(1), Value::StructStart) => {
                let (ep, attr, la, d) = decode_attribute_data_ib_full(r)?;
                endpoint = ep;
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
    let mut more_chunks = false;
    let mut suppress_response = false;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated report data"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
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
        more_chunks,
        suppress_response,
    })
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
    w.put_bool(Tag::Context(3), false); // IsFabricFiltered
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
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

/// CommandFields for colorcontrol MoveToHueAndSaturation (cluster spec
/// §3.2.11.7): `{0: hue, 1: saturation, 2: transition_time (0.1 s units),
/// 3: options_mask, 4: options_override}`. Options are fixed to 0 (execute
/// unconditionally), which is what chip-tool sends by default too.
pub fn encode_move_to_hue_and_saturation_fields(
    hue: u8,
    saturation: u8,
    transition_time_ds: u16,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(hue));
    w.put_uint(Tag::Context(1), u64::from(saturation));
    w.put_uint(Tag::Context(2), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(3), 0);
    w.put_uint(Tag::Context(4), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for colorcontrol MoveToColorTemperature (cluster spec
/// §3.2.11.10): `{0: ColorTemperatureMireds(u16), 1: TransitionTime(u16,
/// 0.1 s units), 2: OptionsMask(u8), 3: OptionsOverride(u8)}`. Options are
/// fixed to 0 (execute per the device's Options attribute), matching what
/// chip-tool sends by default.
pub fn encode_move_to_color_temperature_fields(mireds: u16, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(mireds));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}

/// InvokeRequestMessage (spec §8.9.4) の共通本体。`timed` が TimedRequest
/// フィールド（タイムド呼び出し、spec §8.5）の値になる。公開関数
/// `encode_invoke_request` / `encode_invoke_request_timed` はどちらもこれを
/// 呼ぶだけの薄いラッパで、ワイヤ形状は完全に共有する。
///
/// `fields_tlv`, if given, must be one complete, well-formed TLV element
/// (any tag; it is re-tagged) holding the command's CommandFields struct.
/// M2's onoff commands (on/off/toggle) take no fields, so this is `None` in
/// practice; the parameter exists so the wire format doesn't have to change
/// when a fielded command is added later. Panics if `fields_tlv` is not
/// well-formed TLV — a caller/programmer error, not a device response to
/// validate defensively.
fn encode_invoke_request_inner(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
    timed: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse
    w.put_bool(Tag::Context(1), timed); // TimedRequest
    w.start_array(Tag::Context(2)); // InvokeRequests
    w.start_struct(Tag::Anonymous); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container(); // CommandPath
    if let Some(fields) = fields_tlv {
        w.put_raw_element(Tag::Context(1), fields);
    }
    w.end_container(); // CommandDataIB
    w.end_container(); // InvokeRequests
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// InvokeRequestMessage (spec §8.9.4) for a single command. TimedRequest is
/// always `false` — see `encode_invoke_request_timed` for the timed variant
/// (spec §8.5, タイムド呼び出し).
pub fn encode_invoke_request(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    encode_invoke_request_inner(endpoint, cluster, command, fields_tlv, false)
}

/// InvokeRequestMessage (spec §8.9.4) with TimedRequest = true. Must be sent
/// on the same exchange as a preceding `encode_timed_request` whose
/// StatusResponse(SUCCESS) has already been received — the timeout window it
/// establishes covers exactly this InvokeRequest (spec §8.5.1). Same fields
/// contract as `encode_invoke_request` otherwise.
pub fn encode_invoke_request_timed(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    encode_invoke_request_inner(endpoint, cluster, command, fields_tlv, true)
}

/// TimedRequestMessage (spec §8.5.1, タイムド呼び出し): `{0: Timeout(u16,
/// ミリ秒), 255: InteractionModelRevision}`. Opens a timeout window during
/// which the immediately following InvokeRequest/WriteRequest (same
/// exchange, TimedRequest flag true) must arrive at the device, otherwise it
/// rejects the timed action. `mat-controller` only uses this ahead of a
/// timed invoke (`SecureSession::invoke_for_data`).
pub fn encode_timed_request(timeout_ms: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(timeout_ms));
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

/// InvokeRequestMessage for a groupcast command (spec §8.9.4): group
/// invokes carry no response, so SuppressResponse is true, and the
/// CommandPath is group-scoped (no endpoint — the device's group table
/// routes to its bound endpoints). Fields contract matches
/// `encode_invoke_request`.
pub fn encode_group_invoke_request(
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), true); // SuppressResponse
    w.put_bool(Tag::Context(1), false); // TimedRequest
    w.start_array(Tag::Context(2)); // InvokeRequests
    w.start_struct(Tag::Anonymous); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath (group-scoped)
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container();
    if let Some(fields) = fields_tlv {
        w.put_raw_element(Tag::Context(1), fields);
    }
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

/// StatusIB (spec §8.9.2.3) inside a CommandStatusIB: `{0: status, 1: cluster_status}`.
/// Assumes the caller already consumed the `StructStart` (tag 1) opening it.
fn decode_status_ib(r: &mut Reader) -> Result<(u8, Option<u8>), ImError> {
    let mut status = None;
    let mut cluster_status = None;
    loop {
        let el = r.next()?.ok_or(ImError::Malformed("truncated status ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                status = Some(
                    u8::try_from(v)
                        .map_err(|_| ImError::Malformed("command status code out of range"))?,
                );
            }
            (Tag::Context(1), Value::Uint(v)) => {
                cluster_status = Some(
                    u8::try_from(v)
                        .map_err(|_| ImError::Malformed("cluster status code out of range"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    let status = status.ok_or(ImError::Malformed("status ib without status"))?;
    Ok((status, cluster_status))
}

/// CommandStatusIB (spec §8.9.4.2): `{0: CommandPath, 1: StatusIB}`.
/// Assumes the caller already consumed the `StructStart` (tag 1) that opens
/// this CommandStatusIB (InvokeResponseIB's `Status` field).
fn decode_command_status_ib(r: &mut Reader) -> Result<(u8, Option<u8>), ImError> {
    let mut result = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command status ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::StructStart) => {
                result = Some(decode_status_ib(r)?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed("command status ib without StatusIB"))
}

/// InvokeResponseIB (spec §8.9.4.2): `{0: CommandDataIB} | {1: CommandStatusIB}`.
/// Assumes the caller already consumed the anonymous `StructStart` opening
/// this InvokeResponseIB.
fn decode_invoke_response_ib(r: &mut Reader) -> Result<InvokeOutcome, ImError> {
    let mut outcome = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => {
                // Command (CommandDataIB): a response carrying data is a
                // successful invocation. M2's onoff commands never produce
                // one, but don't choke on a well-formed message that does.
                skip_container(r)?;
                outcome = Some(InvokeOutcome {
                    status: 0,
                    cluster_status: None,
                });
            }
            (Tag::Context(1), Value::StructStart) => {
                let (status, cluster_status) = decode_command_status_ib(r)?;
                outcome = Some(InvokeOutcome {
                    status,
                    cluster_status,
                });
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    outcome.ok_or(ImError::Malformed(
        "invoke response ib without Command or Status",
    ))
}

/// InvokeResponseMessage (spec §8.9.4). Only the first InvokeResponseIB is
/// interpreted (M2 invokes one command at a time).
pub fn decode_invoke_response(payload: &[u8]) -> Result<InvokeOutcome, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut outcome: Option<InvokeOutcome> = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ArrayStart) => {
                // InvokeResponses
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            outcome = Some(decode_invoke_response_ib(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in invoke responses",
                            ))
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
    outcome.ok_or(ImError::Malformed(
        "invoke response without InvokeResponseIB",
    ))
}

/// CommandDataIB (spec §8.9.4.2): `{0: CommandPathIB, 1: CommandFields}`.
/// Assumes the caller already consumed the `StructStart` (tag 0) that opens
/// this CommandDataIB (InvokeResponseIB's `Command` field). Returns the
/// CommandFields struct (tag 1), if present, re-tagged to `Tag::Anonymous`
/// as one complete TLV element — the CommandPathIB (tag 0) is skipped since
/// `decode_invoke_response_data`'s callers only need the fields, not the
/// echoed path.
fn decode_command_data_ib(r: &mut Reader) -> Result<Option<Vec<u8>>, ImError> {
    let mut fields = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command data ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::StructStart) => {
                // CommandFields: always a struct (cluster spec command
                // parameters). Re-tag to Anonymous, same convention as
                // `encode_invoke_request`'s fields_tlv splice.
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, Value::StructStart)?;
                fields = Some(w.finish());
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok(fields)
}

/// InvokeResponseIB (spec §8.9.4.2): `{0: CommandDataIB} | {1: CommandStatusIB}`,
/// decoded into `InvokeResponseData` (data-carrying variant of
/// `decode_invoke_response_ib`). Assumes the caller already consumed the
/// anonymous `StructStart` opening this InvokeResponseIB.
fn decode_invoke_response_ib_data(r: &mut Reader) -> Result<InvokeResponseData, ImError> {
    let mut result = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => {
                // Command (CommandDataIB): a response carrying data is a
                // successful invocation (status 0), possibly with fields.
                let fields_tlv = decode_command_data_ib(r)?;
                result = Some(InvokeResponseData {
                    status: 0,
                    cluster_status: None,
                    fields_tlv,
                });
            }
            (Tag::Context(1), Value::StructStart) => {
                let (status, cluster_status) = decode_command_status_ib(r)?;
                result = Some(InvokeResponseData {
                    status,
                    cluster_status,
                    fields_tlv: None,
                });
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed(
        "invoke response ib without Command or Status",
    ))
}

/// InvokeResponseMessage (spec §8.9.4), data-carrying variant of
/// `decode_invoke_response`: a CommandDataIB response (status 0) yields its
/// CommandFields as `fields_tlv`; a CommandStatusIB response yields
/// `status`/`cluster_status` as today with `fields_tlv: None`. Only the
/// first InvokeResponseIB is interpreted (same single-command scope as
/// `decode_invoke_response`). Unlike `decode_invoke_response`, a non-zero
/// status is returned as data, not as `Err` — callers that want the
/// today's fail-on-error behavior should check `status` themselves (see
/// `SecureSession::invoke_for_data`).
pub fn decode_invoke_response_data(payload: &[u8]) -> Result<InvokeResponseData, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut result: Option<InvokeResponseData> = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::ArrayStart) => {
                // InvokeResponses
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            result = Some(decode_invoke_response_ib_data(&mut r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in invoke responses",
                            ))
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
    result.ok_or(ImError::Malformed(
        "invoke response without InvokeResponseIB",
    ))
}

/// StatusResponseMessage (spec §8.9.3): `{0: Status, 255: revision}`.
pub fn encode_status_response(status: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(status));
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

pub fn decode_status_response(payload: &[u8]) -> Result<u8, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut status = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated status response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                status = Some(
                    u8::try_from(v)
                        .map_err(|_| ImError::Malformed("status response code out of range"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    status.ok_or(ImError::Malformed("status response without status"))
}

/// Encodes an `ImValue` scalar as one standalone, well-formed TLV element
/// (tag is discarded by the caller — `encode_write_request` immediately
/// splices it via `Writer::put_raw_element`).
fn encode_im_value(value: &ImValue) -> Vec<u8> {
    let mut w = Writer::new();
    match value {
        ImValue::Bool(b) => w.put_bool(Tag::Anonymous, *b),
        ImValue::Uint(u) => w.put_uint(Tag::Anonymous, *u),
        ImValue::Int(i) => w.put_int(Tag::Anonymous, *i),
        ImValue::Utf8(s) => w.put_str(Tag::Anonymous, s),
        ImValue::Bytes(b) => w.put_bytes(Tag::Anonymous, b),
        ImValue::Null => w.put_null(Tag::Anonymous),
    }
    w.finish()
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

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
        assert_eq!(
            (els[8].tag, els[8].value),
            (Tag::Context(3), Value::Bool(false))
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
    fn invoke_request_and_response_roundtrip_shapes() {
        let buf = encode_invoke_request(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None);
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        assert_eq!(
            (els[1].tag, els[1].value),
            (Tag::Context(0), Value::Bool(false))
        );
        assert_eq!(
            (els[2].tag, els[2].value),
            (Tag::Context(1), Value::Bool(false))
        );
        assert_eq!(
            (els[3].tag, els[3].value),
            (Tag::Context(2), Value::ArrayStart)
        );
        // CommandDataIB struct → path list {0:1, 1:6, 2:2}
        assert_eq!(els[4].value, Value::StructStart);
        assert_eq!(
            (els[5].tag, els[5].value),
            (Tag::Context(0), Value::ListStart)
        );
        assert_eq!(els[6].value, Value::Uint(1));
        assert_eq!(els[7].value, Value::Uint(6));
        assert_eq!(els[8].value, Value::Uint(2));

        // InvokeResponse: Status(成功)
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let out = decode_invoke_response(&w.finish()).unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.cluster_status, None);
    }

    #[test]
    fn decodes_invoke_response_nonzero_status_with_cluster_status() {
        // CommandStatusIB carrying StatusIB{0: 0x81 UNSUPPORTED_COMMAND, 1: 0x42}.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0x81);
        w.put_uint(Tag::Context(1), 0x42);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let out = decode_invoke_response(&w.finish()).unwrap();
        assert_eq!(out.status, 0x81);
        assert_eq!(out.cluster_status, Some(0x42));
    }

    #[test]
    fn encode_invoke_request_splices_fields_tlv() {
        // A one-field CommandFields struct: { 0: 128 }.
        let mut fw = Writer::new();
        fw.start_struct(Tag::Anonymous);
        fw.put_uint(Tag::Context(0), 128);
        fw.end_container();
        let fields = fw.finish();

        let buf = encode_invoke_request(1, CLUSTER_ON_OFF, CMD_ON_OFF_ON, Some(&fields));
        let mut r = Reader::new(&buf);
        let mut els = Vec::new();
        while let Some(e) = r.next().unwrap() {
            els.push(e);
        }
        // struct{ 0: false, 1: false, 2: array[ struct{ 0: list{1,6,1}, <fields> } ], 255: 12 }
        assert_eq!(els[4].value, Value::StructStart); // CommandDataIB
        assert_eq!(els[5].value, Value::ListStart); // CommandPath
        assert_eq!(els[9].value, Value::ContainerEnd); // end of CommandPath list
                                                       // The spliced fields struct, retagged to Context(1) inside CommandDataIB.
        assert_eq!(
            (els[10].tag, els[10].value),
            (Tag::Context(1), Value::StructStart)
        );
        assert_eq!(
            (els[11].tag, els[11].value),
            (Tag::Context(0), Value::Uint(128))
        );
        assert_eq!(els[12].value, Value::ContainerEnd); // end of fields struct
        assert_eq!(els[13].value, Value::ContainerEnd); // end of CommandDataIB
    }

    #[test]
    fn status_response_roundtrip() {
        assert_eq!(
            decode_status_response(&encode_status_response(0)).unwrap(),
            0
        );
        assert_eq!(
            decode_status_response(&encode_status_response(0x7E)).unwrap(),
            0x7E
        );
    }

    #[test]
    fn move_to_hue_and_saturation_fields_shape() {
        let fields = encode_move_to_hue_and_saturation_fields(200, 254, 10);
        let mut r = Reader::new(&fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let expect = [
            (0u8, 200u64), // hue
            (1, 254),      // saturation
            (2, 10),       // transition time (0.1s 単位)
            (3, 0),        // options mask
            (4, 0),        // options override
        ];
        for (tag, val) in expect {
            let el = r.next().unwrap().unwrap();
            assert_eq!((el.tag, el.value), (Tag::Context(tag), Value::Uint(val)));
        }
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn move_fields_splice_into_invoke_request() {
        // fields_tlv スプライス経路（well-formed 1 要素として受理され panic しない）
        let fields = encode_move_to_hue_and_saturation_fields(1, 2, 3);
        let req = encode_invoke_request(
            1,
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_HUE_AND_SATURATION,
            Some(&fields),
        );
        assert!(!req.is_empty());
    }

    #[test]
    fn move_to_color_temperature_fields_match_wire_shape() {
        // CommandFields (colorcontrol MoveToColorTemperature, cluster §3.2.11.10):
        // {0: ColorTemperatureMireds(u16), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToHueAndSaturation エンコーダと同じ手筋（anonymous struct + context tags）。
        let bytes = encode_move_to_color_temperature_fields(370, 30);
        // anonymous struct open (0x15) ... context-tagged uints ... close (0x18)
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // mireds=370=0x0172 が context tag 0 の u16 として載る（0x25 = ctx-tag u16）
        assert!(
            bytes.windows(4).any(|w| w == [0x25, 0x00, 0x72, 0x01]),
            "mireds 370 as ctx-tag-0 u16 little-endian, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u8 として載る（0x24 = ctx-tag u8）
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x01, 0x1E]),
            "transition 30 as ctx-tag-1 u8, got {bytes:02X?}"
        );
    }

    #[test]
    fn group_invoke_request_suppresses_response_and_omits_endpoint() {
        let got = encode_group_invoke_request(CLUSTER_ON_OFF, CMD_ON_OFF_ON, None);
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), true); // SuppressResponse: group は応答なし
        w.put_bool(Tag::Context(1), false); // TimedRequest
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0)); // CommandPath: group-scoped、endpoint なし
        w.put_uint(Tag::Context(1), u64::from(CLUSTER_ON_OFF));
        w.put_uint(Tag::Context(2), u64::from(CMD_ON_OFF_ON));
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
        w.end_container();
        assert_eq!(got, w.finish());
    }

    #[test]
    fn timed_request_shape() {
        let p = encode_timed_request(10_000);
        let mut r = Reader::new(&p);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Uint(10_000)));
    }

    #[test]
    fn invoke_request_timed_sets_flag() {
        let p = encode_invoke_request_timed(0, 0x3E, 0x00, None);
        let mut r = Reader::new(&p);
        r.next().unwrap(); // struct
        r.next().unwrap(); // SuppressResponse
        let e = r.next().unwrap().unwrap(); // TimedRequest
        assert!(matches!(e.value, Value::Bool(true)));
    }

    #[test]
    fn decode_invoke_response_with_command_fields() {
        // InvokeResponseMessage { 1: [ { 0: CommandDataIB { 0: path, 1: fields } } ] }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous); // InvokeResponseIB
        w.start_struct(Tag::Context(0)); // CommandDataIB
        w.start_list(Tag::Context(0)); // CommandPathIB
        w.put_uint(Tag::Context(0), 0);
        w.put_uint(Tag::Context(1), 0x3E);
        w.put_uint(Tag::Context(2), 0x01);
        w.end_container();
        w.start_struct(Tag::Context(1)); // CommandFields
        w.put_bytes(Tag::Context(0), b"elements");
        w.put_bytes(Tag::Context(1), &[0xAB; 64]);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let d = decode_invoke_response_data(&w.finish()).unwrap();
        assert_eq!(d.status, 0);
        let fields = d.fields_tlv.unwrap();
        let mut fr = Reader::new(&fields);
        assert!(matches!(
            fr.next().unwrap().unwrap().value,
            Value::StructStart
        ));
        assert!(matches!(fr.next().unwrap().unwrap().value, Value::Bytes(b) if b == b"elements"));
    }

    #[test]
    fn decode_invoke_response_data_status_form() {
        // 既存 decode_invoke_response の「nonzero status + cluster status」
        // ケース (decodes_invoke_response_nonzero_status_with_cluster_status)
        // と同じ CommandStatusIB 形（InvokeResponseIB{1: CommandStatusIB}）で
        // 合成し、status/cluster_status が透過し fields_tlv は None になる
        // ことを確認する。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0x81);
        w.put_uint(Tag::Context(1), 0x42);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let d = decode_invoke_response_data(&w.finish()).unwrap();
        assert_eq!(d.status, 0x81);
        assert_eq!(d.cluster_status, Some(0x42));
        assert_eq!(d.fields_tlv, None);
    }

    #[test]
    fn group_invoke_request_carries_fields() {
        let fields = encode_move_to_color_temperature_fields(370, 0);
        let got = encode_group_invoke_request(
            CLUSTER_COLOR_CONTROL,
            CMD_MOVE_TO_COLOR_TEMPERATURE,
            Some(&fields),
        );
        // fields が ctx1 で CommandDataIB に入ること（unicast 版と同じ再タグ規約）。
        // 厳密比較: unicast 版のテストに倣い Writer で期待列を組む。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), true);
        w.put_bool(Tag::Context(1), false);
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0));
        w.put_uint(Tag::Context(1), u64::from(CLUSTER_COLOR_CONTROL));
        w.put_uint(Tag::Context(2), u64::from(CMD_MOVE_TO_COLOR_TEMPERATURE));
        w.end_container();
        w.start_struct(Tag::Context(1));
        w.put_uint(Tag::Context(0), 370);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(2), 0);
        w.put_uint(Tag::Context(3), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
        w.end_container();
        assert_eq!(got, w.finish());
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
    fn tlv_to_json_rejects_pathological_nesting() {
        // Construct deeply nested TLV (100 levels) to test stack overflow protection.
        // ArrayStart × 100 without matching ContainerEnd should fail with Malformed
        // when depth limit (32) is exceeded.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        w.start_list(Tag::Context(1));
        w.put_uint(Tag::Context(4), 1);
        w.end_container();
        // Data (Context(2)) with 100 nested arrays
        for _ in 0..100 {
            w.start_array(Tag::Context(2));
        }
        for _ in 0..100 {
            w.end_container();
        }
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        let err = decode_report_data_message(&w.finish()).unwrap_err();
        assert!(
            matches!(err, ImError::Malformed(_)),
            "Expected Malformed error, got {err:?}"
        );
    }

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
}
