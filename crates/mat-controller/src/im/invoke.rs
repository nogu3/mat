//! InvokeRequest / InvokeResponse / TimedRequest / StatusResponse の
//! encode / decode。コントローラ側の invoke と、device 側の InvokeRequest
//! 受理 / InvokeResponse 送出の両方向。

use crate::tlv::{copy_value, Reader, StructFields, Tag, TlvError, Value, Writer};

use super::{
    expect_struct_start, put_status_ib, skip_container, ImError, InvokeOutcome, InvokeResponseData,
    IM_REVISION,
};

/// `StructFields`（`next_scalar`/`next_field`）の走査 `Err` を `ImError` に写す。
/// `ContainerEnd` 到達前の入力終端はすべて `Truncated` として `truncated`
/// ラベルの `Malformed` に、それ以外の TLV デコードエラーはそのまま `Tlv` に
/// 渡す。`pase.rs`/`case/wire.rs` の同名ヘルパーと同じ役割。im/write.rs
/// 等の他の IM ファイルも `use super::invoke::field_err;` でこれを共有する
/// （struct 走査 helper を一箇所に集約するため）。
pub(super) fn field_err(truncated: &'static str) -> impl Fn(TlvError) -> ImError {
    move |e| match e {
        TlvError::Truncated => ImError::Malformed(truncated),
        other => ImError::Tlv(other),
    }
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
    let mut f = StructFields::inside(r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated command data ib"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(0), Value::ListStart) => {
                // CommandPath: 中身は全部スカラーなので next_scalar でよい
                // （未知の入れ子は自動で skip される）。
                let mut cp = StructFields::inside(f.reader());
                while let Some(e2) = cp
                    .next_scalar()
                    .map_err(field_err("truncated command path"))?
                {
                    match (e2.tag, e2.value) {
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
                        _ => {}
                    }
                }
            }
            (Tag::Context(1), Value::StructStart) => {
                // CommandFields: re-tag to Anonymous, same convention as
                // `decode_command_data_ib`'s response-side echo.
                let mut w = Writer::new();
                copy_value(&mut w, f.reader(), Tag::Anonymous, Value::StructStart)?;
                fields_tlv = w.finish();
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
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
    let mut f = StructFields::inside(&mut r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated invoke request"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(0), Value::Bool(b)) => suppress_response = b,
            (Tag::Context(1), Value::Bool(b)) => timed = b,
            (Tag::Context(2), Value::ArrayStart) => {
                // InvokeRequests: array element walk stays hand-rolled (only
                // the first entry is interpreted, rest are skipped).
                let r = f.reader();
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke requests"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            let (ep, cl, cmd, fields) = decode_request_command_data_ib(r)?;
                            endpoint = ep;
                            cluster = cl;
                            command = cmd;
                            fields_tlv = fields;
                            first = false;
                        }
                        Value::StructStart => skip_container(r)?,
                        _ => {
                            return Err(ImError::Malformed("unexpected element in invoke requests"))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
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
    let mut f = StructFields::inside(r);
    while let Some(el) = f.next_scalar().map_err(field_err("truncated status ib"))? {
        match (el.tag, el.value) {
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
    let mut f = StructFields::inside(r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated command status ib"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(1), Value::StructStart) => {
                result = Some(decode_status_ib(f.reader())?);
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
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
    let mut f = StructFields::inside(r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated invoke response ib"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(0), Value::StructStart) => {
                // Command (CommandDataIB): a response carrying data is a
                // successful invocation. M2's onoff commands never produce
                // one, but don't choke on a well-formed message that does.
                skip_container(f.reader())?;
                outcome = Some(InvokeOutcome {
                    status: 0,
                    cluster_status: None,
                });
            }
            (Tag::Context(1), Value::StructStart) => {
                let (status, cluster_status) = decode_command_status_ib(f.reader())?;
                outcome = Some(InvokeOutcome {
                    status,
                    cluster_status,
                });
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
            }
            _ => {}
        }
    }
    outcome.ok_or(ImError::Malformed(
        "invoke response ib without Command or Status",
    ))
}

/// Shared outer walk of an InvokeResponseMessage (spec §8.9.4): finds the
/// `InvokeResponses` array (tag 1) and decodes only its first
/// InvokeResponseIB with `decode_ib`, skipping every other element and any
/// later responses (M2 invokes one command at a time). Common body of
/// `decode_invoke_response` / `decode_invoke_response_data`, which differ
/// only in which IB decoder they pass in.
fn decode_first_invoke_response_ib<T>(
    payload: &[u8],
    decode_ib: fn(&mut Reader) -> Result<T, ImError>,
) -> Result<T, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut result: Option<T> = None;
    let mut f = StructFields::inside(&mut r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated invoke response"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(1), Value::ArrayStart) => {
                // InvokeResponses: array element walk stays hand-rolled.
                let r = f.reader();
                let mut first = true;
                loop {
                    let e2 = r
                        .next()?
                        .ok_or(ImError::Malformed("truncated invoke responses"))?;
                    match e2.value {
                        Value::ContainerEnd => break,
                        Value::StructStart if first => {
                            result = Some(decode_ib(r)?);
                            first = false;
                        }
                        Value::StructStart => skip_container(r)?,
                        _ => {
                            return Err(ImError::Malformed(
                                "unexpected element in invoke responses",
                            ))
                        }
                    }
                }
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
            }
            _ => {}
        }
    }
    result.ok_or(ImError::Malformed(
        "invoke response without InvokeResponseIB",
    ))
}

/// InvokeResponseMessage (spec §8.9.4). Only the first InvokeResponseIB is
/// interpreted (M2 invokes one command at a time).
pub fn decode_invoke_response(payload: &[u8]) -> Result<InvokeOutcome, ImError> {
    decode_first_invoke_response_ib(payload, decode_invoke_response_ib)
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
    let mut f = StructFields::inside(r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated command data ib"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(1), Value::StructStart) => {
                // CommandFields: always a struct (cluster spec command
                // parameters). Re-tag to Anonymous, same convention as
                // `encode_invoke_request`'s fields_tlv splice.
                let mut w = Writer::new();
                copy_value(&mut w, f.reader(), Tag::Anonymous, Value::StructStart)?;
                fields = Some(w.finish());
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
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
    let mut f = StructFields::inside(r);
    while let Some(el) = f
        .next_field()
        .map_err(field_err("truncated invoke response ib"))?
    {
        match (el.tag, el.value) {
            (Tag::Context(0), Value::StructStart) => {
                // Command (CommandDataIB): a response carrying data is a
                // successful invocation (status 0), possibly with fields.
                let fields_tlv = decode_command_data_ib(f.reader())?;
                result = Some(InvokeResponseData {
                    status: 0,
                    cluster_status: None,
                    fields_tlv,
                });
            }
            (Tag::Context(1), Value::StructStart) => {
                let (status, cluster_status) = decode_command_status_ib(f.reader())?;
                result = Some(InvokeResponseData {
                    status,
                    cluster_status,
                    fields_tlv: None,
                });
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(f.reader())?;
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
    decode_first_invoke_response_ib(payload, decode_invoke_response_ib_data)
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
    put_status_ib(&mut w, Tag::Context(1), status, cluster_status); // StatusIB
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
    let mut f = StructFields::inside(&mut r);
    while let Some(el) = f
        .next_scalar()
        .map_err(field_err("truncated status response"))?
    {
        if let (Tag::Context(0), Value::Uint(v)) = (el.tag, el.value) {
            status = Some(
                u8::try_from(v)
                    .map_err(|_| ImError::Malformed("status response code out of range"))?,
            );
        }
    }
    status.ok_or(ImError::Malformed("status response without status"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::*;
    use crate::tlv::{Reader, Tag, TlvError, Value, Writer};

    /// `Writer` で struct を組み、先頭の `StructStart`（Tag::Anonymous、1 byte）
    /// だけを剥がして、「呼び出し元が StructStart を消費済み」という decoder
    /// の前提に合わせたバイト列を作る。末尾の自分自身の `ContainerEnd` は
    /// decoder 自身が読み切る対象なので残す。`decode_status_ib` 等、
    /// `r: &mut Reader` を直接受ける private decoder を単体で叩くのに使う。
    fn inner_bytes(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        f(&mut w);
        w.end_container();
        let full = w.finish();
        full[1..].to_vec()
    }

    /// InvokeRequestMessage: `struct{0:false,1:false,2:[ struct{ <cmd_data> } ]}`
    /// — CommandDataIB の中身だけ `cmd_data` で差し替える。
    fn invoke_request_with(cmd_data: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.put_bool(Tag::Context(1), false);
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous);
        cmd_data(&mut w);
        w.end_container();
        w.end_container();
        w.end_container();
        w.finish()
    }

    /// `StructFields` 置換前の手書き走査と同じ `ImError` を出すことを固定する。
    /// 唯一の意図的な
    /// 差分（`// accepted delta` 印）は、struct フィールド走査中に *要素自体・
    /// 未知の入れ子コンテナが途中で切れる* ケース: 以前は Reader の
    /// `Truncated` を素通しで `Tlv(Truncated)` にするか、`skip_container` の
    /// `Malformed("truncated container")` に畳んでいたが、
    /// `StructFields::next_scalar`/`next_field` はどちらも入力終端と同じ
    /// `Truncated` を返すため、その struct 呼び出し元ごとの `"truncated X"`
    /// ラベルに統一される。どちらも mat-native 側のエラー種別
    /// (`errmap.rs:92`) は `Tlv`/`Malformed` を同じ種類にまとめるので detail
    /// 文言のみの差分。
    #[test]
    fn decoder_error_labels_are_stable() {
        // --- top-level struct check (expect_struct_start, 4 エントリポイント共通) ---
        assert_eq!(
            decode_invoke_request(&[]).unwrap_err(),
            ImError::Malformed("empty payload")
        );
        assert_eq!(
            decode_invoke_request(&[0x04, 0x2A]).unwrap_err(),
            ImError::Malformed("expected struct")
        );
        assert_eq!(
            decode_invoke_response(&[]).unwrap_err(),
            ImError::Malformed("empty payload")
        );
        assert_eq!(
            decode_invoke_response(&[0x04, 0x2A]).unwrap_err(),
            ImError::Malformed("expected struct")
        );
        assert_eq!(
            decode_invoke_response_data(&[]).unwrap_err(),
            ImError::Malformed("empty payload")
        );
        assert_eq!(
            decode_status_response(&[]).unwrap_err(),
            ImError::Malformed("empty payload")
        );
        assert_eq!(
            decode_status_response(&[0x04, 0x2A]).unwrap_err(),
            ImError::Malformed("expected struct")
        );

        // --- struct フィールド走査: ContainerEnd なしで入力終端 ---
        assert_eq!(
            decode_invoke_request(&[0x15]).unwrap_err(),
            ImError::Malformed("truncated invoke request")
        );
        assert_eq!(
            decode_invoke_response(&[0x15]).unwrap_err(),
            ImError::Malformed("truncated invoke response")
        );
        assert_eq!(
            decode_status_response(&[0x15]).unwrap_err(),
            ImError::Malformed("truncated status response")
        );

        // --- struct フィールド走査中の Reader エラー（予約 element type）は Tlv のまま ---
        assert_eq!(
            decode_invoke_request(&[0x15, 0x19, 0x18]).unwrap_err(),
            ImError::Tlv(TlvError::InvalidType(0x19))
        );
        assert_eq!(
            decode_status_response(&[0x15, 0x19, 0x18]).unwrap_err(),
            ImError::Tlv(TlvError::InvalidType(0x19))
        );

        // --- accepted delta (a): 要素自体が途中で切れている ---
        // 旧: r.next()? の Truncated がそのまま Tlv(Truncated) に素通しされて
        // いた。新: StructFields::next_field も同じ Truncated を返すが、
        // field_err が呼び出し元ごとの "truncated X" ラベルに畳む。
        assert_eq!(
            decode_invoke_request(&[0x15, 0x24]).unwrap_err(),
            ImError::Malformed("truncated invoke request") // accepted delta: 以前は Tlv(Truncated)
        );
        assert_eq!(
            decode_status_response(&[0x15, 0x24]).unwrap_err(),
            ImError::Malformed("truncated status response") // accepted delta: 以前は Tlv(Truncated)
        );

        // --- accepted delta (b): 未知の入れ子コンテナが途中で切れている ---
        {
            // struct{ ctx5: struct{ ctx1: 300 } } を組んでから、入れ子 struct と
            // 外側 struct 両方の ContainerEnd を落として「入れ子の中で入力終端」
            // にする。decode_status_response は tag0 (uint) しか知らないので、
            // ctx5 は未知の入れ子コンテナとして skip される。
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.start_struct(Tag::Context(5));
            w.put_uint(Tag::Context(1), 300);
            w.end_container(); // 入れ子 close
            w.end_container(); // 外側 close
            let full = w.finish();
            let cut = &full[..full.len() - 2]; // 両方の ContainerEnd を落とす
            assert_eq!(
                decode_status_response(cut).unwrap_err(),
                ImError::Malformed("truncated status response") // accepted delta: 以前は "truncated container"
            );
        }

        // --- decode_status_ib（"without X" / "out of range" ラベル） ---
        assert_eq!(
            decode_status_ib(&mut Reader::new(&inner_bytes(|_w| {}))).unwrap_err(),
            ImError::Malformed("status ib without status")
        );
        assert_eq!(
            decode_status_ib(&mut Reader::new(&inner_bytes(|w| {
                w.put_uint(Tag::Context(0), 0x100);
            })))
            .unwrap_err(),
            ImError::Malformed("command status code out of range")
        );
        assert_eq!(
            decode_status_ib(&mut Reader::new(&inner_bytes(|w| {
                w.put_uint(Tag::Context(0), 0);
                w.put_uint(Tag::Context(1), 0x100);
            })))
            .unwrap_err(),
            ImError::Malformed("cluster status code out of range")
        );
        assert_eq!(
            decode_status_ib(&mut Reader::new(&[])).unwrap_err(),
            ImError::Malformed("truncated status ib")
        );
        // accepted delta (a) の別例（next_scalar 経由の decode_status_ib）
        assert_eq!(
            decode_status_ib(&mut Reader::new(&[0x24])).unwrap_err(),
            ImError::Malformed("truncated status ib") // accepted delta: 以前は Tlv(Truncated)
        );

        // --- decode_command_status_ib ---
        assert_eq!(
            decode_command_status_ib(&mut Reader::new(&inner_bytes(|_w| {}))).unwrap_err(),
            ImError::Malformed("command status ib without StatusIB")
        );
        assert_eq!(
            decode_command_status_ib(&mut Reader::new(&[])).unwrap_err(),
            ImError::Malformed("truncated command status ib")
        );

        // --- decode_invoke_response_ib ---
        assert_eq!(
            decode_invoke_response_ib(&mut Reader::new(&inner_bytes(|_w| {}))).unwrap_err(),
            ImError::Malformed("invoke response ib without Command or Status")
        );
        assert_eq!(
            decode_invoke_response_ib(&mut Reader::new(&[])).unwrap_err(),
            ImError::Malformed("truncated invoke response ib")
        );

        // --- decode_command_data_ib（response 側） ---
        assert_eq!(
            decode_command_data_ib(&mut Reader::new(&[])).unwrap_err(),
            ImError::Malformed("truncated command data ib")
        );

        // --- decode_invoke_response_ib_data ---
        assert_eq!(
            decode_invoke_response_ib_data(&mut Reader::new(&inner_bytes(|_w| {}))).unwrap_err(),
            ImError::Malformed("invoke response ib without Command or Status")
        );
        assert_eq!(
            decode_invoke_response_ib_data(&mut Reader::new(&[])).unwrap_err(),
            ImError::Malformed("truncated invoke response ib")
        );

        // --- decode_request_command_data_ib（CommandPath の "out of range" / "truncated") ---
        assert_eq!(
            decode_request_command_data_ib(&mut Reader::new(&inner_bytes(|w| {
                w.start_list(Tag::Context(0));
                w.put_uint(Tag::Context(0), 0x1_0000);
                w.end_container();
            })))
            .unwrap_err(),
            ImError::Malformed("command path endpoint out of range")
        );
        assert_eq!(
            decode_request_command_data_ib(&mut Reader::new(&inner_bytes(|w| {
                w.start_list(Tag::Context(0));
                w.put_uint(Tag::Context(0), 1);
                w.put_uint(Tag::Context(1), 0x1_0000_0000);
                w.end_container();
            })))
            .unwrap_err(),
            ImError::Malformed("command path cluster out of range")
        );
        assert_eq!(
            decode_request_command_data_ib(&mut Reader::new(&inner_bytes(|w| {
                w.start_list(Tag::Context(0));
                w.put_uint(Tag::Context(0), 1);
                w.put_uint(Tag::Context(1), 6);
                w.put_uint(Tag::Context(2), 0x1_0000_0000);
                w.end_container();
            })))
            .unwrap_err(),
            ImError::Malformed("command path command out of range")
        );
        assert_eq!(
            decode_request_command_data_ib(&mut Reader::new(&[])).unwrap_err(),
            ImError::Malformed("truncated command data ib")
        );
        {
            // CommandPath list を開いて 1 フィールド書いた後、list 自身の
            // ContainerEnd を落として「CommandPath の中で入力終端」にする。
            let full = inner_bytes(|w| {
                w.start_list(Tag::Context(0));
                w.put_uint(Tag::Context(0), 1);
                w.end_container();
            });
            // list 自身の ContainerEnd と、inner_bytes が付け足した外側
            // wrapper 分の ContainerEnd の両方を落とす。
            let cut = &full[..full.len() - 2];
            assert_eq!(
                decode_request_command_data_ib(&mut Reader::new(cut)).unwrap_err(),
                ImError::Malformed("truncated command path")
            );
        }

        // --- decode_invoke_request: InvokeRequests array（手書き走査のまま） ---
        assert_eq!(
            decode_invoke_request(&invoke_request_with(|_w| {})).unwrap_err(),
            ImError::Malformed("invoke request without endpoint")
        );
        assert_eq!(
            decode_invoke_request(&invoke_request_with(|w| {
                w.start_list(Tag::Context(0));
                w.put_uint(Tag::Context(0), 1);
                w.end_container();
            }))
            .unwrap_err(),
            ImError::Malformed("invoke request without cluster")
        );
        assert_eq!(
            decode_invoke_request(&invoke_request_with(|w| {
                w.start_list(Tag::Context(0));
                w.put_uint(Tag::Context(0), 1);
                w.put_uint(Tag::Context(1), 6);
                w.end_container();
            }))
            .unwrap_err(),
            ImError::Malformed("invoke request without command")
        );
        assert_eq!(
            decode_invoke_request(&[0x15, 0x36, 0x02]).unwrap_err(), // struct{ ctx2: array_start } で入力終端
            ImError::Malformed("truncated invoke requests")
        );
        {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.start_array(Tag::Context(2));
            w.put_uint(Tag::Anonymous, 5); // struct でも ContainerEnd でもない
            w.end_container();
            w.end_container();
            assert_eq!(
                decode_invoke_request(&w.finish()).unwrap_err(),
                ImError::Malformed("unexpected element in invoke requests")
            );
        }

        // --- decode_first_invoke_response_ib（InvokeResponses array、手書き走査のまま） ---
        assert_eq!(
            decode_invoke_response(&[0x15, 0x18]).unwrap_err(),
            ImError::Malformed("invoke response without InvokeResponseIB")
        );
        assert_eq!(
            decode_invoke_response(&[0x15, 0x36, 0x01]).unwrap_err(), // struct{ ctx1: array_start } で入力終端
            ImError::Malformed("truncated invoke responses")
        );
        {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.start_array(Tag::Context(1));
            w.put_uint(Tag::Anonymous, 5);
            w.end_container();
            w.end_container();
            assert_eq!(
                decode_invoke_response(&w.finish()).unwrap_err(),
                ImError::Malformed("unexpected element in invoke responses")
            );
        }
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
