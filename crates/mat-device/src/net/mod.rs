//! I/O layer (tokio, UDP transport, mDNS advertisement) built on top of
//! `core`. Gated behind the `net` feature (default on).

pub mod case;
pub mod endpoint_ledger;
pub mod group_rx;
pub mod mdns;
pub mod pase;
pub(crate) mod runtime;
pub mod stimulus;
pub mod store;
pub mod subscription;

use std::net::SocketAddr;

use mat_controller::exchange::IncomingMessage;
use mat_controller::message::{
    MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_SECURE_CHANNEL,
};
use mat_controller::transport::{Transport, MAX_DATAGRAM};

/// Reads the very first unsecured datagram from any sender — there's no
/// `ResponderExchange` yet to hand this off to (that's what `adopt` is
/// for), so this is a one-shot raw read, not a loop: PASE/CASE responder
/// drivers serve one initiator at a time and each handles exactly one
/// handshake. Malformed datagrams and standalone acks (which can't
/// legitimately arrive before any exchange exists) are skipped. Shared by
/// `net::pase` and `net::case` (each maps the `io::Error` into its own
/// error enum via `From`).
pub(crate) async fn recv_first(
    transport: &Transport,
) -> std::io::Result<(IncomingMessage, SocketAddr)> {
    loop {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = transport.recv_from(&mut buf).await?;
        let Ok((header, off)) = MessageHeader::decode(&buf[..n]) else {
            continue;
        };
        if header.session_id != 0 || header.security_flags != 0 {
            continue;
        }
        let Ok((proto, body_off)) = ProtocolHeader::decode(&buf[off..n]) else {
            continue;
        };
        if !proto.initiator {
            continue;
        }
        if proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
            && proto.opcode == OPCODE_MRP_STANDALONE_ACK
        {
            continue;
        }
        return Ok((
            IncomingMessage {
                header,
                proto,
                payload: buf[off + body_off..n].to_vec(),
            },
            from,
        ));
    }
}
