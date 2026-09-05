//! session サブモジュールのテストが共有する定数・鍵・datagram ビルダ。
//! `#[cfg(test)]` 専用（mod.rs 側で `#[cfg(test)] mod test_util;`）。

use std::sync::Arc;
use std::time::Duration;

use crate::crypto::seal_message;
use crate::exchange::MrpConfig;
use crate::message::{Destination, MessageHeader, ProtocolHeader};
use crate::transport::{ReliableChannel, Transport, UdpTransport, RELIABLE_PEER};

use super::{SecureSession, SessionKeys};

pub(super) const I2R: [u8; 16] = [0x11; 16];
pub(super) const R2I: [u8; 16] = [0x22; 16];
pub(super) const OUR_NODE: u64 = 0xAAAA;
pub(super) const DEV_NODE: u64 = 0xBBBB;
pub(super) const LOCAL_SID: u16 = 0x1234;
pub(super) const PEER_SID: u16 = 0x5678;

pub(super) fn keys() -> SessionKeys {
    SessionKeys {
        i2r: I2R,
        r2i: R2I,
        attestation_challenge: [0; 16],
    }
}

pub(super) fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: Duration::from_millis(50),
        active_interval: Duration::from_millis(50),
        max_retries: 2,
        backoff: 1.0,
        jitter: 0.0,
    }
}

pub(super) async fn bind_local() -> UdpTransport {
    UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap()
}

/// デバイス→controller のセキュアデータグラムを作る。
pub(super) fn device_datagram(
    exchange_id: u16,
    protocol_id: u16,
    opcode: u8,
    acked: Option<u32>,
    needs_ack: bool,
    counter: u32,
    payload: &[u8],
) -> Vec<u8> {
    let header = MessageHeader {
        session_id: LOCAL_SID, // デバイスは「こちらの」session id 宛に送る
        security_flags: 0,
        message_counter: counter,
        source_node_id: None,
        destination: Destination::None,
    };
    let proto = ProtocolHeader {
        initiator: false,
        needs_ack,
        acked_counter: acked,
        opcode,
        exchange_id,
        protocol_id,
        vendor_id: None,
    };
    seal_message(&R2I, &header, &proto, payload, DEV_NODE).unwrap()
}

/// デバイス側で受信 → 復号して (header, proto) を返す。
pub(super) fn open_from_controller(buf: &[u8]) -> (MessageHeader, ProtocolHeader, Vec<u8>) {
    crate::crypto::open_message(&I2R, buf, OUR_NODE).unwrap()
}

/// ReportData shaped like Task 8's `im.rs` test fixture: a single
/// AttributeReportIB for onoff's `OnOff` bool attribute.
pub(super) fn report_data_payload(value: bool, suppress: bool) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
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

/// InvokeResponse shaped like Task 8's `im.rs` test fixture: a single
/// successful InvokeResponseIB (status 0, no cluster status).
pub(super) fn invoke_response_success_payload() -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
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
    w.finish()
}

/// InvokeResponse (error) shaped like Task 8's `im.rs` test fixture:
/// CommandStatusIB carrying `StatusIB{0: status, 1: cluster_status}`.
pub(super) fn invoke_response_error_payload(status: u8, cluster_status: Option<u8>) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false);
    w.start_array(Tag::Context(1));
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
    w.start_list(Tag::Context(0)); // Path
    w.end_container();
    w.start_struct(Tag::Context(1)); // StatusIB
    w.put_uint(Tag::Context(0), u64::from(status));
    if let Some(cs) = cluster_status {
        w.put_uint(Tag::Context(1), u64::from(cs));
    }
    w.end_container();
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// InvokeResponse carrying CommandFields (a data-returning command),
/// shaped like Task 7's `im.rs` fixture: a single successful
/// InvokeResponseIB whose Command is a CommandDataIB with fields.
pub(super) fn invoke_response_with_fields_payload() -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), false);
    w.start_array(Tag::Context(1));
    w.start_struct(Tag::Anonymous); // InvokeResponseIB
    w.start_struct(Tag::Context(0)); // CommandDataIB
    w.start_list(Tag::Context(0)); // CommandPathIB
    w.put_uint(Tag::Context(0), 1);
    w.put_uint(Tag::Context(1), 0x0300);
    w.put_uint(Tag::Context(2), 0x00);
    w.end_container();
    w.start_struct(Tag::Context(1)); // CommandFields
    w.put_uint(Tag::Context(0), 42);
    w.end_container();
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// WriteResponse shaped like Task 5's `im.rs` fixture
/// (`decode_write_response_returns_first_status`): a single
/// AttributeStatusIB with the given status.
pub(super) fn write_response_payload(status: u8) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(0));
    w.start_struct(Tag::Anonymous);
    w.start_list(Tag::Context(0)); // path
    w.end_container();
    w.start_struct(Tag::Context(1)); // StatusIB
    w.put_uint(Tag::Context(0), u64::from(status));
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// Single-attribute ReportData for `read_cluster_json_merges_two_chunks`'s
/// first chunk: one AttributeDataIB (Replace, scalar value).
pub(super) fn report_data_message_attr(
    endpoint: u16,
    cluster: u32,
    attr: u32,
    value: u64,
    more_chunks: bool,
    suppress: bool,
) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(1)); // AttributeReportIBs
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1)); // AttributeDataIB
    w.put_uint(Tag::Context(0), 1); // DataVersion
    w.start_list(Tag::Context(1)); // Path
    w.put_uint(Tag::Context(2), u64::from(endpoint));
    w.put_uint(Tag::Context(3), u64::from(cluster));
    w.put_uint(Tag::Context(4), u64::from(attr));
    w.end_container();
    w.put_uint(Tag::Context(2), value); // Data
    w.end_container();
    w.end_container();
    w.end_container();
    if more_chunks {
        w.put_bool(Tag::Context(3), true);
    }
    if suppress {
        w.put_bool(Tag::Context(4), true);
    }
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// ReportData for `read_cluster_json_merges_two_chunks`'s second (final)
/// chunk: 2 AttributeReportIBs for the same attribute, both list-append
/// (ListIndex = null), matching Task 4's
/// `merge_reports_joins_chunked_list_appends` fixture shape.
pub(super) fn report_data_message_attr_list_append_2(
    endpoint: u16,
    cluster: u32,
    attr: u32,
    v1: u64,
    v2: u64,
    suppress: bool,
) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    fn path(w: &mut Writer, endpoint: u16, cluster: u32, attr: u32) {
        w.start_list(Tag::Context(1));
        w.put_uint(Tag::Context(2), u64::from(endpoint));
        w.put_uint(Tag::Context(3), u64::from(cluster));
        w.put_uint(Tag::Context(4), u64::from(attr));
        w.put_null(Tag::Context(5)); // ListIndex = null -> append
        w.end_container();
    }
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_array(Tag::Context(1));
    for v in [v1, v2] {
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        path(&mut w, endpoint, cluster, attr);
        w.put_uint(Tag::Context(2), v);
        w.end_container();
        w.end_container();
    }
    w.end_container();
    if suppress {
        w.put_bool(Tag::Context(4), true);
    }
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// ReliableChannel ペアで SecureSession（controller 側）と生 Transport（device 側）を組む。
pub(super) fn reliable_session_pair() -> (SecureSession, Transport) {
    let (a, b) = ReliableChannel::pair();
    let s = SecureSession::new(
        Arc::new(a),
        RELIABLE_PEER,
        LOCAL_SID,
        PEER_SID,
        keys(),
        OUR_NODE,
        DEV_NODE,
    );
    (s, b)
}

/// 購読 priming 用 ReportData payload（subscription_id 付き、more 指定可）。
pub(super) fn subscription_report_payload(sub_id: u32, value: bool, more: bool) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(sub_id));
    w.start_array(Tag::Context(1));
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1));
    w.put_uint(Tag::Context(0), 1);
    w.start_list(Tag::Context(1));
    w.put_uint(Tag::Context(2), 1);
    w.put_uint(Tag::Context(3), 6);
    w.put_uint(Tag::Context(4), 0);
    w.end_container();
    w.put_bool(Tag::Context(2), value);
    w.end_container();
    w.end_container();
    w.end_container();
    if more {
        w.put_bool(Tag::Context(3), true);
    }
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// keep-alive（空 report）payload。
pub(super) fn keepalive_payload(sub_id: u32) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(sub_id));
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// SubscribeResponse payload。
pub(super) fn subscribe_response_payload(sub_id: u32, max_interval: u16) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(sub_id));
    w.put_uint(Tag::Context(2), u64::from(max_interval));
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// InvokeResponse (status=0) payload, built with Task 7's server-side
/// encoder — `decode_invoke_response` only reads the status out (the
/// echoed CommandPath is skipped), so `outcome.status == 0` either way.
pub(super) fn invoke_response_status_ok() -> Vec<u8> {
    crate::im::encode_invoke_response_status(1, 0x0006, 1, 0, None)
}
