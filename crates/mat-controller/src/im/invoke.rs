//! InvokeRequest / InvokeResponse / TimedRequest / StatusResponse の
//! encode / decode。コントローラ側の invoke と、device 側の InvokeRequest
//! 受理 / InvokeResponse 送出の両方向。

use crate::tlv::{copy_value, Reader, Tag, Value, Writer};

use super::{
    expect_struct_start, skip_container, ImError, InvokeOutcome, InvokeResponseData, IM_REVISION,
};

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

/// Decoded InvokeRequestMessage for a single command: server-side
/// counterpart of `encode_invoke_request`/`encode_invoke_request_timed`.
/// `fields_tlv` is empty when the request carried no CommandFields.
#[derive(Debug, Clone, PartialEq)]
pub struct InvokeRequestIn {
    pub endpoint: u16,
    pub cluster: u32,
    pub command: u32,
    pub fields_tlv: Vec<u8>,
    pub suppress_response: bool,
    pub timed: bool,
}

/// `decode_request_command_data_ib`'s return: (endpoint, cluster, command,
/// fields_tlv).
type RequestCommandDataFields = (Option<u16>, Option<u32>, Option<u32>, Vec<u8>);

/// CommandDataIB (spec §8.9.4.2): `{0: CommandPath{0:endpoint,1:cluster,
/// 2:command}, 1: CommandFields}`, request-side variant that also extracts
/// the path (`decode_command_data_ib` only extracts fields, for the
/// response side where the path is already known to the caller). Assumes
/// the caller already consumed the anonymous `StructStart` opening this
/// CommandDataIB (an InvokeRequests entry).
fn decode_request_command_data_ib(r: &mut Reader) -> Result<RequestCommandDataFields, ImError> {
    let mut endpoint = None;
    let mut cluster = None;
    let mut command = None;
    let mut fields_tlv = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command data ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => {
                // CommandPath
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated command path"))?;
                    match (e2.tag, e2.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(0), Value::Uint(v)) => {
                            endpoint = Some(u16::try_from(v).map_err(|_| {
                                ImError::Malformed("command path endpoint out of range")
                            })?);
                        }
                        (Tag::Context(1), Value::Uint(v)) => {
                            cluster = Some(u32::try_from(v).map_err(|_| {
                                ImError::Malformed("command path cluster out of range")
                            })?);
                        }
                        (Tag::Context(2), Value::Uint(v)) => {
                            command = Some(u32::try_from(v).map_err(|_| {
                                ImError::Malformed("command path command out of range")
                            })?);
                        }
                        (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                            skip_container(r)?;
                        }
                        _ => {}
                    }
                }
            }
            (Tag::Context(1), Value::StructStart) => {
                // CommandFields: re-tag to Anonymous, same convention as
                // `decode_command_data_ib`'s response-side echo.
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, Value::StructStart)?;
                fields_tlv = w.finish();
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Ok((endpoint, cluster, command, fields_tlv))
}

/// InvokeRequestMessage (spec §8.9.4): server-side decode of
/// `encode_invoke_request`/`encode_invoke_request_timed`'s payload. Only
/// the first InvokeRequestIB is interpreted (mirrors `decode_invoke_response`'s
/// single-command scope).
pub fn decode_invoke_request(payload: &[u8]) -> Result<InvokeRequestIn, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut suppress_response = false;
    let mut timed = false;
    let mut endpoint = None;
    let mut cluster = None;
    let mut command = None;
    let mut fields_tlv = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Bool(b)) => suppress_response = b,
            (Tag::Context(1), Value::Bool(b)) => timed = b,
            (Tag::Context(2), Value::ArrayStart) => {
                // InvokeRequests
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke requests"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            let (ep, cl, cmd, fields) = decode_request_command_data_ib(&mut r)?;
                            endpoint = ep;
                            cluster = cl;
                            command = cmd;
                            fields_tlv = fields;
                            first = false;
                        }
                        Value::StructStart => skip_container(&mut r)?,
                        _ => {
                            return Err(ImError::Malformed("unexpected element in invoke requests"))
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
    Ok(InvokeRequestIn {
        endpoint: endpoint.ok_or(ImError::Malformed("invoke request without endpoint"))?,
        cluster: cluster.ok_or(ImError::Malformed("invoke request without cluster"))?,
        command: command.ok_or(ImError::Malformed("invoke request without command"))?,
        fields_tlv,
        suppress_response,
        timed,
    })
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

/// InvokeResponseMessage (spec §8.9.4) for a single command's
/// CommandStatusIB (status, not data): server-side counterpart of
/// `decode_invoke_response`/`decode_invoke_response_data`. Echoes the
/// CommandPath (spec §8.9.4.2) so a well-behaved controller can correlate
/// the status against the command it invoked.
///
/// `SuppressResponse`（タグ 0, bool）は spec §8.9.4 で **mandatory**。
/// 自前の decoder は未知タグを読み飛ばすので欠けても往復は通るが、chip の
/// `CommandSender::ProcessInvokeResponse` は `GetSuppressResponse` を必ず
/// 引き、無ければ `CHIP Error 0x00000021: End of TLV` で invoke を失敗に
/// する（M2 ゲート 1 の実測 —
/// `docs/superpowers/plans/m2-chip-tool-probe.md`）。デバイス側の応答は
/// 常に `false`（応答を出している時点で抑制していない）。
pub fn encode_invoke_response_status(
    endpoint: u16,
    cluster: u32,
    command: u32,
    status: u8,
    cluster_status: Option<u8>,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse — mandatory, see below
    w.start_array(Tag::Context(1)); // InvokeResponses
    w.start_struct(Tag::Anonymous); // InvokeResponseIB
    w.start_struct(Tag::Context(1)); // CommandStatusIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(command));
    w.end_container(); // CommandPath
    w.start_struct(Tag::Context(1)); // StatusIB
    w.put_uint(Tag::Context(0), u64::from(status));
    if let Some(cs) = cluster_status {
        w.put_uint(Tag::Context(1), u64::from(cs));
    }
    w.end_container(); // StatusIB
    w.end_container(); // CommandStatusIB
    w.end_container(); // InvokeResponseIB
    w.end_container(); // InvokeResponses
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
}

/// InvokeResponseMessage (spec §8.9.4) for a single command's CommandDataIB
/// (a successful invocation that returns data — e.g. a cluster's response
/// command). `fields_tlv` must be one complete, well-formed TLV element
/// (any top-level tag; re-tagged on splice) holding the response
/// CommandFields struct, or an empty slice for a data response with no
/// fields. `response_command` goes in the echoed CommandPath's CommandId,
/// same field `decode_command_data_ib`'s caller ignores today (it only
/// needs the fields) but that a spec-faithful controller would use to
/// distinguish response commands from the invoked one. `SuppressResponse`
/// は `encode_invoke_response_status` と同じ理由で常に書き出す（その doc
/// コメント参照）。
pub fn encode_invoke_response_data(
    endpoint: u16,
    cluster: u32,
    response_command: u32,
    fields_tlv: &[u8],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false); // SuppressResponse — mandatory, see below
    w.start_array(Tag::Context(1)); // InvokeResponses
    w.start_struct(Tag::Anonymous); // InvokeResponseIB
    w.start_struct(Tag::Context(0)); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPath
    w.put_uint(Tag::Context(0), u64::from(endpoint));
    w.put_uint(Tag::Context(1), u64::from(cluster));
    w.put_uint(Tag::Context(2), u64::from(response_command));
    w.end_container(); // CommandPath
    if !fields_tlv.is_empty() {
        w.put_raw_element(Tag::Context(1), fields_tlv); // CommandFields
    }
    w.end_container(); // CommandDataIB
    w.end_container(); // InvokeResponseIB
    w.end_container(); // InvokeResponses
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container(); // outer struct
    w.finish()
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
    use crate::im::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

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

    // Task 7: server-direction codecs, checked against the pre-existing
    // client-direction halves (not just self-inverse).

    #[test]
    fn invoke_request_roundtrip() {
        let payload = encode_invoke_request(1, 0x0006, 1, None);
        let req = decode_invoke_request(&payload).unwrap();
        assert_eq!((req.endpoint, req.cluster, req.command), (1, 0x0006, 1));
        assert!(req.fields_tlv.is_empty());
        assert!(!req.suppress_response);
        assert!(!req.timed);
    }

    #[test]
    fn invoke_request_roundtrip_with_fields() {
        let mut fw = Writer::new();
        fw.start_struct(Tag::Anonymous);
        fw.put_uint(Tag::Context(0), 42);
        fw.end_container();
        let fields = fw.finish();
        let payload =
            encode_invoke_request(1, CLUSTER_LEVEL_CONTROL, CMD_MOVE_TO_LEVEL, Some(&fields));
        let req = decode_invoke_request(&payload).unwrap();
        assert_eq!(
            (req.endpoint, req.cluster, req.command),
            (1, CLUSTER_LEVEL_CONTROL, CMD_MOVE_TO_LEVEL)
        );
        let mut r = Reader::new(&req.fields_tlv);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(j["0"], serde_json::json!(42));
    }

    #[test]
    fn invoke_response_status_decodes_with_client_decoder() {
        let payload = encode_invoke_response_status(1, 0x0006, 1, 0, None);
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, 0);
    }

    /// `SuppressResponse`（タグ 0, bool）は InvokeResponseMessage の
    /// **mandatory** フィールド（spec §8.9.4）。自前の decoder は未知タグを
    /// 読み飛ばすので欠けていても往復テストは通るが、chip の
    /// `CommandSender::ProcessInvokeResponse` は `GetSuppressResponse` で
    /// タグを引きに行き、無ければ `CHIP Error 0x00000021: End of TLV` を
    /// 返して invoke ごと失敗にする（M2 ゲート 1 の実測 —
    /// `docs/superpowers/plans/m2-chip-tool-probe.md`）。ワイヤ形状を直接
    /// 検査する。
    #[test]
    fn invoke_responses_always_carry_suppress_response() {
        for payload in [
            encode_invoke_response_status(1, 0x0006, 1, 0, None),
            encode_invoke_response_data(1, CLUSTER_ON_OFF, 0x00, &[]),
        ] {
            let mut r = Reader::new(&payload);
            expect_struct_start(&mut r).unwrap();
            let first = r.next().unwrap().unwrap();
            assert_eq!(
                (first.tag, first.value),
                (Tag::Context(0), Value::Bool(false)),
                "InvokeResponseMessage must open with SuppressResponse=false: {payload:02X?}"
            );
        }
    }

    #[test]
    fn invoke_response_status_carries_cluster_status() {
        let payload =
            encode_invoke_response_status(1, 0x0006, 1, STATUS_UNSUPPORTED_COMMAND, Some(0x42));
        let out = decode_invoke_response(&payload).unwrap();
        assert_eq!(out.status, STATUS_UNSUPPORTED_COMMAND);
        assert_eq!(out.cluster_status, Some(0x42));
        let data = decode_invoke_response_data(&payload).unwrap();
        assert_eq!(data.status, STATUS_UNSUPPORTED_COMMAND);
        assert_eq!(data.cluster_status, Some(0x42));
        assert!(data.fields_tlv.is_none());
    }

    #[test]
    fn invoke_response_data_decodes_with_client_decoder() {
        let mut fw = Writer::new();
        fw.start_struct(Tag::Anonymous);
        fw.put_bool(Tag::Context(0), true);
        fw.end_container();
        let fields = fw.finish();
        let payload = encode_invoke_response_data(1, CLUSTER_ON_OFF, 0x00, &fields);
        let data = decode_invoke_response_data(&payload).unwrap();
        assert_eq!(data.status, 0);
        let fields_tlv = data.fields_tlv.expect("expected CommandFields");
        let mut r = Reader::new(&fields_tlv);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(j["0"], serde_json::json!(true));
    }
}
