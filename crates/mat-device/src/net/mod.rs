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

use mat_controller::exchange::{IncomingMessage, MrpConfig};
use mat_controller::message::{
    MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK, PROTOCOL_ID_SECURE_CHANNEL,
};
use mat_controller::transport::{Transport, MAX_DATAGRAM};

/// The loopback MRP regime every in-crate driver and test uses: 50 ms
/// intervals, no jitter, four retries with mild backoff — fast enough for
/// a unit test, tolerant enough for a real one-shot handshake driver
/// (`net::pase` / `net::case`). One definition so the six former copies
/// cannot drift apart again.
pub fn fast_cfg() -> MrpConfig {
    MrpConfig {
        initial_interval: std::time::Duration::from_millis(50),
        active_interval: std::time::Duration::from_millis(50),
        max_retries: 4,
        backoff: 1.2,
        jitter: 0.0,
    }
}

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
