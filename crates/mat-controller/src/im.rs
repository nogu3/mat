//! Minimal Interaction Model payloads (Matter Core Spec 1.4, Chapter 8).
//!
//! Only what M2's onoff read/invoke path needs: single-attribute
//! ReadRequest/ReportData, single-command InvokeRequest/InvokeResponse, and
//! StatusResponse. No subscriptions, no batched paths, no chunking.

use crate::tlv::{Reader, Tag, TlvError, Value, Writer};

pub const PROTOCOL_ID_IM: u16 = crate::message::PROTOCOL_ID_INTERACTION_MODEL;
pub const OPCODE_STATUS_RESPONSE: u8 = 0x01;
pub const OPCODE_READ_REQUEST: u8 = 0x02;
pub const OPCODE_REPORT_DATA: u8 = 0x05;
pub const OPCODE_INVOKE_REQUEST: u8 = 0x08;
pub const OPCODE_INVOKE_RESPONSE: u8 = 0x09;
pub const IM_REVISION: u8 = 12;
pub const CLUSTER_ON_OFF: u32 = 0x0006;
pub const ATTR_ON_OFF: u32 = 0x0000;
pub const CMD_ON_OFF_OFF: u32 = 0x00;
pub const CMD_ON_OFF_ON: u32 = 0x01;
pub const CMD_ON_OFF_TOGGLE: u32 = 0x02;

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

/// Deep-copies one TLV element (and, if a container, its full subtree) from
/// `r` into `w`, re-tagging only the top-level element as `tag`. Used to
/// splice a caller-provided, already-encoded CommandFields element into an
/// InvokeRequest without `tlv::Writer` needing a raw-append escape hatch.
fn copy_retagged(w: &mut Writer, r: &mut Reader, tag: Tag) -> Result<(), TlvError> {
    let el = r.next()?.ok_or(TlvError::Truncated)?;
    copy_value(w, r, tag, el.value)
}

fn copy_value(w: &mut Writer, r: &mut Reader, tag: Tag, value: Value) -> Result<(), TlvError> {
    match value {
        Value::Int(v) => w.put_int(tag, v),
        Value::Uint(v) => w.put_uint(tag, v),
        Value::Bool(v) => w.put_bool(tag, v),
        Value::F32(v) => w.put_f32(tag, v),
        Value::F64(v) => w.put_f64(tag, v),
        Value::Utf8(v) => w.put_str(tag, v),
        Value::Bytes(v) => w.put_bytes(tag, v),
        Value::Null => w.put_null(tag),
        Value::StructStart => {
            w.start_struct(tag);
            return copy_container(w, r);
        }
        Value::ArrayStart => {
            w.start_array(tag);
            return copy_container(w, r);
        }
        Value::ListStart => {
            w.start_list(tag);
            return copy_container(w, r);
        }
        Value::ContainerEnd => {}
    }
    Ok(())
}

fn copy_container(w: &mut Writer, r: &mut Reader) -> Result<(), TlvError> {
    loop {
        let el = r.next()?.ok_or(TlvError::Truncated)?;
        if el.value == Value::ContainerEnd {
            w.end_container();
            return Ok(());
        }
        copy_value(w, r, el.tag, el.value)?;
    }
}

/// InvokeRequestMessage (spec §8.9.4) for a single command.
///
/// `fields_tlv`, if given, must be one complete, well-formed TLV element
/// (any tag; it is re-tagged) holding the command's CommandFields struct.
/// M2's onoff commands (on/off/toggle) take no fields, so this is `None` in
/// practice; the parameter exists so the wire format doesn't have to change
/// when a fielded command is added later. Panics if `fields_tlv` is not
/// well-formed TLV — a caller/programmer error, not a device response to
/// validate defensively.
pub fn encode_invoke_request(
    endpoint: u16,
    cluster: u32,
    command: u32,
    fields_tlv: Option<&[u8]>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse
    w.put_bool(Tag::Context(1), false); // TimedRequest
    w.start_array(Tag::Context(2)); // InvokeRequests
    w.start_struct(Tag::Anonymous); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container(); // CommandPath
    if let Some(fields) = fields_tlv {
        let mut fr = Reader::new(fields);
        copy_retagged(&mut w, &mut fr, Tag::Context(1))
            .expect("fields_tlv must be one well-formed TLV element");
    }
    w.end_container(); // CommandDataIB
    w.end_container(); // InvokeRequests
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
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
}
